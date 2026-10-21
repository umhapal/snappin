//! 长截图（滚动拼接）：在指定区域内滚动目标窗口，逐帧抓图并按重叠区去重纵向拼接。
//!
//! 契约（2026-10-14 改为参数化滚动 + 落定等待）：
//! - 把光标移到区域中心 → 循环：发滚轮 → **等画面落定**（连续两帧签名几乎一致）→
//!   与上一帧做顶部/底部重叠行匹配求位移 → 只追加新增行 → 下一步。
//!   终止条件：画面完全没动（到底/目标不吃滚轮，连续两次确认）或达 `max_frames`。
//! - 返回完整拼接后的 RgbaImage（物理像素）。稳健性优先：无法可靠去重时退化为
//!   固定重叠估计并在日志说明，不得静默产出错位图。

use crate::capture::{capture_region, Bounds};
use crate::config::AppConfig;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, SetCursorPos, SetForegroundWindow, WindowFromPoint, GA_ROOT, WHEEL_DELTA,
};

/// 落定轮询间隔（毫秒）：两次抓帧之间至少隔这么久才比较是否稳定。
const SETTLE_POLL_MS: u64 = 40;
/// 落定判定的最小等待（毫秒）。为什么不能等 0：滚轮事件投递后窗口可能还没开始动画，
/// 此时连续两帧本来就一样，会被误判成「已经稳住」→ 一步没滚就收工。
const SETTLE_MIN_MS: u64 = 120;
/// 两帧签名每采样点平均灰度差 ≤ 此值即视为「画面没变」。
const STABLE_AVG: u64 = 1;

/// 长截图滚动参数（用户可调，见 `config.rs` 的 `longshot_*` 字段）。
#[derive(Debug, Clone, Copy)]
pub struct ScrollOptions {
    /// 每步滚轮档数（1 档 = WHEEL_DELTA）。
    pub notches: u32,
    /// 每步等待上限（毫秒）：画面落定即提前进入下一步，超时才按上限硬等。
    pub settle_max_ms: u64,
    /// 滚动步数上限（防跑飞）。
    pub max_frames: u32,
}

impl Default for ScrollOptions {
    fn default() -> Self {
        Self {
            notches: 1,
            settle_max_ms: 400,
            max_frames: 60,
        }
    }
}

impl ScrollOptions {
    /// 从用户配置构造；越界值钳到合法区间（配置可能被手工改过或被 HTTP 写入）。
    pub fn from_config(cfg: &AppConfig, max_frames: u32) -> Self {
        Self {
            notches: cfg.longshot_scroll_notches.clamp(1, 5),
            settle_max_ms: cfg.longshot_settle_ms.clamp(100, 3000),
            max_frames,
        }
    }
}

/// 每行下采样灰度签名：按列步长采样 samples 个点，返回扁平数组（长度 h*samples）。
/// 用灰度而非全像素比对，既省算力又对轻微色差稳健。
fn row_signatures(raw: &[u8], w: usize, h: usize, col_step: usize, samples: usize) -> Vec<u8> {
    let mut sig = vec![0u8; h * samples];
    let rowbytes = w * 4;
    for y in 0..h {
        let base = y * rowbytes;
        for s in 0..samples {
            let x = (s * col_step).min(w - 1);
            let px = &raw[base + x * 4..base + x * 4 + 4];
            // 简易灰度：(r+g+b)/3，忽略 alpha（capture 恒为 255）。
            let g = ((px[0] as u16 + px[1] as u16 + px[2] as u16) / 3) as u8;
            sig[y * samples + s] = g;
        }
    }
    sig
}

/// 两个签名的每采样点平均灰度差（0-255，越小越像）。
fn sig_avg_diff(a: &[u8], b: &[u8]) -> u64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0;
    }
    let mut sum: u64 = 0;
    for i in 0..n {
        sum += (a[i] as i32 - b[i] as i32).unsigned_abs() as u64;
    }
    sum / n as u64
}

/// 用当前帧顶部 strip 行在上一帧中纵向滑动匹配，求最佳重叠位移。
///
/// 返回 (best_shift, avg_diff)：best_shift = 当前帧顶部对应上一帧的行号，
/// 也即“滚动后当前帧新增的底部行数”；avg_diff = 每采样点平均灰度差（0-255，越小越像）。
fn best_overlap(
    prev_sig: &[u8],
    cur_sig: &[u8],
    h: usize,
    samples: usize,
    strip: usize,
) -> (usize, u64) {
    let mut best_p = 0usize;
    let mut best_diff = u64::MAX;
    let max_p = h.saturating_sub(strip);
    for p in 0..=max_p {
        let mut diff: u64 = 0;
        for r in 0..strip {
            let a = &cur_sig[r * samples..r * samples + samples];
            let bb = &prev_sig[(p + r) * samples..(p + r) * samples + samples];
            for k in 0..samples {
                let d = a[k] as i32 - bb[k] as i32;
                diff += d.unsigned_abs() as u64;
            }
            // 剪枝：已超过当前最优就没必要继续这行。
            if diff >= best_diff {
                break;
            }
        }
        if diff < best_diff {
            best_diff = diff;
            best_p = p;
        }
    }
    let denom = (strip as u64 * samples as u64).max(1);
    (best_p, best_diff / denom)
}

/// 发鼠标滚轮下滚 notches 档（负 delta = 下滚）。
///
/// **滚轮投递给「焦点窗口」，不是光标所在窗口**（2026-10-15 实测坐实：记事本搬到
/// 虚拟屏后光标移进去发滚轮，内容三次抓帧 md5 全同——因为前台还是别的程序；
/// 显式 SetForegroundWindow 后立刻生效）。注释旧文写「投递到光标所在窗口」是错的。
/// 所以调用前必须先 `SetForegroundWindow(target_hwnd)` + SetCursorPos。
unsafe fn scroll_down(notches: u32, target: HWND) {
    // 置前放在每次滚动前而不是只做一次：长截图要跑几秒到几十秒，期间用户/别的程序
    // 可能把前台抢走，一旦焦点不在目标上，后续滚轮就全打空（表现为「滚了两下就到底」）。
    let _ = SetForegroundWindow(target);
    // 裸 SetForegroundWindow 常被前台锁忽略（本进程此刻并非前台）→ 补一次
    // AttachThreadInput 版置前，否则第一轮滚轮就全打空、直接判「到底」。
    crate::commands::force_foreground(target.0);
    let n = notches.clamp(1, 5) as i32;
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                // 负值 = 下滚；多档合并进一次事件（Windows 语义就是 n 档）。
                mouseData: (-(WHEEL_DELTA as i32) * n) as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    SendInput(&[input], core::mem::size_of::<INPUT>() as i32);
}

/// 反复抓帧直到画面落定（连续两帧几乎一致）或超过 `settle_max_ms`，返回最后那帧。
///
/// 为什么不能固定 sleep：Chromium/Electron 的平滑滚动是 200–400ms 的逐帧动画，
/// 固定 150ms 抓到的常是动画中途甚至滚动前的帧——重叠匹配把它算成位移 0（以为到底，
/// 只出一帧）或低置信度退化（固定 15% 重叠），产物就是「页面滚了但长图没跟上」
/// （用户 2026-10-14 实测报障）。
fn capture_settled(
    b: Bounds,
    w: usize,
    h: usize,
    col_step: usize,
    samples: usize,
    settle_max_ms: u64,
) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut last_raw = capture_region(b)?.into_raw();
    let mut last_sig = row_signatures(&last_raw, w, h, col_step, samples);
    let mut waited: u64 = 0;
    while waited < settle_max_ms {
        std::thread::sleep(std::time::Duration::from_millis(SETTLE_POLL_MS));
        waited += SETTLE_POLL_MS;
        if waited < SETTLE_MIN_MS {
            continue;
        }
        let raw = capture_region(b)?.into_raw();
        let sig = row_signatures(&raw, w, h, col_step, samples);
        let stable = sig_avg_diff(&last_sig, &sig) <= STABLE_AVG;
        last_raw = raw;
        last_sig = sig;
        if stable {
            break;
        }
    }
    Ok((last_raw, last_sig))
}

/// 在 bounds 区域内滚动拼接长截图。
pub fn capture_scrolling(b: Bounds, opts: ScrollOptions) -> Result<image::RgbaImage, String> {
    if b.w <= 0 || b.h <= 0 {
        return Err(format!("longshot 区域尺寸非法: w={} h={}", b.w, b.h));
    }
    let w = b.w as usize;
    let h = b.h as usize;
    let rowbytes = w * 4;

    // 采样参数：每行最多 64 个采样点；匹配锚定条 = 顶部 40 行（不足则用整帧高）。
    let samples = w.min(64).max(1);
    let col_step = (w / samples).max(1);
    let strip = h.min(40).max(1);

    // 匹配不可靠阈值：每采样点平均灰度差超过此值即判为低置信度。
    const LOW_CONF_AVG: u64 = 24;
    // 到底判定：位移 <= 约 1% 帧高（至少 2 行）视为几乎完全重叠。
    let bottom_eps = (h / 100).max(2);
    // 低置信度退化时用的固定重叠估计：帧高 15% 作为新增行数。
    let fallback_shift = (h * 15 / 100).max(1);

    // 光标移到区域中心，给目标窗口滚动焦点。
    let cx = b.x + b.w / 2;
    let cy = b.y + b.h / 2;
    unsafe {
        let _ = SetCursorPos(cx, cy);
    }
    std::thread::sleep(std::time::Duration::from_millis(80));

    // 滚轮只认**焦点窗口**（见 `scroll_down` 注释），所以必须先知道「用户框的是哪个窗」。
    // 用区域中心 `WindowFromPoint` 取根窗：比让前端传 hwnd 更省一层契约，且对
    // 「用户框住的是子控件」这种情况自动归到顶层窗（GA_ROOT）。
    let scroll_target: HWND = unsafe {
        let h = WindowFromPoint(POINT { x: cx, y: cy });
        if h.0.is_null() {
            HWND(std::ptr::null_mut())
        } else {
            GetAncestor(h, GA_ROOT)
        }
    };
    if scroll_target.0.is_null() {
        return Err(format!(
            "longshot: 区域中心 ({cx},{cy}) 下没有可滚动窗口（WindowFromPoint 返回空）"
        ));
    }

    // 抓首帧，整帧入累积缓冲；签名算完即把 raw 移动进 stitched，省一次复制。
    let first_raw = capture_region(b)?.into_raw();
    let mut prev_sig = row_signatures(&first_raw, w, h, col_step, samples);
    let mut stitched = first_raw;
    let mut acc_rows = h;
    // 连续「一步下去画面完全没变」的次数：这是到底（或目标不吃滚轮）的判据。
    // 为什么要连续两次：窗口可能只是还没开始动画，一次没动就收工会截出半截长图。
    let mut no_motion = 0u32;

    for frame_idx in 0..opts.max_frames {
        unsafe {
            scroll_down(opts.notches, scroll_target);
        }
        let (cur_raw, cur_sig) = capture_settled(b, w, h, col_step, samples, opts.settle_max_ms)?;

        if sig_avg_diff(&prev_sig, &cur_sig) <= STABLE_AVG {
            no_motion += 1;
            if no_motion >= 2 {
                break;
            }
            prev_sig = cur_sig;
            continue;
        }
        no_motion = 0;

        let (mut shift, avg) = best_overlap(&prev_sig, &cur_sig, h, samples, strip);

        if avg <= LOW_CONF_AVG {
            // 匹配可靠：位移接近 0 说明滚不动了（到底），停止。
            if shift <= bottom_eps {
                break;
            }
        } else {
            // 匹配不可靠：退化为固定重叠估计并告警，绝不静默产出错位图。
            eprintln!(
                "longshot: 第 {} 帧重叠匹配置信度低 (avg_diff={}, best_shift={})，退化为固定位移 {} 行",
                frame_idx + 1,
                avg,
                shift,
                fallback_shift
            );
            shift = fallback_shift;
        }

        if shift == 0 {
            // 完全无位移，避免死循环，视为到底。
            break;
        }
        let shift = shift.min(h);

        // shift = 当前帧新增的底部行数，追加这些行到累积缓冲。
        let start = (h - shift) * rowbytes;
        stitched.extend_from_slice(&cur_raw[start..]);
        acc_rows += shift;

        prev_sig = cur_sig;
    }

    image::RgbaImage::from_raw(w as u32, acc_rows as u32, stitched)
        .ok_or_else(|| "longshot: RgbaImage::from_raw 失败，缓冲长度与尺寸不符".to_string())
}
