//! Agent 窗口目标模型（Agent 截图 Phase 1，合同 `docs/Agent截图录屏能力规划.md` §5）。
//!
//! 职责：给 agent 一条**稳定可复用**的窗口目标——`/v1/windows` 返回带 HWND/PID/类名/标题的快照，
//! 执行请求用快照里的 `window_id` 或 `hwnd`(+可选 pid/类名/标题) 重新解析并**重新校验**
//! （窗口销毁后句柄复用必须被拒绝），不合格目标一律结构化报错，绝不静默回退成全屏/区域
//! （合同 §11 关键决策）。
//!
//! 与 `window.rs::describe_window` 的分工：那边服务 overlay 命中测试（z 序、去工具窗），
//! 这边是 agent 目标契约（多带 HWND/PID/类名/最小化）。几何一律复用
//! `window::extended_frame_bounds`——同一窗口在两处算出不同 x/y/w/h 会让「列表位置」与
//! 「实际抓到区域」对不上。

use std::ffi::c_void;

use serde::Serialize;
use serde_json::Value;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible,
};

use crate::window;

/// 单个窗口的能力声明。Phase 1 只落地静态窗口截图；窗口录屏属 Phase 2，未实现前如实报 false
/// ——不虚报能力，否则调用方会按 `record_window=true` 发起录屏并拿到 404。
#[derive(Debug, Clone, Serialize)]
pub struct CaptureCapabilities {
    pub wgc_window: bool,
    pub gdi_screen: bool,
    pub record_window: bool,
}

/// `/v1/windows` 的单项：保留旧字段 x/y/w/h/title，新增 HWND 系与状态字段（合同 §5.1）。
#[derive(Debug, Clone, Serialize)]
pub struct WindowSummary {
    /// 稳定引用串 `hwnd:0x0000000000012345`。
    pub window_id: String,
    /// 裸十六进制句柄串。句柄一律走字符串：JSON number 是 f64，放不下 64 位句柄。
    pub hwnd: String,
    pub pid: u32,
    pub title: String,
    pub class_name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub visible: bool,
    pub minimized: bool,
    pub cloaked: bool,
    pub capture_capabilities: CaptureCapabilities,
}

/// 结构化错误：HTTP 状态 + 稳定错误码 + 中文说明（合同 §7.3）。
#[derive(Debug, Clone)]
pub struct TargetError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl TargetError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

/// 解析并校验通过的目标。
#[derive(Debug, Clone, Copy)]
pub struct Resolved {
    pub hwnd: HWND,
    pub pid: u32,
}

/// 句柄十六进制串（16 位补零，与合同 §5.1 示例同形）。
pub fn hwnd_hex(hwnd: HWND) -> String {
    format!("0x{:016X}", hwnd.0 as usize)
}

/// 该句柄当前所属进程 PID（窗口销毁/复用后句柄仍可能有效，PID 是唯一可比的轻量身份）。
fn hwnd_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// `window_id` = `hwnd:<16hex>:<pid>`。
///
/// 为什么把 pid 编进 ID（2026-10-15 审查发现 H1）：合同 §5.1 要求「窗口销毁后句柄复用必须
/// 被拒绝」，但 HWND 会被 OS 回收复用；只给裸句柄的单字段请求（合同 §5.2/§7.1 的
/// `{ "target": { "window_id": "hwnd:0x…" } }`）就没有任何可比的旧身份，复用必然漏检
/// —— 会抓到另一个窗口并返回成功。把 PID 随 ID 一起下发，单字段也能校验。
pub fn window_id_of(hwnd: HWND) -> String {
    format!("hwnd:{}:{}", hwnd_hex(hwnd), hwnd_pid(hwnd))
}

/// 解析 `window_id` → (句柄, 随 ID 下发的 pid)。旧式纯 `hwnd:0x…` 返回 `None` 的 pid
/// （调用方必须另带 `pid`，否则 400 —— 不允许存在无身份校验的静默路径）。
fn parse_window_id(s: &str) -> Option<(isize, Option<u32>)> {
    let body = s.trim().strip_prefix("hwnd:")?.trim();
    let (hwnd_part, pid_part) = match body.split_once(':') {
        Some((h, p)) => (h.trim(), Some(p.trim())),
        None => (body, None),
    };
    let raw = parse_hwnd(hwnd_part)?;
    let pid = match pid_part {
        Some(p) if !p.is_empty() => Some(p.parse::<u32>().ok()?),
        _ => None,
    };
    Some((raw, pid))
}

/// 解析十六进制句柄串。接受 `0x...` 与 `hwnd:0x...` 两种写法，统一按 16 进制读。
/// 空串、超 16 位（>64bit）一律拒绝（合同 §7.3 `invalid_target`：数值溢出）。
fn parse_hwnd(s: &str) -> Option<isize> {
    let t = s.trim();
    let t = t.strip_prefix("hwnd:").unwrap_or(t).trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    if t.is_empty() || t.len() > 16 {
        return None;
    }
    isize::from_str_radix(t, 16).ok()
}

/// WGC 在本机是否可用（供 `/v1/windows` 如实上报能力）。
///
/// 为什么不能硬编码 true（2026-10-15 审查发现 L4）：缺 WGC 的系统（旧 Win10 版本、
/// 缺 D3D11 设备、远程会话）上恒真会误导调用方按「可截」发起请求，然后收到 422。
/// 探测失败按「不可用」上报——宁可少报，不可假报。
fn wgc_supported() -> bool {
    use std::sync::LazyLock;
    use windows_capture::graphics_capture_api::GraphicsCaptureApi;
    // 进程内缓存：系统能力在进程生命周期内不会变，而 `/v1/windows` 每个窗口都会读它
    // （几十次探测白烧）。
    static SUPPORTED: LazyLock<bool> =
        LazyLock::new(|| GraphicsCaptureApi::is_supported().unwrap_or(false));
    *SUPPORTED
}

/// 读单个窗口的快照（不做合格性过滤，调用方自行判断）。句柄无边界/非窗口返回 None。
pub fn summary(hwnd: HWND) -> Option<WindowSummary> {
    if hwnd.0.is_null() {
        return None;
    }
    let b = window::extended_frame_bounds(hwnd)?;
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    Some(WindowSummary {
        window_id: window_id_of(hwnd),
        hwnd: hwnd_hex(hwnd),
        pid,
        title: window::window_title(hwnd),
        class_name: window::class_name(hwnd),
        x: b.x,
        y: b.y,
        w: b.w,
        h: b.h,
        visible: unsafe { IsWindowVisible(hwnd) }.as_bool(),
        minimized: unsafe { IsIconic(hwnd) }.as_bool(),
        cloaked: window::is_cloaked(hwnd),
        capture_capabilities: CaptureCapabilities {
            wgc_window: wgc_supported(),
            gdi_screen: true,
            // Phase 2 已落地窗口录屏（window_recorder）：能力位按运行时探测如实上报。
            // why-not: 曾写死 false（当时未实现，虚报会让调用方拿到 404）；WGC 不可用的
            // 机器上仍然必须为 false，所以只能探测、不能写死 true。
            record_window: crate::window_recorder::is_supported(),
        },
    })
}

/// 枚举可捕获的顶层窗口（合同 §5.1：可见、非 cloaked、非桌面宿主、非本进程、非最小化、非零尺寸、有标题）。
pub fn list() -> Vec<WindowSummary> {
    let mut out: Vec<WindowSummary> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_proc),
            LPARAM(&mut out as *mut Vec<WindowSummary> as isize),
        );
    }
    out
}

extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let out = &mut *(lparam.0 as *mut Vec<WindowSummary>);
        if let Some(s) = candidate(hwnd) {
            out.push(s);
        }
    }
    BOOL(1)
}

/// 合格候选过滤。顺序对齐 `window.rs::describe_window`，另加「最小化丢弃」。
fn candidate(hwnd: HWND) -> Option<WindowSummary> {
    if window::current_process_owns(hwnd) {
        return None;
    }
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return None;
    }
    if window::is_tool_window(hwnd) {
        return None;
    }
    if window::is_cloaked(hwnd) {
        return None;
    }
    if window::is_desktop_host(hwnd) {
        return None;
    }
    // 最小化窗口默认不入候选（合同 §5.1）；即便列入，`IsIconic` 也保证 execute 侧会拒。
    // 桌面最小化窗口 DWM 扩展框是 (-32000,-32000)-(...)：下面 -20000 坐标闸再兜一层。
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return None;
    }
    let s = summary(hwnd)?;
    if s.title.trim().is_empty() {
        return None;
    }
    if s.x < -20000 || s.y < -20000 {
        return None;
    }
    Some(s)
}

/// 解析并校验捕获目标（合同 §5.3）。`target` 是请求体里的 `target` 对象。
///
/// 优先级：`window_id` → `hwnd`（可另带 pid/class_name/title 快照）。不接受标题模糊匹配。
pub fn resolve(target: &Value) -> Result<Resolved, TargetError> {
    let obj = match target.as_object() {
        Some(o) => o,
        None => {
            return Err(TargetError::new(
                400,
                "invalid_target",
                "缺少 target 或 target 不是对象（需 { window_id } 或 { hwnd, pid? }）",
            ))
        }
    };

    let (raw, id_pid) = if let Some(w) = obj.get("window_id") {
        let s = w
            .as_str()
            .ok_or_else(|| TargetError::new(400, "invalid_target", "window_id 必须是字符串"))?;
        if !s.starts_with("hwnd:") {
            return Err(TargetError::new(
                400,
                "invalid_target",
                "window_id 格式非法（必须以 hwnd: 开头）",
            ));
        }
        parse_window_id(s).ok_or_else(|| {
            TargetError::new(400, "invalid_target", format!("无法解析 window_id：{s}"))
        })?
    } else if let Some(h) = obj.get("hwnd") {
        let s = h
            .as_str()
            .ok_or_else(|| TargetError::new(400, "invalid_target", "hwnd 必须是十六进制字符串"))?;
        let raw = parse_hwnd(s)
            .ok_or_else(|| TargetError::new(400, "invalid_target", format!("无法解析句柄：{s}")))?;
        (raw, None)
    } else {
        return Err(TargetError::new(
            400,
            "invalid_target",
            "缺少 window_id 或 hwnd",
        ));
    };

    let hwnd = HWND(raw as *mut c_void);

    // 句柄仍是有效窗口（销毁/复用检测的第一道闸）。
    if !unsafe { IsWindow(hwnd) }.as_bool() {
        return Err(TargetError::new(
            404,
            "target_not_found",
            "目标窗口不存在或已销毁",
        ));
    }

    let cur_pid = hwnd_pid(hwnd);

    // **身份校验（必须成立，不允许静默跳过）**：请求里至少要有一个可比对的 pid —— 要么
    // 由 `window_id` 随 ID 下发，要么调用方显式给 `pid`。两者都没有时直接 400。
    //
    // 为什么不允许「都没有就放过」：HWND 会被 OS 回收复用，没有旧身份可比就等于没有校验，
    // 会抓到复用该句柄的另一个窗口并返回成功（合同 §5.1「句柄复用必须被拒绝」；
    // 2026-10-15 审查发现 H1）。
    let want_pid: Option<u32> = match id_pid {
        Some(p) => Some(p),
        None => match obj.get("pid") {
            Some(p) => {
                let n = p
                    .as_i64()
                    .ok_or_else(|| TargetError::new(400, "invalid_target", "pid 必须是整数"))?;
                if !(0..=u32::MAX as i64).contains(&n) {
                    return Err(TargetError::new(400, "invalid_target", "pid 超出 u32 范围"));
                }
                Some(n as u32)
            }
            None => None,
        },
    };
    let Some(want_pid) = want_pid else {
        return Err(TargetError::new(
            400,
            "invalid_target",
            "缺少身份校验：window_id 需带 pid（hwnd:0x…:<pid>），或显式提交 pid",
        ));
    };
    if want_pid != cur_pid {
        return Err(TargetError::new(
            409,
            "target_changed",
            format!("PID 不一致：请求 {want_pid}，当前 {cur_pid}"),
        ));
    }
    // 调用方显式给了 pid 且 window_id 也带了 pid → 两者必须一致（防自相矛盾请求）。
    if let (Some(id_p), Some(obj_p)) = (id_pid, obj.get("pid").and_then(|p| p.as_i64())) {
        if obj_p as u32 != id_p {
            return Err(TargetError::new(
                400,
                "invalid_target",
                format!("window_id 内 pid {id_p} 与请求 pid {obj_p} 不一致"),
            ));
        }
    }
    if let Some(c) = obj.get("class_name") {
        let want = c.as_str().unwrap_or_default();
        if window::class_name(hwnd) != want {
            return Err(TargetError::new(
                409,
                "target_changed",
                "窗口类名与快照不一致",
            ));
        }
    }
    if let Some(t) = obj.get("title") {
        let want = t.as_str().unwrap_or_default();
        if window::window_title(hwnd) != want {
            return Err(TargetError::new(
                409,
                "target_changed",
                "窗口标题与快照不一致",
            ));
        }
    }

    validate_capturable(hwnd)?;
    Ok(Resolved { hwnd, pid: cur_pid })
}

/// 拒绝不可捕获目标（合同 §5.3 `validate_capture_target`）。
/// 顺序：句柄失效 → 本进程 → 桌面宿主 → 不可见 → cloaked → 最小化 → 零尺寸。
pub fn validate_capturable(hwnd: HWND) -> Result<(), TargetError> {
    if !unsafe { IsWindow(hwnd) }.as_bool() {
        return Err(TargetError::new(
            404,
            "target_not_found",
            "目标窗口不存在或已销毁",
        ));
    }
    if window::current_process_owns(hwnd) {
        return Err(TargetError::new(
            400,
            "invalid_target",
            "目标是 SnapPin 自身窗口，不可捕获",
        ));
    }
    if window::is_desktop_host(hwnd) {
        return Err(TargetError::new(
            400,
            "invalid_target",
            "桌面宿主窗口不可捕获",
        ));
    }
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return Err(TargetError::new(400, "invalid_target", "目标窗口不可见"));
    }
    if window::is_cloaked(hwnd) {
        return Err(TargetError::new(
            400,
            "invalid_target",
            "目标窗口被 DWM 遮挡（cloaked），不可捕获",
        ));
    }
    if unsafe { IsIconic(hwnd) }.as_bool() {
        return Err(TargetError::new(
            409,
            "target_minimized",
            "目标窗口已最小化，默认不还原",
        ));
    }
    match window::extended_frame_bounds(hwnd) {
        Some(b) if b.w > 0 && b.h > 0 => Ok(()),
        _ => Err(TargetError::new(400, "invalid_target", "目标窗口尺寸无效")),
    }
}
