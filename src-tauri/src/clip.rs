//! 剪贴板历史（轻量版）：捕获 + 本地存储 + 读写命令。
//!
//! 捕获：主窗 HWND 挂 `AddClipboardFormatListener`，`WM_CLIPBOARDUPDATE`
//! 消息驱动。收到消息后 spawn 短线程经 arboard/原生 API 读内容（**绝不
//! 在主线程同步读剪贴板**：打开别的进程剪贴板可能阻塞 UI）。
//! 去重：与最近一条内容相同（文本等值 / 图片同 PNG 字节哈希 / 文件列表等值）
//! 则只更新时间戳置顶，不新增条目。
//! 存储：`app_config_dir/clip-history.json`（条目元数据+文本），图片另存
//! `app_config_dir/clip/<id>.png`。上限 clip_max 条（设置 10–1000，默认 100），超出删最旧（含图片文件）。
//!
//! P1（2026-09-24 落地）：固定/收藏 = ClipEntry `pinned` 字段 + `sort_entries`
//! 统一排序（先 pinned 后 ts）；忽略来源应用/敏感跳过在 `ingest` 入口一次性过滤；
//! 过期清理 `purge_expired`（0=永不过期，固定条豁免）；搜索在前端内存过滤。

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::DefWindowProcW;

/// 历史条数上限默认值 / ConfigState 不可用时的回退（用户可配值 `clip_max` 见 config.rs）。
pub const CLIP_MAX: usize = 100;

/// 缩略图缓存：key = 图片文件名（`<id>.png`，内容不可变），value = data URL。
/// `list_entries_with_thumbs` 命中即用——否则每次 `clip_list` 都对全部图片
/// 重「解码→缩放→PNG 编码→base64」；316 张图在后台每 1.5s 跑一次曾把单进程
/// CPU/磁盘打满，拖慢所有窗口（用户实测「很多操作都卡顿」的根因）。
static THUMB_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// 最近一次由 `copy_back`/`clip_paste` 主动写回剪贴板的内容签名 + 时间戳(ms)。
/// 写回会触发 `WM_CLIPBOARDUPDATE` 回声，`capture_now` 命中此签名即跳过入库；
/// 否则图片按新 `<id>.png` 重新入库、去重比文件名对不上 = 每次粘贴堆一条重复
/// 历史（用户实测「选中要粘贴的内容会再生成一条重复历史」的根因）。
static LAST_SELF_WRITE: Mutex<Option<(u64, SelfSig)>> = Mutex::new(None);

/// 自写回内容签名：文本/文件按内容精确匹配；图片按宽高匹配（arboard 写入-读回
/// 的 RGBA 字节可能因行对齐/预乘差异不完全一致，比宽高更稳，1.2s 窗内碰撞可忽略）。
#[derive(Clone, PartialEq)]
enum SelfSig {
    Text(String),
    Image(u32, u32),
    Files(Vec<String>),
}

/// 若内容与最近一次自写回匹配（1.2s 内），消费该标记并返回 true（调用方跳过入库）。
fn is_self_write_echo(sig: &SelfSig) -> bool {
    let Ok(mut g) = LAST_SELF_WRITE.lock() else {
        return false;
    };
    if let Some((ts, s)) = g.as_ref() {
        if now_ms().saturating_sub(*ts) <= 1200 && s == sig {
            *g = None;
            return true;
        }
    }
    false
}

/// 记录一次自写回签名（copy_back / clip_paste 写完剪贴板后调用）。
fn mark_self_write(sig: SelfSig) {
    if let Ok(mut g) = LAST_SELF_WRITE.lock() {
        *g = Some((now_ms(), sig));
    }
}

/// 从缓存删除单个文件的缩略图（删除条目时调用）。
fn drop_thumb(file: &str) {
    if let Ok(mut g) = THUMB_CACHE.lock() {
        if let Some(m) = g.as_mut() {
            m.remove(file);
        }
    }
}

/// 清空缩略图缓存（清空历史时调用；id 从 1 重排，不清会命中旧缩略图）。
fn clear_thumbs() {
    if let Ok(mut g) = THUMB_CACHE.lock() {
        if let Some(m) = g.as_mut() {
            m.clear();
        }
    }
}

/// 读剪贴板配置（总开关，上限）；ConfigState 不可用（极早期）回退 (开, 100)。
fn clip_cfg(app: &AppHandle) -> (bool, usize) {
    app.try_state::<crate::commands::ConfigState>()
        .and_then(|s| {
            s.0.lock()
                .ok()
                .map(|c| (c.clip_enabled, c.clip_max as usize))
        })
        .unwrap_or((true, CLIP_MAX))
}

/// 一条历史：文本直接存；图片存 PNG 文件名（`<id>.png`，相对 clip 目录）；
/// 文件存路径列表（Explorer 复制的 CF_HDROP）。
/// 去重核心不变量：等值只比内容（Text 比 text / Image 比 file / Files 比 paths），
/// **不比 id/ts**——ingest 靠它删旧留新。derive 的 PartialEq 会比全部字段（含
/// id），去重永远不命中、同一内容重复入库（实测一次复制两条，id 差 1）。
impl PartialEq for ClipEntry {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text { text: a, .. }, Self::Text { text: b, .. }) => a == b,
            (Self::Image { file: a, .. }, Self::Image { file: b, .. }) => a == b,
            (Self::Files { paths: a, .. }, Self::Files { paths: b, .. }) => a == b,
            _ => false,
        }
    }
}
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind")]
pub enum ClipEntry {
    #[serde(rename = "text")]
    Text {
        id: u64,
        ts: u64,
        text: String,
        /// 固定条目（P1 #9）：置顶显示，豁免超限删除与过期清理。
        /// `#[serde(default)]` = 旧历史 JSON 无此字段时按 false 升级，不丢历史。
        #[serde(default)]
        pinned: bool,
    },
    #[serde(rename = "image")]
    Image {
        id: u64,
        ts: u64,
        file: String,
        w: u32,
        h: u32,
        #[serde(default)]
        pinned: bool,
    },
    #[serde(rename = "files")]
    Files {
        id: u64,
        ts: u64,
        paths: Vec<String>,
        #[serde(default)]
        pinned: bool,
    },
}

impl ClipEntry {
    pub fn id(&self) -> u64 {
        match self {
            ClipEntry::Text { id, .. }
            | ClipEntry::Image { id, .. }
            | ClipEntry::Files { id, .. } => *id,
        }
    }

    pub fn ts(&self) -> u64 {
        match self {
            ClipEntry::Text { ts, .. }
            | ClipEntry::Image { ts, .. }
            | ClipEntry::Files { ts, .. } => *ts,
        }
    }

    /// 更新时间戳（去重置顶 / 写回后置顶共用）。
    fn touch(&mut self, ts: u64) {
        match self {
            ClipEntry::Text { ts: t, .. }
            | ClipEntry::Image { ts: t, .. }
            | ClipEntry::Files { ts: t, .. } => *t = ts,
        }
    }

    /// 图片文件名（仅 Image 有）。
    fn image_file(&self) -> Option<&str> {
        match self {
            ClipEntry::Image { file, .. } => Some(file),
            _ => None,
        }
    }

    /// 是否固定（P1 #9）。
    pub fn pinned(&self) -> bool {
        match self {
            ClipEntry::Text { pinned, .. }
            | ClipEntry::Image { pinned, .. }
            | ClipEntry::Files { pinned, .. } => *pinned,
        }
    }

    /// 设置固定标记（去重继承旧条固定状态、toggle_pin 共用）。
    fn set_pinned(&mut self, v: bool) {
        match self {
            ClipEntry::Text { pinned, .. }
            | ClipEntry::Image { pinned, .. }
            | ClipEntry::Files { pinned, .. } => *pinned = v,
        }
    }
}

/// 全局历史状态：Rust 侧持有，前端经命令读。
#[derive(Default)]
pub struct ClipState(pub Mutex<Vec<ClipEntry>>);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn clip_dir(app: &AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("snappin"))
}

fn images_dir(app: &AppHandle) -> PathBuf {
    clip_dir(app).join("clip")
}

pub fn history_path(app: &AppHandle) -> PathBuf {
    clip_dir(app).join("clip-history.json")
}

/// 启动时从磁盘恢复历史（损坏则从空开始，不 panic）。
/// 上限调小后在此截断：丢弃最旧（列表尾）条目并删其图片文件，回写磁盘保持一致。
pub fn load_history(app: &AppHandle) -> Vec<ClipEntry> {
    let mut list = std::fs::read(history_path(app))
        .ok()
        .and_then(|b| serde_json::from_slice::<Vec<ClipEntry>>(&b).ok())
        .unwrap_or_default();
    sort_entries(&mut list);
    // 上限调小后截断：固定条豁免（P1 #9），从尾部（最旧未固定）丢起。
    let max = clip_cfg(app).1;
    let mut changed = false;
    let mut excess = list.len().saturating_sub(max);
    let mut i = list.len();
    while excess > 0 && i > 0 {
        i -= 1;
        if list[i].pinned() {
            continue;
        }
        let removed = list.remove(i);
        if let Some(file) = removed.image_file() {
            let _ = std::fs::remove_file(images_dir(app).join(file));
        }
        excess -= 1;
        changed = true;
    }
    // 过期清理（P1 #8）：启动时清一轮，0=永不过期。
    if purge_expired(app, &mut list) {
        changed = true;
    }
    if changed {
        save_history(app, &list);
    }
    list
}

/// 原子落盘（tmp + rename，同 config.rs 模式）。
fn save_history(app: &AppHandle, entries: &[ClipEntry]) {
    let path = history_path(app);
    if let Ok(json) = serde_json::to_vec_pretty(entries) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn next_id(list: &[ClipEntry]) -> u64 {
    list.iter().map(|e| e.id()).max().unwrap_or(0) + 1
}

/// P1 配置三元组：(忽略来源应用名单, 敏感跳过开关, 过期天数)。
/// ConfigState 不可用（极早期）回退 (空名单, 开, 0)——与 clip_cfg 同一回退纪律。
fn clip_p1_cfg(app: &AppHandle) -> (String, bool, u32) {
    app.try_state::<crate::commands::ConfigState>()
        .and_then(|s| {
            s.0.lock().ok().map(|c| {
                (
                    c.clip_ignore_apps.clone(),
                    c.clip_skip_sensitive,
                    c.clip_expire_days,
                )
            })
        })
        .unwrap_or((String::new(), true, 0))
}

/// 统一排序：固定条置顶，其内按时间新→旧。
/// ingest / load_history / copy_back / toggle_pin 共用——排序键只许在这一处定义。
fn sort_entries(list: &mut [ClipEntry]) {
    list.sort_by(|a, b| b.pinned().cmp(&a.pinned()).then(b.ts().cmp(&a.ts())));
}

/// 过期清理（P1 #8）：超过 clip_expire_days 天的条目删除（含图片文件）；
/// 0 = 永不过期；固定条豁免（P1 #9）。返回是否删了条目。
fn purge_expired(app: &AppHandle, list: &mut Vec<ClipEntry>) -> bool {
    let days = clip_p1_cfg(app).2;
    if days == 0 {
        return false;
    }
    let cutoff = now_ms().saturating_sub(days as u64 * 86_400_000);
    let (keep, dropped): (Vec<_>, Vec<_>) = std::mem::take(list)
        .into_iter()
        .partition(|e| e.pinned() || e.ts() >= cutoff);
    *list = keep;
    for e in &dropped {
        if let Some(file) = e.image_file() {
            let _ = std::fs::remove_file(images_dir(app).join(file));
        }
    }
    !dropped.is_empty()
}

/// 高置信机密特征（P1 #2）：只拦把握大的，宁漏勿误伤普通文本。
/// 覆盖：PEM 私钥块 / AWS AccessKeyId(AKIA|ASIA+16 位) / GitHub token(gh*_+36 位) / JWT。
fn is_sensitive(text: &str) -> bool {
    // PEM 私钥块。
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return true;
    }
    // AWS AccessKeyId / SecretAccessKey 都是 20 位大写字母数字，前缀锚定。
    if id_token(text, "AKIA", 20) || id_token(text, "ASIA", 20) {
        return true;
    }
    // GitHub token：ghp_/gho_/ghu_/ghs_/ghr_ + 36 位。
    if ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"]
        .iter()
        .any(|p| id_token(text, p, p.len() + 36))
    {
        return true;
    }
    // JWT：header 段 base64url 以 eyJ 开头 + 至少 20 位 + 后随 '.'。
    jwt_present(text)
}

/// 在 s 中找 prefix 后紧跟 (total - prefix.len()) 位字母数字的整体 token，
/// 且前一位不是字母数字（防 "task-…" 这类误命中）。
fn id_token(s: &str, prefix: &str, total: usize) -> bool {
    let b = s.as_bytes();
    let p = prefix.as_bytes();
    if total <= p.len() {
        return false;
    }
    b.windows(p.len()).enumerate().any(|(i, w)| {
        w == p
            && i + total <= b.len()
            && b[i + p.len()..i + total]
                .iter()
                .all(|c| c.is_ascii_alphanumeric())
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
    })
}

/// JWT 存在性：`eyJ` + ≥20 位 base64url + `.`，且 eyJ 前一位不是 base64url 字符。
fn jwt_present(s: &str) -> bool {
    let b = s.as_bytes();
    b.windows(3).enumerate().any(|(i, w)| {
        w == b"eyJ"
            && (i == 0
                || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_' || b[i - 1] == b'-'))
            && b[i + 3..]
                .iter()
                .take(20)
                .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
            && b.get(i + 23) == Some(&b'.')
    })
}

// 剪贴板属主查询走裸 FFI（与 commands.rs::SetWindowDisplayAffinity 同款做法）：
// 这五个函数的 C 契约稳定，不值得押 windows crate 各版本的签名漂移。
extern "system" {
    fn GetClipboardOwner() -> *mut core::ffi::c_void;
    fn GetWindowThreadProcessId(hwnd: *mut core::ffi::c_void, lpdwprocessid: *mut u32) -> u32;
    fn OpenProcess(
        dwdesiredaccess: u32,
        binherithandle: i32,
        dwprocessid: u32,
    ) -> *mut core::ffi::c_void;
    fn QueryFullProcessImageNameW(
        hprocess: *mut core::ffi::c_void,
        dwflags: u32,
        lpexefilename: *mut u16,
        lpdwsize: *mut u32,
    ) -> i32;
    fn CloseHandle(hobject: *mut core::ffi::c_void) -> i32;
}

/// 剪贴板属主进程名（小写文件名，如 "notepad.exe"）；查不到返回 None。
/// 150ms 合并窗后属主仍有效：属主在下一次 SetClipboardData 前不变，我们只读不清空。
fn clipboard_owner_process() -> Option<String> {
    unsafe {
        let owner = GetClipboardOwner();
        if owner.is_null() {
            return None;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(owner, &mut pid);
        if pid == 0 {
            return None;
        }
        // 0x1000 = PROCESS_QUERY_LIMITED_INFORMATION：查系统进程名不需要完全访问权。
        let h = OpenProcess(0x1000, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = vec![0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut size);
        CloseHandle(h);
        if ok == 0 || size == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        let name = path.rsplit(['/', '\\']).next().unwrap_or(&path);
        Some(name.to_ascii_lowercase())
    }
}

/// 忽略名单匹配（P1 #1）：多行文本，每行一个进程名；忽略大小写与 ".exe" 后缀差异。
fn ignored_by(app: &AppHandle, proc_name: &str) -> bool {
    let cfg = clip_p1_cfg(app).0;
    if cfg.trim().is_empty() {
        return false;
    }
    fn norm(s: &str) -> String {
        let s = s.trim().to_ascii_lowercase();
        s.strip_suffix(".exe").unwrap_or(&s).to_string()
    }
    let target = norm(proc_name);
    !target.is_empty()
        && cfg
            .lines()
            .any(|l| !l.trim().is_empty() && norm(l) == target)
}

/// 唯一入库入口：分配 id → 去重（同内容则置顶不新增）→ 超限清理 → 落盘。
/// 后续加固定/忽略/敏感过滤都在这里改，不散到调用方。
fn ingest(app: &AppHandle, mut entry: ClipEntry) {
    let Some(state) = app.try_state::<ClipState>() else {
        return;
    };
    let Ok(mut list) = state.0.lock() else {
        return;
    };
    entry.touch(now_ms());
    // P1 #1/#2 入口过滤（头注释承诺的落点）：忽略来源应用对三类条目生效；
    // 敏感跳过只拦文本（图片/文件没有可判定的机密文本面）。
    if let Some(owner) = clipboard_owner_process() {
        if ignored_by(app, &owner) {
            return;
        }
    }
    if let ClipEntry::Text { text, .. } = &entry {
        if clip_p1_cfg(app).1 && is_sensitive(text) {
            return;
        }
    }
    if entry.id() == 0 {
        // 调用方未带 id：分配（图片调用方在建窗写文件前就需要 id，会自带）。
        let id = next_id(&list);
        match &mut entry {
            ClipEntry::Text { id: e, .. } | ClipEntry::Files { id: e, .. } => *e = id,
            ClipEntry::Image { .. } => {}
        }
    }
    // 去重：删旧留新（PartialEq 只比内容不比 id/ts——见 impl 注释），但**继承旧条
    // 的 pinned**——重复复制同一内容不该丢掉固定状态（P1 #9）。
    if let Some(i) = list.iter().position(|e| e == &entry) {
        let old = list.remove(i);
        if old.pinned() {
            entry.set_pinned(true);
        }
    }
    list.insert(0, entry);
    sort_entries(&mut list);
    // 过期清理（P1 #8）：0=永不过期；固定条豁免。命中连图片一起删。
    purge_expired(app, &mut list);
    // 超限清理（P1 #9 固定条豁免）：统一排序后尾部 = 最旧的未固定条；
    // pop 到固定条说明剩余全是固定条 → 放回并停止（防死循环）。
    let max = clip_cfg(app).1;
    while list.len() > max {
        match list.pop() {
            Some(removed) if !removed.pinned() => {
                if let Some(file) = removed.image_file() {
                    let _ = std::fs::remove_file(images_dir(app).join(file));
                }
            }
            Some(pinned_back) => {
                list.push(pinned_back);
                break;
            }
            None => break,
        }
    }
    save_history(app, &list);
    drop(list);
    // 事件驱动刷新：面板不再后台轮询，捕获到新内容时通知它重拉（面板未开则无副作用）。
    let _ = app.emit_to("clip", "clip-changed", ());
}

/// 捕获当前剪贴板（文本 / 图片 / 文件路径），入库。监听线程收到消息后调用。
pub fn capture_now(app: &AppHandle) {
    // 0) 总开关：clip_enabled=false 不记录新内容（已有历史保留，面板仍可打开）。
    if !clip_cfg(app).0 {
        return;
    }
    // 1) 文件路径列表（Explorer 复制文件；优先判，避免文件路径被当文本吞）。
    if let Some(paths) = read_clipboard_files() {
        if is_self_write_echo(&SelfSig::Files(paths.clone())) {
            return; // 自写回回声，条目已在顶部，别重复入库。
        }
        ingest(
            app,
            ClipEntry::Files {
                id: 0,
                ts: 0,
                paths,
                pinned: false,
            },
        );
        return;
    }
    // 2) 图片 / 3) 文本，走 arboard（单实例顺序读）。
    let Ok(mut cb) = arboard::Clipboard::new() else {
        return;
    };
    if let Ok(img) = cb.get_image() {
        let (w, h) = (img.width as u32, img.height as u32);
        if is_self_write_echo(&SelfSig::Image(w, h)) {
            return; // 自写回回声：不建新 <id>.png、不入库（否则堆重复历史）。
        }
        // 异常大图（原始 RGBA > 64MB）不入库，防历史文件膨胀失控。
        if w > 0 && h > 0 && (w as u64) * (h as u64) * 4 <= 64 * 1024 * 1024 {
            if let Some(rgba) = image::RgbaImage::from_raw(w, h, img.bytes.into_owned()) {
                let dir = images_dir(app);
                let _ = std::fs::create_dir_all(&dir);
                let id = {
                    let state = app.try_state::<ClipState>();
                    state
                        .and_then(|s| s.0.lock().ok().map(|g| next_id(&g)))
                        .unwrap_or(1)
                };
                let file = format!("{id}.png");
                if let Ok(png) = crate::capture::png_bytes(&rgba) {
                    if std::fs::write(dir.join(&file), &png).is_ok() {
                        ingest(
                            app,
                            ClipEntry::Image {
                                id,
                                ts: 0,
                                file,
                                w,
                                h,
                                pinned: false,
                            },
                        );
                    }
                }
            }
            return;
        }
        return;
    }
    if let Ok(text) = cb.get_text() {
        if !text.is_empty() {
            if is_self_write_echo(&SelfSig::Text(text.clone())) {
                return; // 自写回回声，条目已在顶部。
            }
            ingest(
                app,
                ClipEntry::Text {
                    id: 0,
                    ts: 0,
                    text,
                    pinned: false,
                },
            );
        }
    }
}

/// 捕获线程拿到的 AppHandle（setup 阶段 `register_capture_app` 写入一次；
/// 运行期注入，按规则保留 OnceLock 而非 LazyLock）。
static CLIP_APP: std::sync::OnceLock<AppHandle> = std::sync::OnceLock::new();

/// 子类化后的原窗口过程（isize = WNDPROC 指针）。
static PREV_WND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 给主窗挂 `WM_CLIPBOARDUPDATE` 监听并子类化窗口过程。setup（主线程）调用一次。
/// 收到消息后 spawn 短线程读剪贴板——**绝不在主线程同步读**：别的进程可能
/// 长时间占着剪贴板，把 UI 卡死。
pub fn init_listener(window: &tauri::WebviewWindow) {
    use windows::Win32::System::DataExchange::AddClipboardFormatListener;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, GWLP_WNDPROC,
    };
    let Ok(hwnd) = window.hwnd() else {
        eprintln!("[snappin] 取主窗 HWND 失败，剪贴板监听未挂（历史不工作）");
        return;
    };
    // tauri 的 HWND（windows 0.61）与本 crate 的 0.58 不是同一类型但同布局，裸指针重包
    // （同 commands.rs 裸 FFI 先例）。
    let hwnd = HWND(hwnd.0);
    if let Err(e) = unsafe { AddClipboardFormatListener(hwnd) } {
        eprintln!("[snappin] AddClipboardFormatListener 失败: {e}");
        return;
    }
    unsafe {
        // 已子类化过不重复挂（防御性；init 只应被调一次）。
        if PREV_WND.load(std::sync::atomic::Ordering::Relaxed) != 0 {
            return;
        }
        let prev = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
        PREV_WND.store(prev, std::sync::atomic::Ordering::Relaxed);
        SetWindowLongPtrW(
            hwnd,
            GWLP_WNDPROC,
            clip_wnd_proc
                as unsafe extern "system" fn(
                    HWND,
                    u32,
                    windows::Win32::Foundation::WPARAM,
                    windows::Win32::Foundation::LPARAM,
                ) -> windows::Win32::Foundation::LRESULT as usize as isize,
        );
    }
    // 常驻消费线程：把 PENDING 标记变成一次 capture_now（见 capture_thread_job 注释）。
    std::thread::Builder::new()
        .name("clip-capture".into())
        .spawn(capture_thread_job)
        .ok();
}

/// 子类化窗口过程：只拦 WM_CLIPBOARDUPDATE（797）触发捕获，其余全部转发原过程。
unsafe extern "system" fn clip_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::{CallWindowProcW, WM_CLIPBOARDUPDATE};
    if msg == WM_CLIPBOARDUPDATE {
        // 只置标记，消费交给常驻工作线程（见 PENDING / capture_thread_job 注释）。
        PENDING.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let prev_raw = PREV_WND.load(std::sync::atomic::Ordering::Relaxed);
    let prev = if prev_raw == 0 {
        None
    } else {
        Some(unsafe {
            std::mem::transmute::<
                isize,
                unsafe extern "system" fn(
                    HWND,
                    u32,
                    windows::Win32::Foundation::WPARAM,
                    windows::Win32::Foundation::LPARAM,
                ) -> windows::Win32::Foundation::LRESULT,
            >(prev_raw)
        })
    };
    match prev {
        Some(p) => CallWindowProcW(Some(p), hwnd, msg, wp, lp),
        None => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// 待处理标记：WM_CLIPBOARDUPDATE 只置 1，由常驻工作线程消费。
/// 旧版「每条消息 spawn 一个线程」有竞态：两个线程都在对方 ingest 前读到旧
/// 列表、各自插入，去重失效（实测一次复制入库两条，时差 5ms）。
/// 常驻单线程 + 150ms 合并窗：同一批连发的多条消息自然收敛成一次消费。
static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn capture_thread_job() {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(150));
        if !PENDING.swap(false, std::sync::atomic::Ordering::Relaxed) {
            continue;
        }
        // 清完标记再消费：期间新来的消息再置 1，下轮接着处理。
        if let Some(app) = CLIP_APP.get() {
            capture_now(app);
        }
    }
}

/// 注册捕获用的 AppHandle（setup 阶段调用一次）。
pub fn register_capture_app(app: AppHandle) {
    let _ = CLIP_APP.set(app);
}

/// 打开剪贴板（带重试）：别的进程可能正占着剪贴板，此时 `OpenClipboard` 直接失败。
///
/// why-not 失败就放弃：剪贴板被占用是**瞬时**状态（对方正在复制），重试几次
/// 就能拿到；放弃则等于「那次复制没记下来」。
fn open_clipboard_retry() -> bool {
    use windows::Win32::System::DataExchange::OpenClipboard;
    for _ in 0..12 {
        match unsafe { OpenClipboard(None) } {
            Ok(()) => return true,
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
    false
}

/// 关剪贴板（RAII 用）：异常/提前返回路径也必须关，否则剪贴板被本进程锁住，
/// 用户点一次复制就没反应。
struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        use windows::Win32::System::DataExchange::CloseClipboard;
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// 读一次剪贴板里的文件列表。
///
/// 返回值三种状态要分开（读不到 ≠ 里面没有）：
/// - `Some(paths)`：确实有文件列表
/// - `None`：剪贴板打开了，但里面没有文件（不是错误）
/// - `Err(())`：`GetClipboardData` 拿不到句柄 —— 多半是**延迟渲染**
///   （资源管理器复制文件时，文件列表要等源程序渲染），值得重试。
fn read_clipboard_files_once() -> Result<Option<Vec<String>>, ()> {
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    if !open_clipboard_retry() {
        return Err(());
    }
    let _guard = ClipboardGuard;

    unsafe {
        let Ok(handle) = GetClipboardData(CF_HDROP.0 as u32) else {
            return Err(());
        };
        if handle.is_invalid() {
            return Err(());
        }
        let hdrop = HDROP(handle.0);
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        let mut paths = Vec::new();
        for i in 0..count {
            let len = DragQueryFileW(hdrop, i, None);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let n = DragQueryFileW(hdrop, i, Some(&mut buf));
            if n > 0 {
                paths.push(String::from_utf16_lossy(&buf[..n as usize]));
            }
        }
        Ok((!paths.is_empty()).then_some(paths))
    }
}

/// 用 windows crate 读 CF_HDROP 文件列表；无文件或剪贴板不可读返回 None。
///
/// **必须先 `OpenClipboard`**（D10，2026-10-20）：不打开剪贴板直接 `GetClipboardData`
/// 一律失败返回 None —— 这就是「剪贴板面板里从来没有文件条目、用户得先手动打开
/// 一个程序把文件放进剪贴板才看得到」的根因（实测 1000 条历史里 files = 0 条）。
///
/// 注意：`GetClipboardData` 返回的 HGLOBAL 归剪贴板所有，**只读不 free**。
fn read_clipboard_files() -> Option<Vec<String>> {
    // 延迟渲染（资源管理器复制文件）要重试几次；确实没有文件则立刻返回，不空等。
    for _ in 0..4 {
        match read_clipboard_files_once() {
            Ok(v) => return v,
            Err(()) => std::thread::sleep(std::time::Duration::from_millis(60)),
        }
    }
    // 走到这里 = 剪贴板打开了但拿不到文件列表。**要留下痕迹**：否则用户抱怨
    // 「复制了文件但面板里没有」时，界面上什么线索都没有（2026-10-20 D10 的排查痛点）。
    // 常见原因：源程序还没渲染完（延迟渲染）、别的进程占着剪贴板。
    eprintln!(
        "[snappin] 读剪贴板文件列表失败：CF_HDROP 取不到（延迟渲染或被占用），本次不记文件条目"
    );
    None
}

/// 前端命令支撑：读当前历史（新→旧）；图片条目附带缩略图 base64（data URL 前缀，
/// 直接喂 <img src>；原图最长边 >320 时缩放，控制传输体积）。
/// 缩略图走 `THUMB_CACHE`（key=文件名，内容不可变）：命中直接复用，只有新图才解码，
/// 避免每次 `clip_list` 都对全部图片重编码 base64（曾是全局卡顿根因）。
pub fn list_entries_with_thumbs(state: &ClipState) -> Vec<serde_json::Value> {
    let Ok(g) = state.0.lock() else {
        return Vec::new();
    };
    // 当前仍存在的图片文件集合，用来回收缓存里已删/已过期条目的缩略图。
    let live: std::collections::HashSet<String> = g
        .iter()
        .filter_map(|e| e.image_file().map(|s| s.to_string()))
        .collect();
    let mut cache_guard = THUMB_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let cache = cache_guard.get_or_insert_with(HashMap::new);
    cache.retain(|k, _| live.contains(k));
    g.iter()
        .map(|e| match e {
            ClipEntry::Image { file, w, h, .. } => {
                let thumb = cache
                    .entry(file.clone())
                    .or_insert_with(|| thumb_data_url(file, *w, *h))
                    .clone();
                let mut v = serde_json::to_value(e).unwrap_or(serde_json::Value::Null);
                v["thumb"] = serde_json::Value::String(thumb);
                v
            }
            // D9：文件条目里带动图（gif/webp/apng）时标出来，前端据此懒加载动图本体
            // （`clip_animated`）。动图**不再被压成单帧**：位图分支拿到的是系统位图，
            // 多帧信息在进本进程前就没了；只有文件本体还带着多帧。
            ClipEntry::Files { id, paths, .. } => {
                let mut v = serde_json::to_value(e).unwrap_or(serde_json::Value::Null);
                if paths.iter().any(|p| is_animated_file(p)) {
                    v["animated"] = serde_json::Value::Bool(true);
                    v["animated_id"] = serde_json::Value::String(id.to_string());
                }
                v
            }
            _ => serde_json::to_value(e).unwrap_or(serde_json::Value::Null),
        })
        .collect()
}

/// 动图文件（多帧）：这些格式原样粘出去还是动的（D9）。
fn is_animated_file(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".gif") || p.ends_with(".webp") || p.ends_with(".apng")
}

/// 动图的 MIME（按扩展名）。
fn animated_mime(path: &str) -> &'static str {
    let p = path.to_ascii_lowercase();
    if p.ends_with(".webp") {
        "image/webp"
    } else if p.ends_with(".apng") {
        "image/apng"
    } else {
        "image/gif"
    }
}

/// 前端命令支撑：取动图条目的 data URL（前端懒加载用）。
///
/// why-not 把动图 base64 塞进 `clip_list`：面板条目多、每 1.5s 刷新一次，
/// 几个动图就是几 MB 的 IPC 传输，直接把面板拖死。首帧缩略图仍走 clip_list 的 thumb，
/// 这里只在用户真的看得到那一条时才读一次文件。
#[tauri::command]
pub fn clip_animated(id: String, state: tauri::State<ClipState>) -> Option<String> {
    let Ok(id) = id.parse::<u64>() else {
        return None;
    };
    let g = state.0.lock().ok()?;
    let entry = g.iter().find(|e| e.id() == id)?;
    let ClipEntry::Files { paths, .. } = entry else {
        return None;
    };
    let path = paths.iter().find(|p| is_animated_file(p))?;
    let bytes = std::fs::read(path).ok()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(format!("data:{};base64,{b64}", animated_mime(path)))
}

/// 生成缩略图 data URL：读 PNG → 最长边缩到 ≤320 → PNG 编码 → base64。
/// 读图/解码失败返回空串（前端显示占位，不致命）。
fn thumb_data_url(file: &str, w: u32, h: u32) -> String {
    let Ok(bytes) = std::fs::read(images_dir_for_thumbs().join(file)) else {
        return String::new();
    };
    let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png) else {
        return String::new();
    };
    let max_side = w.max(h);
    let img = if max_side > 320 {
        img.resize(320, 320, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut buf = std::io::Cursor::new(Vec::new());
    if img.write_to(&mut buf, image::ImageFormat::Png).is_err() {
        return String::new();
    }
    format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(buf.get_ref())
    )
}

/// 缩略图用的图片目录（无 AppHandle 的场合，从 CLIP_APP 取）。
fn images_dir_for_thumbs() -> PathBuf {
    match CLIP_APP.get() {
        Some(app) => images_dir(app),
        None => std::env::temp_dir().join("snappin").join("clip"),
    }
}

/// 前端命令支撑：把指定条目写回系统剪贴板（用户再手动 Ctrl+V）。
pub fn copy_back(app: &AppHandle, id: u64) -> Result<(), String> {
    let entry = {
        let state = app.try_state::<ClipState>().ok_or("剪贴板状态未初始化")?;
        let g = state.0.lock().map_err(|e| e.to_string())?;
        g.iter()
            .find(|e| e.id() == id)
            .cloned()
            .ok_or_else(|| format!("条目 {id} 不存在"))?
    };
    {
        let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
        match &entry {
            ClipEntry::Text { text, .. } => {
                cb.set_text(text.clone()).map_err(|e| e.to_string())?;
                mark_self_write(SelfSig::Text(text.clone()));
            }
            ClipEntry::Image { file, .. } => {
                let bytes = std::fs::read(images_dir(app).join(file))
                    .map_err(|e| format!("读历史图片失败: {e}"))?;
                let img = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
                    .map_err(|e| format!("解码历史图片失败: {e}"))?;
                let rgba = img.to_rgba8();
                let (iw, ih) = (rgba.width(), rgba.height());
                cb.set_image(arboard::ImageData {
                    width: iw as usize,
                    height: ih as usize,
                    bytes: std::borrow::Cow::from(rgba.as_raw().as_slice()),
                })
                .map_err(|e| e.to_string())?;
                mark_self_write(SelfSig::Image(iw, ih));
            }
            ClipEntry::Files { paths, .. } => {
                write_clipboard_files(paths)?;
                mark_self_write(SelfSig::Files(paths.clone()));
            }
        }
    }
    // 写回后置顶（重排序 + 刷 ts + 落盘）。
    let state = app.try_state::<ClipState>().ok_or("剪贴板状态未初始化")?;
    let mut g = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(e) = g.iter_mut().find(|e| e.id() == id) {
        e.touch(now_ms());
    }
    sort_entries(&mut g);
    save_history(app, &g);
    Ok(())
}

/// 前端命令支撑：删除一条（图片连文件删）。
pub fn remove_entry(app: &AppHandle, id: u64) -> Result<(), String> {
    let state = app.try_state::<ClipState>().ok_or("剪贴板状态未初始化")?;
    let mut g = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(pos) = g.iter().position(|e| e.id() == id) {
        let removed = g.remove(pos);
        if let Some(file) = removed.image_file() {
            let _ = std::fs::remove_file(images_dir(app).join(file));
            drop_thumb(file);
        }
        save_history(app, &g);
    }
    Ok(())
}

/// 前端命令支撑：固定/取消固定一条（P1 #9）；返回新 pinned 状态。
pub fn toggle_pin(app: &AppHandle, id: u64) -> Result<bool, String> {
    let state = app.try_state::<ClipState>().ok_or("剪贴板状态未初始化")?;
    let mut g = state.0.lock().map_err(|e| e.to_string())?;
    let e = g
        .iter_mut()
        .find(|e| e.id() == id)
        .ok_or_else(|| format!("条目 {id} 不存在"))?;
    let now = !e.pinned();
    e.set_pinned(now);
    sort_entries(&mut g);
    save_history(app, &g);
    Ok(now)
}

/// 前端命令支撑：清空全部历史（含图片文件）。
pub fn clear_all(app: &AppHandle) -> Result<(), String> {
    let state = app.try_state::<ClipState>().ok_or("剪贴板状态未初始化")?;
    let mut g = state.0.lock().map_err(|e| e.to_string())?;
    for e in g.iter() {
        if let Some(file) = e.image_file() {
            let _ = std::fs::remove_file(images_dir(app).join(file));
        }
    }
    g.clear();
    save_history(app, &g);
    clear_thumbs();
    Ok(())
}

/// 前端命令支撑：导出历史到目录 `dest`（P1 #11）。
/// 落 `dest/clip-history.json`（当前历史全量，array）+ 把引用到的图片拷进 `dest/clip-images/`。
/// dest 由前端经 `pick_save_path`（空名=选目录分支）取得，已是既存目录。
pub fn export_history(app: &AppHandle, dest: &str) -> Result<(), String> {
    let dest = PathBuf::from(dest);
    if dest.as_os_str().is_empty() {
        return Err("导出目录为空".into());
    }
    std::fs::create_dir_all(&dest).map_err(|e| format!("建导出目录失败: {e}"))?;
    // 直接拷源 json（load_history 保证过它是合法 array；比重新序列化更少一次转码分歧）。
    let src_json = history_path(app);
    let dst_json = dest.join("clip-history.json");
    std::fs::copy(&src_json, &dst_json).map_err(|e| format!("导出 clip-history.json 失败: {e}"))?;
    // 图片：只拷历史里真正引用到的文件，避免把已删条目的孤儿图带出。
    let img_dst = dest.join("clip-images");
    let files: Vec<String> = {
        let state = app.try_state::<ClipState>().ok_or("剪贴板状态未初始化")?;
        let g = state.0.lock().map_err(|e| e.to_string())?;
        g.iter()
            .filter_map(|e| e.image_file().map(|s| s.to_string()))
            .collect()
    };
    if !files.is_empty() {
        std::fs::create_dir_all(&img_dst).map_err(|e| format!("建图片导出目录失败: {e}"))?;
        let src_dir = images_dir(app);
        for f in files {
            let _ = std::fs::copy(src_dir.join(&f), img_dst.join(&f));
        }
    }
    Ok(())
}

/// 把文件路径列表写回剪贴板（CF_HDROP + DROPFILES 布局）。
/// 结果条的「复制」按钮也用它（GIF/视频没有图片剪贴板表示，只能按文件粘）。
///
/// **D8（2026-10-20）**：只写 CF_HDROP 时，浏览器输入框、多数聊天窗口的文本粘贴区
/// 根本不收文件拖放 —— 用户看到「已复制」却粘不出来（实测：复制基本无效）。
/// 因此这里同时写三种格式：
/// - `CF_HDROP`：拖放文件（资源管理器、聊天窗口的文件粘贴）
/// - `FileNameW`：文件路径列表（部分应用只认它）
/// - `CF_HTML`：HTML 片段（`<img src="file:///…">`），Word/邮件/聊天富文本区认它，
///   图片能直接贴进去，动图还是动的
pub fn write_clipboard_files(paths: &[String]) -> Result<(), String> {
    use windows::core::w;
    use windows::Win32::Foundation::{GlobalFree, BOOL, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    use windows::Win32::System::Ole::CF_HDROP;
    use windows::Win32::UI::Shell::DROPFILES;

    if paths.is_empty() {
        return Err("空文件列表".into());
    }
    // DROPFILES 头 + 双 NUL 结尾的宽字符路径串（fWide=1）。
    let mut wide: Vec<u16> = Vec::new();
    for p in paths {
        wide.extend(p.encode_utf16());
        wide.push(0);
    }
    wide.push(0);
    let header_size = std::mem::size_of::<DROPFILES>();
    let total = header_size + wide.len() * 2;
    // FileNameW：双 NUL 结尾的 UTF-16 路径列表（无 DROPFILES 头）。
    let mut names: Vec<u16> = Vec::new();
    for p in paths {
        names.extend(p.encode_utf16());
        names.push(0);
    }
    names.push(0);
    let html = build_cf_html(paths);
    unsafe {
        OpenClipboard(None).map_err(|e| format!("打开剪贴板失败: {e}"))?;
        let result = (|| {
            EmptyClipboard().map_err(|e| format!("清空剪贴板失败: {e}"))?;
            let h = GlobalAlloc(GMEM_MOVEABLE, total).map_err(|e| format!("分配内存失败: {e}"))?;
            let ptr = GlobalLock(h) as *mut u8;
            if ptr.is_null() {
                return Err("锁定内存失败".into());
            }
            // DROPFILES 头写进块首，路径串跟在后面。注意 windows 0.58 里该结构
            // 是 packed(1)，字段 fNC/fWide 是 Foundation::BOOL。
            let df = DROPFILES {
                pFiles: header_size as u32,
                pt: Default::default(),
                fNC: BOOL(0),
                fWide: BOOL(1),
            };
            std::ptr::write_unaligned(ptr as *mut DROPFILES, df);
            for (i, w) in wide.iter().enumerate() {
                std::ptr::write_unaligned(ptr.add(header_size + i * 2) as *mut u16, *w);
            }
            let _ = GlobalUnlock(h);
            // 成功后剪贴板持有 h 的所有权，不再由我们释放；失败则还在我们手里（必须释放，
            // 否则每次写失败漏一块全局内存）。
            if let Err(e) = SetClipboardData(CF_HDROP.0 as u32, HANDLE(h.0)) {
                let _ = GlobalFree(h);
                return Err(format!("写入剪贴板失败: {e}"));
            }

            // FileNameW（注册格式）：路径列表。
            let fmt_name = RegisterClipboardFormatW(w!("FileNameW"));
            if fmt_name != 0 {
                let _ = set_clipboard_bytes(fmt_name, &names_to_bytes(&names));
            }
            // CF_HTML（注册格式）：富文本片段。
            let fmt_html = RegisterClipboardFormatW(w!("HTML Format"));
            if fmt_html != 0 {
                let _ = set_clipboard_bytes(fmt_html, &html);
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

/// 宽字符字节序列 → UTF-16 原始字节（HGLOBAL 里要放的是 UTF-16LE 原始内存）。
fn names_to_bytes(names: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(names.len() * 2);
    for c in names {
        out.extend_from_slice(&c.to_le_bytes());
    }
    out
}

/// 往剪贴板写一段原始字节（格式由调用方给）。成功时剪贴板接管这块内存。
fn set_clipboard_bytes(format: u32, bytes: &[u8]) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::SetClipboardData;
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    unsafe {
        // +1：CF_HTML 约定末尾要有 NUL 终止符。
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len() + 1)
            .map_err(|e| format!("分配内存失败: {e}"))?;
        let ptr = GlobalLock(h) as *mut u8;
        if ptr.is_null() {
            // 锁不上这块内存时它仍是我们的责任（还没交给剪贴板）→ 释放，否则每次失败漏一块。
            let _ = GlobalFree(h);
            return Err("锁定内存失败".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        *ptr.add(bytes.len()) = 0;
        let _ = GlobalUnlock(h);
        // 交给剪贴板后所有权归系统，失败则还在我们手里 → 必须自己释放。
        if let Err(e) = SetClipboardData(format, HANDLE(h.0)) {
            let _ = GlobalFree(h);
            return Err(format!("写入剪贴板失败: {e}"));
        }
        Ok(())
    }
}

/// 本地路径 → `file:///` URL（CF_HTML 里的 `<img src>` 要用它）。
fn path_to_file_url(p: &str) -> String {
    let slashed = p.replace('\\', "/");
    let mut out = String::from("file:///");
    // 首段是盘符（`C:/…`）时不加斜杠；其余按段做最小百分号编码（空格等）。
    for (i, seg) in slashed.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for b in seg.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
    }
    out
}

/// 拼 CF_HTML 负载（`HTML Format` 注册格式）。
///
/// 头部里的偏移量是**字节偏移**，指向负载内的位置；用定宽 `{:08}` 写数字，
/// 头部长度才不会随数字变化（先定长再回填，避免自己算错偏移）。
fn build_cf_html(paths: &[String]) -> Vec<u8> {
    let fragment: String = paths
        .iter()
        .map(|p| format!("<img src=\"{}\">", path_to_file_url(p)))
        .collect::<Vec<_>>()
        .join("");
    // 头部用定宽 8 位数字：先按定长模板算头部长度，再回填真实偏移——
    // 偏移是**字节偏移**，数字位数一变头部长度就变，自己算容易错位。
    let head_tpl = "Version:0.9\r\nStartHTML:00000000\r\nEndHTML:00000000\r\nStartFragment:00000000\r\nEndFragment:00000000\r\n";
    let header_len = head_tpl.len();
    let start = header_len;
    let end = header_len + fragment.len();
    let head = format!(
        "Version:0.9\r\nStartHTML:{:08}\r\nEndHTML:{:08}\r\nStartFragment:{:08}\r\nEndFragment:{:08}\r\n",
        start, end, start, end
    );
    let mut out = head.into_bytes();
    out.extend_from_slice(fragment.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 去重语义的核心不变量：PartialEq 只比内容不比 id/ts。
    /// （文本比 text、图片比 file、文件比 paths——同内容不同 id 视为重复。）
    #[test]
    fn text_equality_ignores_id_and_ts() {
        let a = ClipEntry::Text {
            id: 1,
            ts: 100,
            text: "hi".into(),
            pinned: false,
        };
        let b = ClipEntry::Text {
            id: 2,
            ts: 200,
            text: "hi".into(),
            pinned: false,
        };
        assert_eq!(a, b);
        let c = ClipEntry::Text {
            id: 2,
            ts: 200,
            text: "ho".into(),
            pinned: false,
        };
        assert_ne!(a, c);
    }

    #[test]
    fn image_equality_by_file() {
        let a = ClipEntry::Image {
            id: 1,
            ts: 100,
            file: "1.png".into(),
            w: 10,
            h: 10,
            pinned: false,
        };
        let b = ClipEntry::Image {
            id: 2,
            ts: 200,
            file: "1.png".into(),
            w: 10,
            h: 10,
            pinned: false,
        };
        assert_eq!(a, b);
    }

    /// P1 #9 排序不变量：固定条置顶，其余按 ts 新→旧（sort_entries 是唯一排序定义处）。
    #[test]
    fn pinned_sorts_before_ts() {
        let mut v = vec![
            ClipEntry::Text {
                id: 1,
                ts: 300,
                text: "new".into(),
                pinned: false,
            },
            ClipEntry::Text {
                id: 2,
                ts: 100,
                text: "old".into(),
                pinned: true,
            },
            ClipEntry::Text {
                id: 3,
                ts: 200,
                text: "mid".into(),
                pinned: false,
            },
        ];
        sort_entries(&mut v);
        let ids: Vec<u64> = v.iter().map(|e| e.id()).collect();
        assert_eq!(ids, vec![2, 1, 3]);
    }
}
