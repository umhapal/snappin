//! OCR：原生 Rust 引擎 **oar-ocr**（PP-OCRv5 mobile det+rec 中文模型，Apache-2.0，离线免 key）。
//! 推理走 ONNX Runtime，经 `ort` 的 `load-dynamic` 特性**运行时**动态加载 onnxruntime.dll。
//!
//! 为什么不是外挂 exe（PaddleOCR-json）：
//! - 想要单二进制、无子进程生命周期、模型可为最新一代（v5/v6）。
//! - 选型实测（2026-09-20）：本机仅 `x86_64-pc-windows-gnu`（无 MSVC），`ort-sys` 对该 target
//!   **无预编译二进制**（`no prebuilt binaries available`），默认 `download-binaries` 链接失败；
//!   改用 `ort` 的 `load-dynamic` 后运行时加载 DLL，链接期不需任何二进制，已验证编译通过。
//!
//! 契约（Phase-A 定，不得改 ocr_png 签名）：
//! - 引擎**惰性常驻单例**：首次调用构建 OAROCR 管线（加载模型），之后复用。
//! - box 为四点多边形（图像物理像素）；OcrLine 的 x/y/w/h 取其外接矩形。
//! - text 汇总：按 text_regions 顺序用换行拼成 OcrResult.text。
//! - lang 暂忽略（PP-OCRv5 默认中英混合模型即可）；保留参数供后续多语言模型切换。
//!
//! 运行时依赖（打包/首启需满足，缺失时返回明确错误，绝不静默返回空）：
//! - `onnxruntime.dll`：探测 `engine_dir()/onnxruntime.dll`，存在则设 `ORT_DYLIB_PATH`；
//!   否则交给 `ort` 走系统默认查找（exe 同目录 / PATH）。
//! - 模型文件：`OAR_HOME` 指到 `engine_dir()`；用 bare 名交给 `auto-download` 特性首次联网拉取
//!   到该目录（之后离线复用）。若已预置同名文件则直接用、不联网。

use oar_ocr::oarocr::{OAROCRBuilder, OAROCR};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

#[derive(Debug, Clone, Serialize)]
pub struct OcrLine {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OcrResult {
    pub text: String,
    pub lines: Vec<OcrLine>,
}

/// PP-OCRv5 **mobile** 模型与字典的 bare 名（走 oar-ocr auto-download / 本地同名文件优先）。
///
/// 档位取舍：mobile 档小(det 4.6M + rec 15.8M)、快、省内存，截图 OCR(字大清晰)完全够用，
/// 实测中文识别准确。若以后要认小字/模糊/密集表格，换 **server** 档：只改下面两个 bare 名
/// 为 `pp-ocrv5_server_det.onnx` / `pp-ocrv5_server_rec.onnx`(dict 不变)，auto-download
/// 会拉新模型、其余代码不动；代价是模型大好几倍、推理更慢更吃内存。
const DET_MODEL: &str = "pp-ocrv5_mobile_det.onnx";
const REC_MODEL: &str = "pp-ocrv5_mobile_rec.onnx";
const DICT_FILE: &str = "ppocrv5_dict.txt";

/// 引擎资源目录（onnxruntime.dll + 模型缓存）。
///
/// 顺序：环境变量 `SNAPPIN_OCR_ENGINE_DIR` → exe 同级 `engines/oar-ocr` → 工作目录同名。
/// why-not 写死某台开发机的绝对路径：换机器/换用户即失效，而这三档已覆盖常见装法。
fn engine_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("SNAPPIN_OCR_ENGINE_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    {
        return exe_dir.join("engines").join("oar-ocr");
    }
    PathBuf::from("engines").join("oar-ocr")
}

/// 惰性常驻单例（初始化器无运行时入参，用 LazyLock）：Ok=就绪，Err=初始化失败（记住错误避免重试卡顿）。
static ENGINE: LazyLock<Mutex<Result<OAROCR, String>>> =
    LazyLock::new(|| Mutex::new(build_engine()));

/// 构建 OCR 管线。设置 ORT_DYLIB_PATH / OAR_HOME 后用 PP-OCRv5 mobile 模型建 OAROCR。
fn build_engine() -> Result<OAROCR, String> {
    let dir = engine_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建引擎目录 {} 失败：{e}", dir.display()))?;

    // onnxruntime.dll：显式指向引擎目录内的 DLL；不存在则交给 ort 默认查找。
    let dll = dir.join("onnxruntime.dll");
    if dll.exists() {
        std::env::set_var("ORT_DYLIB_PATH", &dll);
    }
    // 模型缓存根：auto-download 会把 bare 名模型拉到这里，之后离线复用。
    std::env::set_var("OAR_HOME", &dir);

    OAROCRBuilder::new(DET_MODEL, REC_MODEL, DICT_FILE)
        .build()
        .map_err(|e| {
            format!(
                "oar-ocr 引擎初始化失败：{e}。请确认 {} 下有 onnxruntime.dll，且模型可下载或已预置（{DET_MODEL} / {REC_MODEL} / {DICT_FILE}）。",
                dir.display()
            )
        })
}

/// 识别一张 PNG 的文字。惰性初始化并复用引擎；识别在单例锁内串行（管线非并发安全前提下最稳）。
pub fn ocr_png(png_bytes: &[u8], _lang: Option<&str>) -> Result<OcrResult, String> {
    let guard = ENGINE.lock().map_err(|_| "OCR 引擎锁中毒".to_string())?;
    let engine = match guard.as_ref() {
        Ok(e) => e,
        Err(msg) => return Err(msg.clone()),
    };

    let img = image::load_from_memory(png_bytes)
        .map_err(|e| format!("解码 PNG 失败：{e}"))?
        .to_rgb8();

    let mut results = engine
        .predict(vec![img])
        .map_err(|e| format!("OCR 识别失败：{e}"))?;
    let result = results
        .drain(..)
        .next()
        .ok_or_else(|| "OCR 返回空结果集".to_string())?;

    let mut lines = Vec::with_capacity(result.text_regions.len());
    for region in &result.text_regions {
        let Some(text) = region.text.as_ref() else {
            continue;
        };
        let pts = &region.bounding_box.points;
        if pts.is_empty() {
            continue;
        }
        // 四点多边形 → 外接矩形。
        let (mut min_x, mut min_y) = (f32::MAX, f32::MAX);
        let (mut max_x, mut max_y) = (f32::MIN, f32::MIN);
        for p in pts {
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
            max_x = max_x.max(p.x);
            max_y = max_y.max(p.y);
        }
        lines.push(OcrLine {
            text: text.to_string(),
            x: min_x as f64,
            y: min_y as f64,
            w: (max_x - min_x) as f64,
            h: (max_y - min_y) as f64,
        });
    }

    let text = lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(OcrResult { text, lines })
}
