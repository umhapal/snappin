//! WASAPI loopback（系统声音）采集 → 命名管道 → ffmpeg 第二输入。
//!
//! why-not 用 ffmpeg 自带的 wasapi 采集设备：ffmpeg 上游**没有** wasapi 输入设备
//! （FFmpeg-trac #9408 状态仍是 new；本机 6 个 ffmpeg 构建实测全无 wasapi）。换 ffmpeg
//! 构建这条路不存在，只能在 Rust 侧直接抓 loopback 再喂给 ffmpeg。OBS/ShareX 同款做法。
//!
//! why-not 复用 ffmpeg 的 stdin：视频已独占 stdin（rawvideo 定步长喂帧）。音频必须另开
//! 通道，命名管道是零依赖、已验证可行的选择。
//!
//! 线程模型与顺序（不能换，换了就是确定性竞态）：
//! ① `start()` 在**调用线程**同步建好管道实例（`CreateNamedPipeW`）。调用方随后才 spawn
//!    ffmpeg；若实例不存在，ffmpeg 作为客户端会以「找不到管道」直接退出（不重试）。
//! ② WASAPI 的 `Initialize` 与 `GetMixFormat` 也在采集线程里做完并把**实测混音格式**回传
//!    （`start()` 阻塞等它，拿到才好拼 ffmpeg 的 `-f/-ar/-ac`）。
//! ③ `Start()`（真正开流）放在 `ConnectNamedPipe` 返回**之后**：ffmpeg 先探 input 0（stdin
//!    rawvideo，要读到一帧才探完），WGC 首帧实测 0.4–0.6s 才到，那之前 ffmpeg 根本不会来
//!    连管道。若提前 `Start()`，这段等待里 WASAPI 环形缓冲会积着音频，等 ffmpeg 连上一口气
//!    灌进去 → 音轨带 t=0 时间戳进 mp4，比画面领先约 0.5s（可闻）。
//!    why-not 用 `-itsoffset` 延迟音频修这个：那要按实测抖动猜偏移量，抖动变了就失准；
//!    把「开流」钉在连接之后，前导量天然为 0（只剩亚毫秒级抖动）。
//!
//! 暂停语义：与 `window_recorder::feed_loop` 对齐——`paused` 为真时**只排空 WASAPI 缓冲、
//! 不写管道**（必须排空，否则缓冲填满会让引擎停顿）。两侧都不写，ffmpeg 收不到任一输入的
//! 帧就不推进时间轴 → 不卡死且音视频时长天然对齐（主 agent 已用 Node 探针实测，漂移 ≤1 帧）。
//! why-not 暂停期间补静音帧：那要自己配平音视频样本数，多一处出错面。
//!
//! why-not 用同步 I/O 写管道：同步 `WriteFile` 在管道缓冲满时会永久阻塞，`stop()` 只能靠
//! 关句柄碰运气打断。这里全部走 overlapped I/O + 100ms 轮询 `stop`/句柄状态，能确定性地
//! 响应停止（`stop()` 先关句柄让 ffmpeg 音频输入 EOF，线程随即退出）。

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, BOOL, ERROR_PIPE_CONNECTED, HANDLE, RPC_E_CHANGED_MODE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::Storage::FileSystem::{WriteFile, FILE_FLAG_OVERLAPPED, PIPE_ACCESS_OUTBOUND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::window_recorder::RecError;

/// `GetMixFormat()` 实测格式里 `wFormatTag` 为它时，真格式在 `SubFormat`（WAVEFORMATEXTENSIBLE）。
const WAVE_FORMAT_EXTENSIBLE: u16 = 65534;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
/// 自实现 GUID 常量，避免为两个 16 字节常量额外拉 `Win32_Media_Multimedia` /
/// `Win32_Media_KernelStreaming` feature。字面量取自 ks.h 的
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` / `KSDATAFORMAT_SUBTYPE_PCM`，已与本机
/// windows 0.58 源码里 Multimedia/KernelStreaming 模块的字面量逐一核对。
const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x0000_0003_0000_0010_8000_00aa00389b71);
const SUBTYPE_PCM: GUID = GUID::from_u128(0x0000_0001_0000_0010_8000_00aa00389b71);

/// 管道读写缓冲 64 KiB（一个 WASAPI 包远小于它）。
const PIPE_BUF: u32 = 65536;
/// 等采集线程把实测格式回传的上限（COM 冷启动 + 设备激活实测 <0.5s）。
const FORMAT_TIMEOUT: Duration = Duration::from_secs(3);
/// 等采集线程收尾的上限（`stop()` 已关句柄，线程应当立刻退出）。
const JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// 轮询步长（连接等待、管道写入、无数据空转都按它醒来查停止标志）。
const POLL_MS: u32 = 100;

/// 给 ffmpeg 参数构造方用的实测混音格式。
#[derive(Debug, Clone, Copy)]
pub struct MixFormat {
    /// ffmpeg 的 `-f` 值：`"f32le"` 或 `"s16le"`。
    pub format: &'static str,
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
}

/// 一次 loopback 采集会话。只持 `Arc<Atomic*>`、`JoinHandle`、`Receiver`——全部 `Send`，
/// 故存进 `Mutex` 后 `Arc<Shared>` 满足 `Send + Sync`（Windows 句柄一律以 `isize` 持有，
/// 不用裸指针，否则 `Arc<Shared>` 会因不 `Send` 而编不过）。
pub struct LoopbackCapture {
    thread: Option<JoinHandle<()>>,
    /// 停止标志（与 `window_recorder` 传入的是同一个 Arc）。
    stop: Arc<AtomicBool>,
    /// 管道句柄（`isize`，0 = 已关闭）。
    pipe: Arc<AtomicIsize>,
    /// 已写进管道的 PCM 字节数。
    bytes: Arc<AtomicU64>,
    /// 采集线程结束信号（`stop()` 用它做带超时的等待）。
    done: Receiver<()>,
}

impl LoopbackCapture {
    /// 探测默认渲染端点能否开 loopback：枚举 → 激活 → 取混音格式，全程不 `Initialize`、
    /// 不占设备。跑在临时线程里做 COM 初始化（避免污染调用线程的 COM 公寓模型）。
    pub fn is_supported() -> bool {
        let (tx, rx) = mpsc::channel::<bool>();
        let _ = std::thread::spawn(move || {
            let _ = tx.send(probe());
        });
        matches!(rx.recv_timeout(FORMAT_TIMEOUT), Ok(true))
    }

    /// `pipe_name` 是**裸名**（如 `snappin-audio-1234-7`），内部拼 `\\.\pipe\<name>`。
    ///
    /// 返回前**必须**已完成 `CreateNamedPipeW`（管道实例存在）——调用方紧接着 spawn ffmpeg。
    /// 实测混音格式由采集线程回传后才返回（ffmpeg 的 `-f/-ar/-ac` 要用它）。
    pub fn start(
        pipe_name: &str,
        paused: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
    ) -> Result<(Self, MixFormat), RecError> {
        let full = format!(r"\\.\pipe\{pipe_name}");
        let wide: Vec<u16> = full.encode_utf16().chain(std::iter::once(0)).collect();

        // ① 同步建管道实例（只建、不 Connect：Connect 放线程里等 ffmpeg）。
        let h = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED,
                PIPE_TYPE_BYTE | PIPE_WAIT,
                1,
                PIPE_BUF,
                PIPE_BUF,
                0,
                None,
            )
        };
        if h.is_invalid() {
            return Err(RecError::new(
                "audio_unavailable",
                format!("CreateNamedPipeW 失败：{full}"),
            ));
        }

        let pipe = Arc::new(AtomicIsize::new(h.0 as isize));
        let bytes = Arc::new(AtomicU64::new(0));
        let (fmt_tx, fmt_rx) = mpsc::channel::<Result<MixFormat, RecError>>();
        let (done_tx, done_rx) = mpsc::channel::<()>();

        let thread = {
            let paused = paused.clone();
            let stop = stop.clone();
            let pipe = pipe.clone();
            let bytes = bytes.clone();
            std::thread::spawn(move || {
                run(paused, stop, pipe, bytes, fmt_tx, done_tx);
            })
        };

        match fmt_rx.recv_timeout(FORMAT_TIMEOUT) {
            Ok(Ok(fmt)) => Ok((
                Self {
                    thread: Some(thread),
                    stop,
                    pipe,
                    bytes,
                    done: done_rx,
                },
                fmt,
            )),
            Ok(Err(e)) => {
                // 采集线程已自行收尾（含关句柄）。等它退出，返回它给出的错误。
                stop.store(true, Ordering::SeqCst);
                let _ = done_rx.recv_timeout(JOIN_TIMEOUT);
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                // 超时：线程可能卡在设备激活。置停 + 关句柄把它推出来，然后 detach
                // （句柄已关、停止标志已置，线程会自行退出）。
                stop.store(true, Ordering::SeqCst);
                close_pipe(&pipe);
                let _ = done_rx.recv_timeout(JOIN_TIMEOUT);
                Err(RecError::new(
                    "audio_unavailable",
                    "loopback 初始化超时（设备激活未在 3s 内完成）",
                ))
            }
        }
    }

    /// 已写进管道的 PCM 字节数（句柄被 `stop()` 取走后仍可读）。
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::SeqCst)
    }

    /// 收尾（消费 self，天然只调一次）：先置停止标志，再**关管道句柄让 ffmpeg 音频输入
    /// 立刻 EOF**（不能等线程自然退出，否则 ffmpeg 会一直等音频输入），最后等线程退出
    /// （带超时兜底）。
    pub fn stop(self) -> Result<(), RecError> {
        self.stop.store(true, Ordering::SeqCst);
        close_pipe(&self.pipe);
        if let Some(t) = self.thread {
            if self.done.recv_timeout(JOIN_TIMEOUT).is_ok() {
                let _ = t.join();
            }
            // 超时则放弃 join：句柄已关、停止标志已置，线程会自行退出（JoinHandle drop = detach）。
        }
        Ok(())
    }
}

/// 采集线程主体。所有 COM/Win32 调用都在此线程的公寓里。
fn run(
    paused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    pipe: Arc<AtomicIsize>,
    bytes: Arc<AtomicU64>,
    fmt_tx: Sender<Result<MixFormat, RecError>>,
    done_tx: Sender<()>,
) {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        let com_owned = hr.is_ok();
        if !com_owned && hr != RPC_E_CHANGED_MODE {
            let _ = fmt_tx.send(Err(RecError::new(
                "audio_unavailable",
                format!("CoInitializeEx 失败：{hr:?}"),
            )));
            let _ = done_tx.send(());
            return;
        }

        // ② WASAPI Initialize + GetMixFormat（失败就把错误回传给 start()）。
        let (client, capture, fmt, block_align) = match setup_capture() {
            Ok(v) => v,
            Err(e) => {
                let _ = fmt_tx.send(Err(e));
                close_pipe(&pipe);
                if com_owned {
                    CoUninitialize();
                }
                let _ = done_tx.send(());
                return;
            }
        };

        // 回传实测格式：start() 阻塞等这个。
        if fmt_tx.send(Ok(fmt)).is_err() {
            // 调用方已放弃（超时/失败）→ 直接收尾。
            close_pipe(&pipe);
            if com_owned {
                CoUninitialize();
            }
            let _ = done_tx.send(());
            return;
        }

        // ④ 等 ffmpeg 连上（overlapped Connect，轮询 stop）。
        let ev = match CreateEventW(None, BOOL::from(false), BOOL::from(false), PCWSTR::null()) {
            Ok(e) => e,
            Err(_) => {
                close_pipe(&pipe);
                if com_owned {
                    CoUninitialize();
                }
                let _ = done_tx.send(());
                return;
            }
        };

        let connected = connect(&pipe, ev, &stop);
        if connected {
            // ③ 开流钉在连接之后（见模块头 why-not：提前开流会攒出前导音频）。
            let _ = client.Start();
            // 静音补零的速率锚点：实测混音格式的每秒字节数（rate × channels × 每样本字节）。
            let bytes_per_sec =
                u64::from(fmt.sample_rate) * u64::from(fmt.channels) * u64::from(fmt.bits / 8);
            collect_loop(
                &capture,
                &pipe,
                &bytes,
                &paused,
                &stop,
                ev,
                block_align,
                bytes_per_sec,
            );
            let _ = client.Stop();
        }

        // ⑤ 收尾：断开客户端 + 关句柄（让 ffmpeg 音频输入 EOF）+ 关事件。
        let hv = pipe.load(Ordering::SeqCst);
        if hv != 0 {
            let _ = DisconnectNamedPipe(HANDLE(hv as *mut c_void));
        }
        close_pipe(&pipe);
        let _ = CloseHandle(ev);
        if com_owned {
            CoUninitialize();
        }
        let _ = done_tx.send(());
    }
}

/// 枚举默认渲染端点 → 激活 IAudioClient → 取混音格式 → loopback `Initialize`。
/// 返回 `(client, capture_client, 实测格式, 每帧字节数)`。**不 `Start()`**（开流在连接后）。
unsafe fn setup_capture() -> Result<(IAudioClient, IAudioCaptureClient, MixFormat, u16), RecError> {
    let en: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| {
            RecError::new(
                "audio_unavailable",
                format!("创建 MMDeviceEnumerator 失败：{e}"),
            )
        })?;
    let dev = en
        .GetDefaultAudioEndpoint(eRender, eConsole)
        .map_err(|e| RecError::new("audio_unavailable", format!("取默认渲染端点失败：{e}")))?;
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(|e| {
        RecError::new(
            "audio_unavailable",
            format!("Activate IAudioClient 失败：{e}"),
        )
    })?;

    let fmt_ptr = client
        .GetMixFormat()
        .map_err(|e| RecError::new("audio_unavailable", format!("GetMixFormat 失败：{e}")))?;
    let mf = parse_mix_format(fmt_ptr);
    let block_align = std::ptr::read_unaligned(std::ptr::addr_of!((*fmt_ptr).nBlockAlign));

    // 用引擎混音格式原样 Initialize（SHARED + LOOPBACK）：格式不匹配会被拒。
    let init = client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_LOOPBACK,
        0,
        0,
        fmt_ptr,
        None,
    );
    CoTaskMemFree(Some(fmt_ptr as *const c_void));
    init.map_err(|e| {
        RecError::new(
            "audio_unavailable",
            format!("IAudioClient::Initialize(loopback) 失败：{e}"),
        )
    })?;

    let capture: IAudioCaptureClient = client.GetService().map_err(|e| {
        RecError::new(
            "audio_unavailable",
            format!("取 IAudioCaptureClient 失败：{e}"),
        )
    })?;
    Ok((client, capture, mf, block_align))
}

/// 只做「能否开 loopback」的探测（不 `Initialize`、不占设备）。
fn probe() -> bool {
    unsafe {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if !hr.is_ok() && hr != RPC_E_CHANGED_MODE {
            return false;
        }
        let ok =
            match CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .and_then(|en| en.GetDefaultAudioEndpoint(eRender, eConsole))
                .and_then(|dev| dev.Activate::<IAudioClient>(CLSCTX_ALL, None))
            {
                Ok(client) => match client.GetMixFormat() {
                    Ok(p) => {
                        CoTaskMemFree(Some(p as *const c_void));
                        true
                    }
                    Err(_) => false,
                },
                Err(_) => false,
            };
        if hr.is_ok() {
            CoUninitialize();
        }
        ok
    }
}

/// 从 `GetMixFormat()` 的裸指针解析出 ffmpeg 需要的格式描述。
/// `WAVEFORMATEX`/`WAVEFORMATEXTENSIBLE` 都是 `repr(C, packed(1))`，故一律 `read_unaligned`
/// （直接取字段引用是 E0793）。
unsafe fn parse_mix_format(p: *const WAVEFORMATEX) -> MixFormat {
    let tag = std::ptr::read_unaligned(std::ptr::addr_of!((*p).wFormatTag));
    let sample_rate = std::ptr::read_unaligned(std::ptr::addr_of!((*p).nSamplesPerSec));
    let channels = std::ptr::read_unaligned(std::ptr::addr_of!((*p).nChannels));
    let bits = std::ptr::read_unaligned(std::ptr::addr_of!((*p).wBitsPerSample));

    let (format, bits) = if tag == WAVE_FORMAT_EXTENSIBLE {
        let sub = std::ptr::read_unaligned(std::ptr::addr_of!(
            (*(p as *const WAVEFORMATEXTENSIBLE)).SubFormat
        ));
        if sub == SUBTYPE_IEEE_FLOAT {
            ("f32le", 32u16)
        } else if sub == SUBTYPE_PCM {
            ("s16le", bits)
        } else {
            // 非 IEEE float 的非 PCM 子格式无法如实描述；按 32 位 float 兜底并留痕。
            eprintln!("[snappin] loopback 未知 SubFormat {sub:?}，按 f32le 处理");
            ("f32le", 32u16)
        }
    } else if tag == WAVE_FORMAT_IEEE_FLOAT {
        ("f32le", 32u16)
    } else {
        ("s16le", bits)
    };

    MixFormat {
        format,
        sample_rate,
        channels,
        bits,
    }
}

/// 关掉管道句柄（CAS 取走，双重关闭安全）。
fn close_pipe(pipe: &AtomicIsize) {
    let hv = pipe.swap(0, Ordering::SeqCst);
    if hv != 0 {
        unsafe {
            let _ = CloseHandle(HANDLE(hv as *mut c_void));
        }
    }
}

/// overlapped `ConnectNamedPipe`：等 ffmpeg 连上，期间轮询 `stop`。
unsafe fn connect(pipe: &AtomicIsize, ev: HANDLE, stop: &AtomicBool) -> bool {
    let hv = pipe.load(Ordering::SeqCst);
    if hv == 0 {
        return false;
    }
    let h = HANDLE(hv as *mut c_void);
    let mut ov = OVERLAPPED::default();
    ov.hEvent = ev;

    if ConnectNamedPipe(h, Some(&mut ov as *mut OVERLAPPED)).is_ok() {
        return true;
    }
    // windows-rs 把 BOOL 失败统一转 Err，`ERROR_PIPE_CONNECTED`（客户端已在等）也走这里，
    // 而那种情况下 overlapped 操作根本没启动、事件永不 signal → 必须显式识别，否则死等。
    if GetLastError() == ERROR_PIPE_CONNECTED {
        return true;
    }
    loop {
        let r = WaitForSingleObject(ev, POLL_MS);
        if r == WAIT_OBJECT_0 {
            let mut w: u32 = 0;
            return GetOverlappedResult(h, &ov, &mut w, BOOL::from(false)).is_ok();
        }
        if r != WAIT_TIMEOUT {
            return false;
        }
        // 同 `overlapped_write`：句柄已被 `stop()` 关闭时不再 CancelIoEx（避免打在复用句柄上），
        // 关句柄本身会让内核取消这个 pending Connect。
        if pipe.load(Ordering::SeqCst) == 0 {
            return false;
        }
        if stop.load(Ordering::SeqCst) {
            let _ = CancelIoEx(h, Some(&ov as *const OVERLAPPED));
            // bWait=TRUE 收尾，否则本函数返回后栈上 OVERLAPPED 仍被内核异步写 → 悬垂。
            let mut w: u32 = 0;
            let _ = GetOverlappedResult(h, &ov, &mut w, BOOL::from(true));
            return false;
        }
    }
}

/// 采集循环：`GetNextPacketSize` → `GetBuffer` → 写管道 → `ReleaseBuffer`。
/// `paused` 时排空但不写（保持缓冲不积压、且产物时间轴不含暂停段）。
///
/// why-not 静音时只 `sleep + continue`（旧实现）：WASAPI loopback 在系统无声音时
/// **不推包**（`GetNextPacketSize() == 0`），旧实现于是一字节不写 → ffmpeg 的音频输入
/// 饿死 → 它停止消费视频 stdin → 喂帧阻塞 → **整条录制管线停摆**。2026-10-16 实测：
/// `audio:true` + 系统静音 → 录制 0.57s 后帧数冻结（`audio:false` 同夹具 3.90s 正常）。
/// 现改为按墙钟补写零样本（OBS/ShareX 同款），让音频时间轴按实时推进；有声与静音下
/// 都能持续喂 ffmpeg。零样本只补到「已流逝的实时」为止，真实包到达即接管，不跑快。
unsafe fn collect_loop(
    capture: &IAudioCaptureClient,
    pipe: &AtomicIsize,
    bytes: &AtomicU64,
    paused: &AtomicBool,
    stop: &AtomicBool,
    ev: HANDLE,
    block_align: u16,
    bytes_per_sec: u64,
) {
    let mut zeros = vec![0u8; PIPE_BUF as usize];
    let mut ov = OVERLAPPED::default();
    // nBlockAlign 为 0 的混音格式会让下面的整除 panic——采集线程一死就是音轨断流，
    // 比字节对齐差一点严重得多。真实设备不会给 0，这里按 1 兜底。
    let block_align = block_align.max(1);
    // 静音补零的实时锚点：`Some(t)` = 自 t 起无真实包，已按墙钟补写 `idle_emitted` 字节。
    let mut idle_since: Option<Instant> = None;
    let mut idle_emitted: u64 = 0;

    while !stop.load(Ordering::SeqCst) {
        let hv = pipe.load(Ordering::SeqCst);
        if hv == 0 {
            break; // stop() 已关句柄
        }
        let h = HANDLE(hv as *mut c_void);

        // why-not `.unwrap_or(0)` 兜底（旧实现）：把「设备失效」与「合法静音」混成同一个 0，
        // 无法区分。现分开——Err 才判设备失效退出；Ok(0) 是静音，走下面的补零分支。
        let next = match capture.GetNextPacketSize() {
            Ok(n) => n,
            Err(_) => break, // 设备失效/被移除：退出让 stop 侧收尾
        };
        if next == 0 {
            // **D6b 契约：这一支必须继续产出静音 PCM 帧，不能什么都不写。**
            // 系统静音时 WASAPI loopback 根本不推包，旧实现只 sleep+continue、一字节不写
            // → ffmpeg 的音频输入饿死 → 它不再消费视频 stdin → 喂帧线程阻塞 → 整条管线
            // 停摆（实测 audio:true+静音 0.57s 后帧数冻结，audio:false 同夹具 3.90s/57 帧
            // 正常）。静音段是**合法音频**，补零让音频时间轴继续推进，ffmpeg 照常编码。
            if paused.load(Ordering::SeqCst) {
                // 暂停期间时间轴不走：清锚点，恢复时从零重新补。
                idle_since = None;
                idle_emitted = 0;
            } else {
                let now = Instant::now();
                let since = *idle_since.get_or_insert(now);
                let want = (now.duration_since(since).as_nanos() * bytes_per_sec as u128
                    / 1_000_000_000) as u64;
                let want = want / block_align as u64 * block_align as u64;
                if want > idle_emitted {
                    let n = ((want - idle_emitted) as usize).min(PIPE_BUF as usize);
                    if zeros.len() < n {
                        zeros.resize(n, 0);
                    }
                    match overlapped_write(h, ev, &mut ov, &zeros[..n], stop, pipe) {
                        Some(w) => {
                            bytes.fetch_add(u64::from(w), Ordering::SeqCst);
                            idle_emitted += u64::from(w);
                        }
                        None => break, // 管道已断/句柄已关
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        // 有真实包 → 结束本轮静音补零，真实包自带正确时间轴。
        idle_since = None;
        idle_emitted = 0;

        let mut pdata: *mut u8 = std::ptr::null_mut();
        let mut nframes: u32 = 0;
        let mut flags: u32 = 0;
        if capture
            .GetBuffer(&mut pdata, &mut nframes, &mut flags, None, None)
            .is_err()
        {
            break;
        }

        let nbytes = nframes as usize * block_align as usize;
        if !paused.load(Ordering::SeqCst) {
            let silent = (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0;
            let buf: &[u8] = if silent || pdata.is_null() {
                // SILENT 包数据指针不可用：写零样本，保持 ffmpeg 音频时间轴连续推进。
                if nbytes > zeros.len() {
                    zeros.resize(nbytes, 0);
                }
                &zeros[..nbytes]
            } else {
                std::slice::from_raw_parts(pdata, nbytes)
            };
            match overlapped_write(h, ev, &mut ov, buf, stop, pipe) {
                Some(n) => {
                    bytes.fetch_add(u64::from(n), Ordering::SeqCst);
                }
                None => {
                    let _ = capture.ReleaseBuffer(nframes);
                    break; // 管道已断/句柄已关
                }
            }
        }
        let _ = capture.ReleaseBuffer(nframes);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// overlapped `WriteFile`：`Some(n)` = 实际写入字节数；`None` = 管道断/已停止，应退出。
/// 轮询 `POLL_MS`，让 `stop` 能确定性打断阻塞写。
unsafe fn overlapped_write(
    h: HANDLE,
    ev: HANDLE,
    ov: &mut OVERLAPPED,
    buf: &[u8],
    stop: &AtomicBool,
    pipe: &AtomicIsize,
) -> Option<u32> {
    *ov = OVERLAPPED::default();
    ov.hEvent = ev;

    let mut written: u32 = 0;
    if WriteFile(
        h,
        Some(buf),
        Some(&mut written),
        Some(ov as *mut OVERLAPPED),
    )
    .is_ok()
    {
        return Some(written);
    }
    // 若操作其实已完成（或即时失败），这里能拿到结果；未完成会 Err 并转入轮询。
    if GetOverlappedResult(h, ov, &mut written, BOOL::from(false)).is_ok() {
        return Some(written);
    }
    loop {
        let r = WaitForSingleObject(ev, POLL_MS);
        if r == WAIT_OBJECT_0 {
            let mut w: u32 = 0;
            return GetOverlappedResult(h, ov, &mut w, BOOL::from(false))
                .ok()
                .map(|_| w);
        }
        if r != WAIT_TIMEOUT {
            return None;
        }
        // why-not 直接 `if stop || pipe==0 { CancelIoEx }`：`stop()` 会关句柄，而 CancelIoEx
        // 打在已关闭的句柄值上，若该值已被系统复用，会误取消无关句柄的 I/O。句柄为 0 时
        // 不再碰它——`CloseHandle` 已由 OS 随之取消这个 pending 写，事件也会被 signal。
        if pipe.load(Ordering::SeqCst) == 0 {
            return None;
        }
        if stop.load(Ordering::SeqCst) {
            let _ = CancelIoEx(h, Some(ov as *const OVERLAPPED));
            // 必须 bWait=TRUE 收尾：否则本函数返回后栈上的 OVERLAPPED 仍被内核异步写 → 悬垂。
            let mut w: u32 = 0;
            let _ = GetOverlappedResult(h, ov, &mut w, BOOL::from(true));
            return None;
        }
    }
}
