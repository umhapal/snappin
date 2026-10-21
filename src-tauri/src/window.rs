//! 智能窗口检测：枚举顶层可见窗口，返回物理像素边界（含 DWM 扩展框）。
//! 供 overlay 悬停高亮 / 点击选窗使用。z 序前置（前台）优先。
//!
//! 契约（Phase-A 定，WindowDetect 子代理填实现，不得改签名）：
//! - 返回物理像素坐标（与 capture 同坐标系，含多屏可负）。
//! - 过滤不可见、DWM cloaked、零尺寸、无标题的工具窗；按 z 序（EnumWindows 顺序）返回，
//!   前台窗在前，overlay 侧命中测试取第一个包含光标的窗口即可。
//! - 边界用 DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)，回退 GetWindowRect。

use std::ffi::c_void;

use serde::Serialize;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

/// 顶层窗口的物理像素边界。
#[derive(Debug, Clone, Serialize)]
pub struct WindowRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub title: String,
}

/// 枚举当前可见顶层窗口，z 序前置优先。
///
/// `EnumWindows` 的回调顺序即当前 z 序（前台窗在前），直接保序收集即可。
pub fn list_windows() -> Vec<WindowRect> {
    let mut out: Vec<WindowRect> = Vec::new();
    unsafe {
        // 把 &mut Vec 的裸指针经 LPARAM 传进回调；EnumWindows 同步执行，指针在调用期间有效。
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<WindowRect> as isize),
        );
    }
    out
}

/// EnumWindows 回调：每个顶层窗口调一次，命中过滤条件的收集进 out。
/// 返回 TRUE 继续枚举（返回 FALSE 会提前中断）。
extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let out = &mut *(lparam.0 as *mut Vec<WindowRect>);
        if let Some(rect) = describe_window(hwnd) {
            out.push(rect);
        }
    }
    BOOL(1)
}

/// 取屏幕物理坐标 (x,y) 处的顶层窗口句柄（z 序第一个命中，已排除本进程窗口）。
///
/// 给 UIA 的「先定窗口、再在 UIA 树里下钻」路线用：overlay 全屏置顶时
/// `ElementFromPoint` 只能看到 overlay 自己，必须先自己按 z 序定出下层窗口。
pub fn window_handle_at(x: i32, y: i32) -> Option<HWND> {
    let mut hit: Option<HWND> = None;
    let mut arg: (i32, i32, *mut Option<HWND>) = (x, y, &mut hit);
    unsafe {
        let _ = EnumWindows(
            Some(enum_at),
            LPARAM(&mut arg as *mut (i32, i32, *mut Option<HWND>) as isize),
        );
    }
    hit
}

/// `window_handle_at` 的 EnumWindows 回调：第一个含光标的合格窗口即命中并停止枚举。
extern "system" fn enum_at(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let arg = &mut *(lparam.0 as *mut (i32, i32, *mut Option<HWND>));
        if let Some(r) = describe_window(hwnd) {
            if arg.0 >= r.x && arg.0 <= r.x + r.w && arg.1 >= r.y && arg.1 <= r.y + r.h {
                *arg.2 = Some(hwnd);
                return BOOL(0); // 找到 z 序最前的一个就停
            }
        }
    }
    BOOL(1)
}

/// 该窗口是否属于本进程（overlay/pin/悬浮球/主界面）。
///
/// 两个列表都要排除：overlay 全屏置顶且 z 序第一，不过滤它「点击选窗」只会命中它自己。
pub fn current_process_owns(hwnd: HWND) -> bool {
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid != 0 && pid == unsafe { GetCurrentProcessId() }
}

/// 窗口类名；读不到返回空串（调用方自己决定空串算不算不合格）。
pub fn class_name(hwnd: HWND) -> String {
    let mut cls = [0u16; 256];
    let n = unsafe { GetClassNameW(hwnd, &mut cls) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&cls[..n as usize])
}

/// 窗口标题；读不到返回空串。
pub fn window_title(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..copied as usize])
}

/// WS_EX_TOOLWINDOW 工具窗（不在任务栏、通常是辅助窗）。
pub fn is_tool_window(hwnd: HWND) -> bool {
    let exstyle = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    exstyle & WS_EX_TOOLWINDOW.0 != 0
}

/// DWM cloaked（被合成器隐藏的窗口，如切走的 UWP）。
pub fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .is_ok()
    };
    ok && cloaked != 0
}

/// 桌面宿主窗口（Progman/WorkerW）：命中它们等于「选全屏」。
pub fn is_desktop_host(hwnd: HWND) -> bool {
    matches!(class_name(hwnd).as_str(), "Progman" | "WorkerW")
}

/// 单个窗口的物理像素边界：优先 DWM 扩展框（去掉不可见阴影边），失败回退 `GetWindowRect`。
///
/// overlay 命中测试列表与 agent 目标模型列表共用这一份算法：同一窗口在两处给出不同的
/// x/y/w/h 会让「列表里看到的位置」与「实际抓到的区域」对不上。
pub fn extended_frame_bounds(hwnd: HWND) -> Option<crate::capture::Bounds> {
    let mut rect = RECT::default();
    let got_dwm = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_ok()
    };
    if !got_dwm && unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(crate::capture::Bounds {
        x: rect.left,
        y: rect.top,
        w,
        h,
    })
}

/// 判定单个窗口是否应纳入并算出物理像素边界；不合格返回 None。
///
/// 过滤顺序：不可见 → 工具窗 → 桌面窗（Progman/WorkerW）→ DWM cloaked（UWP 后台窗）→ 无标题 → 零尺寸。
unsafe fn describe_window(hwnd: HWND) -> Option<WindowRect> {
    // 本进程窗口（overlay/pin）排除：overlay 全屏置顶且 z 序第一，
    // 不过滤它会让「点击选窗」永远命中 overlay 自己 → 全屏（2026-09-21 CDP 实测定案）。
    if current_process_owns(hwnd) {
        return None;
    }
    // 不可见窗口丢弃。
    if !IsWindowVisible(hwnd).as_bool() {
        return None;
    }
    // WS_EX_TOOLWINDOW 工具窗（不在任务栏、通常是辅助窗）丢弃。
    if is_tool_window(hwnd) {
        return None;
    }
    // DWM cloaked（被合成器隐藏的窗口，如切走的 UWP）丢弃。
    if is_cloaked(hwnd) {
        return None;
    }
    // 桌面宿主窗口（Progman/WorkerW）丢弃：命中它们等于选全屏，
    // 智能检测不应把桌面当「窗口」（用户反馈「点击默认全屏」的根因之一）。
    if is_desktop_host(hwnd) {
        return None;
    }
    let title = window_title(hwnd);
    if title.trim().is_empty() {
        return None;
    }

    let b = extended_frame_bounds(hwnd)?;
    // -32000 系负坐标 = 最小化的隐藏工具窗（QQ/资源管理器等）：
    // title 非空、IsWindowVisible 为真，上面几道过滤都拦不住，
    // 会混进窗口列表干扰命中测试（0.13 问题②），这里按坐标硬拦。
    if b.x < -20000 || b.y < -20000 {
        return None;
    }

    Some(WindowRect {
        x: b.x,
        y: b.y,
        w: b.w,
        h: b.h,
        title,
    })
}
