//! 内置本地 HTTP 服务（agent 接入用，不依赖 MCP）。
//!
//! 只绑 127.0.0.1，除 `/v1/health`（存活探测）外全部要求
//! `Authorization: Bearer <token>`。HTTP 走无 UI 路径：截图给坐标直接抓帧落盘，
//! 交互式框选/标注由热键触发 overlay，不经 HTTP。

use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use tauri::{AppHandle, Manager};
use tiny_http::{Header, Method, Response, Server};

use crate::capture::{self, Bounds};
use crate::commands::{captures_dir, RecordState};
use crate::config::AppConfig;
use crate::recorder::{RecordOptions, Recorder};
use crate::{longshot, ocr, uia, window, window_recorder, window_target};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// HTTP 幂等存储（学习面 1：借鉴 Snow Shot 的 revision/幂等设计，落成追加式非破坏能力）。
///
/// 契约：POST 请求带合法 `Idempotency-Key` 头时，同一 (method+path+key) 在 TTL 内重复调用
/// 直接回放首次的 (状态码, 响应体)，不重复执行副作用（截图落盘、起录屏等）。不带该头 = 完全
/// 走旧路径，行为不变——既有 REST 调用方与测试套件零感知。该语义最初以一份独立的
/// Node 参考实现逐条对拍过（参考实现未随本仓库公开）。
/// 本 HTTP 服务的 `incoming_requests` 循环是单线程串行处理，Mutex 仅为封装清晰与未来并发兜底。
struct IdemEntry {
    expires_at_ms: u128,
    status: u16,
    body: String,
}

struct IdemInner {
    map: std::collections::HashMap<String, IdemEntry>,
    order: std::collections::VecDeque<String>,
}

struct IdempotencyStore {
    inner: std::sync::Mutex<IdemInner>,
    max_entries: usize,
    ttl_ms: u128,
}

impl IdempotencyStore {
    fn new(max_entries: usize, ttl_ms: u128) -> Self {
        Self {
            inner: std::sync::Mutex::new(IdemInner {
                map: std::collections::HashMap::new(),
                order: std::collections::VecDeque::new(),
            }),
            max_entries: max_entries.max(1),
            ttl_ms,
        }
    }

    /// 命中且未过期返回 Some((status, body))；未命中/已过期返回 None（过期项顺手清）。
    fn get(&self, key: &str) -> Option<(u16, String)> {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        match g.map.get(key).map(|e| now_ms() > e.expires_at_ms) {
            None => None,
            Some(true) => {
                g.map.remove(key);
                None
            }
            Some(false) => g.map.get(key).map(|e| (e.status, e.body.clone())),
        }
    }

    /// 存首次响应；新键且超容量时按 FIFO 淘汰最旧的仍存活键。
    fn put(&self, key: String, status: u16, body: String) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !g.map.contains_key(&key) && g.map.len() >= self.max_entries {
            while let Some(old) = g.order.pop_front() {
                if g.map.remove(&old).is_some() {
                    break;
                }
            }
        }
        let expires_at_ms = now_ms() + self.ttl_ms;
        if g.map
            .insert(
                key.clone(),
                IdemEntry {
                    expires_at_ms,
                    status,
                    body,
                },
            )
            .is_none()
        {
            g.order.push_back(key);
        }
    }
}

fn now_ms() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 幂等键合法性：非空、长度 ≤200、仅可打印 ASCII(0x20..=0x7E)。
fn validate_idem_key(key: &str) -> bool {
    let len = key.len();
    (1..=200).contains(&len) && key.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// 从请求头取 `Idempotency-Key`（大小写不敏感）。
fn idem_key_of(req: &tiny_http::Request) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv("Idempotency-Key"))
        .map(|h| h.value.as_str().to_string())
}

/// 在 127.0.0.1:port 上建监听 socket 并交给 tiny_http。
///
/// 为什么要自己建：tiny_http 的 `Server::http` 内部就是裸 `TcpListener::bind`，
/// 而 Windows 上不设 `SO_REUSEADDR` ⇒ 上一个实例刚退出、端口还在 TIME_WAIT 时
/// `bind 127.0.0.1:PORT os error 10048`，HTTP 永远起不来（实测：杀进程后立刻重启必现，
/// 只能干等 TIME_WAIT 过去）。这里用 WinSock 自建 socket 并开 SO_REUSEADDR。
fn bind_loopback(port: u16) -> std::io::Result<TcpListener> {
    use std::os::windows::io::FromRawSocket;
    use windows::Win32::Networking::WinSock::{
        bind, htonl, htons, listen, setsockopt, WSASocketW, WSAStartup, AF_INET, INADDR_LOOPBACK,
        IN_ADDR, IN_ADDR_0, IPPROTO_TCP, SOCKADDR, SOCKADDR_IN, SOCK_STREAM, SOL_SOCKET,
        SO_REUSEADDR, WSADATA, WSA_FLAG_OVERLAPPED,
    };
    unsafe {
        // 幂等：已初始化过时再调一次也安全（进程内 Rust std 可能已初始化）。
        let mut wsa = WSADATA::default();
        let _ = WSAStartup(0x0202, &mut wsa);
        let sock = WSASocketW(
            AF_INET.0 as i32,
            SOCK_STREAM.0,
            IPPROTO_TCP.0,
            None,
            0,
            WSA_FLAG_OVERLAPPED,
        )
        .map_err(|e| std::io::Error::other(format!("WSASocketW 失败: {e}")))?;
        let one: i32 = 1;
        if setsockopt(sock, SOL_SOCKET, SO_REUSEADDR, Some(&one.to_ne_bytes())) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let addr = SOCKADDR_IN {
            sin_family: AF_INET,
            sin_port: htons(port),
            sin_addr: IN_ADDR {
                S_un: IN_ADDR_0 {
                    // `INADDR_LOOPBACK` 是**主机序**常量（0x7F000001），而 `S_addr`
                    // 要的是网络序——不套 htonl 会变成 1.0.0.127，bind 报
                    // `os error 10049 WSAEADDRNOTAVAIL`（本机实测踩过）。
                    S_addr: htonl(INADDR_LOOPBACK),
                },
            },
            sin_zero: [0; 8],
        };
        if bind(
            sock,
            &addr as *const SOCKADDR_IN as *const SOCKADDR,
            std::mem::size_of::<SOCKADDR_IN>() as i32,
        ) != 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // SOMAXCONN 在 winsock 头里是历史值 5，太小；用 128 与 tiny_http 默认量级一致。
        if listen(sock, 128) != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(TcpListener::from_raw_socket(sock.0 as _))
    }
}

/// 在后台线程启动 HTTP 服务。
pub fn spawn(cfg: AppConfig, app: AppHandle) {
    thread::Builder::new()
        .name("snappin-http".into())
        .spawn(move || {
            // 抓图侧的进程级设置（当前只消费 JPEG 画质）先灌进去：否则 HTTP 出的第一张图
            // 会用默认值编码，用户在设置页选的画质不生效（2026-10-20 缺陷 D11）。
            capture::apply_config(&cfg);
            let server = match bind_loopback(cfg.port).and_then(|l| {
                Server::from_listener(l, None).map_err(|e| std::io::Error::other(e.to_string()))
            }) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[snappin] HTTP 服务启动失败 127.0.0.1:{}: {e}", cfg.port);
                    return;
                }
            };
            eprintln!("[snappin] HTTP 服务已监听 http://127.0.0.1:{}", cfg.port);
            let token = Arc::new(cfg.token);
            // 幂等存储：仅对带合法 Idempotency-Key 的 POST 生效（下方循环内消费）。
            let idem = IdempotencyStore::new(256, 300_000);
            for mut req in server.incoming_requests() {
                let method = req.method().clone();
                let url = req.url().to_string();
                let path = url.split('?').next().unwrap_or("").to_string();
                // query 单独传：`GET /v1/record/status?session_id=...` 只靠 path 拿不到 session_id，
                // 而路由 match 必须继续按 path 精确匹配（`?` 不能进 key）。
                let query = url
                    .split_once('?')
                    .map(|(_, q)| q)
                    .unwrap_or("")
                    .to_string();

                if method == Method::Get && path == "/v1/health" {
                    let _ = req.respond(json_resp(
                        200,
                        format!(r#"{{"ok":true,"product":"SnapPin","version":"{VERSION}"}}"#),
                    ));
                    continue;
                }

                if !authorized(&req, &token) {
                    let _ = req.respond(json_resp(401, unauthorized()));
                    continue;
                }

                let mut body = String::new();
                let _ = req.as_reader().read_to_string(&mut body);

                // 幂等：仅对带 Idempotency-Key 的 POST 生效；其余请求走旧路径，行为不变。
                let idem_key = if method == Method::Post {
                    idem_key_of(&req)
                } else {
                    None
                };
                if let Some(k) = idem_key {
                    if !validate_idem_key(&k) {
                        let _ = req.respond(json_resp(400, err_json("bad_idempotency_key")));
                        continue;
                    }
                    let scoped = format!("POST:{path}:{k}");
                    if let Some((code, payload)) = idem.get(&scoped) {
                        let _ = req.respond(json_resp_replayed(code, payload));
                        continue;
                    }
                    let (code, payload) = handle(&app, &method, &path, &query, &body);
                    idem.put(scoped, code, payload.clone());
                    let _ = req.respond(json_resp(code, payload));
                    continue;
                }

                let (code, payload) = handle(&app, &method, &path, &query, &body);
                let _ = req.respond(json_resp(code, payload));
            }
        })
        .expect("无法启动 snappin-http 线程");
}

fn handle(app: &AppHandle, method: &Method, path: &str, query: &str, body: &str) -> (u16, String) {
    match (method, path) {
        (Method::Post, "/v1/capture") => capture_ep(app, body),
        (Method::Post, "/v1/record/start") => record_start_ep(app, body),
        (Method::Post, "/v1/record/stop") => record_stop_ep(app, query, body),
        (Method::Post, "/v1/record/pause") => record_pause_ep(app, body, true),
        (Method::Post, "/v1/record/resume") => record_pause_ep(app, body, false),
        (Method::Get, "/v1/record/status") => record_status_ep(query),
        (Method::Get, "/v1/windows") | (Method::Post, "/v1/windows") => windows_ep(),
        (Method::Post, "/v1/uia") => uia_ep(body),
        (Method::Get, "/v1/settings") => settings_get_ep(app),
        (Method::Put, "/v1/settings") | (Method::Post, "/v1/settings") => {
            settings_put_ep(app, body)
        }
        (Method::Post, "/v1/ocr") => ocr_ep(app, body),
        (Method::Post, "/v1/pin") => pin_ep(app, body),
        (Method::Post, "/v1/longshot") => longshot_ep(app, body),
        _ => (404, r#"{"ok":false,"error":"not_found"}"#.into()),
    }
}

/// POST /v1/capture {mode:"fullscreen"|"region"|"window", x,y,w,h, target?, save?=true, copy?=false}
fn capture_ep(app: &AppHandle, body: &str) -> (u16, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let mode = v
        .get("mode")
        .and_then(|m| m.as_str())
        .unwrap_or("fullscreen");
    let save = v.get("save").and_then(|b| b.as_bool()).unwrap_or(true);
    let copy = v.get("copy").and_then(|b| b.as_bool()).unwrap_or(false);

    // 窗口模式：静默 WGC 单帧，走窗口目标解析 + 结构化错误（合同 §7.1/§7.3）。
    if mode == "window" {
        return capture_window_ep(app, &v, save, copy);
    }

    let img = match mode {
        "region" => {
            let b = Bounds {
                x: v.get("x").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
                y: v.get("y").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
                w: v.get("w").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
                h: v.get("h").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
            };
            capture::capture_region(b)
        }
        _ => capture::capture_virtual_screen(),
    };
    let img = match img {
        Ok(i) => i,
        Err(e) => return (500, err_json(&e)),
    };
    let (w, h) = (img.width(), img.height());
    if copy {
        if let Err(e) = capture::copy_to_clipboard(&img) {
            return (500, err_json(&e));
        }
    }
    let mut path_out = String::new();
    if save {
        let dir = captures_dir(app);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return (500, err_json(&e.to_string()));
        }
        // agent 面与 UI 面必须是同一套命名/格式设置：否则设置里切成「序号 + JPG」后，
        // UI 截图是 SnapPin_0007.jpg，agent 走 HTTP 却还是 SnapPin_<秒>.png（2026-09-21 实测发现）。
        let (ext, fmt) = crate::commands::image_ext_and_format(app);
        let p = dir.join(format!(
            "{}.{}",
            crate::commands::product_stem(app, ""),
            ext
        ));
        // 设置页改过画质后 HTTP 面要立刻用新值（画质是进程级状态，抓图前同步一次）。
        refresh_capture_config(app);
        if let Err(e) = capture::save_image(&img, &p, fmt) {
            return (500, err_json(&e));
        }
        path_out = p.to_string_lossy().to_string();
    }
    (
        200,
        serde_json::json!({
            "ok": true,
            "mode": mode,
            "source": if mode == "region" { "gdi_region" } else { "gdi_screen" },
            "path": path_out,
            "width": w,
            "height": h,
            // 降级标记（D1）：全屏/区域是**直接**抓屏，没有「拿到别的画面」的可能，
            // 恒为 false——但字段必须一直在，调用方才能用同一套判据读两种模式。
            "degraded": false,
            "degraded_reason": null,
        })
        .to_string(),
    )
}

/// POST /v1/capture {mode:"window", target:{window_id|hwnd,pid}, save?, copy?, format?}
///
/// 静默窗口截图（合同 §7.1）：WGC 单帧、遮挡免疫；失败一律结构化报错，**绝不**回落成
/// 全屏/区域（合同 §11：目标语义比「总有一个文件」重要）。
fn capture_window_ep(
    app: &AppHandle,
    v: &serde_json::Value,
    save: bool,
    copy: bool,
) -> (u16, String) {
    // 请求字段 `format` 必须真读（合同 §7.1 承诺了它）：白名单 png/jpg，其余 400。
    // 为什么不能静默忽略：调用方按 `{"format":"jpeg"}` 发起却拿到设置里的 .png，
    // 无任何提示 = 假成功（2026-10-15 审查发现 L2）。
    let fmt_req = match v.get("format").and_then(|f| f.as_str()) {
        None => None,
        Some("png") => Some(image::ImageFormat::Png),
        Some("jpg") | Some("jpeg") => Some(image::ImageFormat::Jpeg),
        Some(other) => {
            return (
                400,
                err_detail(
                    "invalid_target",
                    &format!("format 不支持：{other}（只允许 png/jpg）"),
                ),
            )
        }
    };
    // 既不落盘也不复制 = 什么都不会产出：返回 200 + 空 path 是「用成功状态掩盖没做事」
    // （合同 §7.3 禁止）。直接 400（2026-10-15 审查发现 L2）。
    if !save && !copy {
        return (
            400,
            err_detail(
                "invalid_target",
                "save 与 copy 不能同时为 false（无任何产出）",
            ),
        );
    }
    let target = v.get("target").cloned().unwrap_or(serde_json::Value::Null);
    let resolved = match window_target::resolve(&target) {
        Ok(r) => r,
        Err(e) => return (e.status, err_detail(e.code, &e.message)),
    };
    let hwnd = resolved.hwnd;
    let pid = resolved.pid;
    let window_id = window_target::window_id_of(hwnd);

    let shot = match capture::capture_window_marked(hwnd.0 as isize) {
        Ok(s) => s,
        Err(e) => return (status_for_code(e.code), err_detail(e.code, &e.message)),
    };
    // 先把标记取出来再移走图：结构体是部分移动，后面不能再整体用。
    let degraded = shot.degraded;
    let degraded_reason = shot.degraded_reason;
    let img = shot.image;
    let (w, h) = (img.width(), img.height());
    if copy {
        if let Err(e) = capture::copy_to_clipboard(&img) {
            return (500, err_detail("capture_failed", &e));
        }
    }
    let mut path_out = String::new();
    if save {
        let dir = captures_dir(app);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return (500, err_detail("capture_failed", &e.to_string()));
        }
        // 未显式给 format 时沿用设置（与 UI 面/其它 agent 面一致，见 capture_ep 同名注释）；
        // 显式给了就以请求为准。
        let (ext, fmt) = match fmt_req {
            Some(f) => (
                if f == image::ImageFormat::Jpeg {
                    "jpg"
                } else {
                    "png"
                },
                f,
            ),
            None => crate::commands::image_ext_and_format(app),
        };
        let p = dir.join(format!(
            "{}.{}",
            crate::commands::product_stem(app, ""),
            ext
        ));
        // 设置页改过画质后 HTTP 面要立刻用新值（画质是进程级状态，抓图前同步一次）。
        refresh_capture_config(app);
        if let Err(e) = capture::save_image(&img, &p, fmt) {
            return (500, err_detail("capture_failed", &e));
        }
        path_out = p.to_string_lossy().to_string();
    }
    (
        200,
        serde_json::json!({
            "ok": true,
            "mode": "window",
            "source": "wgc_window",
            "path": path_out,
            "width": w,
            "height": h,
            "window_id": window_id,
            "pid": pid,
            // 降级标记（D1）：窗口抓取失败一律结构化报错、**绝不**回落成全屏/区域，
            // 所以成功结果的 degraded 恒为 false；字段常在，调用方才能统一判读，
            // 不用靠「响应长什么样」去猜自己拿的是不是目标窗口。
            "degraded": degraded,
            "degraded_reason": degraded_reason,
        })
        .to_string(),
    )
}

/// 把「活的」配置同步进抓图层（当前只有 JPEG 画质）：设置页改完立刻生效，
/// 不必重启 HTTP 服务或整个应用。
fn refresh_capture_config(app: &AppHandle) {
    if let Some(st) = app.try_state::<crate::commands::ConfigState>() {
        if let Ok(c) = st.0.lock() {
            capture::apply_config(&c);
        }
    }
}

/// 设置里的「录屏录音」开关（`cfg.record_audio`）。取不到配置时按 true：
/// 与 config.rs 的默认值一致——宁可多录，也不要「装完即无声音」（缺陷 D6a/D7）。
fn record_audio_default(app: &AppHandle) -> bool {
    app.try_state::<crate::commands::ConfigState>()
        .and_then(|st| st.0.lock().ok().map(|c| c.record_audio))
        .unwrap_or(true)
}

/// 音频源说明（D7）：告诉调用方这次录到的是系统声音还是麦克风。
///
/// - `system_loopback`：WASAPI loopback 采的系统声（窗口模式固定走这条）
/// - `dshow:<设备名>`：ffmpeg dshow 采的音频输入设备（区域/全屏模式）
/// - `none`：本次没有音轨
fn audio_source_of(device: Option<&str>) -> String {
    match device {
        Some(d) if !d.trim().is_empty() => format!("dshow:{}", d.trim()),
        _ => "none".to_string(),
    }
}

/// 设置里的「码率档位」（`cfg.record_bitrate_k`）。取不到配置时回落到 15000
/// （旧默认，对应「中」档）。请求体显式给了 `bitrate_k` 时以请求为准。
///
/// why-not 固定写死 12000/15000：设置页的码率滑块对 HTTP 调用方不可见，
/// 档位映射（`recorder::quality_for`）又按用户选的档走——不同入口用不同档会让
/// 「同一台机器录出来的大小差很多」变成常态。
fn record_bitrate_default(app: &AppHandle) -> u32 {
    app.try_state::<crate::commands::ConfigState>()
        .and_then(|st| st.0.lock().ok().map(|c| c.record_bitrate_k))
        .unwrap_or(15000)
}

/// 可选的质量档位表（`bitrate_k` 档位键 + 中文名），给调用方看「能选哪几档」。
///
/// why-not 让调用方自己猜：`record_bitrate_k` 现在是**档位键**而不是实际码率，
/// 写别的值会被向下取整到某一档——不把表给出去，调用方只能试。
fn quality_presets_json() -> Vec<serde_json::Value> {
    crate::recorder::QUALITY_PRESETS
        .iter()
        .map(|(k, name)| serde_json::json!({ "bitrate_k": k, "name": name }))
        .collect()
}

/// POST /v1/record/start {mode?, target?, x?,y?,w?,h?,fps?, bitrate_k?, cursor?, audio?, gif?}
///
/// `mode:"window"` → 窗口录屏 session（合同 §7.2，走 `window_recorder`）；
/// 其余取值（含缺省）→ 旧的全屏/区域路径。旧路径唯一的语义变化是反方向互斥（见下），
/// 其余逐字未动。
fn record_start_ep(app: &AppHandle, body: &str) -> (u16, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    if v.get("mode").and_then(|m| m.as_str()) == Some("window") {
        return record_start_window_ep(app, &v);
    }
    // 反方向互斥（合同 §7.3 `recording_exists` = 「当前实现不支持并行录制」，与从哪条路径
    // 发起无关）：窗口 session 与旧的全局 `RecordState` 是两个互不可见的槽位，不在这里堵住
    // 就能同时开两份录制。只在**确实有窗口会话在跑**时改变旧路径行为；没有窗口会话时，
    // 旧路径行为与改动前逐字一致（下方 guard.is_some() 那段仍是原来的判断与文案）。
    if let Some(sid) = window_recorder::active_session_id() {
        return (
            409,
            err_detail(
                "recording_exists",
                &format!("已有窗口录制会话在运行：{sid}，不支持并行录制"),
            ),
        );
    }
    let bounds = match (v.get("w"), v.get("h")) {
        (Some(_), Some(_)) => Some(Bounds {
            x: v.get("x").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
            y: v.get("y").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
            w: v.get("w").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
            h: v.get("h").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        }),
        _ => None,
    };
    let state = app.state::<RecordState>();
    let mut guard = match state.0.lock() {
        Ok(g) => g,
        Err(e) => return (500, err_json(&e.to_string())),
    };
    if guard.is_some() {
        return (409, err_json("已有录制在进行"));
    }
    let dir = captures_dir(app);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (500, err_json(&e.to_string()));
    }
    let out_path = dir.join(format!(
        "SnapPin_{}.{}",
        now_secs(),
        if v.get("gif").and_then(|b| b.as_bool()).unwrap_or(false) {
            "gif"
        } else {
            "mp4"
        }
    ));
    let gif = v.get("gif").and_then(|b| b.as_bool()).unwrap_or(false);
    // 音频：body 显式给了 audio/audio_device 就用它，否则跟随设置（与 UI 路径同口径）。
    // GIF 容器没有音轨，恒不录。
    let audio_device = if gif {
        None
    } else {
        let (enabled, configured) = match app.try_state::<crate::commands::ConfigState>() {
            Some(st) => match st.0.lock() {
                Ok(c) => (
                    v.get("audio")
                        .and_then(|b| b.as_bool())
                        .unwrap_or(c.record_audio),
                    v.get("audio_device")
                        .and_then(|s| s.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| c.record_audio_device.clone()),
                ),
                Err(_) => (false, String::new()),
            },
            None => (false, String::new()),
        };
        crate::recorder::resolve_audio_device(enabled, &configured)
    };
    // 音频源说明要在设备名被移进 opts 之前算好（`Option<String>` 会被 move）。
    let audio_source = audio_source_of(audio_device.as_deref());
    // 码率档位跟随设置（请求体显式给 `bitrate_k` 时以请求为准）。
    let bitrate_k = v
        .get("bitrate_k")
        .and_then(|n| n.as_u64())
        .unwrap_or_else(|| record_bitrate_default(app) as u64) as u32;
    let opts = RecordOptions {
        bounds,
        fps: v.get("fps").and_then(|n| n.as_u64()).unwrap_or(60) as u32,
        bitrate_k,
        out_path: out_path.clone(),
        gif,
        audio_device,
    };
    match Recorder::start(opts) {
        Ok(r) => {
            *guard = Some(r);
            // HTTP 录制同样先收球（A 方案 2026-09-23，与 start_record_impl 同式）：
            // 本端点自带 audio 覆盖参数不经 start_record_impl，缺口必须在这里补，
            // 否则 HTTP 发起的录制会把球拍进成品。
            crate::commands::close_ball_now(app);
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "path": out_path.to_string_lossy(),
                    // 本次录到的音频来源（D7）：区域/全屏模式走 dshow 设备（麦克风），
                    // 窗口模式走 WASAPI loopback（系统声）——两套口径必须让调用方看得见，
                    // 否则「录屏没声音」无从判断是设置问题还是环境问题。
                    "audio_source": audio_source,
                    // 本次生效的质量档位（低/中/高）。`record_bitrate_k` 现在是**档位键**
                    // 不是实际码率，实际体积由画面内容自适应（缺陷 D11）。
                    "quality_preset": crate::recorder::preset_for(bitrate_k).1,
                    "quality_presets": quality_presets_json(),
                })
                .to_string(),
            )
        }
        Err(e) => (500, err_json(&e)),
    }
}

/// POST /v1/record/start {mode:"window", target, fps?=30, bitrate_k?=12000, cursor?=false, audio?=false}
///
/// 窗口录屏 session（合同 §7.2）：目标解析与校验全部复用 `window_target`（先 `resolve`
/// 再 `validate_capturable`，两步的失败码直接透传）；失败一律结构化报错，
/// **绝不**回落成全屏/区域路径——「总有一个文件」比「目标语义正确」次要（合同 §7.3 末条）。
fn record_start_window_ep(app: &AppHandle, v: &serde_json::Value) -> (u16, String) {
    // 合同 §7.3：`recording_exists` = 当前实现不支持并行录制。窗口 session 只能看见自己的
    // 活跃会话，看不见旧 ffmpeg 路线那个全局 `RecordState`；不在这里互斥，两条路线就能同时
    // 开录。反方向（先起窗口 session、再走旧无 session_id 路径）已由 `record_start_ep` 开头
    // 的 `active_session_id()` 检查堵住——两个方向都要闭合，漏一个就等于没有互斥。
    let legacy = app.state::<RecordState>();
    match legacy.0.lock() {
        Ok(g) => {
            if g.is_some() {
                return (
                    409,
                    err_detail("recording_exists", "已有屏幕录制在进行，不支持并行录制"),
                );
            }
        }
        Err(e) => return (500, err_detail("capture_failed", &e.to_string())),
    }
    let target = v.get("target").cloned().unwrap_or(serde_json::Value::Null);
    let resolved = match window_target::resolve(&target) {
        Ok(r) => r,
        Err(e) => return (e.status, err_detail(e.code, &e.message)),
    };
    // resolve 内部已调 validate_capturable；这里再显式过一次，是为了让「目标校验」
    // 在本端点自身可读，而不是依赖另一个模块的实现细节（校验是幂等的只读检查）。
    if let Err(e) = window_target::validate_capturable(resolved.hwnd) {
        return (e.status, err_detail(e.code, &e.message));
    }
    let hwnd = resolved.hwnd;
    let window_id = window_target::window_id_of(hwnd);

    // audio 原样交给模块：`audio:true` 时模块走 WASAPI loopback 采集系统声音
    // （ffmpeg 第二输入读命名管道）；本机默认渲染端点不可用时模块返回
    // `audio_unavailable`。在 http 层不再拦一遍——两套口径将来必然漂移。
    //
    // 缺省值取配置（`cfg.record_audio`，默认 true）：硬编码 false 等于「窗口录屏永远
    // 没声音」，用户在设置页开了录音也白开（2026-10-20 缺陷 D7）。请求体显式给了
    // `audio` 时以请求为准。
    let audio = v
        .get("audio")
        .and_then(|b| b.as_bool())
        .unwrap_or_else(|| record_audio_default(app));

    let dir = captures_dir(app);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return (500, err_detail("capture_failed", &e.to_string()));
    }
    // 文件名用毫秒：旧路径用秒，同秒内连发两次会撞名（窗口模式由 agent 高频调用）。
    let out_path = dir.join(format!("SnapPin_rec_{}.mp4", now_ms()));

    // 码率档位跟随设置（与区域/全屏路径同口径）。
    let bitrate_k = v
        .get("bitrate_k")
        .and_then(|n| n.as_u64())
        .unwrap_or_else(|| record_bitrate_default(app) as u64) as u32;
    let opts = window_recorder::StartOptions {
        hwnd: hwnd.0 as isize,
        window_id,
        pid: resolved.pid,
        title: window::window_title(hwnd),
        class_name: window::class_name(hwnd),
        fps: v.get("fps").and_then(|n| n.as_u64()).unwrap_or(30) as u32,
        bitrate_k,
        cursor: v.get("cursor").and_then(|b| b.as_bool()).unwrap_or(false),
        audio,
        path: out_path,
    };
    // why-not 调 `close_ball_now`（旧路径在成功分支里调）：悬浮球是 SnapPin 自己的顶层窗口，
    // 而 WGC 只采目标窗口的内容，球永远进不了帧；为「防进帧」去动球只会给用户一个看得见的
    // 副作用。同理 stop 也不调 `spawn_ball_restore`——从没隐藏过，无从恢复。
    match window_recorder::start(opts) {
        Ok(s) => {
            // 记下窗口 id：收尾快照要回填它（`window_recorder::Stopped` 不带 window_id）。
            remember_window_id(&s.session_id, &s.window_id);
            (
                200,
                serde_json::json!({
                    "ok": true,
                    "session_id": s.session_id,
                    "mode": "window",
                    "source": s.source,
                    "state": s.state,
                    "width": s.width,
                    "height": s.height,
                    "fps": s.fps,
                    "window_id": s.window_id,
                    "audio": s.audio,
                    // 本次录到的是系统声音还是麦克风（D7）：窗口模式固定走 WASAPI loopback
                    // 采系统声；调用方据此判断「没声音」是设置问题还是环境问题。
                    "audio_source": if s.audio { "system_loopback" } else { "none" },
                    // 本次生效的质量档位（低/中/高），同 D11：`record_bitrate_k` 是档位键。
                    "quality_preset": crate::recorder::preset_for(bitrate_k).1,
                })
                .to_string(),
            )
        }
        Err(e) => (status_for_code(e.code), err_detail(e.code, &e.message)),
    }
}

/// 窗口 session 的统一响应体（status / pause / resume 共用，合同 §7.2 要求的字段全集）。
fn status_json(st: &window_recorder::Status) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "session_id": st.session_id,
        "mode": "window",
        "source": st.source,
        "state": st.state,
        "width": st.width,
        "height": st.height,
        "fps": st.fps,
        "window_id": st.window_id,
        "duration_ms": st.duration_ms,
        "frames": st.frames,
        "dropped": st.dropped,
        "geometry_changed": st.geometry_changed,
        "audio": st.audio,
        "audio_bytes": st.audio_bytes,
        "error_code": st.error_code,
        "error": st.error,
    })
}

/// 会话状态表（D5）：活动 → 已完成 的迁移与保留。
///
/// 为什么需要它：`window_recorder::stop` 会把会话从活动槽里摘掉，之后
/// `status(session_id)` 只剩 `recording_not_found`——调用方 stop 拿到成品后
/// 就再也查不到这条会话的最终元数据（2026-10-20 缺陷 D5）。
/// 这里在 HTTP 层留一份「已完成」快照：stop 时落一条，status 查不到活动会话时回查它。
///
/// 活动会话的权威状态仍在 `window_recorder` 里，这里**不**复制活动状态——
/// 两处各存一份活动状态必然打架。
struct SessionTable {
    /// session_id → 最终状态 JSON（字段集与 `status_json` 一致，外加产物路径）。
    done: std::collections::HashMap<String, serde_json::Value>,
    /// session_id → 窗口 id（`window_recorder::Stopped` 不带 window_id，只能 start 时记）。
    window_ids: std::collections::HashMap<String, String>,
    /// 完成顺序（超过上限时丢最旧的一条）。
    order: std::collections::VecDeque<String>,
}

/// 已完成会话的保留上限：进程存活期间都留着，但必须有闸门——
/// 长跑不能无限涨内存（每条一个小 JSON，量不大也不能没有上限）。
const DONE_SESSION_LIMIT: usize = 200;

static SESSION_TABLE: std::sync::LazyLock<std::sync::Mutex<SessionTable>> =
    std::sync::LazyLock::new(|| {
        std::sync::Mutex::new(SessionTable {
            done: std::collections::HashMap::new(),
            window_ids: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
        })
    });

/// start 时记下窗口 id：收尾快照要回填 window_id，而 `window_recorder::Stopped` 不带它。
fn remember_window_id(session_id: &str, window_id: &str) {
    let mut g = match SESSION_TABLE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    g.window_ids
        .insert(session_id.to_string(), window_id.to_string());
}

/// 会话收尾后落进「已完成」表（D5）。
fn remember_done(session_id: &str, mut status: serde_json::Value) {
    let mut g = match SESSION_TABLE.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    if let Some(wid) = g.window_ids.remove(session_id) {
        status["window_id"] = serde_json::json!(wid);
    }
    g.done.insert(session_id.to_string(), status);
    g.order.push_back(session_id.to_string());
    // 超上限丢最旧的（顺序表头部就是最旧的一条）。
    while g.order.len() > DONE_SESSION_LIMIT {
        match g.order.pop_front() {
            Some(old) => {
                g.done.remove(&old);
            }
            None => break,
        }
    }
}

/// 回查「已完成」会话的最终状态（D5）。
fn lookup_done(session_id: &str) -> Option<serde_json::Value> {
    let g = SESSION_TABLE.lock().ok()?;
    g.done.get(session_id).cloned()
}

/// 收尾后的最终状态快照（D5）：字段集与 `status_json` 一致，外加产物路径。
///
/// `state` 统一用 `"done"`：它是「这条会话已结束」的稳定终态值；
/// 成功还是失败看 `error_code`/`error`（模块里 error_code 非空 ⇔ 收尾状态为 failed）。
fn done_json(st: &window_recorder::Stopped) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "session_id": st.session_id,
        "mode": "window",
        "source": st.source,
        "state": "done",
        "width": st.width,
        "height": st.height,
        "fps": st.fps,
        "duration_ms": st.duration_ms,
        "frames": st.frames,
        "dropped": st.dropped,
        "geometry_changed": st.geometry_changed,
        "audio": st.audio,
        "audio_bytes": st.audio_bytes,
        "error_code": st.error_code,
        "error": st.error,
        "path": st.path,
    })
}

/// GET /v1/record/status?session_id=... —— 窗口 session 状态（合同 §7.2）。
fn record_status_ep(query: &str) -> (u16, String) {
    let sid = match query_param(query, "session_id").filter(|s| !s.is_empty()) {
        Some(s) => s,
        // 缺参数按「session 不存在」报：契约只给了 recording_not_found 这一个
        // session 相关的码，新造一个码会脱离 §7.3 表。message 必须与「给了但不存在的
        // session_id」可区分，否则调用方无法判断是自己漏传还是 ID 过期。
        None => {
            return (
                409,
                err_detail(
                    "recording_not_found",
                    "缺少 session_id 查询参数（不是 session 已结束）",
                ),
            )
        }
    };
    match window_recorder::status(&sid) {
        Ok(st) => (200, status_json(&st).to_string()),
        // 活动表里没有 → 回查「已完成」表（D5）：stop 之后这条会话仍要能查到
        // 最终元数据（state=done + frames/duration/dropped/path）。
        // 模块对「不存在的 session_id」的 message 是「没有该录制会话：<id>」，
        // 与上面缺参的文案互不混淆。
        Err(e) => match lookup_done(&sid) {
            Some(done) => (200, done.to_string()),
            None => (status_for_code(e.code), err_detail(e.code, &e.message)),
        },
    }
}

/// 请求体里的非空 `session_id`（空白串视为未给，避免 `" "` 被当成有效会话 ID）。
fn session_id_of(v: &serde_json::Value) -> Option<String> {
    v.get("session_id")
        .and_then(|s| s.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 从 URL query（`a=1&b=2`）取单个参数值。
///
/// 不做百分号解码：session_id 是模块自己生成的 ASCII 标识符，不含需要转义的字符；
/// 贸然解码反而可能把调用方原样传入的 `%` 改成别的字符。
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// POST /v1/record/stop {session_id?, discard?:bool} —— discard=true 丢弃录制不产 MP4。
///
/// 带 `session_id` → 窗口录屏 session 收尾（合同 §7.2：返回真实 `path`/时长/尺寸/帧数）；
/// 不带 → 旧路径，行为一字不动。
///
/// `session_id` 现在有三个来源，**body 优先**（2026-10-20 缺陷 D2）：
/// body `{"session_id":…}` → query `?session_id=…` → 「当前只有一个活动录制会话」时直接停它。
/// 历史上 stop 必须把 session_id 放在 body 里，否则一律报「当前没有录制」，
/// 而会话其实还在录（实测：同一时刻 status 报 state:"recording"）。
fn record_stop_ep(app: &AppHandle, query: &str, body: &str) -> (u16, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let sid = session_id_of(&v).or_else(|| {
        query_param(query, "session_id").and_then(|s| {
            let t = s.trim().to_string();
            if t.is_empty() {
                None
            } else {
                Some(t)
            }
        })
    });
    if let Some(sid) = sid {
        return record_stop_window_ep(&sid, &v);
    }
    // 两个来源都没给：不猜。若「当前只有一个活动录制会话」，直接停它（D2）。
    // 两条录制路线都开着（老 ffmpeg 全屏路线 + 窗口 session）→ 有歧义，让调用方指明。
    let legacy_active = app
        .state::<RecordState>()
        .0
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false);
    if let Some(sid) = window_recorder::active_session_id() {
        if legacy_active {
            return (
                409,
                err_detail(
                    "recording_exists",
                    "同时存在全屏录制与窗口录制会话，请带 session_id 指定停哪个",
                ),
            );
        }
        return record_stop_window_ep(&sid, &v);
    }
    let discard = v.get("discard").and_then(|b| b.as_bool()).unwrap_or(false);
    let state = app.state::<RecordState>();
    let rec = match state.0.lock() {
        Ok(mut g) => g.take(),
        Err(e) => return (500, err_json(&e.to_string())),
    };
    let had = rec.is_some();
    let out = match rec {
        Some(r) => {
            if discard {
                match r.discard() {
                    Ok(()) => (
                        200,
                        serde_json::json!({"ok":true,"discarded":true}).to_string(),
                    ),
                    Err(e) => (500, err_json(&e)),
                }
            } else {
                match r.stop(|_| {}) {
                    Ok(p) => (
                        200,
                        serde_json::json!({"ok":true,"path":p.to_string_lossy()}).to_string(),
                    ),
                    Err(e) => (500, err_json(&e)),
                }
            }
        }
        None => (409, err_json("当前没有录制")),
    };
    // 录制已停 → 恢复悬浮球（A 方案 2026-09-23，与 stop_record 同式；
    // 录制期间/overlay 可见时 spawn_ball_restore 会自行跳过）。
    if had {
        crate::commands::spawn_ball_restore(app.clone());
    }
    out
}

/// 窗口 session 收尾（合同 §7.2）：`discard=true` 删文件并 `path: null`；
/// `discard=false` 必须校验产物真实存在且非零字节——禁止用成功状态返回空 path 或零字节文件。
fn record_stop_window_ep(session_id: &str, v: &serde_json::Value) -> (u16, String) {
    let discard = v.get("discard").and_then(|b| b.as_bool()).unwrap_or(false);
    let st = match window_recorder::stop(session_id, discard) {
        Ok(s) => s,
        Err(e) => return (status_for_code(e.code), err_detail(e.code, &e.message)),
    };
    // 为什么在 HTTP 层再查一次（模块内部已有同名检查）：合同 §7.3 末条是**对外契约**，
    // 它必须在 API 边界上无条件成立——模块若回归出「Ok + 零字节文件」，调用方拿到的就是
    // 假成功。这层守卫让契约不依赖另一个模块的实现正确性。
    if let Some(p) = st.path.as_deref() {
        let ok = std::fs::metadata(p)
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false);
        if !ok {
            return (
                500,
                err_detail("capture_failed", &format!("录制产物缺失或为空文件：{p}")),
            );
        }
    } else if !discard {
        return (
            500,
            err_detail("capture_failed", "录制未产出文件（非 discard 请求）"),
        );
    }
    // `gif:true` 时才转码（合同 §8 Phase 3：GIF 只在显式请求时生成），失败不改主结论。
    let gif_req = v.get("gif").and_then(|b| b.as_bool()).unwrap_or(false);
    let mut out = serde_json::json!({
        "ok": true,
        "session_id": st.session_id,
        "mode": "window",
        "source": st.source,
        "state": st.state,
        "path": st.path,
        "duration_ms": st.duration_ms,
        "width": st.width,
        "height": st.height,
        "fps": st.fps,
        "frames": st.frames,
        "dropped": st.dropped,
        "geometry_changed": st.geometry_changed,
        "audio": st.audio,
        "audio_bytes": st.audio_bytes,
        "error_code": st.error_code,
        "error": st.error,
    });
    if gif_req {
        // 未落盘（discard 或失败）时没有可转码的源文件：如实报 gif 失败原因，
        // 不静默略过——调用方显式要了 GIF，缺文件就是结果的一部分。
        match st.path.as_deref() {
            Some(src) => {
                let dst = std::path::Path::new(src).with_extension("gif");
                match crate::recorder::mp4_to_gif(std::path::Path::new(src), &dst, &mut |_p: u8| {})
                {
                    Ok(()) => {
                        out["gif_path"] = serde_json::json!(dst.display().to_string());
                    }
                    Err(e) => {
                        out["gif_error"] = serde_json::json!(e);
                    }
                }
            }
            None => {
                out["gif_error"] = serde_json::json!("录制未产出 MP4，无法转 GIF");
            }
        }
    }
    // 收尾后把最终元数据落进「已完成」表（D5）：之后 status 仍能查到这条会话
    // （state=done + frames/duration/dropped/path）。上面的失败分支不落表——
    // 那时会话可能还在活动槽里，状态以模块为准。
    remember_done(&st.session_id, done_json(&st));
    (200, out.to_string())
}

/// POST /v1/record/pause | /v1/record/resume —— 真暂停/继续（分段录制，stop 时无损拼接）。
///
/// 带 `session_id` → 窗口 session（合同 §7.2，回 session 状态体）；不带 → 旧路径不变。
fn record_pause_ep(app: &AppHandle, body: &str, pause: bool) -> (u16, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    if let Some(sid) = session_id_of(&v) {
        let res = if pause {
            window_recorder::pause(&sid)
        } else {
            window_recorder::resume(&sid)
        };
        return match res {
            Ok(st) => (200, status_json(&st).to_string()),
            Err(e) => (status_for_code(e.code), err_detail(e.code, &e.message)),
        };
    }
    let state = app.state::<RecordState>();
    let mut guard = match state.0.lock() {
        Ok(g) => g,
        Err(e) => return (500, err_json(&e.to_string())),
    };
    match guard.as_mut() {
        Some(r) => {
            let res = if pause { r.pause() } else { r.resume() };
            match res {
                Ok(()) => (
                    200,
                    serde_json::json!({"ok":true,"paused":pause}).to_string(),
                ),
                Err(e) => (409, err_json(&e)),
            }
        }
        None => (409, err_json("当前没有录制")),
    }
}

/// GET/POST /v1/windows → 可捕获顶层窗口快照（含 HWND/PID/类名/最小化等，合同 §5.1）。
///
/// 旧字段 x/y/w/h/title 全部保留；新增字段供 agent 构造稳定目标（`window_id`）。
fn windows_ep() -> (u16, String) {
    let ws = window_target::list();
    (200, serde_json::json!({"ok":true,"windows":ws}).to_string())
}

/// POST /v1/uia {x,y} → UIA 子区域检测。
/// 命中 → {ok:true, rect:{x,y,w,h}}；无更细元素/超时/失败 → {ok:false}。
fn uia_ep(body: &str) -> (u16, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let x = v.get("x").and_then(|n| n.as_i64()).unwrap_or(0) as i32;
    let y = v.get("y").and_then(|n| n.as_i64()).unwrap_or(0) as i32;
    match uia::element_rect_at(x, y) {
        // 成功分支：拿到更细的 UI 元素矩形。
        Some(rect) => (200, serde_json::json!({"ok":true,"rect":rect}).to_string()),
        // 错误/回退分支：无更细元素、命中 overlay 自身、或 UIA 超时/失败。
        None => (200, serde_json::json!({"ok":false}).to_string()),
    }
}

/// POST /v1/ocr {path} | {x,y,w,h}, lang? → {text, lines[]}
fn ocr_ep(app: &AppHandle, body: &str) -> (u16, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let lang = v.get("lang").and_then(|s| s.as_str());
    let png = match v.get("path").and_then(|s| s.as_str()) {
        Some(p) => match std::fs::read(p) {
            Ok(b) => b,
            Err(e) => return (400, err_json(&e.to_string())),
        },
        None => {
            let b = Bounds {
                x: v.get("x").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
                y: v.get("y").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
                w: v.get("w").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
                h: v.get("h").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
            };
            let img = match capture::capture_region(b) {
                Ok(i) => i,
                Err(e) => return (500, err_json(&e)),
            };
            let mut buf = std::io::Cursor::new(Vec::new());
            if let Err(e) = img.write_to(&mut buf, image::ImageFormat::Png) {
                return (500, err_json(&e.to_string()));
            }
            buf.into_inner()
        }
    };
    match ocr::ocr_png(&png, lang) {
        Ok(r) => {
            // 与 Tauri 命令路径同口径：`ocr_auto_copy` 关掉时完全不碰剪贴板。
            crate::commands::auto_copy_ocr(app, &r.text);
            (
                200,
                serde_json::json!({"ok":true,"text":r.text,"lines":r.lines}).to_string(),
            )
        }
        Err(e) => (500, err_json(&e)),
    }
}

/// POST /v1/pin {path} | {png_base64}, x?, y? → {label}
fn pin_ep(app: &AppHandle, body: &str) -> (u16, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let b64 = if let Some(p) = v.get("path").and_then(|s| s.as_str()) {
        match std::fs::read(p) {
            Ok(b) => {
                use base64::{engine::general_purpose::STANDARD, Engine};
                STANDARD.encode(b)
            }
            Err(e) => return (400, err_json(&e.to_string())),
        }
    } else if let Some(s) = v.get("png_base64").and_then(|s| s.as_str()) {
        s.to_string()
    } else {
        return (400, err_json("需要 path 或 png_base64"));
    };
    let x = v.get("x").and_then(|n| n.as_i64()).unwrap_or(120) as i32;
    let y = v.get("y").and_then(|n| n.as_i64()).unwrap_or(120) as i32;

    // 建窗必须在主线程。用通道回收结果。
    let (tx, rx) = std::sync::mpsc::channel::<Result<String, String>>();
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        let state = app2.state::<crate::pin::PinState>();
        let _ = tx.send(crate::pin::create_pin(&app2, &state, b64, x, y));
    });
    match rx.recv() {
        Ok(Ok(label)) => (
            200,
            serde_json::json!({"ok":true,"label":label}).to_string(),
        ),
        Ok(Err(e)) => (500, err_json(&e)),
        Err(e) => (500, err_json(&e.to_string())),
    }
}

/// POST /v1/longshot {x,y,w,h, save?=true, copy?=false} → {ok, path}
fn longshot_ep(app: &AppHandle, body: &str) -> (u16, String) {
    let v: serde_json::Value = serde_json::from_str(body).unwrap_or(serde_json::json!({}));
    let b = Bounds {
        x: v.get("x").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        y: v.get("y").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        w: v.get("w").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
        h: v.get("h").and_then(|n| n.as_i64()).unwrap_or(0) as i32,
    };
    let save = v.get("save").and_then(|b| b.as_bool()).unwrap_or(true);
    let copy = v.get("copy").and_then(|b| b.as_bool()).unwrap_or(false);
    let opts = app
        .try_state::<crate::commands::ConfigState>()
        .and_then(|s| {
            s.0.lock()
                .ok()
                .map(|c| longshot::ScrollOptions::from_config(&c, 60))
        })
        .unwrap_or_default();
    let img = match longshot::capture_scrolling(b, opts) {
        Ok(i) => i,
        Err(e) => return (500, err_json(&e)),
    };
    let (w, h) = (img.width(), img.height());
    if copy {
        if let Err(e) = capture::copy_to_clipboard(&img) {
            return (500, err_json(&e));
        }
    }
    let mut path_out = String::new();
    if save {
        let dir = captures_dir(app);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return (500, err_json(&e.to_string()));
        }
        let (ext, fmt) = crate::commands::image_ext_and_format(app);
        let p = dir.join(format!(
            "{}.{}",
            crate::commands::product_stem(app, "long_"),
            ext
        ));
        if let Err(e) = capture::save_image(&img, &p, fmt) {
            return (500, err_json(&e));
        }
        path_out = p.to_string_lossy().to_string();
    }
    (
        200,
        serde_json::json!({"ok":true,"path":path_out,"width":w,"height":h}).to_string(),
    )
}

/// GET /v1/settings → 全部设置（token 一并回；本地同机 agent 面，与 UI 同权）。
fn settings_get_ep(app: &AppHandle) -> (u16, String) {
    match app.try_state::<crate::commands::ConfigState>() {
        Some(s) => match s.0.lock() {
            Ok(c) => (
                200,
                serde_json::json!({"ok":true,"settings":serde_json::to_value(&*c).unwrap_or_default()})
                    .to_string(),
            ),
            Err(e) => (500, err_json(&e.to_string())),
        },
        None => (500, err_json("配置状态未初始化")),
    }
}

/// PUT/POST /v1/settings {…AppConfig 字段} → 整体更新并落盘，回清洗后的最终值。
fn settings_put_ep(app: &AppHandle, body: &str) -> (u16, String) {
    let new_cfg: AppConfig = match serde_json::from_str(body) {
        Ok(c) => c,
        Err(e) => return (400, err_json(&format!("设置 JSON 非法：{e}"))),
    };
    let state = match app.try_state::<crate::commands::ConfigState>() {
        Some(s) => s,
        None => return (500, err_json("配置状态未初始化")),
    };
    let mut c = match state.0.lock() {
        Ok(g) => g,
        Err(e) => return (500, err_json(&e.to_string())),
    };
    let token = c.token.clone();
    let port = c.port;
    let prev_hotkeys = c.hotkeys.clone();
    let prev_guard = (c.hotkeys_disabled, c.hotkeys_disable_fullscreen);
    *c = new_cfg;
    c.token = token;
    c.port = port; // 端口改了要重启，不走运行时接口
    c.normalize();
    let hotkeys_touched = (c.hotkeys_disabled, c.hotkeys_disable_fullscreen) != prev_guard
        || c.hotkeys != prev_hotkeys;
    let path = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("snappin"))
        .join("config.json");
    c.save(&path);
    // 系统副作用与 Tauri update_settings 对齐：开机自启写/删注册表 Run 键。
    crate::system::set_autostart(c.auto_start);
    let val = serde_json::to_value(&*c).unwrap_or_default();
    // 热键重注册要带一份配置快照回主线程；不能在闭包里再锁 ConfigState（会与 HTTP
    // 线程自身的锁语义纠缠，且此刻锁还没放）。
    let cfg_snapshot = c.clone();
    // **先放锁再调 allow**：allow_asset_captures → captures_dir 会再取同一把
    // ConfigState std Mutex，同线程重锁 = 无声永久挂起（2026-09-24 探针实锤：
    // 首个 PUT 卡死本单线程循环 → 全端点假死、新连接无响应、CLOSE_WAIT 堆积）。
    drop(c);
    // save_dir 可能经 HTTP 改 → 同步重授 asset 协议白名单（保存条回放依赖）。
    crate::commands::allow_asset_captures(app);
    // 热键相关字段变了 → 回**主线程**重注册（agent 面与 UI 同权：设置页改这两个开关会
    // 立即生效，HTTP 面不生效就是两套语义）。必须回主线程：全局快捷键插件在主线程上
    // 操作注册表，HTTP 线程直接调可能悬挂。
    if hotkeys_touched {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            crate::sync_hotkey_surfaces(&app2, &cfg_snapshot);
        });
    }
    (
        200,
        serde_json::json!({"ok":true,"settings":val}).to_string(),
    )
}

fn authorized(req: &tiny_http::Request, token: &str) -> bool {
    req.headers().iter().any(|h| {
        h.field.equiv("Authorization") && h.value.as_str().strip_prefix("Bearer ") == Some(token)
    })
}

fn unauthorized() -> String {
    r#"{"ok":false,"error":"unauthorized"}"#.into()
}

fn err_json(msg: &str) -> String {
    serde_json::json!({"ok":false,"error":msg}).to_string()
}

/// 结构化错误体（合同 §7.3）：`{ ok:false, error:<码>, message:<中文说明> }`。
/// 与旧的 `err_json`（只带 error 字符串）并存：窗口模式新契约要求「稳定错误码 + 人读说明」分离。
fn err_detail(code: &str, message: &str) -> String {
    serde_json::json!({"ok":false,"error":code,"message":message}).to_string()
}

/// 错误码 → HTTP 状态（合同 §7.3 表）。表内含未来阶段码（encoder_unavailable 等），
/// 先按表实现，避免后续阶段又改一处映射。
///
/// `audio_unavailable` 是合同表外新增（§7.3 未列，但 §8 Phase 2 明确音频属 Phase 3）：
/// 归 422 与 `capture_unavailable` 同档——都是「请求合法但本阶段做不到」，语义一致。
fn status_for_code(code: &str) -> u16 {
    match code {
        "invalid_target" => 400,
        "target_not_found" => 404,
        "target_changed" => 409,
        "target_minimized" => 409,
        "recording_exists" => 409,
        "recording_not_found" => 409,
        "capture_unavailable" => 422,
        "audio_unavailable" => 422,
        "encoder_unavailable" => 503,
        "capture_failed" => 500,
        _ => 500,
    }
}

fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn json_resp(code: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let header =
        Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).expect("合法 header");
    Response::from_string(body)
        .with_status_code(code)
        .with_header(header)
}

/// 幂等回放响应：在标准 json 响应上加 `X-Idempotent-Replayed: true`，方便调用方/测试观测命中。
fn json_resp_replayed(code: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let replayed =
        Header::from_bytes(&b"X-Idempotent-Replayed"[..], &b"true"[..]).expect("合法 header");
    json_resp(code, body).with_header(replayed)
}
