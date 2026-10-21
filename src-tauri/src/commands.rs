//! 前端可调的 Tauri 命令：截图取帧、合成落盘/复制、开关 overlay、录屏起停。
//! 真实能力在 capture.rs / recorder.rs；这里只做编解码、落盘位置、窗口与状态管理。

use crate::capture::{self, Bounds};
use crate::config::{AppConfig, NameFormat};
use crate::longshot;
use crate::ocr::{self, OcrResult};
use crate::pin::{self, PinState};
use crate::recorder::{RecordOptions, Recorder};
use crate::uia;
use crate::window::{self, WindowRect};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RPC_E_CHANGED_MODE, WPARAM};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::{
    Common::COMDLG_FILTERSPEC, FileOpenDialog, FileSaveDialog, IFileDialog, IShellItem,
    SHCreateItemFromParsingName, FOS_FORCEFILESYSTEM, FOS_NOREADONLYRETURN, FOS_OVERWRITEPROMPT,
    FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsWindow, IsWindowVisible, PostMessageW, WM_CLOSE,
};

/// 全局配置共享状态（前端读写设置走这里；HTTP 线程也读）。
pub struct ConfigState(pub Arc<Mutex<AppConfig>>);

/// overlay「该不该显示」的世代号：`close_overlay` 每关一次 +1。
/// 页面抓到帧后带着**抓帧那一刻**的世代号调 `show_overlay`；号变了说明用户在抓帧期间
/// 已经按 Esc/✕ 关了窗，这时不能再把它弹回来（否则表现为「关掉又自己冒出来」）。
static OVERLAY_SHOW_EPOCH: AtomicU64 = AtomicU64::new(0);

/// `RECT` 的最小镜像（GUI 线程信息里要带插入符矩形）。
#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct RawRect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

/// `GUITHREADINFO` 的最小镜像：某线程的活动顶层窗 / 焦点控件 / 插入符位置。
///
/// why-not 用 windows crate 的类型：本文件里焦点相关 API（GetForegroundWindow 等）
/// 已经走裸 FFI，同一套风格写完不容易把结构体布局写错。
#[repr(C)]
#[allow(dead_code)]
struct RawGuiThreadInfo {
    cb_size: u32,
    flags: u32,
    hwnd_active: *mut core::ffi::c_void,
    hwnd_focus: *mut core::ffi::c_void,
    hwnd_capture: *mut core::ffi::c_void,
    hwnd_menu_owner: *mut core::ffi::c_void,
    hwnd_move_size: *mut core::ffi::c_void,
    hwnd_caret: *mut core::ffi::c_void,
    rc_caret: RawRect,
}

impl RawGuiThreadInfo {
    fn new() -> Self {
        // cbSize 必须由调用方填成结构体大小，否则 GetGUIThreadInfo 直接失败。
        RawGuiThreadInfo {
            cb_size: std::mem::size_of::<RawGuiThreadInfo>() as u32,
            flags: 0,
            hwnd_active: std::ptr::null_mut(),
            hwnd_focus: std::ptr::null_mut(),
            hwnd_capture: std::ptr::null_mut(),
            hwnd_menu_owner: std::ptr::null_mut(),
            hwnd_move_size: std::ptr::null_mut(),
            hwnd_caret: std::ptr::null_mut(),
            rc_caret: RawRect {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
        }
    }
}

#[link(name = "user32")]
extern "system" {
    fn GetForegroundWindow() -> *mut core::ffi::c_void;
    fn SetForegroundWindow(hwnd: *mut core::ffi::c_void) -> i32;
    fn GetWindowLongPtrW(hwnd: *mut core::ffi::c_void, nindex: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: *mut core::ffi::c_void, nindex: i32, dwnewlong: isize) -> isize;
    fn SetWindowRgn(hwnd: *mut core::ffi::c_void, hrgn: isize, bredraw: i32) -> i32;
    // 剪贴板自动粘贴焦点归还（#2）：面板 hide 后本进程已非前台，裸 SetForegroundWindow
    // 常被 Windows 前台锁忽略 → 焦点没回到目标输入框、Ctrl+V 落空。附加输入线程
    // （AttachThreadInput）后 SetForegroundWindow 才稳定生效，是该场景的既定解法。
    fn AttachThreadInput(idattach: u32, idattachto: u32, fattach: i32) -> i32;
    fn GetWindowThreadProcessId(hwnd: *mut core::ffi::c_void, lpdwprocessid: *mut u32) -> u32;
    fn GetCurrentThreadId() -> u32;
    fn BringWindowToTop(hwnd: *mut core::ffi::c_void) -> i32;
    // 控件级焦点存取（2026-10-20 D13）：顶层窗激活 ≠ 原输入框拿回插入符。
    fn GetGUIThreadInfo(idthread: u32, lpgui: *mut RawGuiThreadInfo) -> i32;
    fn SetFocus(hwnd: *mut core::ffi::c_void) -> *mut core::ffi::c_void;
    fn GetFocus() -> *mut core::ffi::c_void;
    // GW_CHILD：取 WebView2 宿主下的子窗口（键盘真正落在那里）。
    fn GetWindow(hwnd: *mut core::ffi::c_void, ucmd: u32) -> *mut core::ffi::c_void;
}

/// 把 `hwnd` 强制拉到前台并夺取键盘焦点。从全局热键 handler 调用时本进程并非前台，
/// 裸 `SetForegroundWindow` 会被 Windows 前台锁忽略 —— 面板「亮一下就没焦点」，随即
/// 被失焦自动关闭吞掉，表现为「热键打不开/无法切换」（用户实测 #1 根因）。
/// 附加当前前台窗线程的输入队列后再设前台，是该场景的既定解法。
///
/// `pub(crate)`：长截图滚动（`longshot::scroll_down`）也要「把目标窗设成前台」——
/// 滚轮只投给焦点窗口，裸 SetForegroundWindow 同样会被前台锁忽略。
pub(crate) fn force_foreground(hwnd: *mut core::ffi::c_void) {
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let fore = GetForegroundWindow();
        let cur = GetCurrentThreadId();
        let fore_thread = if fore.is_null() {
            0
        } else {
            GetWindowThreadProcessId(fore, std::ptr::null_mut())
        };
        // 目标窗所在线程：SetForegroundWindow 在前台锁下要靠与目标线程共享输入队列才生效，
        // SetFocus（还焦回原控件）更**只能**在控件所属线程的上下文里调用。
        // why-not 只附加前台线程：面板自己就是前台时 fore_thread == 本线程 → 附加被跳过 →
        // 退化成裸 SetForegroundWindow → 撞前台锁 → 「粘贴落空」（2026-10-20 用户实测 D13）。
        let target_thread = GetWindowThreadProcessId(hwnd, std::ptr::null_mut());
        let mut attached = [0u32; 2];
        let mut attached_n = 0usize;
        for t in [target_thread, fore_thread] {
            if t == 0 || t == cur || attached[..attached_n].contains(&t) {
                continue;
            }
            if AttachThreadInput(cur, t, 1) != 0 {
                attached[attached_n] = t;
                attached_n += 1;
            }
        }
        SetForegroundWindow(hwnd);
        BringWindowToTop(hwnd);
        for i in 0..attached_n {
            AttachThreadInput(cur, attached[i], 0);
        }
    }
}

/// 抓当前前台线程的「活动顶层窗 + 焦点控件」两个句柄。
///
/// why-not 只记 `GetForegroundWindow()`：顶层窗激活不代表原输入框重新拿到插入符，
/// 目标应用里没有控件持有焦点时，用户手动 Ctrl+V 什么都不发生（2026-10-20 D13 实测）。
/// 控件级句柄只能从 `GetGUIThreadInfo` 的 hwndFocus 拿。
pub(crate) fn capture_focus_chain() -> (isize, isize) {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.is_null() {
            return (0, 0);
        }
        let thread = GetWindowThreadProcessId(fg, std::ptr::null_mut());
        if thread == 0 {
            return (fg as isize, 0);
        }
        let mut info = RawGuiThreadInfo::new();
        if GetGUIThreadInfo(thread, &mut info) == 0 {
            return (fg as isize, 0);
        }
        let top = if info.hwnd_active.is_null() {
            fg as isize
        } else {
            info.hwnd_active as isize
        };
        (top, info.hwnd_focus as isize)
    }
}

/// 把键盘焦点还给目标窗口里的**原控件**（插入符真正回到原输入框）。
///
/// why-not 只 `SetForegroundWindow`：它只激活顶层窗，不会把焦点交回原来的子控件。
/// `SetFocus` 又**只能**在控件所属线程的上下文里调用，所以先把本线程附加到那个线程。
pub(crate) fn restore_focus_to_control(child: isize) -> bool {
    if child == 0 {
        return false;
    }
    unsafe {
        let h = child as *mut core::ffi::c_void;
        let cur = GetCurrentThreadId();
        let thread = GetWindowThreadProcessId(h, std::ptr::null_mut());
        let attached = thread != 0 && thread != cur && AttachThreadInput(cur, thread, 1) != 0;
        let set_ok = !SetFocus(h).is_null();
        // GetFocus 复核：**必须在附加期间**取（脱离后拿到的是本线程的队列）。
        // 不一致只记日志不改判据——SetFocus 返回非空才是「焦点已交出去」的权威信号。
        let focus_now = GetFocus();
        if set_ok && focus_now != h {
            eprintln!(
                "[snappin] 还焦后 GetFocus 不一致 child=0x{:x} focus=0x{:x}",
                child, focus_now as isize
            );
        }
        if attached {
            AttachThreadInput(cur, thread, 0);
        }
        set_ok
    }
}

/// 开关窗口的 WS_EX_NOACTIVATE：面板「不抢焦」↔「允许键盘输入」两态切换。
fn set_no_activate(hwnd: *mut core::ffi::c_void, on: bool) {
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE_RAW);
        let next = if on {
            ex | WS_EX_NOACTIVATE_RAW
        } else {
            ex & !WS_EX_NOACTIVATE_RAW
        };
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE_RAW, next);
    }
}

/// 合成 Ctrl+V（SendInput，走系统输入管线）。只在目标窗已成为前台后调用——前台核对
/// 通过的才发，避免把粘贴打到面板自己或未知窗口（2026-10-14 方案 a 的既定顺序）。
fn send_ctrl_v() {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP,
        VIRTUAL_KEY, VK_CONTROL, VK_V,
    };
    let kbd = |vk: VIRTUAL_KEY, flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let seq = [
        kbd(VK_CONTROL, KEYBD_EVENT_FLAGS(0)),
        kbd(VK_V, KEYBD_EVENT_FLAGS(0)),
        kbd(VK_V, KEYEVENTF_KEYUP),
        kbd(VK_CONTROL, KEYEVENTF_KEYUP),
    ];
    unsafe {
        SendInput(&seq, core::mem::size_of::<INPUT>() as i32);
    }
}

// HRGN 全程用 isize 裸句柄（与本文件既有 raw FFI 风格一致）。
#[link(name = "gdi32")]
extern "system" {
    fn CreateRectRgn(x1: i32, y1: i32, x2: i32, y2: i32) -> isize;
    fn CombineRgn(rgndst: isize, rgnsrc1: isize, rgnsrc2: isize, fnmode: i32) -> i32;
    fn DeleteObject(h: isize) -> i32;
}

// ---- 录制态输入权让渡（2026-09-24 用户报障「录屏时我应该可以使用键盘，这是传统
// 录屏软件都行的。gif也是」）：录制中 overlay 全屏可见（透明窗+红框+录制条），病在
// show_overlay 的 set_focus 持焦不还 + 根节点吞点击（KeyboardBlockScout 定位）。
// 修法三件套：① **SetWindowRgn 区域裁剪**——录制条矩形 ∪ 红框环（环内挖洞），区域外
// 根本不参与命中测试（连 WebView2 子窗一并绕开），鼠标直落底下应用，选区内部照常
// 穿透到用户正在录的程序；环内挖洞 + 录制条留区 = 红框可视、录制条可点，其余透明
// 像素被裁本也看不见，视觉零损失。**WM_NCHITTEST 父窗子类方案已实测否决**：
// WebView2 子窗先吃 hit-test，父窗子类收不到（B/E 绿证明命令已执行、子类已装，
// 穿透仍失败）。② WS_EX_NOACTIVATE，点录制条只执行不夺焦。③ show_overlay 暂存
// 原前台窗，开录 SetForegroundWindow 归还 → 键盘即刻回用户程序。停止/重开复原。

const GWL_EXSTYLE_RAW: i32 = -20;
const WS_EX_NOACTIVATE_RAW: isize = 0x0800_0000;
const RGN_OR_RAW: i32 = 2;
const RGN_DIFF_RAW: i32 = 4;

static OVERLAY_PREV_FOCUS: Mutex<Option<isize>> = Mutex::new(None);

/// 打开剪贴板面板前的前台窗（用户正在用的程序）：clip_paste 写回后要把焦点还给它
/// 再合成 Ctrl+V，否则粘贴落到面板自己身上。面板抢焦前（toggle_clip_history 入口）暂存。
static CLIP_PREV_FOCUS: Mutex<Option<isize>> = Mutex::new(None);

/// 打开面板前**焦点控件**（用户原输入框）的句柄。
///
/// why-not 单独记：顶层窗激活 ≠ 原输入框拿回插入符，目标应用里没有控件持有焦点时
/// 用户手动 Ctrl+V 什么都不发生（2026-10-20 用户实测 D13「粘贴落空」）。
/// 控件级句柄只能从 `GetGUIThreadInfo` 的 hwndFocus 取（见 capture_focus_chain）。
static CLIP_PREV_FOCUS_CHILD: Mutex<Option<isize>> = Mutex::new(None);

/// 前端推送的矩形（overlay client/CSS 坐标；无边框窗 client 原点 == 窗口原点）。
#[derive(serde::Deserialize)]
pub struct CssRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// 录制态输入让渡所需的全部几何：录制条 + 红框（选区）+ 红框 border 宽度（挖洞内缩）。
#[derive(serde::Deserialize)]
pub struct RecInputRects {
    pub bar: CssRect,
    pub frame: CssRect,
    pub inset: f64,
}

/// 录制态输入权让渡/收回（前端在 recording 生命周期里推送录制条+红框几何）。
/// `rects=None`（停止录制）= 撤区域/撤 NOACTIVATE/回焦 overlay（保存条 Esc 语义不变）。
#[tauri::command]
pub fn rec_input_passthrough(app: AppHandle, rects: Option<RecInputRects>) -> Result<(), String> {
    let win = app.get_webview_window("overlay").ok_or("overlay 不在")?;
    let hwnd = win.hwnd().map_err(|e| e.to_string())?.0 as *mut core::ffi::c_void;
    match rects {
        Some(r) => {
            let sf = win.scale_factor().unwrap_or(1.0);
            // SetWindowRgn 的区域是**窗口本地坐标**（相对窗左上角），不是屏幕坐标——
            // 加 outer_position 会在非 (0,0) 窗（VDD 1920,0）把整区推到窗外 = 全窗点不动
            // （2026-09-24 check-11 暂停三连点不中实锤；物理全屏 origin=(0,0) 恰好掩盖）。
            let tx = |v: f64| (v * sf).round() as i32;
            let (bx0, by0) = (tx(r.bar.x), tx(r.bar.y));
            let (bx1, by1) = (tx(r.bar.x + r.bar.w), tx(r.bar.y + r.bar.h));
            let (fx0, fy0) = (tx(r.frame.x), tx(r.frame.y));
            let (fx1, fy1) = (tx(r.frame.x + r.frame.w), tx(r.frame.y + r.frame.h));
            let inset = (r.inset * sf).round().max(0.0) as i32;
            unsafe {
                // 红框环 = 外矩形(选区) − 内缩矩形(挖洞 → 选区内部照常穿透)
                let outer = CreateRectRgn(fx0, fy0, fx1, fy1);
                let inner = CreateRectRgn(fx0 + inset, fy0 + inset, fx1 - inset, fy1 - inset);
                let ring = CreateRectRgn(0, 0, 0, 0);
                if outer == 0 || inner == 0 || ring == 0 {
                    if outer != 0 {
                        DeleteObject(outer);
                    }
                    if inner != 0 {
                        DeleteObject(inner);
                    }
                    if ring != 0 {
                        DeleteObject(ring);
                    }
                    return Err("CreateRectRgn 失败".into());
                }
                CombineRgn(ring, outer, inner, RGN_DIFF_RAW);
                DeleteObject(outer);
                DeleteObject(inner);
                // ∪ 录制条矩形（可点区）
                let bar = CreateRectRgn(bx0, by0, bx1, by1);
                if bar == 0 {
                    DeleteObject(ring);
                    return Err("CreateRectRgn(bar) 失败".into());
                }
                CombineRgn(ring, bar, ring, RGN_OR_RAW);
                DeleteObject(bar);
                if SetWindowRgn(hwnd, ring, 1) == 0 {
                    DeleteObject(ring);
                    return Err("SetWindowRgn 失败".into());
                }
                // 成功后系统拥有 ring 生命周期，不再 DeleteObject。
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE_RAW);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE_RAW, ex | WS_EX_NOACTIVATE_RAW);
            }
            // 焦点归还：此刻 overlay 是前台窗，本进程有权设前台（SetForegroundWindow 限制不触发）。
            if let Ok(p) = OVERLAY_PREV_FOCUS.lock() {
                if let Some(h) = *p {
                    if h != 0 {
                        unsafe {
                            SetForegroundWindow(h as *mut core::ffi::c_void);
                        }
                    }
                }
            }
        }
        None => {
            unsafe {
                SetWindowRgn(hwnd, 0, 1); // NULL = 恢复整窗命中
                let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE_RAW);
                SetWindowLongPtrW(hwnd, GWL_EXSTYLE_RAW, ex & !WS_EX_NOACTIVATE_RAW);
            }
            let _ = win.set_focus();
        }
    }
    Ok(())
}

/// 录制会话全局状态（同一时刻仅一个）。
#[derive(Default)]
pub struct RecordState(pub Mutex<Option<Recorder>>);

/// 「已武装」的录制参数：overlay 里选区已提交、就等一个开始信号。
///
/// 为什么需要它：截图热键在 overlay 开着且选区已定时应当**开始/停止录制**
/// （PixPin 同款手感，用户 2026-09-21 点名要求）。选区边界只有前端知道，
/// 所以前端在进入可录态时把参数推上来，热键处理器直接取用。
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")] // 前端传 bitrateK（Tauri 只对顶层命令参数做大小写转换，嵌套结构体要靠这个）
pub struct ArmInfo {
    pub bounds: Bounds,
    pub fps: u32,
    pub bitrate_k: u32,
    pub gif: bool,
}

/// 武装状态（None = 当前没有可录的选区）。
#[derive(Default)]
pub struct RecordArmState(pub Mutex<Option<ArmInfo>>);

/// 前端进入「可录态」（选区已提交且未在录制）时调用，把参数武装起来。
#[tauri::command]
pub fn arm_record(state: tauri::State<RecordArmState>, info: ArmInfo) -> Result<(), String> {
    let mut g = state.0.lock().map_err(|e| e.to_string())?;
    *g = Some(info);
    Ok(())
}

/// 退出可录态（选区变了 / 开始录制 / overlay 关闭）时调用。
#[tauri::command]
pub fn disarm_record(state: tauri::State<RecordArmState>) -> Result<(), String> {
    let mut g = state.0.lock().map_err(|e| e.to_string())?;
    *g = None;
    Ok(())
}

/// 识别屏幕某块区域的文字（「检测文本」用：悬停命中文字元素后按 Shift+C 取字）。
/// 走与截图同一套 GDI 抓帧，再交给本地 OCR 引擎；不经过剪贴板。
///
/// **必须是 async**：抓帧 + OCR 推理都是重活（推理可达秒级），同步命令会冻住主线程，
/// 而这条路径正是用户在 overlay 里按 Shift+C 时触发的——堵住就等于截图界面卡死。
#[tauri::command]
pub async fn ocr_region(
    app: AppHandle,
    bounds: Bounds,
    lang: Option<String>,
) -> Result<OcrResult, String> {
    let img = capture::capture_region(bounds)?;
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let res = ocr::ocr_png(&buf.into_inner(), lang.as_deref())?;
    auto_copy_ocr(&app, &res.text);
    Ok(res)
}

/// 由 UI（悬浮球）触发的截图。**必须走 run_on_main_thread 排队**，不能直接建窗：
/// 实测在另一个 webview 的 IPC 回调里创建 overlay 窗口，新 webview 会停在 about:blank
/// （页面永不 ready → 看门狗接管 → 用户看到「点了没反应」甚至应用退出）。热键路径本来就
/// 是排队执行的，所以这里复用同一条路径。
#[tauri::command]
pub fn trigger_capture_now(app: AppHandle) {
    crate::trigger_capture(&app);
}

// ============================ 桌面悬浮球 ============================

/// 悬浮条尺寸（逻辑像素）——必须与前端 `Ball.tsx` 的窗口布局一致。
const BALL_W: f64 = 176.0;
const BALL_H: f64 = 52.0;

/// 「建窗代次」：每次真的要建新 webview 窗口就 +1（见 `bump_window_epoch`）。
/// 正在等待的「延迟恢复悬浮球」用它判断刚才有没有人动过窗口（见 `spawn_ball_restore`）。
static WINDOW_BUILD_EPOCH: AtomicU64 = AtomicU64::new(0);

/// 记一次「要建窗了」：延迟恢复悬浮球（`spawn_ball_restore`）据此让位。
pub fn bump_window_epoch() {
    WINDOW_BUILD_EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// 正在「准备建窗」的计数（> 0 = 有延迟建窗在路上）。
///
/// 为什么必须有它（不是锦上添花）：延迟建窗要**等桌面安静**（overlay 隐藏、球消失），
/// 而延迟恢复悬浮球正好会在同一时刻把球建回来——两边互相顶：等待方等球消失、恢复方建球，
/// 结果等待方超时后**带着球**建窗，正好命中「已有 webview 在场」的坑。
/// 所以从「决定延迟建窗」到「窗口真的建出来」这段期间计数 > 0，恢复方必须让位。
static WINDOW_BUILD_PENDING: AtomicUsize = AtomicUsize::new(0);

/// 延迟建窗「进行中」标记（RAII）：建窗闭包执行完（含失败）自动归零。
/// 跨线程传递：在主线程创建，随闭包移到 `run_on_main_thread` 的回调里，回调结束才 drop。
pub struct BuildPendingGuard(());

impl BuildPendingGuard {
    pub fn new() -> Self {
        WINDOW_BUILD_PENDING.fetch_add(1, Ordering::SeqCst);
        Self(())
    }
}

impl Default for BuildPendingGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for BuildPendingGuard {
    fn drop(&mut self) {
        WINDOW_BUILD_PENDING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 有没有延迟建窗在路上（见 `WINDOW_BUILD_PENDING`）。
fn build_pending() -> bool {
    WINDOW_BUILD_PENDING.load(Ordering::SeqCst) > 0
}

/// 长截图是否正在跑（整屏抓帧拼接期间**不许**把悬浮球建出来，否则球条会被拍进产物）。
static LONGSHOT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// 长截图的「进行中」标记，RAII：任何路径退出（含 `?` 提前返回）都复位。
struct LongshotGuard;

impl LongshotGuard {
    fn new() -> Self {
        LONGSHOT_ACTIVE.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for LongshotGuard {
    fn drop(&mut self) {
        LONGSHOT_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// 悬浮球窗口当前在不在。
pub fn ball_present(app: &AppHandle) -> bool {
    app.get_webview_window("ball").is_some()
}

/// 请求关闭悬浮球：**只发关闭请求，不等待**。返回是否真有球需要关。
///
/// 为什么不能在这里等：`close()` 只是把「销毁窗口」这条消息排进主线程的事件队列，
/// 窗口真正被销毁要等主线程回到消息循环把它派发掉（见 `wait_until_quiet` 的注释）。
pub fn close_ball_now(app: &AppHandle) -> bool {
    match app.get_webview_window("ball") {
        Some(win) => {
            let _ = win.close();
            true
        }
        None => false,
    }
}

/// 等桌面安静到可以建新 webview：**overlay 已真的不可见 且 悬浮球已从窗口表消失**。
/// 只允许后台线程调用（理由见 `wait_until_quiet`）。
///
/// 贴图路径必须用这个（而不是只等球）：前端 `doPin` = `close_overlay()` → `pin_image()`，
/// 而 `close_overlay` 只发 hide 请求就返回、球又是后台延迟恢复——pin 命令到达时 overlay
/// 的 hide **还没被事件循环处理**。此时若同步建 pin 窗，就是「已有 webview 在场还建窗」，
/// 实测 pin 停 `about:blank`、连 overlay 窗口都被一起搞坏。
///
/// 等待期间若又冒出悬浮球（例如上一次截图排的延迟恢复恰好落在这一刻），**再关一次**：
/// 不这么做的话等待会被顶到超时，最后带着球建窗，正好撞上那个坑。
pub fn wait_desktop_quiet(app: &AppHandle, timeout_ms: u64) {
    wait_until_quiet(app, "overlay 隐藏 + 悬浮球关闭", timeout_ms, |a| {
        if ball_present(a) {
            close_ball_now(a);
            return false;
        }
        overlay_hidden(a)
    });
}

/// 等某个窗口从 Tauri 窗口表里消失（＝它的销毁消息已被主线程派发）。只允许后台线程调用。
///
/// 什么时候需要它：句柄已失效的残留窗口（见 `window_alive`）必须先从表里清掉，
/// 否则 `prepare_window` 见到同名 label 会直接报 `WindowLabelAlreadyExists`，重建被挡死。
pub fn wait_window_gone(app: &AppHandle, label: &str, timeout_ms: u64) {
    wait_until_quiet(
        app,
        &format!("窗口 {label} 从窗口表移除"),
        timeout_ms,
        |a| a.get_webview_window(label).is_none(),
    );
}

/// 后台轮询等待「安静」条件成立，成立后再让出 300ms 给 WebView2 收尾；超时只记日志。
///
/// **这是本轮最值钱的一条教训（2026-09-21 隔离实例 + CDP 决定性对照）**：
/// 绝不在主线程 sleep 等窗口销毁/隐藏。`close()`/`hide()` 只是把消息排进主线程事件队列，
/// 窗口真正被销毁要等主线程回到消息循环把它派发掉。以前是「`close()` 之后在**主线程**
/// `sleep(250ms)` 再建新窗」——主线程一睡，那条销毁消息就没人派发，球**根本没被销毁**；
/// 醒来直接建 overlay，球仍在场，于是新 webview 停在 `about:blank`（页面永不 ready →
/// 15s 后看门狗接管 → 整个应用被 `exit(1)` 带走）。
/// 对照实验：先走 `close_ball` 命令（命令返回 → 主线程回到事件循环 → 球被真正销毁）→
/// 隔几秒再按热键 → overlay 正常加载；而「关球后紧接着建窗」3/3 停在 about:blank。
///
/// 正确形状就是本函数：**命令立即返回**（把 label/结果先给调用方），在**后台线程**轮询
/// 「条件是否真的成立」，成立了再 `run_on_main_thread` 回主线程建窗。
/// 判据用 Tauri 窗口表而不是「发过请求了」：窗口从表里被摘掉，就说明销毁消息已经被主线程
/// 派发过（`tauri::app` 的 `RuntimeWindowEvent::Destroyed` → `manager.on_window_close`
/// 才会 remove 条目）；超时只记一行日志、照旧往下走——宁可有日志的降级，
/// 也不要让用户的截图动作卡死在等待里。
fn wait_until_quiet(
    app: &AppHandle,
    what: &str,
    timeout_ms: u64,
    quiet: impl Fn(&AppHandle) -> bool,
) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    while Instant::now() < deadline {
        if quiet(app) {
            std::thread::sleep(Duration::from_millis(300));
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "[snappin] 等「{what}」超时（{timeout_ms}ms）→ 仍继续建窗，可能撞上 about:blank / label 冲突"
    );
}

/// overlay 是否已经真的不可见（hide 已被主线程派发）；窗口不在表里也算不可见。
fn overlay_hidden(app: &AppHandle) -> bool {
    match app.get_webview_window("overlay") {
        Some(w) => !w.is_visible().unwrap_or(false),
        None => true,
    }
}

/// 窗口句柄是否真的还活着（按 Win32 层面判，**别信 Tauri 的窗口表**）。
///
/// 为什么必须判（2026-09-21 CDP 实测）：建 pin 撞坑之后 `get_webview_window("overlay")`
/// 仍返回 `Some`，但 HWND 已经失效——`show()` 静默失败，用户看到的是「按热键永远没反应」。
/// Tauri 的窗口表只在收到运行时 `Destroyed` 事件（→ `on_window_close`）时才清条目，
/// 所以「表里有」不等于「屏幕上还活着」。
fn window_alive(win: &tauri::WebviewWindow) -> bool {
    match win.hwnd() {
        // 注意：tauri 的 `hwnd()` 给的是**它自己依赖的那份 windows crate**（0.61）的 HWND，
        // 与本 crate 的 windows 0.58 不是同一个类型，不能直接传给 `IsWindow`。
        // 两边都是 `HWND(pub *mut c_void)`，取裸指针重新包一层即可。
        Ok(h) => unsafe { IsWindow(HWND(h.0)) }.as_bool(),
        Err(_) => false,
    }
}

/// 把 overlay 藏起来（若可见）。**只发隐藏请求，不等待、不 sleep**：
/// 隐藏/销毁这类窗口消息同样要主线程回消息循环才生效，在这里 sleep 只会把它们堵住
/// （见 `wait_until_quiet` 注释）。overlay 只 hide 不 close（复用窗口，见 `close_overlay`）。
pub fn hide_overlay_if_visible(app: &AppHandle) {
    if let Some(ov) = app.get_webview_window("overlay") {
        if ov.is_visible().unwrap_or(true) {
            let _ = ov.hide();
        }
    }
}

/// 配置里是否开着悬浮球（恢复悬浮球前要再确认一次，见 `spawn_ball_restore`）。
fn ball_wanted(app: &AppHandle) -> bool {
    app.try_state::<ConfigState>()
        .and_then(|s| s.0.lock().ok().map(|c| c.desktop_ball))
        .unwrap_or(false)
}

/// 延迟恢复悬浮球：离开「hide overlay 的那个调用栈」再建窗，并等桌面上的窗口操作停下来。
///
/// 为什么不能立刻建（2026-09-21 用户实测「点贴图后整个界面卡死、按钮全无响应」）：
/// 前端「先 close_overlay 再 pin_image」时，同步恢复会把悬浮球**正好**建在 pin 建窗之前
/// ——建 pin 窗时桌面上有球，命中「已有 webview 在场 → 新建 webview 卡死 / 停在
/// about:blank」这条已定案的失败条件（见 `wait_until_quiet` 注释）。
///
/// 为什么是轮询而不是固定睡一觉：固定 400ms 只是让位一次，若这期间又有人建窗
/// （pin / 下一次 overlay），球的建窗会紧跟在后面，仍落在危险窗口期。所以每 400ms 看一次，
/// 三个条件都满足才真的建球：
/// 1. `WINDOW_BUILD_EPOCH` 没变（最近 400ms 内没人建过窗）；
/// 2. 长截图没在跑（球条会被拍进拼接产物）；
/// 3. **没有延迟建窗在路上**（`WINDOW_BUILD_PENDING`）——这条最关键：延迟建窗要等球消失，
///    球若这时被建回来，等待方会超时后带着球建窗，两边互相顶死。
/// 上限 120s（长截图最多 60 帧、逐帧 150ms 等待，整段可以跑到几十秒，别在半途超时把球弹出来），
/// 超时就放弃这次恢复——下次截图结束还会再来一次。
pub(crate) fn spawn_ball_restore(app: AppHandle) {
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut epoch = WINDOW_BUILD_EPOCH.load(Ordering::SeqCst);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(400));
            let now = WINDOW_BUILD_EPOCH.load(Ordering::SeqCst);
            if now == epoch && !LONGSHOT_ACTIVE.load(Ordering::SeqCst) && !build_pending() {
                break;
            }
            epoch = now;
        }
        let app2 = app.clone();
        if let Err(e) = app.run_on_main_thread(move || {
            // 到点前用户可能自己把球关了（close_ball 会把 desktop_ball 写 false）——再确认一次，
            // 别把用户明确关掉的球弹回来。
            // 录制进行中 / overlay 还可见时也不回弹（A 方案 2026-09-23）：球弹回会被拍进
            // 成品或盖在选区上；两者的恢复分别由停止路径与 close_overlay 再次触发本函数。
            if !ball_wanted(&app2) || crate::is_recording(&app2) || !overlay_hidden(&app2) {
                return;
            }
            if let Err(e) = open_ball(app2) {
                eprintln!("[snappin] 恢复悬浮球失败: {e}");
            }
        }) {
            eprintln!("[snappin] 恢复悬浮球排队失败: {e}");
        }
    });
}

/// 悬浮球默认位置：贴主屏右侧、垂直居中（配置里 ball_x/ball_y 为 -1 时用）。
fn ball_default_pos() -> (f64, f64) {
    unsafe {
        let w = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CXSCREEN,
        ) as f64;
        let h = windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
            windows::Win32::UI::WindowsAndMessaging::SM_CYSCREEN,
        ) as f64;
        (w - BALL_W - 12.0, (h - BALL_H) / 2.0)
    }
}

/// 建悬浮球窗口（**必须在主线程调用**；调用时机见 `open_ball` 的注释）。
fn build_ball_window(app: &AppHandle) -> Result<(), String> {
    // 延迟建窗期间可能已经有人建好了（连点两次开关）：有就直接显示，别再建第二个。
    if let Some(win) = app.get_webview_window("ball") {
        let _ = win.show();
        return Ok(());
    }
    // 走到这里球必然不存在，所以**没有任何等待**（不等球消失、也不动 overlay）：
    // 托盘/设置里开球时用户可能正在框选，把 overlay 藏掉是无提示地打断用户操作。
    // 只记一次代次，让正在排队的「延迟恢复悬浮球」知道刚有人建过窗。
    bump_window_epoch();

    let (cx, cy) = app
        .try_state::<ConfigState>()
        .and_then(|s| s.0.lock().ok().map(|c| (c.ball_x, c.ball_y)))
        .filter(|(x, y)| *x >= 0 && *y >= 0)
        .map(|(x, y)| (x as f64, y as f64))
        .unwrap_or_else(ball_default_pos);

    WebviewWindowBuilder::new(app, "ball", WebviewUrl::App("index.html".into()))
        .title("SnapPin Ball")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .resizable(false)
        .position(cx, cy)
        .inner_size(BALL_W, BALL_H)
        // 参数必须与主窗/overlay/pin 完全一致，否则 WebView2 环境创建失败（见 lib.rs 注释）。
        .additional_browser_args(crate::browser_args())
        .build()
        .map_err(|e| e.to_string())?;
    // 球条**不设** WDA_EXCLUDEFROMCAPTURE（2026-09-23 用户拍板 A 方案，回撤 898970e 球部分）：
    // 带排除的**可见**窗会被 NVIDIA Overlay 当受保护内容，弹「SnapPin 正在阻止桌面录制」
    // 并禁用其截屏/录屏；PixPin 全窗 affinity=0x0 同款做法（活体 dump 实测）。
    // 自己成品无球改靠行为护栏：开 overlay 关球（open_overlay）+ 录制开始关球
    // （start_record_impl）+ 录制中/overlay 可见时恢复跳过（spawn_ball_restore）。
    Ok(())
}

/// 显示（必要时创建）桌面悬浮球。位置取配置，未设置则贴右侧居中。
///
/// **建窗绝不在本命令里同步做**（2026-09-22 实测定案，用户报「设置里开启悬浮球但什么都没出现」）：
/// 设置面板的开关走 IPC 调本命令，在命令回调里直接 `WebviewWindowBuilder::build()` 会得到一个
/// **页面永远停在 `about:blank`** 的窗口——窗口确实建出来了（176×52、可见），但 webview 从不导航，
/// `invoke` 的 Promise 也永不返回。透明的空窗 = 用户眼里「什么都没发生」。
/// 更糟的是这个卡住的球窗会让随后的截图撞上「已有 webview 在场」的坑：overlay 也停 `about:blank`
/// → 全屏置顶空窗吃掉整个桌面输入（用户报的「截图直接出现根本性错误」）。
/// 做法与 `open_overlay` 一致：命令**立即返回**，把建窗挪到后台线程，再由 `run_on_main_thread`
/// 排到主线程——那时命令已返回、主线程回到事件循环，WebView2 的创建回调才跑得动。
#[tauri::command]
pub fn open_ball(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("ball") {
        let _ = win.show();
        return Ok(());
    }
    let pending = BuildPendingGuard::new();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let app3 = app2.clone();
        if let Err(e) = app2.run_on_main_thread(move || {
            // 闭包持有它：窗口真的建出来（或建失败）才归零。
            let _pending = pending;
            if let Err(e) = build_ball_window(&app3) {
                eprintln!("[snappin] 悬浮球创建失败: {e}");
            }
        }) {
            eprintln!("[snappin] 悬浮球建窗排队失败: {e}");
        }
    });
    Ok(())
}

/// 关闭桌面悬浮球（前端「隐藏」按钮走这里）。
///
/// 同时把配置 `desktop_ball` 写成 false 并落盘（2026-09-21 用户实测）：只关窗不回写配置，
/// 用户点了「隐藏」，下一次 `close_overlay` 又按 `desktop_ball=true` 把球弹回来——
/// 在用户眼里就是「隐藏按钮没用，球自己会回来」。用户的「隐藏」是**持久意图**，
/// 必须落到配置里（托盘菜单的「桌面悬浮球」开关也走同一份配置，两边保持一致）。
#[tauri::command]
pub fn close_ball(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("ball") {
        win.close().map_err(|e| e.to_string())?;
    }
    if let Some(state) = app.try_state::<ConfigState>() {
        if let Ok(mut cfg) = state.0.lock() {
            cfg.desktop_ball = false;
            let dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("snappin"));
            let _ = cfg.save(&dir.join("config.json"));
        }
    }
    Ok(())
}

/// 记住悬浮球位置（前端拖动结束时调用；下次启动原位出现）。
#[tauri::command]
pub fn set_ball_pos(app: AppHandle, x: i32, y: i32) -> Result<(), String> {
    let Some(state) = app.try_state::<ConfigState>() else {
        return Ok(());
    };
    let mut cfg = state.0.lock().map_err(|e| e.to_string())?;
    cfg.ball_x = x;
    cfg.ball_y = y;
    let dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("snappin"));
    let path = dir.join("config.json");
    let _ = cfg.save(&path);
    Ok(())
}

/// 显示主配置窗（悬浮球的「设置」按钮走这里）。
#[tauri::command]
pub fn show_main_window(app: AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

#[derive(serde::Serialize)]
pub struct CaptureData {
    /// data:image/png;base64,... 供 overlay 当冻结背景。
    pub data_url: String,
    pub bounds: Bounds,
    /// 抓帧这一刻的「显示世代号」，原样回传给 `show_overlay`（见 OVERLAY_SHOW_EPOCH 注释）。
    pub epoch: u64,
    /// 各显示器的物理矩形（与 `bounds` 同坐标系）。
    ///
    /// 为什么必须随帧下发：overlay 窗覆盖的是**整个虚拟桌面并集**，页面里的
    /// `window.innerWidth/innerHeight` 不等于任一物理屏；浮层（工具条/保存条/提示卡）
    /// 若按视口夹紧，选区贴近物理屏边缘时按钮会被推到屏外——用户看到「按钮穿出屏幕、
    /// 复制/保存点不到」（2026-10-15 用户报障）。前端按「选区所在显示器」夹紧即可。
    pub monitors: Vec<Bounds>,
}

/// 抓整屏一帧，返回 base64 PNG + 虚拟屏边界。
///
/// **必须是 async**：整屏 GDI 抓帧 + PNG 编码 + base64 在 4K 屏上是几百毫秒级重活，
/// 同步命令跑在主线程上会把页面/托盘/热键全部堵住（用户 2026-09-21 报的「卡死」族）。
#[tauri::command]
pub async fn capture_screen_data() -> Result<CaptureData, String> {
    let b = capture::overlay_bounds();
    let img = capture::capture_region(b)?;
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    let b64 = STANDARD.encode(buf.into_inner());
    Ok(CaptureData {
        data_url: format!("data:image/png;base64,{b64}"),
        bounds: b,
        epoch: OVERLAY_SHOW_EPOCH.load(Ordering::SeqCst),
        monitors: capture::monitors().into_iter().map(|m| m.rect).collect(),
    })
}

#[derive(serde::Serialize)]
pub struct FinishResult {
    pub path: Option<String>,
}

/// overlay 合成好的最终 PNG（base64）落盘 / 复制剪贴板。
///
/// **必须是 async**：base64 解码 + PNG 编解码 + 剪贴板写入 + 落盘全是重活
/// （4K 全屏图动辄几十 MB 内存与百毫秒级耗时），同步命令会冻住主线程。
/// 剪贴板走 arboard 的裸 Win32 实现、无 COM/窗口线程亲和，放异步线程安全。
#[tauri::command]
pub async fn finish_capture(
    app: AppHandle,
    png_base64: String,
    copy: bool,
    save: bool,
) -> Result<FinishResult, String> {
    let raw = png_base64
        .split_once(",")
        .map(|(_, b)| b)
        .unwrap_or(&png_base64);
    let bytes = STANDARD.decode(raw.trim()).map_err(|e| e.to_string())?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| e.to_string())?
        .to_rgba8();

    if copy {
        capture::copy_to_clipboard(&img)?;
    }
    let mut out = None;
    if save {
        // **这里写的只是暂存件**（产物目录下的 `.pending/`），不是最终产物：用户在确认条上
        // 点头之前，产物目录里不该出现文件（D12：先确认、再落盘；用户 2026-09-21 原话
        // 「应该像 PixPin 有个确认阶段」）。真正落盘在 `commit_capture`——用户点「快速保存」
        // 之后那一步才把暂存件搬到最终路径；取消/删除则只删暂存件，不留半成品。
        let dir = pending_dir(&app);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // 命名（时间戳/序号）与编码格式（PNG/JPG）都按设置走，见 product_stem / image_ext_and_format。
        let (ext, fmt) = image_ext_and_format(&app);
        let path = unique_path(&dir, &product_stem(&app, ""), ext);
        capture::save_image(&img, &path, fmt)?;
        out = Some(path.to_string_lossy().to_string());
    }
    Ok(FinishResult { path: out })
}

/// 把**暂存产物**落到最终路径（前端在用户点「快速保存」之后调用）。
///
/// 这一步才是「这里才落盘」：finish_capture 只写 `.pending/` 暂存件，用户在确认条上
/// 点头之前产物目录里没有文件（D12）。暂存目录与产物目录同卷 → rename 原子完成。
///
/// 非暂存路径（录制 mp4/gif 等本来就直接落盘的产物）原样返回，让前端一条路径走通两类产物。
#[tauri::command]
pub fn commit_capture(app: AppHandle, path: String) -> Result<String, String> {
    let p = PathBuf::from(&path);
    let pending = pending_dir(&app);
    // 只有暂存目录里的文件才需要搬；别的路径已经是最终件。
    if p.parent() != Some(pending.as_path()) {
        return Ok(path);
    }
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "png".to_string());
    let dir = captures_dir(&app);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let dest = unique_path(&dir, &stem, &ext);
    // 同卷 rename 是原子操作；万一跨卷（理论上不会，暂存目录就在产物目录里）退回复制。
    match std::fs::rename(&p, &dest) {
        Ok(()) => Ok(dest.to_string_lossy().to_string()),
        Err(rename_err) => {
            eprintln!("[snappin] 暂存件 rename 失败，退回复制：{rename_err}");
            std::fs::copy(&p, &dest).map_err(|e| format!("落盘失败：{e}"))?;
            let _ = std::fs::remove_file(&p);
            Ok(dest.to_string_lossy().to_string())
        }
    }
}

/// overlay 页面就绪标志：页面挂载后调用 `overlay_ready` 置位，看门狗据此判断页面是否活着。
static OVERLAY_READY: AtomicBool = AtomicBool::new(false);

/// 页面报到超时：超过这个时间没收到 `overlay_ready` 就认定页面没加载成功。
// 页面就绪超时：系统忙（编译/录屏/多窗口）时 WebView2 新建窗口的加载会明显变慢，
// 5s 太紧会误判成「页面没加载」并把好窗口关掉。
const OVERLAY_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// 看门狗超时（秒）：默认 15s，可用 `SNAPPIN_OVERLAY_READY_TIMEOUT_SECS` 覆盖。
/// 该环境变量只为验证「超时自救」路径而存在（自动化测试会驱动它）——
/// 真实用户永远不会设它，取值仍是 15s。
fn overlay_ready_timeout() -> Duration {
    std::env::var("SNAPPIN_OVERLAY_READY_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(OVERLAY_READY_TIMEOUT)
}

/// overlay 页面挂载完成信号（Overlay.tsx 挂载时调用）。
/// `SNAPPIN_OVERLAY_NO_READY` 置位时**忽略**上报——专供验证看门狗超时路径，
/// 否则页面总是秒级就绪、那条自救分支永远跑不到。
#[tauri::command]
pub fn overlay_ready() {
    if std::env::var_os("SNAPPIN_OVERLAY_NO_READY").is_some() {
        return;
    }
    OVERLAY_READY.store(true, Ordering::SeqCst);
}

/// 打开（或显示）全屏透明置顶 overlay 窗口。
#[tauri::command]
pub fn open_overlay(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("overlay") {
        if window_alive(&win) {
            // **这里不 show()**（2026-09-22 用户实测「截图页面里还有一张截图页面」后定案）：
            // 窗口一显示，上一轮留在 DOM 里的冻结帧大图 + 工具条 + 尺寸标签就回到屏幕上，
            // 紧接着的 GDI BitBlt 会把它们原样拍进新帧 → 递归的「截屏里套截屏」。
            // 正确时序：只通知页面重置状态并重抓帧，页面抓完自己调 `show_overlay` 亮窗。
            // 先 hide：这一支可能在窗口**正显示着**时被调用（如已开 overlay 又按截图键），
            // hide 掉才能保证抓帧干净——不依赖 WDA 那一层（老系统上它可能不生效）。
            let _ = win.hide();
            let _ = app.emit_to("overlay", "overlay-shown", ());
            return Ok(());
        }
        // 窗口表里有、屏幕上已经没了（2026-09-21 CDP 实测：建 pin 撞坑后 overlay 的 HWND
        // 失效，`show()` 静默失败 → 用户「按热键永远没反应」）。销毁这个失效句柄，再按
        // 「新建」走；但**必须先等它从窗口表里消失**——`prepare_window` 见到同名 label 会直接
        // 报 `WindowLabelAlreadyExists`，残留条目会把重建挡死。
        eprintln!("[snappin] overlay 窗口句柄已失效（窗口表里有、HWND 无效）→ 销毁残留并重建");
        let _ = win.destroy();
        arm_overlay_watchdog();
        // 从这一刻起「有建窗在路上」：延迟恢复悬浮球会让位，别插进下面的等待里。
        let pending = BuildPendingGuard::new();
        let app2 = app.clone();
        std::thread::spawn(move || {
            wait_window_gone(&app2, "overlay", 3000);
            wait_desktop_quiet(&app2, 3000);
            let app3 = app2.clone();
            if let Err(e) = app2.run_on_main_thread(move || {
                // 建窗闭包持有它：窗口真的建出来（或建失败）才归零。
                let _pending = pending;
                if let Err(e) = build_overlay_window(&app3) {
                    eprintln!("[snappin] 重建 overlay 失败: {e}");
                }
            }) {
                eprintln!("[snappin] 重建 overlay 排队失败: {e}");
            }
        });
        return Ok(());
    }

    arm_overlay_watchdog();

    // 关键（2026-09-21 实测定案）：**悬浮球窗口在时，新建 overlay 的 webview 永远停在
    // about:blank**（页面不导航 → 永不 ready → 看门狗接管）。
    //
    // 但**不能**在这里「close 球 + 睡一会儿」：`close()` 只是把销毁消息排进主线程事件队列，
    // 主线程一睡就没人派发它，球根本没被销毁，醒来建窗照样撞坑（隔离实例 + CDP 对照 3/3，
    // 见 `wait_until_quiet` 注释）。正确做法：命令**立即返回**，把建窗挪到后台线程，
    // 等球真从窗口表里消失、再 `run_on_main_thread` 回主线程建窗。
    // 收起 overlay 时球会按配置放回来（见 close_overlay）。
    //
    // **无球也必须延迟建（2026-09-23 定案，smoke 4/4 红 + 探针 3/3 红：新 exe 无球 /
    // 显主窗后 / 旧装机 exe 同红）**：原先无球分支
    // 直接在命令派发上下文里同步 `WebviewWindowBuilder::build()`，WebView2 初始化会挂死——
    // 实测页面停 about:blank 永不导航、命令永无回包、主线程不再泵消息（watchdog 连发 6 次
    // WM_CLOSE 都关不掉，日志停在「残留但不可见」）。旧装机 exe 同样复现 = 旧病非本批回归；
    // 历史全绿是因为实例/用户配置一直有球、必走延迟分支（check-15 同路径绿）。
    if ball_present(&app) {
        close_ball_now(&app);
    }
    // 从这一刻起「有建窗在路上」：延迟恢复悬浮球会让位（否则它会在这段等待里把球建回来，
    // 等待方超时后带着球建窗，正好撞坑）。
    let pending = BuildPendingGuard::new();
    let app2 = app.clone();
    std::thread::spawn(move || {
        wait_desktop_quiet(&app2, 3000);
        let app3 = app2.clone();
        if let Err(e) = app2.run_on_main_thread(move || {
            let _pending = pending;
            if let Err(e) = build_overlay_window(&app3) {
                eprintln!("[snappin] 打开 overlay 失败: {e}");
            }
        }) {
            eprintln!("[snappin] 打开 overlay 排队失败: {e}");
        }
    });
    // 页面会在窗口建好后自行挂载并报 overlay_ready；调用方（热键/托盘/悬浮球）不等建窗。
    Ok(())
}

/// 测试专用：直接触发全局热键动作的后端路径（capture/pin/record/clip）。
///
/// 为什么存在：运行态验证套件过去用 SendInput 物理注入全局热键——注入进
/// 全局输入流，测试中断会残留按下的修饰键（用户键盘「失效」）且抢焦点。
/// 此命令让测试走与真实热键**完全相同**的 handler 分支（lib.rs with_handler
/// 里 match action 的四个目标函数），物理键盘零参与。仅在哨兵
/// snappin-cdp-flag 存在（= 测试实例）时放行，用户实例拒绝。
/// 四个目标都可从命令线程直接调：capture/pin/record 内部 run_on_main_thread，
/// toggle_clip_history 建窗分支自带排队（见其文档注释）。
#[tauri::command]
pub fn debug_trigger_hotkey(app: AppHandle, action: String) -> Result<(), String> {
    let flag = std::env::var("TEMP")
        .ok()
        .map(|t| std::path::Path::new(&t).join("snappin-cdp-flag").exists())
        .unwrap_or(false);
    if !flag {
        return Err("debug_trigger_hotkey 仅测试实例可用（缺 snappin-cdp-flag 哨兵）".into());
    }
    match action.as_str() {
        "capture" => crate::trigger_capture(&app),
        "pin" => crate::trigger_pin(&app),
        "record" => crate::toggle_record(&app),
        "clip" => {
            let _ = toggle_clip_history(app.clone());
        }
        other => return Err(format!("debug_trigger_hotkey: 未知动作 {other}")),
    }
    Ok(())
}

/// 武装 overlay 看门狗：**必须在建窗之前**调用——建窗本身就可能卡住（卡在 WebView2 环境
/// 创建里），建完再起等于不设防。复用已有窗口（只 show）时不需要重新武装。
///
/// 安全网（2026-09-21 实机踩到）：overlay 是全屏置顶窗，页面若没加载出来（WebView2 停在
/// about:blank），它会吃掉整个桌面的点击，用户只能重启机器。
fn arm_overlay_watchdog() {
    OVERLAY_READY.store(false, Ordering::SeqCst);
    let timeout = overlay_ready_timeout();
    std::thread::spawn(move || {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if OVERLAY_READY.load(Ordering::SeqCst) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!(
            "[snappin] overlay 页面 {}s 内未就绪 → 自救（防全屏置顶窗吃掉桌面输入）",
            timeout.as_secs()
        );
        // 用窗口类名 + 标题直接找 HWND，不依赖 app 内部状态——主线程卡住时
        // `get_webview_window` 那类 API 也拿不到东西，只有 Win32 层面才可靠。
        let hwnd = match unsafe { FindWindowW(w!("Tauri Window"), w!("SnapPin Overlay")) } {
            Ok(h) => h,
            Err(_) => {
                eprintln!("[snappin] 未发现残留 overlay 窗口（建窗失败，无残留）");
                return;
            }
        };
        // 连试三次 WM_CLOSE（间隔 2s）：主线程偶发忙（例如正在跑一次拖动循环）时，
        // 窗口消息仍会被派发，多给几次机会就能关掉。
        for attempt in 1..=6 {
            unsafe {
                let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
            }
            std::thread::sleep(Duration::from_secs(3));
            if !unsafe { IsWindow(hwnd) }.as_bool() {
                eprintln!("[snappin] overlay 已在第 {attempt} 次 WM_CLOSE 后关闭");
                return;
            }
        }
        // 还活着才考虑退出进程——但**只有窗口真的可见**（在吃桌面输入）才值得走这一步。
        // 不可见的残留窗口杀不掉也没关系，没必要为此把整个应用（含用户未保存的录制状态）带走。
        if unsafe { IsWindowVisible(hwnd) }.as_bool() {
            eprintln!("[snappin] overlay 可见且关不掉（主线程无响应）→ 退出进程自救");
            std::process::exit(1);
        }
        eprintln!("[snappin] overlay 残留但不可见 → 不退出进程（避免误杀整个应用）");
    });
}

/// 建 overlay 窗口（**必须在主线程调用**：Tauri/wry 要求，别为了 async 把它挪走）。
fn build_overlay_window(app: &AppHandle) -> Result<(), String> {
    // 延迟建窗期间可能已经有人建好了（用户连按两次截图键）：有就直接显示+通知，别再建第二个。
    if app.get_webview_window("overlay").is_some() {
        // 同 open_overlay：复用时不 show，等页面抓到新帧自己调 show_overlay（抓帧期间窗口必须不可见）。
        let _ = app.emit_to("overlay", "overlay-shown", ());
        return Ok(());
    }
    bump_window_epoch();

    let b = capture::overlay_bounds();
    WebviewWindowBuilder::new(app, "overlay", WebviewUrl::App("index.html".into()))
        .title("SnapPin Overlay")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .position(b.x as f64, b.y as f64)
        .inner_size(b.w.max(1) as f64, b.h.max(1) as f64)
        // 几何防护（2026-09-23 用户实测「误拖分离后固定位置大小」根因之一）：
        // 全屏贴边的可调窗从屏幕边缘 1-2px 拖动会被**原生**改窗（从左上角拖 = 位置+
        // 大小一起变），Win11 snap 键/手势同理可用 → 关掉 resizable/movable，
        // overlay 几何只有构建期一个来源（重开由 show_overlay 再钉一次 bounds）。
        .resizable(false)
        // **建出来先不可见**：页面要等抓到冻结帧才调 `show_overlay`（见 open_overlay 注释）。
        .visible(false)
        // 参数必须与主窗/pin 完全一致，否则 WebView2 环境创建失败 → 窗口建不出来
        // （见 lib.rs::browser_args 注释）。
        .additional_browser_args(crate::browser_args())
        .build()
        .map_err(|e| e.to_string())?;

    // 注：tauri 的 WebviewWindow 无 movable 开关（builder 与窗 API 均无，cargo 实测）——
    // 移动/改尺寸的现实路径（贴边拖、Win+方向键 snap）都需要 WS_THICKFRAME，已由
    // .resizable(false) 堵死；任何残余几何漂移由 show_overlay 显示前重钉 bounds 兜底。
    //
    // A+（2026-09-24）：**不再设 WDA_EXCLUDEFROMCAPTURE**——带排除的**可见**窗会被
    // NVIDIA Overlay 当受保护内容，弹「SnapPin 正在阻止桌面录制」并禁用其截屏/录屏
    // （0.34 撤球、09-24 用户实拍 overlay 同款；全仓原无 WDA_NONE 清除 → 终身在场）。
    // 成品无红框/条改靠几何+时序层：红框外扩 2px 出 crop（Overlay.tsx recframe 定位）、
    // 录制条 recbarPos 换边/横向逃逸、尺寸标签录制期隐藏、startRecord 先置态再起流。
    // 断言：check-12 ③（时序口径）+ check-19（成品内缘环带无饱和红）。
    Ok(())
}

/// 冻结帧就绪后由前端调用：把 overlay 窗口亮出来。
///
/// 与 `open_overlay` 分成两步的原因见那里的注释——抓帧必须在窗口不可见时进行，
/// 否则上一轮的冻结帧与工具条会被拍进新帧（「截屏里还有截屏」）。
///
/// `epoch` 是前端从 `capture_screen_data` 拿到的**抓帧那一刻**的世代号：若期间
/// `close_overlay` 已经跑过（用户按 Esc/✕ 取消），号会对不上，这时**不显示**——
/// 否则窗口会在用户关掉之后又自己冒出来。
/// 窗口不存在（页面已自关）时静默返回：调用方（前端）无需区分。
#[tauri::command]
pub fn show_overlay(app: AppHandle, epoch: u64) -> Result<(), String> {
    if epoch != OVERLAY_SHOW_EPOCH.load(Ordering::SeqCst) {
        return Ok(()); // 抓帧期间用户已经关了窗：这次不显示
    }
    if let Some(win) = app.get_webview_window("overlay") {
        // 暂存此刻前台窗（overlay 还没 show，此刻是用户原来的程序）：录制开始由
        // rec_input_passthrough 归还键盘焦点——修「录屏中键盘不可用」的第三件套。
        let prev = unsafe { GetForegroundWindow() } as isize;
        if let Ok(mut g) = OVERLAY_PREV_FOCUS.lock() {
            *g = (prev != 0).then_some(prev);
        }
        // 录制态让渡的带病兜底：上一轮录制若在异常/HTTP 竞态里没走到复原，
        // 复用窗不得把窗口区域/NOACTIVATE 残留带进下一轮截图（hide 复用设计的配套清理）。
        if let Ok(h) = win.hwnd() {
            let p = h.0 as *mut core::ffi::c_void;
            unsafe {
                SetWindowRgn(p, 0, 1);
                let ex = GetWindowLongPtrW(p, GWL_EXSTYLE_RAW);
                SetWindowLongPtrW(p, GWL_EXSTYLE_RAW, ex & !WS_EX_NOACTIVATE_RAW);
            }
        }
        // 每次显示前把几何钉回 overlay_bounds（2026-09-23「误拖分离后固定位置大小」
        // 根因之二）：close_overlay 只 hide 不销毁，几何一旦被改（边缘误拖/系统 snap/
        // 显示器变化）会被复用永久固化——「再按截图位置大小固定、改不了」。
        // bounds 与抓帧同源（capture::overlay_bounds），保证背景图与坐标不错位。
        let b = capture::overlay_bounds();
        let _ = win.set_position(tauri::LogicalPosition::new(b.x as f64, b.y as f64));
        let _ = win.set_size(tauri::LogicalSize::new(
            b.w.max(1) as f64,
            b.h.max(1) as f64,
        ));
        win.show().map_err(|e| e.to_string())?;
        let _ = win.set_focus();
    }
    Ok(())
}

/// 关闭 overlay 窗口。
///
/// **只隐藏、不销毁**（2026-09-21 实测定案）：反复 close→重建会让新建的 webview 停在
/// about:blank（页面永不 ready → 看门狗接管 → 用户看到「按了没反应」甚至应用退出）。
/// 复用同一个窗口既绕开这个坑，又让再次截图变快（不用重新建 WebView2）。
/// 页面在收到 overlay-shown 事件后自行重置状态并重抓冻结帧。
///
/// `restore_ball`（默认 true）：是否按 `desktop_ball` 把建 overlay 时收走的悬浮球放回来。
/// **长截图路径要传 false**——长截图是整屏抓帧拼接，球条若落在选区里会被拍进产物
/// （2026-09-21 用户实测）；这条路径的调用方负责事后自己恢复球。
#[tauri::command]
pub fn close_overlay(app: AppHandle, restore_ball: Option<bool>) -> Result<(), String> {
    // 先推进「显示世代号」：抓帧在途的页面即便随后调 show_overlay 也会被拒（见该命令注释）。
    OVERLAY_SHOW_EPOCH.fetch_add(1, Ordering::SeqCst);
    if let Some(win) = app.get_webview_window("overlay") {
        win.hide().map_err(|e| e.to_string())?;
    }
    // 把建 overlay 时收走的悬浮球放回来（用户开着才放）。
    // 注意是**后台延迟**恢复，不是这里立刻建窗：紧跟着 hide 建新 webview 会撞上
    // 「已有 webview 在场 → 建窗卡死/停在 about:blank」的坑（见 spawn_ball_restore）。
    if restore_ball.unwrap_or(true) && ball_wanted(&app) {
        spawn_ball_restore(app);
    }
    Ok(())
}

/// 开始录屏。`gif=true` 时产 GIF（先录 MP4，停止时转码），扩展名随之变 .gif。
///
/// **必须是 async**：拉起 ffmpeg（建进程 + 写参数 + 等首帧就绪）是重活，同步命令跑在
/// 主线程上会让整个界面在「按下录制」到「录制条出现」之间僵住。真正建窗/抓帧仍在
/// ffmpeg 子进程里，主线程只是不再被占。
#[tauri::command]
pub async fn start_record(
    app: AppHandle,
    state: tauri::State<'_, RecordState>,
    cfg: tauri::State<'_, ConfigState>,
    bounds: Option<Bounds>,
    fps: Option<u32>,
    bitrate_k: Option<u32>,
    gif: Option<bool>,
) -> Result<String, String> {
    start_record_impl(
        &app,
        state.inner(),
        cfg.inner(),
        bounds,
        fps,
        bitrate_k,
        gif,
    )
}

/// 开始录屏的**同步实现**：async 命令与热键/托盘路径（`lib.rs` 在主线程上直接调用）
/// 共用同一份逻辑——async fn 在热键回调里没法 await，不抽出来就会有两份实现慢慢漂移。
pub fn start_record_impl(
    app: &AppHandle,
    state: &RecordState,
    cfg: &ConfigState,
    bounds: Option<Bounds>,
    fps: Option<u32>,
    bitrate_k: Option<u32>,
    gif: Option<bool>,
) -> Result<String, String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if guard.is_some() {
        return Err("已有录制在进行".into());
    }
    // 录制开始前先把悬浮球收掉（A 方案 2026-09-23）：热键全屏录时球还在桌上，
    // 不关会被拍进成品；overlay 路径的球早在 open_overlay 就关了，这里是统一兜底。
    close_ball_now(app);
    let gif = gif.unwrap_or(false);
    let dir = captures_dir(app);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let ext = if gif { "gif" } else { "mp4" };
    // 录制产物同样按 save_name_format 命名（序号与截图共用一个空间）。
    let out_path = dir.join(format!("{}.{}", product_stem(app, ""), ext));
    let (dfps, dbr, audio_device) = {
        let c = cfg.0.lock().map_err(|e| e.to_string())?;
        // GIF 容器没有音轨：即便设置里开了音频也不录（否则 ffmpeg 会因流无法封装而失败）。
        let audio = if gif {
            None
        } else {
            crate::recorder::resolve_audio_device(c.record_audio, &c.record_audio_device)
        };
        (c.record_fps, c.record_bitrate_k, audio)
    };
    let opts = RecordOptions {
        bounds,
        fps: fps.unwrap_or(dfps),
        bitrate_k: bitrate_k.unwrap_or(dbr),
        out_path: out_path.clone(),
        gif,
        audio_device,
    };
    let rec = Recorder::start(opts)?;
    *guard = Some(rec);
    Ok(out_path.to_string_lossy().to_string())
}

/// 停止录屏。discard=true 时丢弃录制（杀进程删产物），返回空串；否则优雅收尾返回产物路径。
///
/// **必须是 async**（2026-09-21 用户实测「停止后卡顿、无法立刻再截图」）：
/// `Recorder::stop` 要等 ffmpeg 退出（区域录制 hwdownload 回读，收尾可达数秒），
/// GIF 模式还要同步跑 palettegen/paletteuse 转码（长录可达几十秒）。同步命令跑在
/// 主线程上，期间整个应用（含再次按截图键、托盘、UIA 响应）全部冻结；
/// async 让 Tauri 把它丢进异步运行时，主线程继续泵消息。
#[tauri::command]
pub async fn stop_record(
    app: AppHandle,
    state: tauri::State<'_, RecordState>,
    discard: Option<bool>,
) -> Result<String, String> {
    let rec = {
        let mut guard = state.0.lock().map_err(|e| e.to_string())?;
        guard.take()
    };
    let had = rec.is_some();
    let out = match rec {
        Some(r) => {
            if discard.unwrap_or(false) {
                r.discard().map(|()| String::new())
            } else {
                let res = r.stop(|pct| emit_transcode(&app, true, pct));
                emit_transcode(&app, false, 100);
                res.map(|p| p.to_string_lossy().to_string())
            }
        }
        None => Err("当前没有录制".into()),
    };
    // 录制已停 → 恢复悬浮球（录制期间 spawn_ball_restore 因 is_recording 跳过，A 方案）。
    if had {
        spawn_ball_restore(app.clone());
    }
    out
}

/// 广播 GIF 转码进度（`active=false` = 结束）。前端在录制条上显示「转码中 xx%」——
/// 长录转码几十秒，没有它用户只会看到「停止后卡住」。
pub fn emit_transcode(app: &AppHandle, active: bool, percent: u8) {
    let _ = app.emit_to(
        "overlay",
        "snappin-transcode",
        serde_json::json!({ "active": active, "percent": percent }),
    );
}

/// 暂停录制：当前段封存，继续时新起一段（stop 时无损拼接）。
#[tauri::command]
pub fn pause_record(state: tauri::State<'_, RecordState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    match guard.as_mut() {
        Some(r) => r.pause(),
        None => Err("当前没有录制".into()),
    }
}

/// 继续录制（暂停后调用）。
#[tauri::command]
pub fn resume_record(state: tauri::State<'_, RecordState>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    match guard.as_mut() {
        Some(r) => r.resume(),
        None => Err("当前没有录制".into()),
    }
}

/// 枚举本机可用的音频输入设备（设置面板的下拉候选）。
/// 空数组 = 本机没有 dshow 音频设备（此时开「录音频」也不会录到东西）。
#[tauri::command]
pub async fn list_audio_devices() -> Vec<String> {
    crate::recorder::audio_devices()
}

// ============================ M2 ============================

/// 智能窗口检测：列出当前可见顶层窗口（物理像素边界，z 序前置优先）。
///
/// **必须是 async**：`EnumWindows` 要枚举并逐个取窗口矩形/标题，桌面上窗口多时是
/// 百毫秒级重活，同步命令会把主线程（含 overlay 自己的鼠标响应）堵住。
#[tauri::command]
pub async fn list_windows() -> Result<Vec<WindowRect>, String> {
    Ok(window::list_windows())
}

/// UIA 子区域检测：返回光标处 UI 元素的物理像素矩形（PixPin 同款「智能放大」）。
/// 无更细元素、命中 overlay 自身、或 UIA 超时/失败时返回 None，前端回退顶层窗口命中。
///
/// 为什么是 `async`：UIA 查询要等常驻线程回话（最长 400ms）。同步命令跑在**主线程**上，
/// 主线程一堵，UIA 反过来要等我们自己的窗口响应 → 实测每次都撞满超时、识别恒为空。
/// `async` 让 Tauri 把它丢到异步运行时，主线程继续泵消息。
#[tauri::command]
pub async fn ui_element_at(x: i32, y: i32) -> Option<uia::Rect> {
    uia::element_rect_at(x, y)
}

/// UIA 智能识别的**层级链**：返回光标处元素及其祖先（内→外）的矩形 + 类型/名称。
/// overlay 用滚轮在这条链上切换识别层级（PixPin 同款）。空数组 = 未命中，
/// 前端回退顶层窗口命中（`list_windows`）。`async` 理由同 `ui_element_at`。
#[tauri::command]
pub async fn ui_chain_at(x: i32, y: i32) -> Vec<uia::ElementInfo> {
    uia::element_chain_at(x, y)
}

fn decode_png_base64(png_base64: &str) -> Result<Vec<u8>, String> {
    let raw = png_base64
        .split_once(',')
        .map(|(_, b)| b)
        .unwrap_or(png_base64);
    STANDARD.decode(raw.trim()).map_err(|e| e.to_string())
}

/// 贴图载荷副本（label → base64）。
///
/// why-not 直接用 PinState：pin.rs 的载荷是**一次性**的（`take_payload` 取走即删），
/// 贴图窗口挂载后想「保存到文件」就再也取不到内容了 → 建贴图时另存一份。
static PIN_PAYLOADS: LazyLock<Mutex<std::collections::HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// 把一张 PNG（base64）钉成置顶小窗，返回窗口 label。
#[tauri::command]
pub fn pin_image(
    app: AppHandle,
    state: tauri::State<PinState>,
    png_base64: String,
    x: i32,
    y: i32,
) -> Result<String, String> {
    let label = pin::create_pin(&app, &state, png_base64.clone(), x, y)?;
    // 留载荷副本给 pin_save 用（见 PIN_PAYLOADS 的说明）。
    if let Ok(mut m) = PIN_PAYLOADS.lock() {
        m.insert(label.clone(), png_base64);
    }
    Ok(label)
}

/// 把贴图（pin）产物保存成文件，返回**保存后的绝对路径**。
///
/// D12：贴图原先只进内存 PinState，界面上没有任何保存入口（用户 2026-10-20 登记）。
/// `id` = 贴图窗口的 label（`pin-<n>`，Pin.tsx 用 `getCurrentWindow().label` 就能拿到）。
/// 找不到该 id 的内容时返回 `pin_not_found`（前端可直接显示）。
#[tauri::command]
pub fn pin_save(
    app: AppHandle,
    id: String,
    state: tauri::State<crate::pin::PinState>,
) -> Result<String, String> {
    let pin_state: &crate::pin::PinState = &state;
    let b64 = PIN_PAYLOADS
        .lock()
        .ok()
        .and_then(|m| m.get(&id).cloned())
        // 还没被 take_pin_payload 取走时，PinState 里也有一份。
        .or_else(|| pin_state.0.lock().ok().and_then(|m| m.get(&id).cloned()))
        .ok_or_else(|| "pin_not_found".to_string())?;
    let bytes = decode_png_base64(&b64)?;
    let img = image::load_from_memory(&bytes)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    let dir = captures_dir(&app);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    // 命名与截图产物同一套（时间戳/序号 + PNG/JPG 格式设置）。
    let (ext, fmt) = image_ext_and_format(&app);
    let path = unique_path(&dir, &product_stem(&app, ""), ext);
    capture::save_image(&img, &path, fmt)?;
    Ok(path.to_string_lossy().to_string())
}

/// Pin.tsx 挂载后取走本窗待显示的图像 base64。
#[tauri::command]
pub fn take_pin_payload(state: tauri::State<PinState>, label: String) -> Option<String> {
    pin::take_payload(&state, &label)
}

/// 关闭指定贴图窗口。
#[tauri::command]
pub fn close_pin(app: AppHandle, label: String) -> Result<(), String> {
    if let Some(win) = app.get_webview_window(&label) {
        win.close().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 对一张 PNG（base64）做 OCR，返回全文与逐行框。
///
/// **必须是 async**：OCR 推理（含首次加载模型）是秒级重活，同步命令会冻住主线程。
/// 识别出的全文按 `config.ocr_auto_copy` 自动进剪贴板（见 `auto_copy_ocr`）。
#[tauri::command]
pub async fn ocr_image(
    app: AppHandle,
    png_base64: String,
    lang: Option<String>,
) -> Result<OcrResult, String> {
    let bytes = decode_png_base64(&png_base64)?;
    let res = ocr::ocr_png(&bytes, lang.as_deref())?;
    auto_copy_ocr(&app, &res.text);
    Ok(res)
}

/// 长截图：在给定区域滚动拼接，落盘/复制，返回路径。
///
/// **必须是 async**：滚动拼接要跑几秒（每步等画面落定，最多 60 步），同步命令
/// 会冻结主线程（页面/托盘/UIA 全堵）。**调用方必须先关掉 overlay**——全屏置顶窗
/// 在时滚轮注入全被它吃掉，目标窗口不滚 → 第一帧即判到底 → 只出一帧就收回
/// （2026-09-21 用户实测「亮一下录制界面就收回」的根因）。
#[tauri::command]
pub async fn capture_long(
    app: AppHandle,
    bounds: Bounds,
    copy: bool,
    save: bool,
) -> Result<FinishResult, String> {
    // 拼接期间挂上「长截图进行中」标记：延迟恢复悬浮球会因此一直让位（见 spawn_ball_restore）。
    // 为什么必须在后端兜这一层：前端 close_overlay 之后才调本命令，而球的恢复是**后台延迟**
    // 的——不拦的话球会在拼接途中弹出来，被后续帧抓进产物（用户 2026-09-21 实测
    // 「长截图把悬浮球拍进去了」）。前端传 restoreBall:false 是第一道，这里是第二道。
    let _guard = LongshotGuard::new();
    // 抓首帧前必须等 overlay **真的从屏上消失**：前端 `doLong` 先 `close_overlay`
    // 再调本命令，而 `hide()` 只是把消息排进主线程事件队列——命令到达时 overlay 仍在屏上，
    // 首帧抓到的就是浮层自己（白底 + 选区手柄 + 工具条，记事本文本被压成灰字），
    // 重叠匹配随后无真实位移，只追加重复块并早早 break（2026-10-15 check-2 复现：
    // 产物 500x565，前 380 行是遮罩、其后重复；用户 2026-09-21 报的「亮一下就收回」同源）。
    // why-not 在前端 await 一个固定延时：延时猜不准（hide 派发时机取决于主线程忙闲），
    // 等久了白等、等短了照旧拍到浮层；这里按窗口表判据等，是同一套已验证的形状。
    crate::commands::wait_desktop_quiet(&app, 3000);
    // 滚动参数来自用户设置（步长/等待上限），不写死在命令里。
    let opts = app
        .try_state::<ConfigState>()
        .and_then(|s| {
            s.0.lock()
                .ok()
                .map(|c| longshot::ScrollOptions::from_config(&c, 60))
        })
        .unwrap_or_default();
    let img = longshot::capture_scrolling(bounds, opts)?;
    if copy {
        capture::copy_to_clipboard(&img)?;
    }
    let mut out = None;
    if save {
        // 同 finish_capture：先写 `.pending/` 暂存件，用户在确认条上点头后才由
        // commit_capture 落到最终路径（D12：先确认、再落盘）。
        let dir = pending_dir(&app);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        // 长截图与普通截图共用同一套命名/格式设置（序号也共用，不会撞名）。
        let (ext, fmt) = image_ext_and_format(&app);
        let path = unique_path(&dir, &product_stem(&app, "long_"), ext);
        capture::save_image(&img, &path, fmt)?;
        out = Some(path.to_string_lossy().to_string());
    }
    Ok(FinishResult { path: out })
}

/// 可直接粘成**位图**（CF_DIB）的静态图扩展名。
///
/// 只列本仓 `image` crate 真编了解码器的格式（Cargo.toml 里 image 只开了 png/jpeg）：
/// webp/bmp 虽在旧白名单里，但本仓没编它们的解码器，`image::open` 必然失败 →
/// 「复制」按钮直接报错。
const IMAGE_PASTE_EXTS: [&str; 3] = ["png", "jpg", "jpeg"];

/// 以**文件**形式复制到剪贴板（CF_HDROP）的扩展名：动图容器 + 本仓解不了的格式。
///
/// why-not 动图走位图：Windows 位图剪贴板本就没有帧信息，多帧在进本进程前已丢；
/// `image` 的 GIF 解码器又只解第一帧 → 动图被**静默压成静图**且零报错
/// （用户 2026-10-20 登记 D9「动图复制只出单图」）。文件列表里才是会动的原文件。
const FILE_PASTE_EXTS: [&str; 4] = ["gif", "webp", "apng", "bmp"];

/// 把产物复制到系统剪贴板（结果条「复制」按钮，2026-10-14 加）。
///
/// 分两路：静态图片类走 CF_DIB（能直接粘进聊天/文档，即用户说的「复制这张图」）；
/// 动图/视频没有「图片剪贴板」表示，按文件列表（CF_HDROP）粘——粘进 Explorer/聊天
/// 是作为一个文件附件，这是 Windows 上最接近「复制」的语义。
#[tauri::command]
pub fn copy_product(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.is_file() {
        return Err(format!("文件不存在：{path}"));
    }
    let ext = p
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        e if IMAGE_PASTE_EXTS.contains(&e) => {
            let img = image::open(&p).map_err(|e| format!("读取图片失败：{e}"))?;
            capture::copy_to_clipboard(&img.to_rgba8())
        }
        // 动图（gif/apng）与本仓没编解码器的格式（webp/bmp）按文件复制。
        e if FILE_PASTE_EXTS.contains(&e) => crate::clip::write_clipboard_files(&[path]),
        _ => crate::clip::write_clipboard_files(&[path]),
    }
}

/// 把产物目录加进 asset 协议读取白名单（保存确认阶段的 mp4/gif 回放预览用，
/// 前端 `convertFileSrc(path)` → asset:// URL）。启动（lib.rs setup）与 save_dir
/// 变更（update_settings / reset_settings / HTTP PUT）时调用。
/// `allow_directory` 只推 pattern 不查 FS（tauri 2.11.6 scope/fs.rs 实证），目录
/// 还不存在也能先授权；recursive=true 覆盖子目录。失败只记日志：回放 403 不致命。
///
/// **调用方严禁持有 ConfigState 锁**：内部 `captures_dir` 会再取同一把 std Mutex，
/// 同线程重锁 = 无声永久挂起（2026-09-24 三处调用点全踩过，探针实锤）。
pub fn allow_asset_captures(app: &AppHandle) {
    let dir = captures_dir(app);
    if let Err(e) = app.asset_protocol_scope().allow_directory(&dir, true) {
        eprintln!(
            "[snappin] asset 协议授权产物目录失败（保存条回放将403）: {} → {e}",
            dir.display()
        );
    }
}

/// 截图/录屏产物目录：用户设置优先，空则退系统图片目录下 SnapPin，再退临时目录。
pub fn captures_dir(app: &AppHandle) -> PathBuf {
    let from_cfg = app
        .try_state::<ConfigState>()
        .and_then(|s| s.0.lock().ok().map(|c| c.save_dir.trim().to_string()))
        .unwrap_or_default();
    if !from_cfg.is_empty() {
        return PathBuf::from(from_cfg);
    }
    app.path()
        .picture_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("SnapPin")
}

/// 待确认产物的**暂存目录**：产物目录下的 `.pending`。
///
/// why-not 直接写最终路径：用户 2026-09-21 明确要「先确认、再落盘」，而 D12 实测是
/// 「点保存就直接落盘、确认条形同虚设」。暂存目录与产物目录同卷 → 确认后 rename 即可；
/// 用户取消/删除时只删暂存件，产物目录里不留半成品（D12 验收判据）。
fn pending_dir(app: &AppHandle) -> PathBuf {
    captures_dir(app).join(".pending")
}

/// 清理上次运行遗留的暂存件（`.pending/` 里 1 小时前的文件）。
///
/// 用户点「取消」走前端的 remove_file；但进程被强杀/崩溃时暂存件会留下，
/// 而 `.pending` 就躺在用户的产物目录里——留着等于「目录里堆着一堆没用的图」。
/// 只删 1 小时前的：还挂在确认条上等着的那些不能误删。
pub fn purge_stale_pending(app: &AppHandle) {
    let dir = pending_dir(app);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for e in rd.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let age = now.duration_since(modified).unwrap_or_default();
        if age.as_secs() > 3600 {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// 在 `dir` 里取一个**尚不存在**的 `stem.ext` 路径：同名时依次尝试 `_2`、`_3`……。
///
/// why-not 覆盖写：同秒连拍两张（或序号与时间戳撞上）时后一张会把前一张覆盖掉，
/// 用户表现为「抓了两张只剩一张」（2026-10-20 次要发现 1）。后缀保持可读，
/// 不用随机串，产物在文件管理器里仍看得出是一批。
fn unique_path(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let mut candidate = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while candidate.exists() && n < 1000 {
        candidate = dir.join(format!("{stem}_{n}.{ext}"));
        n += 1;
    }
    candidate
}

// ============================ 另存为 / 文件操作 ============================

/// 用户点「取消」时 `IFileSaveDialog::Show` 返回的 HRESULT（`HRESULT_FROM_WIN32(ERROR_CANCELLED)`）。
/// 单独认出来是为了：取消是正常路径，不该刷错误日志；其它失败才值得记一行。
const HR_CANCELLED: i32 = 0x8007_04C7u32 as i32;

/// 弹 Windows 原生「另存为」对话框，返回用户选的完整路径；取消/失败 → `None`。
///
/// **必须是 async**：`Show()` 会**阻塞**到用户做出选择（可能几十秒）。同步命令跑在
/// 主线程上＝整个应用（含托盘、热键、overlay）在这段时间里全无响应——正是用户
/// 2026-09-21 报的那类「卡死」。丢到异步运行时后，主线程继续泵消息。
///
/// `kind` 决定扩展名与过滤器（`"png"|"mp4"|"gif"`）。
/// `default_name` 为空 = 调用方要的是**目录**（设置面板「浏览保存目录」走这条，
/// 传的就是空名）：这时用文件夹选择器，返回目录路径；非空则是标准另存为，
/// 返回含文件名的完整路径。
/// 已有另存为对话框开着时的重入闸门（0.26 问题 2 的后端双保险）：
/// 对话框是模态 COM 循环，前端 busy 闸门挡 UI 连点，这里挡绕过前端的直接调用。
static SAVE_DIALOG_OPEN: AtomicBool = AtomicBool::new(false);

#[tauri::command]
pub async fn pick_save_path(app: AppHandle, default_name: String, kind: String) -> Option<String> {
    // 已有对话框开着 → 直接拒绝，不再弹第二个。
    if SAVE_DIALOG_OPEN
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        eprintln!("[snappin] 另存为对话框已开着，忽略重复请求");
        return None;
    }
    let out = pick_save_path_impl(&app, &default_name, &kind);
    SAVE_DIALOG_OPEN.store(false, Ordering::SeqCst);
    out
}

/// `pick_save_path` 的实现（同步，必须整段跑在同一个线程上——COM 套间是线程私有的）。
fn pick_save_path_impl(app: &AppHandle, default_name: &str, kind: &str) -> Option<String> {
    // 扩展名与过滤器按产物类型给（用户 2026-09-21：「录屏和 gif 也要能选保存位置」）。
    // "jpg" 是 save_format_png=false 时的截图产物（确认条按真实扩展名传 kind）。
    let (ext, filter_label) = match kind {
        "mp4" => ("mp4", "MP4 视频"),
        "gif" => ("gif", "GIF 动图"),
        "jpg" | "jpeg" => ("jpg", "JPEG 图片"),
        // 动图与 webp 也要能选保存位置（D9 白名单放开后的配套过滤项）。
        "apng" => ("apng", "APNG 动图"),
        "webp" => ("webp", "WebP 图片"),
        _ => ("png", "PNG 图片"),
    };
    // 初始目录 = 产物目录。`FOS_PATHMUSTEXIST` 要求它存在，否则对话框直接报错。
    let dir = captures_dir(app);
    let _ = std::fs::create_dir_all(&dir);

    // 另存为对话框的 owner：取 overlay 窗口（0.26 问题 1）。无主的原生对话框会被
    // always_on_top 的全屏覆盖层压在下面（用户实测「另存为被截图页面遮住」）；
    // 设了 owner，对话框归 overlay 所有，必然浮在其上。overlay 不在场（如设置面板
    // 「浏览保存目录」）就退回 None，行为与旧版一致。
    let owner: Option<HWND> = app
        .get_webview_window("overlay")
        .filter(window_alive)
        .and_then(|ov| ov.hwnd().ok())
        .map(|h| HWND(h.0));

    unsafe {
        // COM：本命令跑在异步运行时的线程上，而套间是**线程私有**的，每个进来的线程都得
        // 自己初始化。容忍 `RPC_E_CHANGED_MODE`（线程已被初始化成别的套间，例如运行时
        // 线程早先被初始化成 MTA）：这时继续用对话框，但**不能再 CoUninitialize**——
        // 配对调用会把别的代码还没用完的套间拆掉。
        // `S_FALSE`（本线程已按同模式初始化过）算成功，按 COM 规则仍要配一次 CoUninitialize。
        let hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let need_uninit = hr.is_ok();
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            eprintln!("[snappin] CoInitializeEx 失败，无法弹另存为对话框: {hr:?}");
            return None;
        }
        let out = save_dialog(owner, &dir, default_name, ext, filter_label);
        if need_uninit {
            CoUninitialize();
        }
        out
    }
}

/// 真正弹对话框的部分（调用方保证本线程 COM 已初始化）。
///
unsafe fn save_dialog(
    owner: Option<HWND>,
    dir: &Path,
    default_name: &str,
    ext: &str,
    filter_label: &str,
) -> Option<String> {
    // `default_name` 为空 = 调用方要的是目录（设置面板的「浏览保存目录」）。
    let folder_mode = default_name.trim().is_empty();
    let dialog: IFileDialog = if folder_mode {
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)
    } else {
        CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER)
    }
    .map_err(|e| eprintln!("[snappin] 创建文件对话框失败: {e}"))
    .ok()?;

    let mut opts = FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST | FOS_NOREADONLYRETURN;
    if folder_mode {
        // 只要目录：`FOS_OVERWRITEPROMPT`（针对文件名的覆盖提示）没有意义，不要带。
        opts |= FOS_PICKFOLDERS;
    } else {
        opts |= FOS_OVERWRITEPROMPT;
    }
    dialog.SetOptions(opts).ok()?;
    let _ = dialog.SetTitle(&HSTRING::from(if folder_mode {
        "选择保存目录"
    } else {
        "另存为"
    }));

    // 初始目录：用户设置的产物目录（不存在已由调用方创建）。
    let dir_w = HSTRING::from(dir.as_os_str().to_string_lossy().to_string());
    if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&dir_w, None) {
        let _ = dialog.SetFolder(&item);
    }

    // 过滤器/文件名只在调用期间需要有效：`SetFileTypes` 会把字符串拷进对话框，
    // 两个宽串活到函数结束，指针全程有效。
    let name_wide: Vec<u16> = format!("{filter_label} (*.{ext})")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let spec_wide: Vec<u16> = format!("*.{ext}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    if !folder_mode {
        let spec = COMDLG_FILTERSPEC {
            pszName: PCWSTR(name_wide.as_ptr()),
            pszSpec: PCWSTR(spec_wide.as_ptr()),
        };
        let _ = dialog.SetFileTypes(&[spec]);
        // 1-based：让上面这一条成为默认选中的过滤器。
        let _ = dialog.SetFileTypeIndex(1);
        let _ = dialog.SetDefaultExtension(&HSTRING::from(ext));
        let _ = dialog.SetFileName(&HSTRING::from(default_name));
    }

    // `Show` 的 Err 既包含「用户取消」也包含真实失败：对调用方来说都是「没选」。
    // windows-0.58 的 `Param<HWND>` 对可空参数只支持 `Option<&HWND>`（值形式会撞
    // CopyType 约束），必须传引用。
    if let Err(e) = dialog.Show(owner.as_ref()) {
        if e.code().0 != HR_CANCELLED {
            eprintln!("[snappin] 另存为对话框失败: {e}");
        }
        return None;
    }

    let item = match dialog.GetResult() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[snappin] 另存为：取结果失败: {e}");
            return None;
        }
    };
    // `SIGDN_FILESYSPATH` 要的就是文件系统路径；返回的 PWSTR 由 COM 分配，用完必须 CoTaskMemFree。
    match item.GetDisplayName(SIGDN_FILESYSPATH) {
        Ok(p) => {
            let path = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const core::ffi::c_void));
            path
        }
        Err(e) => {
            eprintln!("[snappin] 另存为：取路径失败: {e}");
            None
        }
    }
}

/// 两个路径是否指同一份文件：优先 canonicalize（大小写、短名、相对路径都归一），
/// 目标还不存在（canonicalize 失败）时退回不区分大小写的字符串比较——Windows 文件系统
/// 本身不区分大小写。
fn same_path(a: &str, b: &str) -> bool {
    if let (Ok(pa), Ok(pb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        return pa == pb;
    }
    a.eq_ignore_ascii_case(b)
}

/// 把 `src` 复制到 `dst`（前端「另存为」的落盘动作：复制过去后自己删掉默认路径那份，等价移动）。
/// `src == dst` 视为成功——用户在对话框里原地确认时，复制到自身会被系统拒绝（共享冲突），
/// 但语义上这就是「保存好了」。
#[tauri::command]
pub async fn save_file_as(src: String, dst: String) -> Result<(), String> {
    if same_path(&src, &dst) {
        return Ok(());
    }
    if let Some(parent) = Path::new(&dst).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目标目录失败：{e}"))?;
    }
    std::fs::copy(&src, &dst).map_err(|e| format!("复制到 {dst} 失败：{e}"))?;
    Ok(())
}

/// 删除文件（「另存为」后清掉默认路径那份、确认条上的「删除」）。
/// 文件不存在视为成功：重复点、产物已被移走时不该给用户报错。
#[tauri::command]
pub async fn remove_file(path: String) -> Result<(), String> {
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("删除 {path} 失败：{e}")),
    }
}

// ============================ 设置面板 ============================

/// 读全部设置（前端渲染表单用；token/端口一并回——同机本地 UI，无跨用户面）。
#[tauri::command]
pub fn get_settings(cfg: tauri::State<ConfigState>) -> Result<AppConfig, String> {
    cfg.0.lock().map(|c| c.clone()).map_err(|e| e.to_string())
}

/// 更新设置：整体传入，normalize 清洗后落盘。返回清洗后的最终值（前端表单回填）。
#[tauri::command]
pub fn update_settings(
    app: AppHandle,
    cfg: tauri::State<ConfigState>,
    new_cfg: AppConfig,
) -> Result<AppConfig, String> {
    let mut c = cfg.0.lock().map_err(|e| e.to_string())?;
    let token = c.token.clone();
    let port = c.port;
    // 变更前留存副作用相关字段：只有真变了才写注册表/重授白名单，
    // 否则每次切一个开关都做一次注册表写 + asset scope，设置页每点必卡（用户实测）。
    let prev_autostart = c.auto_start;
    let prev_save_dir = c.save_dir.clone();
    // 热键守卫开关变了才重注册热键（否则每改一个无关设置都注销+重注册一遍）。
    let prev_hotkey_guard = (c.hotkeys_disabled, c.hotkeys_disable_fullscreen);
    // 版本号不能被前端整份回传降回 0：0 = 「旧配置」，下次启动会被当旧配置迁移，
    // 把用户显式关掉的 record_audio 强行打开（契约 C-4）。
    let prev_config_version = c.config_version;
    *c = new_cfg;
    // token/端口不从 UI 改：保持现值（端口改了要重启进程，不走这里）。
    c.token = token;
    c.port = port;
    if c.config_version == 0 {
        c.config_version = prev_config_version;
    }
    c.normalize();
    // 画质是**进程级状态**（capture.rs 里的静态量），设置一改就同步，
    // 否则桌面 UI 截图路径还在用旧值（http 路径另有 refresh_capture_config 兜底）。
    crate::capture::apply_config(&c);
    let path = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("snappin"))
        .join("config.json");
    c.save(&path);
    // 系统副作用即时生效，但仅在相关字段真变化时触发（run_as_admin 下次启动才生效，
    // hotkeys 变更由前端另调 re_register_hotkeys）。
    let autostart_changed = c.auto_start != prev_autostart;
    let save_dir_changed = c.save_dir != prev_save_dir;
    if autostart_changed {
        crate::system::set_autostart(c.auto_start);
    }
    let out = c.clone();
    // **先放锁再调 allow**：allow_asset_captures → captures_dir 会再取同一把
    // ConfigState std Mutex，同线程重锁 = 无声永久挂起（2026-09-24 探针实锤：
    // 持锁调用点让设置保存整个挂死，HTTP 单线程循环一并假死）。
    drop(c);
    // save_dir 变了才重授 asset 协议白名单（回放预览依赖它）；没变就免这次开销。
    if save_dir_changed {
        allow_asset_captures(&app);
    }
    // 守卫开关（禁用全部热键 / 全屏自动禁用）变更即时生效：前端只发 update_settings，
    // 不另调 re_register_hotkeys，所以这里必须自己同步，否则开关点了没反应。
    if (out.hotkeys_disabled, out.hotkeys_disable_fullscreen) != prev_hotkey_guard {
        crate::sync_hotkey_surfaces(&app, &out);
    }
    Ok(out)
}

/// 恢复全部默认设置（保留 token/端口），落盘并返回。
#[tauri::command]
pub fn reset_settings(app: AppHandle, cfg: tauri::State<ConfigState>) -> Result<AppConfig, String> {
    let mut c = cfg.0.lock().map_err(|e| e.to_string())?;
    let token = c.token.clone();
    let port = c.port;
    *c = AppConfig::default();
    c.token = token;
    c.port = port;
    c.normalize();
    let path = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::temp_dir().join("snappin"))
        .join("config.json");
    c.save(&path);
    // 重置后同样对齐系统副作用：默认关自启；热键守卫开关也回到默认（false），
    // 这里直接同步一次，不依赖前端随后调 re_register_hotkeys（托盘勾选态也一并回写）。
    crate::system::set_autostart(c.auto_start);
    let out = c.clone();
    // 同 update_settings：先放锁再调 allow，防同线程重锁自锁（见其注释）。
    drop(c);
    // save_dir 被重置回默认 → 同步重授 asset 协议白名单。
    allow_asset_captures(&app);
    crate::sync_hotkey_surfaces(&app, &out);
    Ok(out)
}

/// 重绑热键后前端调用：按当前 ConfigState 重注册全局热键（无参，读现值）。
/// 返回每动作注册结果（requested vs effective + 失败原因）——前端据此把
/// 「注册失败已回退」如实显示出来，不再静默（2026-09-24「热键改不了」根因修复）。
#[tauri::command]
pub fn re_register_hotkeys(
    app: AppHandle,
    cfg: tauri::State<ConfigState>,
) -> Result<Vec<crate::hotkeys::HotkeyRegResult>, String> {
    let c = cfg.0.lock().map_err(|e| e.to_string())?.clone();
    Ok(crate::sync_hotkey_surfaces(&app, &c))
}

/// 产物名里的时间戳：**epoch 毫秒**。
///
/// why-not 只用秒：同一秒里连拍两张会算出同一个文件名，后一张直接把前一张覆盖掉
/// （用户 2026-10-20 实测「抓两张只剩一张」，次要发现 1）。毫秒精度把撞名压到可忽略，
/// 又不牺牲可读性（纯数字、字典序即时间序）。
/// why-not 不用「秒_毫秒」两段：`next_sequence` 按「最后一段下划线后的数字」认序号，
/// 拆两段会把毫秒当成序号号（见 next_sequence）。
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    ms.to_string()
}

/// 产物文件基名（不含扩展名）。`kind` 是类别中缀：截图 `""`、长截图 `"long_"`、录制 `""`。
///
/// 谁在什么时机读它：`finish_capture` / `capture_long` / `start_record_impl` / `pin_save` 落盘前读
/// `config.save_name_format`。`Timestamp` = `SnapPin_<epoch 毫秒>`（毫秒是为了同秒不撞名，
/// 见 timestamp）；`Sequence` = 扫产物目录里已有的 `SnapPin_*` 取最大序号 +1，补零 4 位
/// （`SnapPin_0007.png`）。三种产物共用一个序号空间，序号不会互相撞车（见 `next_sequence`）。
pub(crate) fn product_stem(app: &AppHandle, kind: &str) -> String {
    let fmt = app
        .try_state::<ConfigState>()
        .and_then(|s| s.0.lock().ok().map(|c| c.save_name_format))
        .unwrap_or_default();
    match fmt {
        NameFormat::Timestamp => format!("SnapPin_{}{}", kind, timestamp()),
        NameFormat::Sequence => format!("SnapPin_{}{:04}", kind, next_sequence(&captures_dir(app))),
    }
}

/// 扫 `dir` 里所有 `SnapPin_*` 文件，取「最后一段下划线后的数字」最大值 +1（没有则 1）。
///
/// 为什么认最后一段而不是固定位置：三种产物的中缀不同（`SnapPin_0001.png` /
/// `SnapPin_long_0001.png` / `SnapPin_0001.mp4`），都取尾段数字才能共用一个序号空间。
/// **只认 ≤9 位纯数字**：时间戳格式的产物名是 epoch 毫秒（13 位），混进来会让用户从
/// 「时间戳」切到「序号」时直接跳到十亿号。
fn next_sequence(dir: &Path) -> u32 {
    let mut max = 0u32;
    // 暂存目录（`.pending`）里的产物同样占序号空间：暂存件还没搬进最终目录，
    // 只扫最终目录会让第二个待确认截图拿到同一个序号。
    for d in [dir.to_path_buf(), dir.join(".pending")] {
        if let Ok(rd) = std::fs::read_dir(&d) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with("SnapPin_") {
                    continue;
                }
                let stem = name
                    .rsplit_once('.')
                    .map(|(s, _)| s)
                    .unwrap_or(name.as_str());
                if let Some((_, tail)) = stem.rsplit_once('_') {
                    if tail.len() <= 9 {
                        if let Ok(v) = tail.parse::<u32>() {
                            max = max.max(v);
                        }
                    }
                }
            }
        }
    }
    max + 1
}

/// 静态图产物的扩展名与编码格式，按 `config.save_format_png` 决定。
///
/// 谁在什么时机读它：`finish_capture` / `capture_long` 落盘前。true = PNG（无损，现有行为）；
/// false = JPEG（真编码，扩展名 .jpg）。
pub(crate) fn image_ext_and_format(app: &AppHandle) -> (&'static str, image::ImageFormat) {
    let png = app
        .try_state::<ConfigState>()
        .and_then(|s| s.0.lock().ok().map(|c| c.save_format_png))
        .unwrap_or(true);
    if png {
        ("png", image::ImageFormat::Png)
    } else {
        ("jpg", image::ImageFormat::Jpeg)
    }
}

/// OCR 出文字后按 `config.ocr_auto_copy` 决定是否写剪贴板。
///
/// 谁在什么时机读它：`ocr_image` / `ocr_region` 识别完成后立刻调用。
/// 关掉时**完全不碰剪贴板**（前端 Overlay 也不再自己复制，见 `doOcr`），开关才有可见差异。
/// 识别为空文本时不复制——否则会把用户原有的剪贴板内容清掉。
pub(crate) fn auto_copy_ocr(app: &AppHandle, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    let on = app
        .try_state::<ConfigState>()
        .and_then(|s| s.0.lock().ok().map(|c| c.ocr_auto_copy))
        .unwrap_or(true);
    if !on {
        return;
    }
    if let Err(e) = capture::copy_text_to_clipboard(text) {
        eprintln!("[snappin] OCR 结果自动复制失败: {e}");
    }
}

// ==================== 剪贴板历史（clip.rs 承担真实能力） ====================

/// 切换剪贴板历史面板：开（建窗/显示）↔ 关（隐藏）。
/// 热键与托盘共用。**建窗不在命令里同步做**（同 open_ball 的 about:blank 坑）：
/// 已存在就显示/隐藏切换；不存在则命令立即返回，建窗走后台线程 + run_on_main_thread。
#[tauri::command]
pub fn toggle_clip_history(app: AppHandle) -> Result<(), String> {
    if let Some(win) = app.get_webview_window("clip") {
        if win.is_visible().unwrap_or(false) {
            // 关闭路径（Esc/✕/热键再按）统一到这里：面板现在会激活，若关闭瞬间前台仍是
            // 我们的 clip 窗，就把焦点还给「打开面板前暂存的原输入程序」，否则用户原输入框
            // 一直失焦（2026-10-14 方案 a 的代价补偿）。用户中途点了别的程序（前台已不是
            // clip）则不抢回来，避免把焦点从用户正在用的窗夺走。
            if let Ok(h) = win.hwnd() {
                let clip_hwnd = h.0 as *mut core::ffi::c_void;
                let prev = CLIP_PREV_FOCUS.lock().ok().and_then(|g| *g);
                let prev_child = CLIP_PREV_FOCUS_CHILD.lock().ok().and_then(|g| *g);
                unsafe {
                    if GetForegroundWindow() == clip_hwnd {
                        if let Some(p) = prev {
                            let target = p as *mut core::ffi::c_void;
                            if !target.is_null() {
                                force_foreground(target);
                                // 顶层窗激活后把焦点交回**原控件**：插入符得回到用户原来那个
                                // 输入框，光标才不像「消失」（2026-10-20 用户实测 D13）。
                                if let Some(c) = prev_child {
                                    restore_focus_to_control(c);
                                }
                            }
                        }
                    }
                }
            }
            hide_clip_panel(&app);
        } else {
            // 显示前暂存当前前台窗与**焦点控件**（此刻是用户正在用的程序）：
            // 关闭/粘贴时把焦点还给它。
            let (prev, prev_child) = capture_focus_chain();
            if let Ok(mut g) = CLIP_PREV_FOCUS.lock() {
                *g = (prev != 0).then_some(prev);
            }
            if let Ok(mut g) = CLIP_PREV_FOCUS_CHILD.lock() {
                *g = (prev_child != 0).then_some(prev_child);
            }
            // **不抢焦**（2026-10-20 D13 修复，推翻 2026-10-14 的「面板建窗即激活」）：
            // 面板一激活，用户原输入框的插入符就消失、面板搜索框还把焦点抢走，
            // 实测「按 Win+V 后输入状态被破坏、粘贴落空」。
            // 面板确实要键盘输入时，由前端显式调 clip_activate_panel（解除 NOACTIVATE 并激活）。
            let _ = win.show();
            // 面板显示事件（P1 2026-09-24）：webview 只 hide 不销毁，页面拿不到 Tauri 的
            // show 回调，而 `visibilitychange` 在 Tauri hide/show 下不保证触发 —— 实测
            // 「重开面板仍用旧设置」（密度/失焦开关改了不生效）。显示后显式通知页面重拉设置。
            let _ = app.emit_to("clip", "clip-shown", ());
        }
        return Ok(());
    }
    // 建新窗路径：已在此刻（热键触发、用户程序在前台）暂存目标窗与焦点控件，
    // 供 clip_paste / 关闭还焦。
    {
        let (prev, prev_child) = capture_focus_chain();
        if let Ok(mut g) = CLIP_PREV_FOCUS.lock() {
            *g = (prev != 0).then_some(prev);
        }
        if let Ok(mut g) = CLIP_PREV_FOCUS_CHILD.lock() {
            *g = (prev_child != 0).then_some(prev_child);
        }
    }
    crate::clip::register_capture_app(app.clone());
    let pending = BuildPendingGuard::new();
    let app2 = app.clone();
    std::thread::spawn(move || {
        let app3 = app2.clone();
        if let Err(e) = app2.run_on_main_thread(move || {
            // 闭包持有它：窗口真的建出来（或建失败）才归零。
            let _pending = pending;
            if let Err(e) = build_clip_window(&app3) {
                eprintln!("[snappin] 剪贴板面板创建失败: {e}");
            }
        }) {
            eprintln!("[snappin] 剪贴板面板建窗排队失败: {e}");
        }
    });
    Ok(())
}

/// 建剪贴板历史面板窗（**必须在主线程调用**）。
fn build_clip_window(app: &AppHandle) -> Result<(), String> {
    if app.get_webview_window("clip").is_some() {
        return Ok(());
    }
    bump_window_epoch();
    // 定位：主屏（index 0）右上角靠边（贴 Win+V 用户习惯），CSS 尺寸 360x520。
    let (w, h) = (360.0_f64, 520.0_f64);
    let mon = crate::capture::monitors()
        .into_iter()
        .min_by_key(|m| m.index)
        .ok_or("没有可用显示器")?;
    let x = (mon.rect.x + mon.rect.w as i32) - w as i32 - 12;
    let y = mon.rect.y + 12;
    let _win = WebviewWindowBuilder::new(app, "clip", WebviewUrl::App("index.html".into()))
        .title("SnapPin 剪贴板")
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(true)
        .resizable(false)
        // **不抢焦**（2026-10-20 D13 修复，推翻 2026-10-14 的「建窗即激活」）：
        // 面板一激活，用户原输入框的插入符就消失（实测「按 Win+V 后输入状态被破坏」）。
        // 面板要键盘输入（↑↓/Enter、搜索框打字）时由前端显式调 clip_activate_panel。
        .focused(false)
        .position(x.max(0) as f64, y.max(0) as f64)
        .inner_size(w, h)
        // 参数必须与主窗/overlay/pin 完全一致（见 lib.rs::browser_args）。
        .additional_browser_args(crate::browser_args())
        .build()
        .map_err(|e| e.to_string())?;
    // 面板默认 WS_EX_NOACTIVATE：显示/点击都不激活它，前台与插入符留在用户原程序里。
    // 显式要键盘输入时由 clip_activate_panel 解除该样式（与 overlay 录制态的用法同一套）。
    if let Ok(h) = _win.hwnd() {
        set_no_activate(h.0 as *mut core::ffi::c_void, true);
    }
    Ok(())
}

/// 收起剪贴板面板，并重新武装「不抢焦」（下次热键打开不夺走用户输入框的插入符）。
fn hide_clip_panel(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("clip") {
        if let Ok(h) = win.hwnd() {
            set_no_activate(h.0 as *mut core::ffi::c_void, true);
        }
        let _ = win.hide();
    }
}

/// 显式激活剪贴板面板（前端在用户要跟面板交互时调：点搜索框 / 按 Ctrl+F / 点列表项）。
///
/// why-not 面板一直可激活：那样它一显示就抢走用户原输入框的插入符（2026-10-20 D13 实测）。
/// 这里把 NOACTIVATE 摘掉并把键盘焦点交给 WebView2 子窗口，之后面板才能收 ↑↓/Enter 与搜索词。
#[tauri::command]
pub fn clip_activate_panel(app: AppHandle) -> Result<(), String> {
    let win = app
        .get_webview_window("clip")
        .ok_or_else(|| "剪贴板面板未创建".to_string())?;
    let hwnd = win.hwnd().map_err(|e| e.to_string())?.0 as *mut core::ffi::c_void;
    // 先摘 NOACTIVATE，否则 SetForegroundWindow 激活不了它。
    set_no_activate(hwnd, false);
    unsafe {
        force_foreground(hwnd);
        // 键盘要落在 WebView2 的子窗口上，DOM 里的 input 才能真正收到按键。
        let child = GetWindow(hwnd, 5 /* GW_CHILD */);
        if !child.is_null() {
            SetFocus(child);
        }
    }
    Ok(())
}

/// 前端命令：读当前剪贴板历史（新→旧）。
#[tauri::command]
pub fn clip_list(state: tauri::State<crate::clip::ClipState>) -> Vec<serde_json::Value> {
    crate::clip::list_entries_with_thumbs(&state)
}

/// 前端命令：把指定条目写回系统剪贴板。
#[tauri::command]
pub fn clip_copy_back(app: AppHandle, id: u64) -> Result<(), String> {
    crate::clip::copy_back(&app, id)
}

/// 前端命令：删除一条历史。
#[tauri::command]
pub fn clip_remove(app: AppHandle, id: u64) -> Result<(), String> {
    crate::clip::remove_entry(&app, id)
}

/// 前端命令：清空历史。
#[tauri::command]
pub fn clip_clear(app: AppHandle) -> Result<(), String> {
    crate::clip::clear_all(&app)
}

/// 前端命令：切换某条的固定/收藏（P1）。返回切换后的 pinned 值供前端即时反映。
#[tauri::command]
pub fn clip_toggle_pin(app: AppHandle, id: u64) -> Result<bool, String> {
    crate::clip::toggle_pin(&app, id)
}

/// 前端命令：导出历史到目录 dest（P1 #11）。dest 由前端 pick_save_path(空名=选目录)取得。
#[tauri::command]
pub fn clip_export(app: AppHandle, dest: String) -> Result<(), String> {
    crate::clip::export_history(&app, &dest)
}

/// 前端命令：自动粘贴（P1 #5）——写回剪贴板 → 焦点还给目标窗并核对 → 合成 Ctrl+V → 按设置收起面板。
///
/// 顺序即全部（2026-10-20 D13 修复后重写）：
/// ①先写回剪贴板（即便后续还焦失败，内容也已进剪贴板，用户可手动 Ctrl+V 兜底）；
/// ②确定目标窗——前台是面板自己（面板被显式激活过）时退回打开面板前暂存的 CLIP_PREV_FOCUS，
///   若前台已是别的程序（用户中途切走）就用当前前台，别硬把焦点抢回旧目标；
/// ③SetForegroundWindow(target) 并核对 `GetForegroundWindow()==target`（最多 3 次，附加输入
///   线程兜底见 force_foreground）；④**核对成功才**把焦点交回原控件（SetFocus）并发 Ctrl+V，
///   失败不静默（打日志）；⑤最后才按 clip_close_after_paste 决定收不收面板。
#[tauri::command]
pub fn clip_paste(app: AppHandle, id: u64) -> Result<(), String> {
    crate::clip::copy_back(&app, id)?;
    let prev = CLIP_PREV_FOCUS.lock().ok().and_then(|g| *g);
    let prev_child = CLIP_PREV_FOCUS_CHILD.lock().ok().and_then(|g| *g);
    unsafe {
        let fg = GetForegroundWindow();
        let clip_hwnd = app
            .get_webview_window("clip")
            .and_then(|w| w.hwnd().ok())
            .map(|h| h.0 as *mut core::ffi::c_void);
        let fg_is_clip = clip_hwnd.map_or(false, |c| c == fg);
        // 目标窗：前台不是面板自己就用当前前台；是面板自己（面板被显式激活过）就退回暂存窗。
        let target = if !fg.is_null() && !fg_is_clip {
            fg
        } else {
            prev.map(|p| p as *mut core::ffi::c_void)
                .unwrap_or(std::ptr::null_mut())
        };
        // 还焦 + 核对：只有核对成功才发键，绝不把 Ctrl+V 打到面板自己或未知窗口。
        let mut landed = false;
        if !target.is_null() {
            force_foreground(target);
            for _ in 0..3 {
                if GetForegroundWindow() == target {
                    landed = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(30));
                force_foreground(target);
            }
        }
        if landed {
            // 顶层窗激活 ≠ 原输入框拿回插入符：把焦点交回打开面板前持有焦点的那个控件，
            // 否则「粘贴」落在没有控件接收的窗口里（2026-10-20 用户实测 D13「粘贴落空」）。
            if let Some(c) = prev_child {
                if !restore_focus_to_control(c) {
                    eprintln!("[snappin] clip_paste 原控件还焦失败 child=0x{:x}", c);
                }
            }
            send_ctrl_v();
        } else {
            eprintln!(
                "[snappin] clip_paste 还焦失败 fg=0x{:x} target=0x{:x} prev=0x{:x}",
                GetForegroundWindow() as isize,
                target as isize,
                prev.unwrap_or(0)
            );
        }
    }
    // 最后一步：按设置决定是否收起面板（默认收起）。
    // why-not：2026-09-30 曾写死「不自动关，留面板供连续粘贴」，被用户 2026-10-14 实报
    // 推翻（「面板不关」本身就是故障）→ 改为可配置，默认仍关闭；false 时才保留面板供连续粘贴。
    // 读锁在 and_then 内取值后即刻释放，hide 在锁外做（避免持锁期间再取同一把锁挂死）。
    let close_after_paste = app
        .try_state::<ConfigState>()
        .and_then(|s| s.0.lock().ok().map(|c| c.clip_close_after_paste))
        .unwrap_or(true);
    if close_after_paste {
        hide_clip_panel(&app);
    }
    Ok(())
}
