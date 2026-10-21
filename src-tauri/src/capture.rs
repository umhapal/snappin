//! 静态截图核心：GDI `BitBlt` 抓虚拟屏一帧 + 存 PNG + 复制剪贴板。
//! 无 UI 的纯能力，供 HTTP 端点直接调用，也供 overlay 抓冻结帧背景。
//!
//! 契约（Phase-A 定，CaptureRust 子代理填实现，不得改签名）：
//! 全部函数返回物理像素（需 Per-Monitor-V2 DPI 感知，见 lib.rs 启动处）。
//!
//! 实现参考 ShareX `CaptureRectangleNative`（docs/截图录屏技术分析.md §1.1）：
//! GetDC(desktop) → CreateCompatibleDC → CreateCompatibleBitmap → BitBlt(SRCCOPY|CAPTUREBLT)
//! → GetDIBits(top-down BGRA) → 转 RGBA。CAPTUREBLT 才能抓到分层/DWM 叠加窗口。

use std::borrow::Cow;
use std::ffi::c_void;
use std::path::Path;

use windows::Win32::Foundation::{GetLastError, HWND};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, ROP_CODE,
    SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// 屏幕矩形（物理像素，含多屏虚拟坐标，可为负）。
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// `GetDC(NULL)` 及一切 Win32 句柄需要的空窗口句柄。
#[inline]
fn null_hwnd() -> HWND {
    HWND(std::ptr::null_mut())
}

/// 取最近一次 Win32 错误码，用于失败信息。
#[inline]
fn last_err() -> u32 {
    unsafe { GetLastError().0 }
}

/// ShareX 的抓帧 ROP：SRCCOPY | CAPTUREBLT（CAPTUREBLT 抓分层/DWM 叠加窗口）。
#[inline]
fn capture_rop() -> ROP_CODE {
    ROP_CODE(SRCCOPY.0 | windows::Win32::Graphics::Gdi::CAPTUREBLT.0)
}

/// BGRA 内存序 → RGBA 内存序：交换 R/B，alpha 置 255（不透明）。
///
/// 两条抓帧路径的源内存序都是 `[B,G,R,A]`（GDI 的 `GetDIBits` 与 WGC 的 `Bgra8`），
/// 而 `image::RgbaImage` 要的是 `[R,G,B,A]`。曾经两条路径各写各的：GDI 记得交换、
/// WGC 忘了交换（2026-10-20 缺陷 D4：窗口截图橙色变蓝紫，R-B 差 -47.1）。
/// 收敛到这一个函数，让「再跑偏」不再可能。
#[inline]
fn bgra_to_rgba(bgra: [u8; 4]) -> [u8; 4] {
    [bgra[2], bgra[1], bgra[0], 255]
}

/// 整个虚拟屏（跨所有显示器）的物理边界。
pub fn virtual_bounds() -> Bounds {
    // 进程已声明 Per-Monitor-V2 DPI 感知，GetSystemMetrics 返回物理像素。
    unsafe {
        Bounds {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            w: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            h: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    }
}

/// overlay 覆盖的物理边界：默认整个虚拟屏（跨屏框选是产品行为）。
///
/// `SNAPPIN_OVERLAY_BOUNDS="x,y,w,h"` 可把它钉到某个矩形——这是**无头验证钩子**：
/// overlay 是全屏置顶窗，默认必然盖住用户的物理屏；把 bounds 指到虚拟显示器
/// （`tools/vdd/vdd.ps1 on`，默认 800x600 @ (1920,0)）就能在不打扰用户的前提下
/// 跑真实的渲染/交互断言。抓帧与建窗必须用同一份 bounds，否则背景图与坐标会错位。
pub fn overlay_bounds() -> Bounds {
    if let Ok(raw) = std::env::var("SNAPPIN_OVERLAY_BOUNDS") {
        let n: Vec<i32> = raw
            .split(',')
            .filter_map(|s| s.trim().parse().ok())
            .collect();
        if n.len() == 4 && n[2] > 0 && n[3] > 0 {
            return Bounds {
                x: n[0],
                y: n[1],
                w: n[2],
                h: n[3],
            };
        }
        eprintln!("[snappin] SNAPPIN_OVERLAY_BOUNDS 无效（需 \"x,y,w,h\" 四个整数）: {raw}");
    }
    virtual_bounds()
}

/// 一块显示器：`index` = 枚举序，**与 ddagrab 的 `output_idx` 同序**。
/// 2026-09-21 实测：本机 index 0 = 主屏 1920×1080、index 1 = 虚拟屏 800×600，
/// 与 `ddagrab=0/1` 的产物尺寸一一对应（`ddagrab=2` 直接报 Failed to enumerate）。
#[derive(Debug, Clone, Copy)]
pub struct Monitor {
    pub index: usize,
    pub rect: Bounds,
}

/// 枚举所有显示器（物理像素）。录屏要按「选区在哪块屏」挑 ddagrab 的 output_idx，
/// 否则 crop 的坐标基准（主屏原点）与选区所在屏不一致，裁出来是错的画面。
pub fn monitors() -> Vec<Monitor> {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };

    unsafe extern "system" fn cb(hmon: HMONITOR, _hdc: HDC, _rc: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<Monitor>);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            list.push(Monitor {
                index: list.len(),
                rect: Bounds {
                    x: mi.rcMonitor.left,
                    y: mi.rcMonitor.top,
                    w: mi.rcMonitor.right - mi.rcMonitor.left,
                    h: mi.rcMonitor.bottom - mi.rcMonitor.top,
                },
            });
        }
        BOOL(1) // 继续枚举
    }

    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&mut list as *mut Vec<Monitor> as isize),
        );
    }
    list
}

/// 选区落在哪块显示器上：取与之相交面积最大的那块（跨屏选区只录该屏上的部分）。
pub fn monitor_for_bounds(b: Bounds) -> Option<Monitor> {
    monitors()
        .into_iter()
        .filter_map(|m| {
            let x0 = b.x.max(m.rect.x);
            let y0 = b.y.max(m.rect.y);
            let x1 = (b.x + b.w).min(m.rect.x + m.rect.w);
            let y1 = (b.y + b.h).min(m.rect.y + m.rect.h);
            let area = (x1 - x0).max(0) as i64 * (y1 - y0).max(0) as i64;
            (area > 0).then_some((area, m))
        })
        .max_by_key(|(a, _)| *a)
        .map(|(_, m)| m)
}

/// 抓整个虚拟屏为 RGBA 图。
pub fn capture_virtual_screen() -> Result<image::RgbaImage, String> {
    capture_region(virtual_bounds())
}

/// 抓指定区域为 RGBA 图。
///
/// 全程成对释放 GDI 句柄；任一 Win32 调用失败返回带 GetLastError 的错误。
pub fn capture_region(b: Bounds) -> Result<image::RgbaImage, String> {
    if b.w <= 0 || b.h <= 0 {
        return Err(format!("capture_region: 非法尺寸 w={} h={}", b.w, b.h));
    }
    let (w, h) = (b.w, b.h);
    let null = null_hwnd();

    unsafe {
        // 屏幕 DC。
        let screen_dc = GetDC(null);
        if screen_dc.0.is_null() {
            return Err(format!("GetDC 失败: GetLastError={}", last_err()));
        }

        // 内存 DC。
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.0.is_null() {
            let e = last_err();
            ReleaseDC(null, screen_dc);
            return Err(format!("CreateCompatibleDC 失败: GetLastError={}", e));
        }

        // 与屏幕兼容的位图（DDB），承接 BitBlt 的像素。
        let bitmap = CreateCompatibleBitmap(screen_dc, w, h);
        if bitmap.0.is_null() {
            let e = last_err();
            let _ = DeleteDC(mem_dc);
            ReleaseDC(null, screen_dc);
            return Err(format!("CreateCompatibleBitmap 失败: GetLastError={}", e));
        }

        // 把位图选进内存 DC，保存旧对象以便还原。
        let old_obj = SelectObject(mem_dc, bitmap);

        // 从屏幕 DC 抓帧到内存 DC。
        let blt = BitBlt(mem_dc, 0, 0, w, h, screen_dc, b.x, b.y, capture_rop());
        let blt_err = if blt.is_err() { Some(last_err()) } else { None };

        // 以 top-down 32bpp BI_RGB 读出像素（GDI 存 BGRA）。
        // biHeight 取负 => 行序 top-down，避免上下颠倒。
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };

        let lines = if blt.is_ok() {
            GetDIBits(
                mem_dc,
                bitmap,
                0,
                h as u32,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut bmi,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };
        let dibits_err = if blt.is_ok() && lines == 0 {
            Some(last_err())
        } else {
            None
        };

        // 成对释放：还原旧对象 → 删位图 → 删内存 DC → 释放屏幕 DC。
        SelectObject(mem_dc, old_obj);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(null, screen_dc);

        if blt.is_err() {
            return Err(format!(
                "BitBlt 失败: GetLastError={}",
                blt_err.unwrap_or_default()
            ));
        }
        if lines == 0 {
            return Err(format!(
                "GetDIBits 失败: GetLastError={}",
                dibits_err.unwrap_or_default()
            ));
        }

        // GDI 给的是 BGRA，image::RgbaImage 要 RGBA：走共用的 bgra_to_rgba
        // （GDI alpha 位常为 0，统一置 255 不透明）。
        for px in buf.chunks_exact_mut(4) {
            let rgba = bgra_to_rgba([px[0], px[1], px[2], px[3]]);
            px.copy_from_slice(&rgba);
        }

        image::RgbaImage::from_raw(w as u32, h as u32, buf)
            .ok_or_else(|| "RgbaImage::from_raw 失败: 缓冲长度与尺寸不符".to_string())
    }
}

/// 存图到路径，编码格式由调用方给（PNG 无损 / JPEG 有损）。
///
/// 为什么格式由调用方给而不是让本函数猜：扩展名与**真实编码**必须一致
/// （`save_format_png=false` 时产物是 .jpg，里面就得是真 JPEG 字节，不能是改了后缀的 PNG）。
/// JPEG 走 `JpegEncoder::encode_image` 直接吃 `RgbaImage`：它按 `to_rgb()` 逐像素取色、
/// 丢弃 alpha（截图本来就不透明），既不整图拷贝（4K 图省几十 MB），也避开
/// `ImageEncoder::write_image`——那条路只收 L8/Rgb8，喂 Rgba8 会直接返回 Unsupported。
pub fn save_image(
    img: &image::RgbaImage,
    path: &Path,
    format: image::ImageFormat,
) -> Result<(), String> {
    if format == image::ImageFormat::Jpeg {
        let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        let mut w = std::io::BufWriter::new(file);
        // 画质取自设置（`cfg.jpeg_quality`，默认 92，40..100 由 config.rs clamp）：
        // 硬编码画质会让设置页的「画质」完全不起作用（2026-10-20 缺陷 D11）。
        // 截图以文字/UI 为主，质量调太低边缘振铃明显，这也是默认值偏高的原因。
        let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut w, jpeg_quality());
        enc.encode_image(img).map_err(|e| e.to_string())?;
        use std::io::Write;
        w.flush().map_err(|e| e.to_string())?;
        return Ok(());
    }
    if format == image::ImageFormat::Png && is_opaque(img) {
        // PNG 且 alpha 恒 255 → 裁掉 alpha 通道（原始体积省 25%，画面一个像素都不变）。
        // 只在「确认全不透明」时才裁：只要有一个半透明像素就必须保留 alpha，
        // 否则圆角/半透明区域会变成黑底（外观被改掉）。
        //
        // 走原始字节而不是逐像素 API：image 0.25 的 `Rgba` 字段是私有的，
        // 且 `ImageBuffer` 上没有 `to_rgb8`（那是 `DynamicImage` 的方法）。
        let (w, h) = (img.width(), img.height());
        let mut buf = Vec::with_capacity((w as usize) * (h as usize) * 3);
        for px in img.as_raw().as_slice().chunks_exact(4) {
            buf.push(px[0]);
            buf.push(px[1]);
            buf.push(px[2]);
        }
        let rgb = image::RgbImage::from_raw(w, h, buf)
            .ok_or_else(|| "RgbImage::from_raw 失败: 缓冲长度与尺寸不符".to_string())?;
        return rgb
            .save_with_format(path, format)
            .map_err(|e| e.to_string());
    }
    img.save_with_format(path, format)
        .map_err(|e| e.to_string())
}

/// 进程级 JPEG 画质（`cfg.jpeg_quality`）。
///
/// 为什么是进程级而不是 `save_image` 的参数：`save_image` 的调用方横跨 HTTP / 桌面命令 /
/// 长截图三条路径，后两条不在本文件里，加参数会把它们一起改坏；而「画质」本来就是
/// 用户设置，天然是进程级状态。写入点：`apply_config`（HTTP 启动时灌一次，
/// 每次保存前再从活的 `ConfigState` 同步一次，保证设置页改了立刻生效）。
static JPEG_QUALITY: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(92);

/// 当前生效的 JPEG 画质。
fn jpeg_quality() -> u8 {
    JPEG_QUALITY.load(std::sync::atomic::Ordering::Relaxed)
}

/// 把配置灌进进程级状态（当前只消费 `cfg.jpeg_quality`）。
pub fn apply_config(cfg: &crate::config::AppConfig) {
    JPEG_QUALITY.store(
        cfg.jpeg_quality.clamp(40, 100),
        std::sync::atomic::Ordering::Relaxed,
    );
}

/// 图是否完全不透明（所有像素 alpha = 255）。
///
/// 走原始字节而不是 `pixels()`：image 0.25 的 `Rgba` 字段是私有的，取不到 `.0`。
fn is_opaque(img: &image::RgbaImage) -> bool {
    img.as_raw()
        .as_slice()
        .chunks_exact(4)
        .all(|px| px[3] == 255)
}

/// 把文本写进系统剪贴板（OCR 自动复制用）。
pub fn copy_text_to_clipboard(text: &str) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard
        .set_text(text.to_string())
        .map_err(|e| e.to_string())
}

/// 把图复制到系统剪贴板（arboard 要 RGBA，正好匹配）。
pub fn copy_to_clipboard(img: &image::RgbaImage) -> Result<(), String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard
        .set_image(arboard::ImageData {
            width: img.width() as usize,
            height: img.height() as usize,
            bytes: Cow::from(img.as_raw().as_slice()),
        })
        .map_err(|e| e.to_string())
}

/// 从系统剪贴板读一张图（贴图热键 Ctrl+2 / 托盘「贴图」用）。
/// 剪贴板里没有位图（只有文本/文件）时返回 Err，调用方据此提示而不是静默什么都不做。
pub fn clipboard_image() -> Result<image::RgbaImage, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = clipboard
        .get_image()
        .map_err(|e| format!("剪贴板里没有图片：{e}"))?;
    let (w, h) = (img.width as u32, img.height as u32);
    image::RgbaImage::from_raw(w, h, img.bytes.into_owned())
        .ok_or_else(|| "剪贴板图片缓冲长度与尺寸不符".to_string())
}

/// RGBA 图 → PNG 字节（贴图命令收 base64 PNG）。
pub fn png_bytes(img: &image::RgbaImage) -> Result<Vec<u8>, String> {
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img.clone())
        .write_to(&mut buf, image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(buf.into_inner())
}

// ---- WGC 窗口单帧捕获（Agent 截图 Phase 1，合同 §6.1）----
//
// 主路径是 Windows Graphics Capture：目标被别的窗口完全遮挡时仍能拿到**窗口自身**内容
// （GDI BitBlt / PrintWindow 做不到；Phase 0 探针实测证实遮挡无关）。单帧允许一次
// GPU→CPU 回读，取完立刻关会话——不把这条回读路径留给未来的窗口录屏（合同 §11）。

use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window as WgcWindow;

/// WGC 捕获错误：稳定错误码 + 说明。HTTP 层按码映射状态（合同 §7.3）。
#[derive(Debug, Clone)]
pub struct CaptureError {
    pub code: &'static str,
    pub message: String,
}

impl CaptureError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// 单帧捕获：拿到第一帧就回读、发结果、停会话。
struct OneFrameCapture {
    sink: std::sync::mpsc::Sender<Result<image::RgbaImage, CaptureError>>,
    sent: bool,
}

impl GraphicsCaptureApiHandler for OneFrameCapture {
    type Flags = std::sync::mpsc::Sender<Result<image::RgbaImage, CaptureError>>;
    // Error 类型用不上（失败一律经 sink 回传），占位为 String 以满足 trait。
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self {
            sink: ctx.flags,
            sent: false,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if !self.sent {
            self.sent = true;
            let _ = self.sink.send(frame_to_rgba(frame));
        }
        capture_control.stop();
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        // 目标在捕获期间被关闭/销毁：如实报错，**绝不**回落成全屏或返回空文件（合同 §11）。
        if !self.sent {
            self.sent = true;
            let _ = self.sink.send(Err(CaptureError::new(
                "target_not_found",
                "目标窗口在捕获期间被关闭",
            )));
        }
        Ok(())
    }
}

/// WGC 帧 → 不透明 RGBA。
///
/// 两个必须自己处理的坑（Phase 0 探针实测）：
/// 1. `row_pitch` 由 D3D 决定、可能带 padding（宽度非 64 倍数时）：必须按行取，不能当 `width*4` 连续读。
/// 2. WGC 帧是**预乘 alpha**：直接落盘会得到半透明/透明角。这里反预乘后 alpha 置 255
///    （截图按不透明图消费）；alpha=0 的圆角像素反预乘无意义，置 0（黑）。
fn frame_to_rgba(frame: &mut Frame) -> Result<image::RgbaImage, CaptureError> {
    let mut fb = frame
        .buffer()
        .map_err(|e| CaptureError::new("capture_failed", format!("WGC 帧缓冲映射失败：{e}")))?;
    let w = fb.width();
    let h = fb.height();
    if w == 0 || h == 0 {
        return Err(CaptureError::new("capture_failed", "WGC 返回零尺寸帧"));
    }
    let pitch = fb.row_pitch() as usize;
    if pitch < (w as usize) * 4 {
        return Err(CaptureError::new(
            "capture_failed",
            "WGC 帧 row_pitch 小于行宽",
        ));
    }
    let raw = fb.as_raw_buffer();
    if raw.len() < pitch * h as usize {
        return Err(CaptureError::new(
            "capture_failed",
            "WGC 帧缓冲长度与尺寸不符",
        ));
    }

    let mut out = vec![0u8; (w as usize) * (h as usize) * 4];
    for y in 0..h as usize {
        let row = &raw[y * pitch..y * pitch + (w as usize) * 4];
        let dst = &mut out[y * (w as usize) * 4..(y + 1) * (w as usize) * 4];
        for x in 0..w as usize {
            // WGC 给的是**预乘** BGRA（Phase 0 探针实测）。截图产物语义 = 「不透明的一帧画面」，
            // 所以这里统一按「合成到黑底」出 straight 色：预乘值本身就是黑底上的合成结果，
            // 直接取用即自洽，不再对半透明像素做反预乘。
            //
            // 为什么 not 反预乘（`rgb*255/a`）+ 保留 alpha：那条路要么把 a>0 的半透明像素
            // 提亮（与 a==0 的黑角冲突，出现黑环+亮环），要么落 PNG 时让圆角/亚克力区
            // 透出下层——两种都不是截图该有的样子（2026-10-15 审查发现 M2 的两种混用约定）。
            // 代价（已知并接受）：半透明区域显示为黑底上的合成色，比原窗自身观感略暗。
            let a = row[x * 4 + 3] as u32;
            // WGC 的字节序是 `[B,G,R,A]`：第 0 字节是**蓝**、第 2 字节才是红。
            let (b, g, r) = if a == 0 {
                (0u8, 0u8, 0u8)
            } else {
                (row[x * 4], row[x * 4 + 1], row[x * 4 + 2])
            };
            // R/B 交换走共用助手（D4：这里曾经漏了交换，橙色变蓝紫，R-B 差 -47.1）。
            let rgba = bgra_to_rgba([b, g, r, 255]);
            dst[x * 4..x * 4 + 4].copy_from_slice(&rgba);
        }
    }
    image::RgbaImage::from_raw(w, h, out)
        .ok_or_else(|| CaptureError::new("capture_failed", "RgbaImage::from_raw 失败"))
}

/// 窗口抓取结果（D1 契约）：把「是否降级」写进结果本身。
///
/// 为什么要单独一个结构而不是裸 `RgbaImage`：调用方拿到一张图时**无法从图本身判断**
/// 它是不是目标窗口的忠实画面——缺陷 D1 的表现正是「抓到整屏，但响应里看不出异常」。
/// 结果里带 `degraded` + `degraded_reason`，降级就不可能再是静默的。
#[derive(Debug, Clone)]
pub struct WindowCapture {
    /// 抓到的画面（物理像素）。
    pub image: image::RgbaImage,
    /// 是否走了降级路径（拿到的不是目标窗口本身）。false = 目标窗口的忠实画面。
    pub degraded: bool,
    /// 降级原因（`degraded=false` 时恒为 `None`）。给人读的说明。
    pub degraded_reason: Option<String>,
}

/// 窗口抓取（带降级标记，D1）。
///
/// 当前实现**没有降级路径**：WGC 失败一律返回结构化错误（`capture_unavailable` /
/// `capture_failed`），绝不回落成全屏/区域抓取——合同 §11「不合格目标一律结构化报错，
/// 绝不静默回退成全屏/区域」。所以成功结果的 `degraded` 恒为 `false`。
/// 保留这两个字段是为了让「将来若引入降级」无法静默发生：降级必须同时给出
/// `degraded: true` 与 `degraded_reason`。
pub fn capture_window_marked(hwnd: isize) -> Result<WindowCapture, CaptureError> {
    let image = capture_window(hwnd)?;
    // 0×0 的帧（WGC 抓到空画面）当失败处理：空图落盘后调用方拿到的是
    // ok:true + 0×0，又是「看不出异常」的静默失败（与 D1 同一类问题）。
    if image.width() == 0 || image.height() == 0 {
        return Err(CaptureError::new(
            "capture_failed",
            "WGC 返回空帧（目标可能已最小化或不可见）",
        ));
    }
    Ok(WindowCapture {
        image,
        degraded: false,
        degraded_reason: None,
    })
}

/// 用 WGC 抓单个窗口的静默单帧（不显示 UI、不动鼠标、不抢前台，合同 §4 静默契约）。
///
/// `hwnd` 传裸句柄值（调用方给 `HWND.0 as isize`）：刻意不传 `HWND` 类型——`windows-capture`
/// 依赖 `windows` 0.62，本仓依赖 `windows` 0.58，两个 `HWND` 是不同类型，只有裸指针能跨过去。
pub fn capture_window(hwnd: isize) -> Result<image::RgbaImage, CaptureError> {
    use std::time::Duration;

    let (tx, rx) = std::sync::mpsc::channel::<Result<image::RgbaImage, CaptureError>>();
    let win = WgcWindow::from_raw_hwnd(hwnd as *mut c_void);
    let settings = Settings::new(
        win,
        CursorCaptureSettings::WithoutCursor,
        // 关掉 WGC 黄色捕获边框（合同 §4：IsBorderRequired=false）。
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Exclude,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        tx,
    );
    // start_free_threaded：把消息循环放到独立线程并回一个 CaptureControl，
    // 使调用线程（HTTP 处理线程，单线程串行）不被阻塞，且三种结局都能主动停会话。
    let control = OneFrameCapture::start_free_threaded(settings)
        .map_err(|e| CaptureError::new("capture_unavailable", format!("WGC 会话启动失败：{e}")))?;

    // 首帧实测 ~0.4–0.6s（Phase 0）。上限收到 **8s**：HTTP 面是**单线程串行**
    // （`http.rs` 的 `incoming_requests()` 循环），一次「永不送帧」的请求会把
    // `/v1/health` 与所有其它端点一起卡住 —— 20s 太长（2026-10-15 审查发现 M1）。
    let res = match rx.recv_timeout(Duration::from_secs(8)) {
        Ok(r) => r,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(CaptureError::new(
            "capture_failed",
            "WGC 在超时内未返回首帧（目标可能已最小化或不可捕获）",
        )),
        // **必须与超时区分**（2026-10-15 审查发现 L1）：`Disconnected` = 采集线程在
        // `start_free_threaded` 返回 Ok 之后自己失败并退出（消息循环失败、D3D 设备移除、
        // 收尾失败…）。报成「超时」会把真实原因藏起来，调用方按超时排查会走错方向。
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(CaptureError::new(
            "capture_unavailable",
            "WGC 采集线程在返回首帧前已退出（消息循环/设备丢失）",
        )),
    };
    // 无论成败都主动停会话，避免消息循环线程挂死。**失败必须可见**（合同 §10）：
    // stop() 的错误是 crate 唯一的收尾诊断（FailedToShutdownDispatcherQueue 等），
    // 不能像以前那样 `let _ =` 丢掉（2026-10-15 审查发现 L1）。
    if let Err(e) = control.stop() {
        eprintln!("[snappin] WGC 会话收尾失败：{e:?}");
    }
    res
}
