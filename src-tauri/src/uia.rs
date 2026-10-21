//! UIA 智能识别：对屏幕坐标 (x,y) 走 UI Automation 控件树，返回光标处元素
//! **及其祖先链**（内→外）的物理像素边界，供 overlay 的「智能识别」按层级切换
//! （PixPin 同款：滚轮放大到父容器，Tab 在元素/窗口两种模式间切）。
//! 未命中时调用方回退到顶层窗口命中（window.rs::list_windows）。
//!
//! 线程模型（2026-09-21 改）：UIA 是 COM 组件，需要每线程 CoInitializeEx。
//! 旧实现**每次查询**都新建线程 + CoCreateInstance(CUIAutomation8) + 每次只取
//! 最深层一个元素，单次开销数毫秒，只能靠前端 150ms 节流掩盖（手感发滞），
//! 且没有层级可用。现在改成**一条常驻工作线程 + 复用 IUIAutomation**，一次
//! 往返拿回整条祖先链，前端每帧查询也不会卡。
//!
//! 超时保护：UIA 在无响应窗体上 ElementFromPoint 会阻塞很久，
//! 故调用方 recv_timeout 等待，超时即返回空链（前端回退顶层窗口命中）。
//! 超时会把常驻线程标记为「可能卡死」，下一次查询重建线程（旧线程自行退出）。

use serde::Serialize;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, DeleteObject, SetWindowRgn, HRGN, RGN_DIFF,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Accessibility::{
    CUIAutomation8, IUIAutomation, IUIAutomationElement, TreeScope_Children,
    UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_CustomControlTypeId, UIA_DataItemControlTypeId, UIA_DocumentControlTypeId,
    UIA_EditControlTypeId, UIA_GroupControlTypeId, UIA_HeaderControlTypeId,
    UIA_HeaderItemControlTypeId, UIA_HyperlinkControlTypeId, UIA_ImageControlTypeId,
    UIA_ListControlTypeId, UIA_ListItemControlTypeId, UIA_MenuItemControlTypeId,
    UIA_PaneControlTypeId, UIA_RadioButtonControlTypeId, UIA_ScrollBarControlTypeId,
    UIA_SliderControlTypeId, UIA_TabControlTypeId, UIA_TabItemControlTypeId,
    UIA_TableControlTypeId, UIA_TextControlTypeId, UIA_TitleBarControlTypeId,
    UIA_ToolBarControlTypeId, UIA_TreeControlTypeId, UIA_TreeItemControlTypeId,
    UIA_WindowControlTypeId, UIA_CONTROLTYPE_ID,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON, VK_RBUTTON};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
    SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
};

/// 单次查询的等待上限。首个查询要等常驻线程建 COM + CUIAutomation（冷启动实测
/// >200ms，实测踩过：200ms 会让每次查询都超时→重建线程→永远查不出东西），
/// 所以给足；稳态查询实测 ~1-3ms。
const QUERY_TIMEOUT: Duration = Duration::from_millis(400);

/// 连续超时到这个次数才判定线程卡死并重建（单次超时通常只是冷启动）。
const MAX_CONSECUTIVE_TIMEOUTS: u32 = 3;

/// 祖先链最多上溯层数（防病态控件树无限上溯）。
const MAX_DEPTH: usize = 12;

/// 元素 Name 裁剪长度（悬停标签用，别把整篇文档标题塞进 UI）。
const NAME_MAX: usize = 48;

/// 诊断开关：设了 `SNAPPIN_UIA_DEBUG` 就把每次查询的走向打到 stderr。
/// 智能识别出问题时，先看这行——UIA 失败是静默的，没有它只能靠猜。
fn dbg(msg: &str) {
    if std::env::var_os("SNAPPIN_UIA_DEBUG").is_some() {
        eprintln!("[snappin][uia] {msg}");
    }
}

/// UI 元素的物理像素边界（与 capture / window.rs 同坐标系，多屏可为负）。
#[derive(Debug, Clone, Serialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 祖先链上的一个元素：几何 + 可读标签（供前端显示「按钮 1/3 · 确定」）。
#[derive(Debug, Clone, Serialize)]
pub struct ElementInfo {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// 控件类型的中文短名（按钮/编辑框/…）；未知类型为空串。
    pub kind: String,
    /// 元素 Name（窗口标题 / 按钮文字）；可能为空。
    pub name: String,
}

/// 取屏幕坐标 (x,y) 处 UI 元素的矩形（= 祖先链最内层）。
/// 保留给 HTTP `/v1/uia` 用；overlay 走 `element_chain_at` 拿整条链。
pub fn element_rect_at(x: i32, y: i32) -> Option<Rect> {
    element_chain_at(x, y).into_iter().next().map(|e| Rect {
        x: e.x,
        y: e.y,
        w: e.w,
        h: e.h,
    })
}

/// 取屏幕坐标 (x,y) 处元素的祖先链（内→外）。
/// 空数组 = 未命中/超时（前端回退顶层窗口命中）。
pub fn element_chain_at(x: i32, y: i32) -> Vec<ElementInfo> {
    // 两次尝试：第一次可能撞上已卡死的工作线程，重建后再试一次。
    for attempt in 0..2 {
        let Some(tx) = worker_sender() else {
            return Vec::new();
        };
        let (rtx, rrx) = mpsc::channel();
        if tx.send(Job { x, y, reply: rtx }).is_err() {
            // 工作线程已退出：清空句柄，下一轮重建。
            drop_worker();
            if attempt == 0 {
                continue;
            }
            return Vec::new();
        }
        match rrx.recv_timeout(QUERY_TIMEOUT) {
            Ok(chain) => {
                CONSECUTIVE_TIMEOUTS.store(0, Ordering::Relaxed);
                return chain;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let n = CONSECUTIVE_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
                dbg(&format!("查询超时（第 {n} 次）"));
                if n >= MAX_CONSECUTIVE_TIMEOUTS {
                    // 连续超时才判定线程卡在无响应窗体上：丢掉它（旧线程跑完自行退出）。
                    dbg("连续超时 → 重建工作线程");
                    CONSECUTIVE_TIMEOUTS.store(0, Ordering::Relaxed);
                    drop_worker();
                }
                return Vec::new();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                drop_worker();
                if attempt == 0 {
                    continue;
                }
                return Vec::new();
            }
        }
    }
    Vec::new()
}

struct Job {
    x: i32,
    y: i32,
    reply: mpsc::Sender<Vec<ElementInfo>>,
}

/// 常驻工作线程的发送端。None = 线程创建失败（系统线程耗尽）。
static WORKER: LazyLock<Mutex<Option<mpsc::Sender<Job>>>> = LazyLock::new(|| Mutex::new(None));

/// 连续超时计数：单次超时不清线程，连续多次才判定卡死。
static CONSECUTIVE_TIMEOUTS: AtomicU32 = AtomicU32::new(0);

/// 预热常驻 UIA 线程：app 启动时调一次，把 COM/CUIAutomation 的冷启动成本
/// 从「用户第一次悬停」挪到启动阶段（否则首个查询要吃 200ms+ 的建对象开销）。
pub fn warmup() {
    let _ = worker_sender();
}

fn worker_sender() -> Option<mpsc::Sender<Job>> {
    let mut guard = WORKER.lock().ok()?;
    if guard.is_none() {
        *guard = spawn_worker();
    }
    guard.clone()
}

/// 丢掉当前工作线程句柄（下次查询重建）。
fn drop_worker() {
    if let Ok(mut guard) = WORKER.lock() {
        *guard = None;
    }
}

/// 起一条常驻线程：CoInitializeEx(MTA) 一次 → 复用 IUIAutomation 处理所有查询。
fn spawn_worker() -> Option<mpsc::Sender<Job>> {
    let (tx, rx) = mpsc::channel::<Job>();
    std::thread::Builder::new()
        .name("snappin-uia".into())
        .spawn(move || unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: Option<IUIAutomation> =
                CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER).ok();
            dbg(&format!(
                "worker 启动，automation 可用={}",
                automation.is_some()
            ));
            while let Ok(job) = rx.recv() {
                let chain = match &automation {
                    Some(a) => query_chain(a, job.x, job.y),
                    None => Vec::new(),
                };
                // 调用方可能已超时走人，send 失败不影响线程继续服务后续请求。
                let _ = job.reply.send(chain);
            }
            CoUninitialize();
        })
        .ok()?;
    Some(tx)
}

/// 真正的 UIA 查询入口。
///
/// 命中测试穿透 + 兜底（2026-09-21 实测定案）：
/// overlay 是全屏置顶窗，UIA 的 `ElementFromPoint` 永远命中 overlay 自己，
/// 元素级识别在最需要它的时候完全失效。试过且**无效**的两条路：
/// ① 临时给窗口加 `WS_EX_TRANSPARENT`（UIA 不认这个样式，实测无效）；
/// ② 从顶层窗口往下枚举 UIA 子树（Chromium/WebView2 从窗口层枚举是空的）。
/// **有效的是开洞**：在 overlay 的窗口区域上、光标右下 1px 处开一个 1×1 的洞，
/// 命中测试就从洞里穿过去（实测同一点从 overlay 变成记事本编辑区 1928,78 784x492）。
/// 洞开不成（鼠标键按着 / 开洞失败）时退回「先按 z 序定窗口、再在 UIA 树里下钻」。
unsafe fn query_chain(automation: &IUIAutomation, x: i32, y: i32) -> Vec<ElementInfo> {
    let own = own_window_at(x, y);
    let punch = match own {
        Some(h) => punch_through(h, x, y),
        None => PunchThrough::none(),
    };
    // 开了洞就查洞的位置（右下 1px），否则查原坐标。
    let (px, py) = if punch.punched {
        (x + 1, y + 1)
    } else {
        (x, y)
    };
    if let Ok(el) = automation.ElementFromPoint(POINT { x: px, y: py }) {
        if punch.punched || own.is_none() {
            let v = chain_upward(automation, el);
            dbg(&format!(
                "FromPoint({px},{py}) punched={} -> {} 层",
                punch.punched,
                v.len()
            ));
            if !v.is_empty() {
                return v;
            }
        }
    }
    let v = if own.is_some() {
        chain_via_window(automation, x, y)
    } else {
        Vec::new()
    };
    dbg(&format!("via_window -> {} 层", v.len()));
    v
}

/// 命中测试穿透现场：开过洞的窗口，`Drop` 时把窗口区域还原成整窗。
struct PunchThrough {
    punched: bool,
    hwnds: Vec<HWND>,
}

impl PunchThrough {
    fn none() -> Self {
        PunchThrough {
            punched: false,
            hwnds: Vec::new(),
        }
    }
}

impl Drop for PunchThrough {
    fn drop(&mut self) {
        for hwnd in self.hwnds.drain(..) {
            unsafe {
                // 传 NULL 区域 = 还原成整个窗口（SetWindowRgn 接管传入区域的所有权）。
                let _ = SetWindowRgn(hwnd, HRGN(std::ptr::null_mut()), BOOL(1));
            }
        }
    }
}

/// 在 `own` 窗口的区域上、光标右下 1px 处开一个 1×1 的洞。
///
/// 洞故意偏 1px：用户点击落在光标那个像素上，仍然命中 overlay；
/// 只有 UIA 的命中测试从洞里穿过去。
unsafe fn punch_through(own: HWND, x: i32, y: i32) -> PunchThrough {
    // 鼠标键按下时不开洞：那几毫秒里点击可能落到下层应用。
    if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 || GetAsyncKeyState(VK_RBUTTON.0 as i32) < 0 {
        return PunchThrough::none();
    }
    let mut wr = RECT::default();
    if GetWindowRect(own, &mut wr).is_err() {
        return PunchThrough::none();
    }
    let (hx, hy) = (x + 1, y + 1);
    if hx < wr.left || hx >= wr.right || hy < wr.top || hy >= wr.bottom {
        return PunchThrough::none();
    }
    let full = CreateRectRgn(0, 0, wr.right - wr.left, wr.bottom - wr.top);
    let hole = CreateRectRgn(hx - wr.left, hy - wr.top, hx - wr.left + 1, hy - wr.top + 1);
    CombineRgn(full, full, hole, RGN_DIFF);
    let _ = DeleteObject(hole);
    if SetWindowRgn(own, full, BOOL(1)) == 0 {
        // 失败时区域句柄所有权不转移 → 自己删掉，别泄漏 GDI 对象。
        let _ = DeleteObject(full);
        return PunchThrough::none();
    }
    PunchThrough {
        punched: true,
        hwnds: vec![own],
    }
}

/// 光标 (x,y) 下压着的**本进程**可见顶层窗口（overlay / pin）。
unsafe fn own_window_at(x: i32, y: i32) -> Option<HWND> {
    let mut hit: Option<HWND> = None;
    let mut arg: (i32, i32, *mut Option<HWND>) = (x, y, &mut hit);
    let _ = EnumWindows(
        Some(enum_own_at),
        LPARAM(&mut arg as *mut (i32, i32, *mut Option<HWND>) as isize),
    );
    hit
}

/// `own_window_at` 的 EnumWindows 回调：第一个含光标的合格自窗口即命中并停止枚举。
unsafe extern "system" fn enum_own_at(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let arg = &mut *(lparam.0 as *mut (i32, i32, *mut Option<HWND>));
    let mut pid = 0u32;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != GetCurrentProcessId() || !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut r = RECT::default();
    if GetWindowRect(hwnd, &mut r).is_ok()
        && arg.0 >= r.left
        && arg.0 < r.right
        && arg.1 >= r.top
        && arg.1 < r.bottom
    {
        *arg.2 = Some(hwnd);
        return BOOL(0);
    }
    BOOL(1)
}

/// 从命中元素沿 ControlView 逐级上溯，收集每级矩形（内→外）。
///
/// 过滤规则：
/// - 本进程窗口（overlay / pin 自己）：整条链都不可用，直接停——命中它等于选中自己；
/// - 空矩形 / 面积 ≈ 整个虚拟屏的桌面根元素：跳过；
/// - 与上一级完全同矩形的重复层：跳过（父链里很常见）。
unsafe fn chain_upward(
    automation: &IUIAutomation,
    start: IUIAutomationElement,
) -> Vec<ElementInfo> {
    let mut out: Vec<ElementInfo> = Vec::new();
    let Ok(walker) = automation.ControlViewWalker() else {
        return out;
    };
    let area = virtual_area();
    let mut el = start;
    for _ in 0..MAX_DEPTH {
        push_if_usable(&mut out, &el, area);
        let Ok(parent) = walker.GetParentElement(&el) else {
            break;
        };
        // 父元素与自身相同（病态控件树）→ 停，别死循环。
        if automation
            .CompareElements(&el, &parent)
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            break;
        }
        el = parent;
    }
    out
}

/// 先按 z 序定出光标下的顶层窗口（复用 window.rs 的过滤），再在它的 UIA 子树里
/// 逐层挑「包含光标的子元素中面积最小的那个」往下钻，得到元素级矩形。
/// 不依赖命中测试，所以 overlay 挡着也成立。
unsafe fn chain_via_window(automation: &IUIAutomation, x: i32, y: i32) -> Vec<ElementInfo> {
    let Some(hwnd) = crate::window::window_handle_at(x, y) else {
        dbg("via_window: 未找到光标下的顶层窗口");
        return Vec::new();
    };
    let Ok(root) = automation.ElementFromHandle(hwnd) else {
        dbg("via_window: ElementFromHandle 失败");
        return Vec::new();
    };
    let Ok(cond) = automation.CreateTrueCondition() else {
        return Vec::new();
    };
    let area = virtual_area();
    // 先「外→内」收集，最后 reverse 成「内→外」。
    let mut collected: Vec<ElementInfo> = Vec::new();
    push_if_usable(&mut collected, &root, area);
    let mut cur = root;
    for _ in 0..MAX_DEPTH {
        let Ok(kids) = cur.FindAll(TreeScope_Children, &cond) else {
            break;
        };
        let Ok(n) = kids.Length() else { break };
        // 同一层里可能多个子元素都含光标（父容器与它的子），取面积最小的 = 最内层。
        let mut best: Option<(IUIAutomationElement, ElementInfo, i64)> = None;
        for i in 0..n {
            let Ok(kid) = kids.GetElement(i) else {
                continue;
            };
            let Ok(rc) = kid.CurrentBoundingRectangle() else {
                continue;
            };
            let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
            if w <= 0 || h <= 0 || !rect_contains(&rc, x, y) {
                continue;
            }
            let a = w as i64 * h as i64;
            if best.as_ref().is_some_and(|b| b.2 <= a) {
                continue;
            }
            let info = ElementInfo {
                x: rc.left,
                y: rc.top,
                w,
                h,
                kind: kind_label(kid.CurrentControlType().ok()),
                name: name_of(&kid),
            };
            best = Some((kid, info, a));
        }
        let Some((kid, info, _)) = best else { break };
        // ≈整个虚拟屏的容器层不入链（否则滚轮层级里会混进「整块屏幕」）。
        if (info.w as i64) * (info.h as i64) * 100 <= area * 92 {
            collected.push(info);
        }
        cur = kid;
    }
    collected.reverse();
    collected
}

/// 入链判据（命中测试与树下钻共用）：矩形有效、不是桌面根元素、且不与链上
/// 最后一层完全重合。
unsafe fn push_if_usable(out: &mut Vec<ElementInfo>, el: &IUIAutomationElement, area: i64) {
    if is_desktop(el) {
        return;
    }
    let Ok(rc) = el.CurrentBoundingRectangle() else {
        return;
    };
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let dup = out
        .last()
        .is_some_and(|p| p.x == rc.left && p.y == rc.top && p.w == w && p.h == h);
    if w <= 0 || h <= 0 || dup || (w as i64) * (h as i64) * 100 > area * 92 {
        return;
    }
    out.push(ElementInfo {
        x: rc.left,
        y: rc.top,
        w,
        h,
        kind: kind_label(el.CurrentControlType().ok()),
        name: name_of(el),
    });
}

/// 桌面根元素（Progman / WorkerW / #32769）不该进识别链。
///
/// 为什么单独判：面积过滤用的是**整个虚拟屏**，单屏时桌面=100% 会被过滤掉，
/// 但多屏（或本机开着 VDD 虚拟屏）时桌面只占一部分 → 面积过滤放它过去，
/// 于是滚轮「放大到父容器」会一路放大到整块桌面（实测：第 3 级 = 桌面 1）。
/// 判据与 window.rs 的桌面窗过滤保持一致。
unsafe fn is_desktop(el: &IUIAutomationElement) -> bool {
    match el.CurrentClassName() {
        Ok(c) => {
            let s = c.to_string();
            s == "Progman" || s == "WorkerW" || s == "#32769"
        }
        Err(_) => false,
    }
}

/// 点是否落在矩形内（UIA 的 right/bottom 为开区间，这里取闭区间足够）。
fn rect_contains(rc: &RECT, x: i32, y: i32) -> bool {
    x >= rc.left && x <= rc.right && y >= rc.top && y <= rc.bottom
}

/// 控件类型 → 中文短名（只覆盖悬停时真正有用的类型，其余留空）。
/// 常量名沿用 windows crate 的 UIA_*ControlTypeId 命名，不符合 Rust 大写规范。
#[allow(non_upper_case_globals)]
fn kind_label(ct: Option<UIA_CONTROLTYPE_ID>) -> String {
    let Some(ct) = ct else {
        return String::new();
    };
    let s = match ct {
        UIA_ButtonControlTypeId => "按钮",
        UIA_CheckBoxControlTypeId => "复选框",
        UIA_ComboBoxControlTypeId => "下拉框",
        UIA_EditControlTypeId => "编辑框",
        UIA_HyperlinkControlTypeId => "超链接",
        UIA_ImageControlTypeId => "图片",
        UIA_ListControlTypeId => "列表",
        UIA_ListItemControlTypeId => "列表项",
        UIA_MenuItemControlTypeId => "菜单项",
        UIA_RadioButtonControlTypeId => "单选框",
        UIA_ScrollBarControlTypeId => "滚动条",
        UIA_SliderControlTypeId => "滑块",
        UIA_TabControlTypeId => "选项卡",
        UIA_TabItemControlTypeId => "选项卡项",
        UIA_TextControlTypeId => "文本",
        UIA_ToolBarControlTypeId => "工具栏",
        UIA_TreeControlTypeId => "树",
        UIA_TreeItemControlTypeId => "树项",
        UIA_GroupControlTypeId => "分组",
        UIA_DataItemControlTypeId => "数据项",
        UIA_DocumentControlTypeId => "文档",
        UIA_WindowControlTypeId => "窗口",
        UIA_PaneControlTypeId => "窗格",
        UIA_HeaderControlTypeId => "表头",
        UIA_HeaderItemControlTypeId => "表头项",
        UIA_TableControlTypeId => "表格",
        UIA_TitleBarControlTypeId => "标题栏",
        UIA_CustomControlTypeId => "自定义",
        _ => "",
    };
    s.to_string()
}

/// 元素 Name：去空白、截断到 NAME_MAX 个字符。
unsafe fn name_of(el: &IUIAutomationElement) -> String {
    let Ok(raw) = el.CurrentName() else {
        return String::new();
    };
    let s = raw.to_string();
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    s.chars().take(NAME_MAX).collect()
}

/// 整个虚拟屏的物理像素面积（过滤「矩形≈全屏」的桌面/根元素用）。
///
/// 必须用**虚拟屏**而不是主屏：overlay 的坐标系是虚拟屏，主屏面积会把
/// 「主屏上最大化的窗口」误判成全屏根元素而砍掉。
fn virtual_area() -> i64 {
    unsafe {
        GetSystemMetrics(SM_CXVIRTUALSCREEN) as i64 * GetSystemMetrics(SM_CYVIRTUALSCREEN) as i64
    }
}
