//! 贴图（Pin）：把一张 PNG 钉成置顶无边框小窗，可拖动/缩放/调透明度/关闭。
//! 窗口 label 形如 `pin-<n>`，前端 Pin.tsx 按 label 分流并拉取图像。
//!
//! 契约（Phase-A 定，PinModule 子代理填实现，不得改 create_pin/take_payload 签名）：
//! - create_pin：解码 base64 拿到物理像素宽高 → 建 `pin-<n>` 无边框置顶窗，
//!   位置 (x,y)（物理像素，overlay 传选区左上），内容尺寸按图像 CSS 像素（除以缩放）。
//!   把 base64 存进 PinState[label]，供 Pin.tsx 挂载后 take。
//! - Pin.tsx（PinModule 子代理写）：挂载 invoke("take_pin_payload",{label}) 取图铺满，
//!   窗口整体可 startDragging，滚轮缩放（set_size），右键/Esc 关闭，快捷键调透明度。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use tauri::{AppHandle, WebviewUrl, WebviewWindowBuilder};

/// 贴图窗口序号（全局递增，label 形如 `pin-<n>`）。
static PIN_SEQ: AtomicUsize = AtomicUsize::new(1);

/// 待取的贴图载荷：label -> png base64（Pin.tsx 挂载后一次性取走）。
#[derive(Default)]
pub struct PinState(pub Mutex<HashMap<String, String>>);

/// 建贴图窗口，返回其 label（`pin-<n>`）。
pub fn create_pin(
    app: &AppHandle,
    state: &PinState,
    png_base64: String,
    x: i32,
    y: i32,
) -> Result<String, String> {
    // 解 base64（兼容 data:URL 前缀），仅用于读取 PNG 尺寸，原串仍整体存入 state。
    let raw = png_base64
        .split_once(',')
        .map(|(_, b)| b)
        .unwrap_or(png_base64.as_str());
    let bytes = STANDARD.decode(raw.trim()).map_err(|e| e.to_string())?;
    // 物理像素宽高：直接解析 PNG IHDR，避免整图解码。
    let (phys_w, phys_h) = png_dimensions(&bytes)?;

    // 递增 label。
    let n = PIN_SEQ.fetch_add(1, Ordering::Relaxed);
    let label = format!("pin-{n}");

    // 存载荷，供 Pin.tsx 挂载后 take。
    state
        .0
        .lock()
        .map_err(|e| e.to_string())?
        .insert(label.clone(), png_base64);

    // 目标显示器缩放：把物理像素折算成 CSS/逻辑像素作为初始内容尺寸；
    // 拿不到就退回物理尺寸（缩放视为 1.0），前端 Pin.tsx 会按 naturalWidth/Height 再 set_size 校正。
    let scale = app
        .monitor_from_point(x as f64, y as f64)
        .ok()
        .flatten()
        .map(|m| m.scale_factor())
        .filter(|s| *s > 0.0)
        .unwrap_or(1.0);
    let css_w = (phys_w as f64 / scale).max(1.0);
    let css_h = (phys_h as f64 / scale).max(1.0);

    // 关键（2026-09-21 CDP 实测「点贴图后 pin 停在 about:blank、连 overlay 窗口都被搞坏」）：
    // 前端贴图链路是 `close_overlay()` → `pin_image()`，而 `close_overlay` **只发 hide 请求就
    // 返回**、悬浮球又是后台延迟恢复——pin 命令到达时，overlay 的 hide 还没被事件循环处理，
    // 球也还没建出来。此时同步建 pin 窗就是「已有 webview 在场还建窗」：实测 pin 停
    // `about:blank`，且 overlay 窗口被一起搞坏（之后按热键再也开不出 overlay）。
    //
    // 所以**永远走延迟路径**（不再按「球在不在」分流）：先发 hide overlay / 关球请求，
    // 命令**立即返回 label**，把建窗挪到后台线程，等桌面真的安静（overlay 已不可见且球已
    // 从窗口表消失）再 `run_on_main_thread` 回主线程建窗。label 与 payload 已经存进 state，
    // Pin.tsx 挂载后照样 take 得到。
    // **绝不在主线程 sleep 等窗口**：主线程一睡，hide/销毁这类窗口消息就没人派发
    // （见 commands.rs::wait_until_quiet 注释）。
    crate::commands::hide_overlay_if_visible(app);
    crate::commands::close_ball_now(app);
    // 从这一刻起「有建窗在路上」：延迟恢复悬浮球会让位，不会插进下面的等待里
    // （否则它把球建回来 → 等待超时 → 带着球建窗 → 正是要避开的那个坑）。
    let pending = crate::commands::BuildPendingGuard::new();
    let app2 = app.clone();
    let label2 = label.clone();
    std::thread::spawn(move || {
        crate::commands::wait_desktop_quiet(&app2, 3000);
        let app3 = app2.clone();
        if let Err(e) = app2.run_on_main_thread(move || {
            // 建窗闭包持有它：窗口真的建出来（或建失败）才归零。
            let _pending = pending;
            if let Err(e) = build_pin_window(&app3, &label2, x, y, css_w, css_h) {
                eprintln!("[snappin] 建贴图窗口失败: {e}");
            }
        }) {
            eprintln!("[snappin] 建贴图窗口排队失败: {e}");
        }
    });
    Ok(label)
}

/// 建贴图窗口（**必须在主线程调用**：Tauri/wry 要求）。
/// 调用前 overlay 必须已经真的不可见（由 `commands::wait_desktop_quiet` 保证）——
/// 全屏置顶窗还在时建第三个 webview 会把主线程堵死（前端 doPin 也是先 close_overlay 再
/// pin_image，这里做防御）。
fn build_pin_window(
    app: &AppHandle,
    label: &str,
    x: i32,
    y: i32,
    css_w: f64,
    css_h: f64,
) -> Result<(), String> {
    crate::commands::bump_window_epoch();

    WebviewWindowBuilder::new(app, label, WebviewUrl::App("index.html".into()))
        .title("SnapPin Pin")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .resizable(true)
        .position(x as f64, y as f64)
        .inner_size(css_w, css_h)
        // 参数必须与主窗/overlay 完全一致（见 lib.rs::browser_args）。
        .additional_browser_args(crate::browser_args())
        .build()
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 从 PNG 字节流解析物理像素宽高（读 8 字节签名后的 IHDR 块）。
fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    // PNG 签名(8) + 块长度(4) + "IHDR"(4) + 宽(4) + 高(4)，宽从偏移 16 起，高从 20 起。
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return Err("非法 PNG：无法解析 IHDR".into());
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    if w == 0 || h == 0 {
        return Err("PNG 尺寸为 0".into());
    }
    Ok((w, h))
}

/// Pin.tsx 挂载后取走对应 label 的图像 base64。
pub fn take_payload(state: &PinState, label: &str) -> Option<String> {
    state.0.lock().ok()?.remove(label)
}
