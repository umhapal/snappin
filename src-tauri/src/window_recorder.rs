//! Agent 窗口录屏 session（规划 Phase 2，合同 §6.2 / §7.2）。
//!
//! 为什么是「WGC 拷帧 + 独立喂帧线程 + ffmpeg 编码」这三段，而不是直接把 WGC 的帧
//! 喂给 windows-capture 自带的 Media Foundation 编码器：
//! ① **WGC 只在画面变化时出帧**（crate 文档明写 `MinUpdateInterval` 只限制更新频率、
//!    不保证周期出帧）。直喂编码器会得到「帧数 = 变化次数」的产物：静止窗口录 10 秒
//!    只有一两帧，时长与用户看到的录制时长完全不符。
//! ② 直喂还要让「停止」依赖「再来一帧」——crate 的 `InternalCaptureControl` 只能在
//!    `on_frame_arrived` 里拿到且 `stop()` 消费它，外部线程停不了消息循环。目标窗口
//!    静止时 stop() 会一直挂着。
//! 所以：WGC 线程只负责把帧拷进 `latest` 槽；喂帧线程按 fps 定步长取槽里的帧写进
//! ffmpeg stdin（没有新帧就重发上一帧）——产物是真 CFR，时长 = 真实录制时长，
//! 收尾只依赖喂帧线程（关 stdin → ffmpeg 写 trailer），与目标窗口是否再出帧无关。
//!
//! 暂停语义（已选定，别改成别的）：**暂停期间不写帧，暂停时段被剔除出产物时间轴**，
//! 与既有屏幕录制（recorder.rs 分段拼接）语义一致。做不到「暂停时段留成冻结画面」而
//! 不写帧——那是另一种语义，改它要连带改时间轴记账，别顺手改。

use std::ffi::c_void;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{InvalidateRect, UpdateWindow};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window as WgcWindow;

use crate::loopback::{LoopbackCapture, MixFormat};

/// 响应里如实标注的采集来源（合同 §7.2）。
pub const SOURCE: &str = "wgc_window";

/// GUI 应用里拉起控制台子进程不加它，ffmpeg 黑框会弹到前台（与 `recorder.rs` 同因）。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 等首帧的上限。WGC 首帧实测 0.4–0.6s，给 3s 覆盖冷启动；再久就是真的不可捕获。
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(3);
/// 等喂帧线程收尾（关 stdin → ffmpeg 写 trailer）的上限。
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone)]
pub struct RecError {
    pub code: &'static str,
    pub message: String,
}

impl RecError {
    /// `pub(crate)`：同 crate 的 `loopback.rs` 要构造同型错误（契约冻结成同一个 `RecError`，
    /// 免得两模块各定义一份、HTTP 层的 `e.code` 匹配错位）。
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StartOptions {
    pub hwnd: isize,
    pub window_id: String,
    pub pid: u32,
    pub title: String,
    pub class_name: String,
    pub fps: u32,
    pub bitrate_k: u32,
    pub cursor: bool,
    pub audio: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct Started {
    pub session_id: String,
    pub state: &'static str,
    pub source: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub window_id: String,
    /// 本次是否含音轨（loopback 系统声）。回显请求，也是产物形态的权威声明。
    pub audio: bool,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub session_id: String,
    pub state: &'static str,
    pub source: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub window_id: String,
    pub duration_ms: u64,
    pub frames: u64,
    pub dropped: u64,
    pub geometry_changed: bool,
    /// 首个致命错误的**机读码**（契约 §7.3 码表；无错误为 None）。与 `error` 同一错误，
    /// 一个给调用方分支、一个给人读——别让调用方去解析中文。
    pub error_code: Option<&'static str>,
    pub error: Option<String>,
    /// 本次是否含音轨（loopback 系统声）。
    pub audio: bool,
    /// 已写进音频管道的 PCM 字节数（无音频恒 0）。调用方可据此判断音频是否真的在流动。
    pub audio_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct Stopped {
    pub session_id: String,
    pub state: &'static str,
    pub source: &'static str,
    pub path: Option<String>,
    pub duration_ms: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u64,
    pub dropped: u64,
    /// 首个致命错误的机读码（与 `Status::error_code` 同源同义）。收尾响应也带上它：
    /// 运行中出过事故的会话，`stop` 的调用方同样需要区分「收尾正常」与「带着 target_changed 收尾」。
    pub error_code: Option<&'static str>,
    /// 首个致命错误的人读说明（与 `Status::error` 同源同义）。
    pub error: Option<String>,
    /// 会话是否因目标几何变化而提前终止（与 `Status::geometry_changed` 同源同义）。
    pub geometry_changed: bool,
    /// 本次是否含音轨（loopback 系统声）。
    pub audio: bool,
    /// 本次写进音频管道的 PCM 字节数（无音频恒 0）。
    pub audio_bytes: u64,
}

/// 会话内共享状态：WGC 线程写帧与计数，喂帧线程取帧与写 stdin，HTTP 线程置停 + 读统计。
struct Shared {
    /// false = 该收尾了（喂帧线程的循环条件）。
    running: AtomicBool,
    /// 暂停标志。**Arc 而非裸 AtomicBool**：loopback 采集线程也要读它（暂停期间音频同样不写管道），
    /// 两线程必须看同一个对象，否则会各自持有副本、暂停语义分叉。
    paused: Arc<AtomicBool>,
    geometry_changed: AtomicBool,
    /// 本次是否含音轨。会话生命周期内不变（音频链一旦起就跑到 stop）。
    audio: bool,
    /// 传给 loopback 采集线程的停止标志（`stop()` 先置它，再关管道）。
    audio_stop: Arc<AtomicBool>,
    /// loopback 采集句柄。`stop()` 从这里取走并 join；`status()` 只读 `bytes()`。
    audio_cap: Mutex<Option<LoopbackCapture>>,
    /// 已写进音频管道的 PCM 字节数。从 `audio_cap.bytes()` 刷新而来（句柄被取走后仍可读）。
    audio_bytes: AtomicU64,
    /// 已写进 ffmpeg 的帧数。
    frames: AtomicU64,
    /// WGC 交来的帧数（含暂停期间丢弃的）。
    captured: AtomicU64,
    /// 暂停期间丢弃的帧数。
    dropped: AtomicU64,
    /// 建会话时 WGC 首帧的尺寸：帧尺寸变了就说明目标窗口被缩放。
    want_w: u32,
    want_h: u32,
    /// 会话身份（`rec-YYYYMMDD-NNNN`）与目标 window_id。**存在 Shared 而不是 Session 里**：
    /// WGC 线程（`on_frame_arrived`）只拿得到 `Arc<Shared>`，几何变化/窗口关闭这些
    /// 「录制中事故」的落盘日志必须带会话身份，否则事后无法把日志行对到具体会话。
    id: String,
    window_id: String,
    started: Instant,
    paused_since: Mutex<Option<Instant>>,
    paused_acc: Mutex<Duration>,
    latest: Mutex<Option<Arc<Vec<u8>>>>,
    sink: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,
    /// 首个致命错误：`(错误码, 人读说明)`。**码与说明放同一个 Mutex**，不拆成两个锁——
    /// 拆开后读者可能读到「有码无说明」或反之，status 就会出现自相矛盾的中间态。
    error: Mutex<Option<(&'static str, String)>>,
    first_frame: Mutex<Option<Sender<()>>>,
}

impl Shared {
    fn new(
        id: String,
        window_id: String,
        want_w: u32,
        want_h: u32,
        first_tx: Sender<()>,
        audio: bool,
        paused: Arc<AtomicBool>,
        audio_stop: Arc<AtomicBool>,
        audio_cap: Option<LoopbackCapture>,
    ) -> Self {
        Self {
            running: AtomicBool::new(true),
            paused,
            geometry_changed: AtomicBool::new(false),
            audio,
            audio_stop,
            audio_cap: Mutex::new(audio_cap),
            audio_bytes: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            captured: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            want_w,
            want_h,
            id,
            window_id,
            started: Instant::now(),
            paused_since: Mutex::new(None),
            paused_acc: Mutex::new(Duration::ZERO),
            latest: Mutex::new(None),
            sink: Mutex::new(None),
            child: Mutex::new(None),
            error: Mutex::new(None),
            first_frame: Mutex::new(Some(first_tx)),
        }
    }

    /// 记首个致命错误：`(错误码, 人读说明)`。首个即权威——后续错误多是首个的连带
    /// （如几何变化 → ffmpeg 收尾失败），覆盖会把根因换成症状。
    fn set_error(&self, code: &'static str, msg: impl Into<String>) {
        if let Ok(mut g) = self.error.lock() {
            if g.is_none() {
                *g = Some((code, msg.into()));
            }
        }
    }

    /// 首个错误的**错误码**（无错误时 None）。契约 §7.3 的码是稳定的机读标识，
    /// 调用方按它分支，不解析中文 message。
    fn error_code(&self) -> Option<&'static str> {
        self.error
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|(c, _)| *c))
    }

    /// 首个错误的**人读说明**（无错误时 None）。
    fn error_text(&self) -> Option<String> {
        self.error
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|(_, m)| m.clone()))
    }

    /// 当前已写进音频管道的 PCM 字节数。句柄还在就现取并缓存；已被 `stop()` 取走就读缓存值
    /// （`stop()` 之后 `status()` 仍要能答「本次录到多少音频」）。
    fn audio_bytes_now(&self) -> u64 {
        if let Ok(g) = self.audio_cap.lock() {
            if let Some(c) = g.as_ref() {
                let b = c.bytes();
                self.audio_bytes.store(b, Ordering::SeqCst);
                return b;
            }
        }
        self.audio_bytes.load(Ordering::SeqCst)
    }

    /// 有效录制时长：墙钟时间减去暂停累计（暂停时段不写帧，产物时间轴里也就没有它）。
    fn duration_ms(&self) -> u64 {
        let mut paused = self.paused_acc.lock().map(|g| *g).unwrap_or(Duration::ZERO);
        if let Ok(g) = self.paused_since.lock() {
            if let Some(t) = *g {
                paused += t.elapsed();
            }
        }
        self.started.elapsed().saturating_sub(paused).as_millis() as u64
    }
}

struct Session {
    id: String,
    window_id: String,
    hwnd: isize,
    width: u32,
    height: u32,
    fps: u32,
    path: PathBuf,
    shared: Arc<Shared>,
    done: Receiver<Result<(), String>>,
}

static SLOT: LazyLock<Mutex<Option<Session>>> = LazyLock::new(|| Mutex::new(None));

fn seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

/// `rec-YYYYMMDD-NNNN`（合同 §7.2 示例形态）。自实现民用历，不引日期依赖。
fn session_id_new() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    // Howard Hinnant civil_from_days：1970-01-01 起的天数 → 公历年月日。
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("rec-{y:04}{m:02}{d:02}-{:04}", seq())
}

/// WGC 会话：只把帧拷进 `latest` 槽，别的事都不做（见文件头 why-not）。
struct WindowRecHandler {
    shared: Arc<Shared>,
    scratch: Vec<u8>,
}

impl GraphicsCaptureApiHandler for WindowRecHandler {
    type Flags = Arc<Shared>;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            shared: ctx.flags,
            scratch: Vec::new(),
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if !self.shared.running.load(Ordering::SeqCst) {
            capture_control.stop();
            return Ok(());
        }
        let fb = match frame.buffer() {
            Ok(fb) => fb,
            Err(e) => {
                // 映射失败多是设备丢失/会话被拔：报错并收尾，产物按失败处理（不静默继续）。
                self.shared
                    .set_error("capture_failed", format!("WGC 帧缓冲映射失败：{e}"));
                self.shared.running.store(false, Ordering::SeqCst);
                capture_control.stop();
                return Ok(());
            }
        };
        let (w, h) = (fb.width(), fb.height());
        self.shared.captured.fetch_add(1, Ordering::SeqCst);
        if let Ok(mut tx) = self.shared.first_frame.lock() {
            if let Some(t) = tx.take() {
                let _ = t.send(());
            }
        }
        if w == 0 || h == 0 {
            self.shared.set_error("capture_failed", "WGC 返回零尺寸帧");
            self.shared.running.store(false, Ordering::SeqCst);
            capture_control.stop();
            return Ok(());
        }
        // 目标窗口被缩放：帧尺寸和建会话时不一致 → 明确状态 + 落盘日志 + 停止（合同 §8 Phase 2/3）。
        // why-not 按新尺寸重配编码器：ffmpeg 的 rawvideo 输入尺寸在建进程时就固定了，
        // 中途改尺寸要重起编码器并拼接，属于 Phase 3 范围，这里先如实停。
        if w != self.shared.want_w || h != self.shared.want_h {
            // 错误码用契约 §7.3 既有的 `target_changed`（409 档）——它的语义就是
            // 「目标的几何/身份与建立会话时不一致」，正是这里发生的事；**不新造一个码**，
            // 新码会脱离 §7.3 表、调用方的错误分支就得为它单独加一条。
            // 状态仍是 `failed`：契约 §7.2 的状态集是封闭的
            // `recording|paused|stopping|completed|failed`，运行中事故归 `failed` 是它的本义；
            // 调用方靠 `error_code == "target_changed"` + `geometry_changed` 区分于其他失败。
            self.shared.geometry_changed.store(true, Ordering::SeqCst);
            self.shared.set_error(
                "target_changed",
                format!(
                    "目标窗口尺寸变化（{}x{} → {w}x{h}），已停止录制",
                    self.shared.want_w, self.shared.want_h
                ),
            );
            // 落盘日志：开始录制有「窗口录制开始 …」一行，事故也必须有一行——否则事后
            // 只有内存里的 status 能回答「为什么停」，进程一退就什么都没了。
            // 字段与开始行对齐（session/window_id/尺寸），便于同一会话的两行对照。
            eprintln!(
                "[snappin] 窗口录制几何变化 {} window_id={} 尺寸 {}x{} → {w}x{h}，已停止录制",
                self.shared.id, self.shared.window_id, self.shared.want_w, self.shared.want_h
            );
            self.shared.running.store(false, Ordering::SeqCst);
            capture_control.stop();
            return Ok(());
        }
        if self.shared.paused.load(Ordering::SeqCst) {
            self.shared.dropped.fetch_add(1, Ordering::SeqCst);
            // 暂停期间不更新槽：恢复后先写出的仍是暂停前那一帧，不会漏出暂停时的画面。
            return Ok(());
        }
        let buf = fb.as_nopadding_buffer(&mut self.scratch);
        if let Ok(mut g) = self.shared.latest.lock() {
            *g = Some(Arc::new(buf.to_vec()));
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        // 目标窗口被关闭/销毁：如实报错并让喂帧线程收尾（绝不回落成全屏或返回空文件）。
        // 码用 §7.3 的 `target_not_found`（HWND 已不存在），与建会话时「目标不存在」
        // 同一语义；状态归 `failed`（§7.2 状态集无「目标消失」这一档）。
        self.shared
            .set_error("target_not_found", "目标窗口在录制期间被关闭");
        self.shared.running.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// 定步长喂帧：每 1/fps 从 `latest` 取帧写进 ffmpeg stdin（没有新帧就重发上一帧）。
fn feed_loop(shared: Arc<Shared>, fps: u32, done: Sender<Result<(), String>>) {
    let step = Duration::from_micros(1_000_000 / u64::from(fps.max(1)));
    let mut next = Instant::now() + step;
    let mut last: Option<Arc<Vec<u8>>> = None;
    while shared.running.load(Ordering::SeqCst) {
        // 暂停期间**不写帧**：产物时间轴里就不含暂停段（与 recorder.rs 分段模型同语义）。
        // why-not 只冻结画面照常写帧：那样暂停时长会被原样录进成品（视频变长、
        // 时长与「实际录制段」不符），且用户暂停就是为了剪掉那段时间。
        if !shared.paused.load(Ordering::SeqCst) {
            if let Some(buf) = shared
                .latest
                .lock()
                .ok()
                .and_then(|g| g.clone())
                .or_else(|| last.clone())
            {
                last = Some(buf.clone());
                let write_res = match shared.sink.lock() {
                    Ok(mut g) => match g.as_mut() {
                        Some(sink) => sink
                            .write_all(&buf)
                            .map_err(|e| format!("写 ffmpeg 失败：{e}")),
                        None => Err("ffmpeg stdin 已关闭".to_string()),
                    },
                    Err(_) => Err("ffmpeg stdin 锁中毒".to_string()),
                };
                if let Err(e) = write_res {
                    shared.set_error("capture_failed", e);
                    break;
                }
                shared.frames.fetch_add(1, Ordering::SeqCst);
            }
        }
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        }
        next += step;
        // 落后超过一步（系统卡顿/调度抖动）就不追赶：追帧只会把时间轴越推越快。
        let now = Instant::now();
        if next < now {
            next = now + step;
        }
    }
    // 收尾：**先停 loopback**（关管道 → ffmpeg 音频输入 EOF），再关 stdin → ffmpeg 写 trailer
    // 出 MP4 → 等退出码（退出码是编码失败的唯一诊断来源）。
    // why-not 只关 stdin：双输入下 ffmpeg 会一直等第二路输入的 EOF，`wait()` 挂死。
    // 异常收尾路径（窗口被关/几何变化）也走这里，所以 stop() 之外也必须先停音频。
    take_and_stop_audio(&shared);
    if let Ok(mut g) = shared.sink.lock() {
        *g = None;
    }
    let mut child = shared.child.lock().ok().and_then(|mut g| g.take());
    let result = match child.as_mut() {
        Some(c) => match c.wait() {
            Ok(st) if st.success() => Ok(()),
            Ok(st) => {
                let mut err = String::new();
                if let Some(mut e) = c.stderr.take() {
                    use std::io::Read;
                    let _ = e.read_to_string(&mut err);
                }
                Err(format!("ffmpeg 退出码 {:?}：{}", st.code(), err.trim()))
            }
            Err(e) => Err(format!("等 ffmpeg 退出失败：{e}")),
        },
        None => Err("ffmpeg 进程句柄缺失".to_string()),
    };
    if let Err(e) = &result {
        // 首个错误已存在时（几何变化/窗口关闭）不会被覆盖：那才是根因，这里是它的连带。
        shared.set_error("capture_failed", e.clone());
    }
    let _ = done.send(result);
}

/// 请求目标窗口重绘一次：`stop()` 后让 WGC 会话尽快收到一帧从而退出消息循环。
/// 重绘是窗口自身的正常行为，不动焦点、不动鼠标（静默契约 §4）。
///
/// why-not `RedrawWindow(RDW_UPDATENOW)`：windows 0.58 里它的 `hrgnupdate` 是泛型参数，
/// 传 `None` 要显式标注类型；`InvalidateRect + UpdateWindow` 等价且不必引 HRGN。
fn nudge_repaint(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let h = HWND(hwnd as *mut c_void);
    unsafe {
        let _ = InvalidateRect(h, None, true);
        let _ = UpdateWindow(h);
    }
}

pub fn is_supported() -> bool {
    GraphicsCaptureApi::is_supported().unwrap_or(false)
}

/// 编码参数与屏幕录制（`recorder.rs::build_ffmpeg_args`）保持一致：h264_nvenc + VBR/CQ + faststart。
/// 输入是 WGC 帧缓冲（BGRA，预乘 alpha，视频按不透明消费），走 rawvideo 定步长喂帧。
///
/// 有音频时是**双输入**：视频走 stdin（rawvideo），音频走命名管道（loopback 抓的系统声）。
/// `audio` 里的格式必须来自 `GetMixFormat()` 实测（float32 混音格式下写死 s16le 会把样本字节序
/// 解释错，结果是噪声而不是声音）；`None` 表示无音频，保持 `-an` 单输入路径。
///
/// **音频源口径（D7）**：窗口路径的音频恒是 **WASAPI loopback 抓的系统声音**，不是 dshow
/// 设备（麦克风/立体声混音那套是区域/全屏路径用的，见 `recorder.rs`）。http 层据此输出
/// `audio_source: "system_loopback"`（无音轨时 "none"）——两套口径别混成一句「已录音频」。
/// 实际有没有采到声看 `Status`/`Stopped` 的 `audio_bytes`：为 0 就是没录到音轨。
///
/// why-not 暂停期间填充静音帧：暂停时视频与音频**两边都不写**，ffmpeg 收不到任一输入的帧
/// 就不会推进时间轴 → 不卡死且两侧时长天然对齐（已实测，漂移 ≤1 帧）。加填充逻辑反而要
/// 自己算音频/视频的样本数配平，多一处出错面。
fn build_ffmpeg_args(
    opts: &StartOptions,
    fps: u32,
    bitrate_k: u32,
    w: u32,
    h: u32,
    audio: Option<(&MixFormat, &str)>,
) -> Vec<String> {
    // 逐项 push 而不是拼数组字面量：`&str` 与 `String` 混在一个数组里类型不齐，
    // 而参数里既有字面量也有 format! 出来的 String。
    let mut args: Vec<String> = Vec::with_capacity(24);
    args.push("-y".into());
    args.push("-f".into());
    args.push("rawvideo".into());
    args.push("-pix_fmt".into());
    args.push("bgra".into());
    args.push("-s".into());
    args.push(format!("{w}x{h}"));
    args.push("-r".into());
    args.push(fps.to_string());
    args.push("-i".into());
    args.push("-".into());
    match audio {
        Some((f, pipe)) => {
            args.push("-f".into());
            args.push(f.format.to_string());
            args.push("-ar".into());
            args.push(f.sample_rate.to_string());
            args.push("-ac".into());
            args.push(f.channels.to_string());
            args.push("-i".into());
            args.push(format!("\\\\.\\pipe\\{pipe}"));
            args.push("-map".into());
            args.push("0:v".into());
            args.push("-map".into());
            args.push("1:a".into());
        }
        None => args.push("-an".into()),
    }
    // 宽高对齐偶数：yuv420p 的色度行与亮度行成对，奇数高（实测窗口高 841）会让色度行
    // 错位 → 彩边（R-B 偏差 -12.5；偶数 840 时 -0.6；加 -colorspace 压不住，实测 -12.0）。
    // `-s` 必须保持**实际帧尺寸**（管道里就是 w×h 的 BGRA 原始字节，改了 rawvideo 的
    // 步长就对不上），所以在输出端用 crop 滤镜把成品裁到偶数尺寸。
    if w % 2 != 0 || h % 2 != 0 {
        args.push("-vf".into());
        args.push(format!("crop={}:{}:0:0", w & !1, h & !1));
    }
    // 质量档位 → VBR+CQ（与区域/全屏录制共用 `recorder::QUALITY_TABLE` 同一套映射）。
    let q = crate::recorder::quality_for(bitrate_k);
    args.push("-c:v".into());
    args.push("h264_nvenc".into());
    args.push("-b:v".into());
    args.push(format!("{}k", q.maxrate_k));
    args.push("-maxrate".into());
    args.push(format!("{}k", q.maxrate_k));
    args.push("-bufsize".into());
    args.push(format!("{}k", q.bufsize_k));
    args.push("-preset".into());
    args.push("p5".into());
    args.push("-rc".into());
    args.push("vbr".into());
    args.push("-cq".into());
    args.push(q.cq.to_string());
    args.push("-pix_fmt".into());
    args.push("yuv420p".into());
    if audio.is_some() {
        args.push("-c:a".into());
        args.push("aac".into());
        args.push("-b:a".into());
        args.push("160k".into());
    }
    args.push("-movflags".into());
    args.push("+faststart".into());
    args.push(opts.path.to_string_lossy().into_owned());
    args
}

pub fn start(opts: StartOptions) -> Result<Started, RecError> {
    let fps = if opts.fps == 0 {
        30
    } else {
        opts.fps.clamp(5, 60)
    };
    let bitrate_k = if opts.bitrate_k == 0 {
        12_000
    } else {
        opts.bitrate_k.clamp(500, 100_000)
    };
    let mut guard = SLOT
        .lock()
        .map_err(|_| RecError::new("capture_failed", "会话锁中毒"))?;
    if let Some(s) = guard.as_ref() {
        return Err(RecError::new(
            "recording_exists",
            format!("已有窗口录制会话在运行：{}", s.id),
        ));
    }
    // ① 探针：编码器必须先知道尺寸，而尺寸只有 WGC 帧说了算
    //    （GetWindowRect 含 DWM 阴影，和 WGC 帧尺寸可能不一致）。
    let probe = crate::capture::capture_window(opts.hwnd).map_err(|e| {
        let code = if e.code == "target_not_found" {
            "target_not_found"
        } else {
            "capture_unavailable"
        };
        RecError::new(code, e.message)
    })?;
    let (w, h) = probe.dimensions();
    if w == 0 || h == 0 {
        return Err(RecError::new("capture_unavailable", "WGC 返回零尺寸帧"));
    }
    // ② ffmpeg：rawvideo 定步长喂帧 → h264_nvenc（有音频再加一路命名管道输入）。
    let ffmpeg = crate::recorder::ffmpeg_path()
        .ok_or_else(|| RecError::new("encoder_unavailable", "未找到 ffmpeg"))?;

    // ②a 音频：先起 loopback（它先建管道实例、再在自己的线程里等 ffmpeg 连），
    //    拿到实测混音格式后才好写 ffmpeg 参数。顺序反了就是确定性死锁：
    //    ffmpeg 拿不到管道实例会直接退出（不重试），loopback 则永远等不到连接。
    let paused = Arc::new(AtomicBool::new(false));
    let audio_stop = Arc::new(AtomicBool::new(false));
    let mut audio_cap = if opts.audio {
        if !LoopbackCapture::is_supported() {
            return Err(RecError::new(
                "audio_unavailable",
                "系统未提供可用的默认播放端点，无法采集系统声音（loopback 不可用）",
            ));
        }
        // 管道名带 pid+序号：同一进程连开两次录制也不会撞名（撞名会让第二路 ffmpeg
        // 连到第一路还活着的实例上，采到的是上一个会话的音频）。
        let pipe = format!("snappin-audio-{}-{}", std::process::id(), seq());
        match LoopbackCapture::start(&pipe, paused.clone(), audio_stop.clone()) {
            Ok((cap, fmt)) => Some((cap, fmt, pipe)),
            Err(e) => return Err(e),
        }
    } else {
        None
    };

    let args = build_ffmpeg_args(
        &opts,
        fps,
        bitrate_k,
        w,
        h,
        audio_cap
            .as_ref()
            .map(|(_, fmt, pipe)| (fmt, pipe.as_str())),
    );
    let mut child = match Command::new(&ffmpeg)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            // 音频链已起、ffmpeg 没起来：必须先收掉 loopback，否则它的采集线程
            // 永远卡在 ConnectNamedPipe 上（进程退出前一直占着端点）。
            stop_audio(audio_cap.take());
            return Err(RecError::new(
                "encoder_unavailable",
                format!("拉起 ffmpeg 失败：{e}"),
            ));
        }
    };
    let stdin = match child.stdin.take() {
        Some(s) => s,
        None => {
            stop_audio(audio_cap.take());
            return Err(RecError::new("encoder_unavailable", "ffmpeg stdin 不可用"));
        }
    };

    // ③ WGC 会话 + 喂帧线程。
    // 会话号在这里就生成（而不是等首帧之后）：`Shared` 必须带上会话身份，WGC 线程
    // 才能在几何变化/窗口关闭时打出「哪一次会话出的事」的日志。提前消耗一个序号
    // 没有代价——id 只是单调递增的标签，失败路径不会把它写进任何产物。
    let id = session_id_new();
    let (first_tx, first_rx) = mpsc::channel::<()>();
    let shared = Arc::new(Shared::new(
        id.clone(),
        opts.window_id.clone(),
        w,
        h,
        first_tx,
        opts.audio,
        paused,
        audio_stop,
        audio_cap.map(|(cap, _, _)| cap),
    ));
    if let Ok(mut g) = shared.sink.lock() {
        *g = Some(stdin);
    }
    if let Ok(mut g) = shared.child.lock() {
        *g = Some(child);
    }
    let cursor = if opts.cursor {
        CursorCaptureSettings::WithCursor
    } else {
        CursorCaptureSettings::WithoutCursor
    };
    let settings = Settings::new(
        WgcWindow::from_raw_hwnd(opts.hwnd as *mut c_void),
        cursor,
        // 关掉 WGC 黄色捕获边框（合同 §4：IsBorderRequired=false）。
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Exclude,
        // 只是把「更新频率上限」压到 1/fps，不保证周期出帧（真定步长由喂帧线程做）。
        MinimumUpdateIntervalSettings::Custom(Duration::from_millis(
            (1000 / u64::from(fps)).max(1),
        )),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        shared.clone(),
    );
    let control = WindowRecHandler::start_free_threaded(settings).map_err(|e| {
        abort_child(&shared);
        let _ = std::fs::remove_file(&opts.path);
        RecError::new("capture_unavailable", format!("WGC 会话启动失败：{e}"))
    })?;
    // 句柄不留：停会话由 handler 在自己线程里做（`InternalCaptureControl::stop`），丢弃即放手。
    drop(control);

    let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
    let feeder_shared = shared.clone();
    std::thread::spawn(move || feed_loop(feeder_shared, fps, done_tx));

    // 首帧兜底：WGC 起不来（或目标不可捕获）时 start 必须显式失败，
    // 不能返回一个「永远出不了片」的会话让调用方录到一半才发现。
    if first_rx.recv_timeout(FIRST_FRAME_TIMEOUT).is_err() {
        shared.running.store(false, Ordering::SeqCst);
        let _ = done_rx.recv_timeout(FINALIZE_TIMEOUT);
        abort_child(&shared);
        let _ = std::fs::remove_file(&opts.path);
        return Err(RecError::new(
            "capture_unavailable",
            "WGC 在超时内未返回首帧（目标可能已最小化或不可捕获）",
        ));
    }

    // 录制开始的落盘日志：出问题时只有这行能回答「当时抓的是哪个窗口、什么身份」
    // （目标快照的 pid/类名/标题是 HWND 复用与误抓的唯一对照物）。
    eprintln!(
        "[snappin] 窗口录制开始 {id} window_id={} pid={} class={} title={:?} {}x{} @{}fps {}k path={}",
        opts.window_id,
        opts.pid,
        opts.class_name,
        opts.title,
        w,
        h,
        fps,
        bitrate_k,
        opts.path.display()
    );
    let started = Started {
        session_id: id.clone(),
        state: "recording",
        source: SOURCE,
        width: w,
        height: h,
        fps,
        window_id: opts.window_id.clone(),
        audio: opts.audio,
    };
    *guard = Some(Session {
        id,
        window_id: opts.window_id,
        hwnd: opts.hwnd,
        width: w,
        height: h,
        fps,
        path: opts.path,
        shared,
        done: done_rx,
    });
    Ok(started)
}

/// 收尾失败路径：先停 loopback（关管道让 ffmpeg 音频输入 EOF），再关 stdin、杀子进程
/// （否则留下悬挂 ffmpeg 一直等输入）。
fn abort_child(shared: &Shared) {
    take_and_stop_audio(shared);
    if let Ok(mut g) = shared.sink.lock() {
        *g = None;
    }
    if let Ok(mut g) = shared.child.lock() {
        if let Some(mut c) = g.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// 收掉 loopback 采集（幂等）：先置停止标志（让采集线程尽快从 WriteFile 回来），
/// 再 `stop()` 关管道句柄——管道关了 ffmpeg 的音频输入才会 EOF。
/// 顺序反了会卡死：先关 stdin 时 ffmpeg 仍在等音频输入，永远等不到 EOF。
fn stop_audio(cap: Option<(LoopbackCapture, MixFormat, String)>) {
    if let Some((c, _, _)) = cap {
        let _ = c.stop();
    }
}

/// 从 `Shared` 取走音频采集并收尾（幂等；无音频时是空操作）。
fn take_and_stop_audio(shared: &Shared) {
    shared.audio_stop.store(true, Ordering::SeqCst);
    // 先落一次字节数再取走句柄：`stop()` 之后句柄没了，只剩这个缓存值可答
    // 「本次到底录到多少音频」。
    shared.audio_bytes_now();
    let cap = shared.audio_cap.lock().ok().and_then(|mut g| g.take());
    if let Some(c) = cap {
        if let Err(e) = c.stop() {
            shared.set_error(
                "capture_failed",
                format!("loopback 收尾失败：{}", e.message),
            );
        }
    }
}

fn state_of(shared: &Shared) -> &'static str {
    if !shared.running.load(Ordering::SeqCst) {
        // 喂帧线程已停：要么是异常（有 error / 几何变化），要么正在收尾（stop 已置停）。
        if shared.error_text().is_some() || shared.geometry_changed.load(Ordering::SeqCst) {
            "failed"
        } else {
            "stopping"
        }
    } else if shared.paused.load(Ordering::SeqCst) {
        "paused"
    } else {
        "recording"
    }
}

fn status_of(s: &Session) -> Status {
    Status {
        session_id: s.id.clone(),
        state: state_of(&s.shared),
        source: SOURCE,
        width: s.width,
        height: s.height,
        fps: s.fps,
        window_id: s.window_id.clone(),
        duration_ms: s.shared.duration_ms(),
        frames: s.shared.frames.load(Ordering::SeqCst),
        dropped: s.shared.dropped.load(Ordering::SeqCst),
        geometry_changed: s.shared.geometry_changed.load(Ordering::SeqCst),
        error_code: s.shared.error_code(),
        error: s.shared.error_text(),
        audio: s.shared.audio,
        audio_bytes: s.shared.audio_bytes_now(),
    }
}

fn find_or_err<'a>(guard: &'a Option<Session>, session_id: &str) -> Result<&'a Session, RecError> {
    match guard.as_ref() {
        Some(s) if s.id == session_id => Ok(s),
        _ => Err(RecError::new(
            "recording_not_found",
            format!("没有该录制会话：{session_id}"),
        )),
    }
}

pub fn status(session_id: &str) -> Result<Status, RecError> {
    let guard = SLOT
        .lock()
        .map_err(|_| RecError::new("capture_failed", "会话锁中毒"))?;
    Ok(status_of(find_or_err(&guard, session_id)?))
}

pub fn pause(session_id: &str) -> Result<Status, RecError> {
    let guard = SLOT
        .lock()
        .map_err(|_| RecError::new("capture_failed", "会话锁中毒"))?;
    let s = find_or_err(&guard, session_id)?;
    if !s.shared.paused.swap(true, Ordering::SeqCst) {
        if let Ok(mut g) = s.shared.paused_since.lock() {
            *g = Some(Instant::now());
        }
    }
    Ok(status_of(s))
}

pub fn resume(session_id: &str) -> Result<Status, RecError> {
    let guard = SLOT
        .lock()
        .map_err(|_| RecError::new("capture_failed", "会话锁中毒"))?;
    let s = find_or_err(&guard, session_id)?;
    if s.shared.paused.swap(false, Ordering::SeqCst) {
        let since = s.shared.paused_since.lock().ok().and_then(|mut g| g.take());
        if let Some(t) = since {
            if let Ok(mut g) = s.shared.paused_acc.lock() {
                *g += t.elapsed();
            }
        }
    }
    Ok(status_of(s))
}

pub fn stop(session_id: &str, discard: bool) -> Result<Stopped, RecError> {
    // 先把会话从槽里摘出来（等 ffmpeg 收尾期间不持锁），再置停 + 请目标重绘一次。
    let session = {
        let mut guard = SLOT
            .lock()
            .map_err(|_| RecError::new("capture_failed", "会话锁中毒"))?;
        match guard.as_ref() {
            Some(s) if s.id == session_id => {}
            _ => {
                return Err(RecError::new(
                    "recording_not_found",
                    format!("没有该录制会话：{session_id}"),
                ))
            }
        }
        guard
            .take()
            .ok_or_else(|| RecError::new("recording_not_found", "会话已结束"))?
    };
    session.shared.running.store(false, Ordering::SeqCst);
    nudge_repaint(session.hwnd);
    // 音频必须先收：管道一关 ffmpeg 的音频输入就 EOF。反过来先关 stdin 的话，
    // ffmpeg 还在等音频输入，`wait()` 会一直挂到 FINALIZE_TIMEOUT。
    take_and_stop_audio(&session.shared);
    let result = session.done.recv_timeout(FINALIZE_TIMEOUT).map_err(|_| {
        abort_child(&session.shared);
        let _ = std::fs::remove_file(&session.path);
        RecError::new("capture_failed", "ffmpeg 收尾超时")
    })?;
    let mut out = Stopped {
        session_id: session.id.clone(),
        // 终态如实反映会话结局：正常由调用方 stop → `completed`；被事故提前终止
        // （几何变化 / 目标关闭 → 有 error_code）→ `failed`。**不因为「此刻是 stop 在收尾」
        // 就一律写 completed**——那样 `status` 在 stop 前报 failed、`stop` 后报 completed，
        // 同一会话两个终态，调用方无从判断这次录制是否完整。
        // 产物仍照常保留并返回 path：几何变化前已写进 ffmpeg 的内容是真实录制内容，
        // 丢掉它等于把「录了一半」变成「什么都没录」；`failed` + path 表达的是
        // 「会话异常结束，但你还有一段可用片段」。
        state: if session.shared.error_code().is_some() {
            "failed"
        } else {
            "completed"
        },
        source: SOURCE,
        path: None,
        duration_ms: session.shared.duration_ms(),
        width: session.width,
        height: session.height,
        fps: session.fps,
        frames: session.shared.frames.load(Ordering::SeqCst),
        dropped: session.shared.dropped.load(Ordering::SeqCst),
        error_code: session.shared.error_code(),
        error: session.shared.error_text(),
        geometry_changed: session.shared.geometry_changed.load(Ordering::SeqCst),
        audio: session.shared.audio,
        audio_bytes: session.shared.audio_bytes_now(),
    };
    if discard {
        let _ = std::fs::remove_file(&session.path);
        return Ok(out);
    }
    if let Err(e) = result {
        let _ = std::fs::remove_file(&session.path);
        return Err(RecError::new("capture_failed", e));
    }
    // 禁止用成功状态返回空 path / 零字节文件（合同 §7.3 末条）。
    match std::fs::metadata(&session.path) {
        Ok(m) if m.len() > 0 => out.path = Some(session.path.to_string_lossy().into_owned()),
        Ok(_) => {
            let _ = std::fs::remove_file(&session.path);
            return Err(RecError::new("capture_failed", "产物为空文件"));
        }
        Err(e) => return Err(RecError::new("capture_failed", format!("产物缺失：{e}"))),
    }
    Ok(out)
}

pub fn active_session_id() -> Option<String> {
    SLOT.lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.id.clone()))
}
