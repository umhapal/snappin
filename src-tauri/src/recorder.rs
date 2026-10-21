//! 录屏核心：shell 调 ffmpeg `ddagrab + h264_nvenc` 且不加 hwdownload
//! （GPU 帧零回读直喂 NVENC，ShareX 不卡的核心）。start 拉子进程、
//! stop 向其发 `q` 优雅收尾出 MP4。子进程隐藏控制台窗口（CREATE_NO_WINDOW）。
//!
//! **分段模型（2026-09-21 起）**：暂停无法用 ffmpeg 的 stdin 实现（它只认 `q`），
//! 所以录制被切成若干「段」——`pause()` 把当前段收尾封存、`resume()` 新起一段，
//! `stop()` 用 concat demuxer 无损拼接（`-c copy`，参数完全一致所以安全）。
//! 不暂停时只有一段，stop 直接搬走，与改造前等价。
//!
//! **音频（2026-09-21 起）**：可选 dshow 音频输入（麦克风/立体声混音），
//! 只走视频链路——GIF 容器没有音轨，GIF 模式恒不录音频。

use crate::capture::Bounds;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

/// 隐藏控制台窗口：GUI 应用里拉起控制台子进程不加它，ffmpeg 黑框会弹到前台。
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, serde::Deserialize)]
pub struct RecordOptions {
    /// 录制区域；None = 全屏（主显示器）。
    pub bounds: Option<Bounds>,
    pub fps: u32,
    /// 码率档位（kbps 量纲，默认 15000）。**不是实际码率**：经 `quality_for()`
    /// 映射成 NVENC `-rc vbr -cq` / x264 `-crf`，实际体积由画面内容自适应。
    /// 档位表见 `QUALITY_PRESETS`（前端按契约 C-2 写这三档之一）。
    pub bitrate_k: u32,
    /// 输出路径。`gif=true` 时是最终 `.gif`；中间 MP4 由本模块派生。
    pub out_path: PathBuf,
    /// true = 产 GIF（先按上面的链路录 MP4，停止时再用 palettegen/paletteuse 转成 GIF）。
    pub gif: bool,
    /// dshow 音频设备名（如 `麦克风 (Realtek(R) Audio)`）。None/空 = 不录音频。
    /// **GIF 模式恒为 None**（GIF 没有音轨），由调用方保证。
    #[serde(default)]
    pub audio_device: Option<String>,
}

/// 码率档位表（契约 C-2：前端把 `record_bitrate_k` 写成这三档之一）。
///
/// **这三个数字不是实际码率**，只是「质量档位键」：后端把它们映射到
/// NVENC `-rc vbr -cq` / x264 `-crf`，实际体积由画面内容自适应决定。
/// 数值越大质量越好、体积越大。
pub const QUALITY_PRESETS: [(u32, &str); 3] = [(2000, "低"), (8000, "中"), (20000, "高")];

/// 一档质量对应的编码器参数。
#[derive(Debug, Clone, Copy)]
pub struct QualityParams {
    /// NVENC 的 `-cq`（恒定质量；值越小质量越好，与 x264 的 CRF 同向）。
    pub cq: u32,
    /// x264 的 `-crf`。
    pub crf: u32,
    /// `-maxrate`：峰值码率上限（kbps）。
    pub maxrate_k: u32,
    /// `-bufsize`：码率缓冲（kbps）。取 maxrate 的 2 倍，给 VBR 留平滑空间。
    pub bufsize_k: u32,
}

/// 档位 → 编码器参数。
///
/// | 档位(k) | 名称 | nvenc `-cq` | x264 `-crf` | `-maxrate` | `-bufsize` |
/// |---------|------|-----------|-----------|-----------|-----------|
/// |  2000   | 低   |    28     |    30     |  2000k   |  4000k    |
/// |  8000   | 中   |    23     |    24     |  8000k   | 16000k    |
/// | 20000   | 高   |    18     |    19     | 20000k   | 40000k    |
///
/// why-not 继续用固定 CBR（2026-10-20 实测）：固定码率对静止画面照烧同样多的比特。
/// 同一段 5 秒 1280x720 测试源，CBR 15 Mbps 恒占 8,778 KB，`-rc vbr -cq 23` 只占
/// 3,120 KB；换成纯静态画面，CBR 仍占 8,792 KB，VBR+CQ 只占 **10 KB**。体积与内容
/// 脱钩正是「视频偏大」的根因——**不是 GOP 长度**（实测 GOP≈253 帧、关键帧只占 7.6%，
/// 缩短 GOP 反而更大）。`-maxrate`/`-bufsize` 是兜底上限：CQ 给质量下限，峰值上限
/// 防单帧尖峰把体积顶飞。
const QUALITY_TABLE: [(u32, &str, QualityParams); 3] = [
    (
        2000,
        "低",
        QualityParams {
            cq: 28,
            crf: 30,
            maxrate_k: 2000,
            bufsize_k: 4000,
        },
    ),
    (
        8000,
        "中",
        QualityParams {
            cq: 23,
            crf: 24,
            maxrate_k: 8000,
            bufsize_k: 16000,
        },
    ),
    (
        20000,
        "高",
        QualityParams {
            cq: 18,
            crf: 19,
            maxrate_k: 20000,
            bufsize_k: 40000,
        },
    ),
];

/// 把用户填的 `record_bitrate_k` 落到一档质量上（**向下取整**：不超过 k 的最高档；
/// k 比最低档还小则取最低档）。
///
/// why-not 就近吸附：就近会把 15000 吸到 20000（高），比用户要的 15000 更费空间。
/// 向下取整永远落在 ≤ k 的档位上，方向单调可预期。旧配置里的 15000 → 「中」，
/// 用户实装 24000 → 「高」。
pub fn preset_for(bitrate_k: u32) -> (u32, &'static str) {
    let row = pick_preset(bitrate_k);
    (row.0, row.1)
}

/// 按 `record_bitrate_k` 取该档的编码器参数（窗口录制与区域/全屏录制共用同一套）。
pub fn quality_for(bitrate_k: u32) -> QualityParams {
    pick_preset(bitrate_k).2
}

/// 向下取整选档（`QUALITY_TABLE` 必须按档位值升序）。
fn pick_preset(bitrate_k: u32) -> (u32, &'static str, QualityParams) {
    let mut hit = QUALITY_TABLE[0];
    for row in QUALITY_TABLE {
        if bitrate_k >= row.0 {
            hit = row;
        }
    }
    hit
}

/// 宽高向下取整到偶数（yuv420p 的色度行与亮度行成对，奇数高会错位出彩边）。
/// 0/1 一律给 2：既偶又非零。
fn even_dim(v: i32) -> u32 {
    let e = v & !1;
    if e < 2 {
        2
    } else {
        e as u32
    }
}

/// 运行中的录制会话（分段模型，见模块头注释）。
pub struct Recorder {
    /// 原始参数：resume 新起一段时复用。
    opts: RecordOptions,
    /// 段文件与拼接清单的临时目录（停止/丢弃时整个删掉）。
    tmp_dir: PathBuf,
    /// 已收尾封存的段文件。
    segments: Vec<PathBuf>,
    /// 当前正在录的段子进程（None = 已暂停）。
    cur: Option<Child>,
    cur_path: Option<PathBuf>,
    seg_seq: usize,
}

impl Recorder {
    /// 拼 ffmpeg 参数并拉起第一段子进程。ffmpeg 路径见 `ffmpeg_path()`。
    ///
    /// 采集链路：`ddagrab + h264_nvenc`；全屏零回读，区域录制因 crop 是软件滤镜
    /// 必须先 `hwdownload`（见 `build_ffmpeg_args`）。
    pub fn start(opts: RecordOptions) -> Result<Recorder, String> {
        let tmp_dir = make_tmp_dir()?;
        // 「没录到音轨」不能静默（GIF 例外：容器本来就没有音轨）。
        // 区域/全屏路径的音频源是 **dshow 设备**（麦克风/立体声混音…）；窗口路径是
        // **WASAPI loopback 抓的系统声音**（见 `window_recorder.rs`）——两套口径，
        // http 层分别输出 `dshow:<设备名>` / `system_loopback`，别混成一句「已录音频」。
        if !opts.gif {
            match opts
                .audio_device
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
            {
                Some(dev) => eprintln!("[snappin] 区域/全屏录制音频源：dshow:{dev}"),
                None => {
                    eprintln!("[snappin] 区域/全屏录制没有可用音频设备（dshow），本次产物无音轨")
                }
            }
        }
        let mut rec = Recorder {
            opts,
            tmp_dir,
            segments: Vec::new(),
            cur: None,
            cur_path: None,
            seg_seq: 0,
        };
        rec.spawn_segment()?;
        Ok(rec)
    }

    /// 起一段新录制（段文件落在临时目录，序号递增）。
    fn spawn_segment(&mut self) -> Result<(), String> {
        let ffmpeg = ffmpeg_path().ok_or("未找到 ffmpeg")?;
        let path = self.tmp_dir.join(format!("seg_{:03}.mp4", self.seg_seq));
        self.seg_seq += 1;
        let args = build_ffmpeg_args(&self.opts, &path)?;
        let child = Command::new(&ffmpeg)
            .args(&args)
            .stdin(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("拉起 ffmpeg 失败：{e}"))?;
        self.cur = Some(child);
        self.cur_path = Some(path);
        Ok(())
    }

    /// 收尾当前段（写 `q` 优雅停止 → 等退出 → 校验退出码 → 收进 segments）。
    fn finalize_current(&mut self) -> Result<(), String> {
        let Some(mut child) = self.cur.take() else {
            return Ok(());
        };
        let path = self.cur_path.take().ok_or("内部错误：段路径缺失")?;
        // 发送 q 请 ffmpeg 优雅收尾；写失败则强杀兜底。
        let quit_ok = match child.stdin.take() {
            Some(mut stdin) => stdin.write_all(b"q").and_then(|_| stdin.flush()).is_ok(),
            None => false,
        };
        if !quit_ok {
            let _ = child.kill();
        }
        let status = child
            .wait()
            .map_err(|e| format!("等待 ffmpeg 退出失败：{e}"))?;
        if !status.success() {
            let mut stderr_txt = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_string(&mut stderr_txt);
            }
            let code = status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "未知".into());
            return Err(format!("ffmpeg 退出码 {code}：{}", stderr_txt.trim()));
        }
        self.segments.push(path);
        Ok(())
    }

    /// 暂停：把当前段收尾封存，之后不再录（`resume()` 新起一段接上）。
    pub fn pause(&mut self) -> Result<(), String> {
        if self.cur.is_none() {
            return Err("当前未在录制（已暂停）".into());
        }
        self.finalize_current()?;
        Ok(())
    }

    /// 是否处于暂停态。暂停 = 当前段已封存、没有活跃子进程（见 `cur` 字段语义），
    /// 所以这里不需要单独的 paused 标志——两处状态分开存必然会漂移。
    pub fn is_paused(&self) -> bool {
        self.cur.is_none()
    }

    /// 继续：新起一段（与暂停前的段在 stop 时无损拼接）。
    pub fn resume(&mut self) -> Result<(), String> {
        if self.cur.is_some() {
            return Err("录制未暂停".into());
        }
        self.spawn_segment()?;
        Ok(())
    }

    /// 停止并产出成品：收尾当前段 → 拼接（多段时）→ GIF 转码（gif 模式）→ 搬/写到 out_path。
    ///
    /// `on_progress` 只在 GIF 转码阶段被调用（0-99，百分比）——转码长录可达几十秒，
    /// 没有进度用户只会看到「停止后卡住」。MP4 路径不做转码，不会回调。
    pub fn stop(mut self, mut on_progress: impl FnMut(u8)) -> Result<PathBuf, String> {
        let result = self.stop_inner(&mut on_progress);
        // 无论成败都清临时目录：失败时中间段由错误信息里的路径已不可靠，删掉避免堆垃圾。
        let _ = std::fs::remove_dir_all(&self.tmp_dir);
        result.map(|_| self.opts.out_path.clone())
    }

    fn stop_inner(&mut self, on_progress: &mut dyn FnMut(u8)) -> Result<(), String> {
        self.finalize_current()?;
        if self.segments.is_empty() {
            return Err("没有录到任何内容".into());
        }
        // 单段（没暂停过）直接用；多段用 concat demuxer 无损拼接。
        let merged = if self.segments.len() == 1 {
            self.segments[0].clone()
        } else {
            let dst = self.tmp_dir.join("merged.mp4");
            concat_segments(&self.segments, &dst)?;
            dst
        };
        if self.opts.gif {
            // GIF 转码失败时把中间 MP4 搬到成品旁边再报错——`stop` 会清掉临时目录，
            // 不搬走的话「中间录像保留在…」就是一句空话，用户拿不到任何东西。
            mp4_to_gif(&merged, &self.opts.out_path, on_progress).map_err(|e| {
                let fallback = self.opts.out_path.with_extension("mp4");
                let kept = move_file(&merged, &fallback).is_ok();
                format!(
                    "GIF 转码失败：{e}{}",
                    if kept {
                        format!("（中间录像已保留在 {}）", fallback.display())
                    } else {
                        String::new()
                    }
                )
            })
        } else {
            move_file(&merged, &self.opts.out_path)
        }
    }

    /// 丢弃录制：强杀当前段子进程、删掉临时目录与（可能已存在的）成品，不产出任何文件。
    /// 取消录制走此路径（用户点 × 时前端传 discard=true）。
    pub fn discard(mut self) -> Result<(), String> {
        if let Some(mut child) = self.cur.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.tmp_dir);
        if self.opts.out_path.exists() {
            let _ = std::fs::remove_file(&self.opts.out_path);
        }
        Ok(())
    }
}

/// 建本次录制的临时目录（段文件 + 拼接清单都放这里，stop/discard 整目录删）。
fn make_tmp_dir() -> Result<PathBuf, String> {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("snappin-rec-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("建录制临时目录失败：{e}"))?;
    Ok(dir)
}

/// 同卷 `rename`，跨卷回退「复制 + 删源」。
fn move_file(src: &Path, dst: &Path) -> Result<(), String> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    std::fs::copy(src, dst).map_err(|e| format!("写入产物失败：{e}"))?;
    let _ = std::fs::remove_file(src);
    Ok(())
}

/// 拼出交给 ffmpeg 的完整参数向量。
///
/// 全屏：`-filter_complex ddagrab=0:framerate=<fps>[v] -map [v] -c:v h264_nvenc ...`（零回读）
/// 区域：先按选区所在显示器挑 `output_idx`，再 `hwdownload,format=bgra,crop=w:h:x:y`
///       （x/y 相对该显示器原点）。
/// 音频：`-f dshow -i audio=<dev>` 作为 0 号输入，`-map [v] -map 0:a -c:a aac`。
///
/// **为什么区域录制必须 hwdownload（2026-09-21 实测定案）**：`ddagrab,crop=786:593:100:0`
/// 这条「GPU 内裁切」的写法**静默无效**——ffmpeg 不报错、rc=0，但产物恒为主屏全尺寸
/// 1920×1080，用户拿到的是整屏而不是选区（真机上就是「录出来的东西不对」）。
/// 加上 `hwdownload,format=bgra` 后同一组参数产出 786×593。代价是这一路多一次
/// GPU→CPU 回读（crop 是软件滤镜，无法作用于 D3D11 硬件帧），正确性优先。
fn build_ffmpeg_args(opts: &RecordOptions, out: &Path) -> Result<Vec<String>, String> {
    // GIF 模式走 gdigrab 采集（真彩光标）：ddagrab 的光标合成是 XOR 反色
    // （2026-09-21 实测：用户彩色自定义光标在 GIF 里变成深色剪影），gdigrab 走 GDI
    // DrawIcon 画的是真彩色光标。代价是 GDI 采集慢（本机实测 790x562@30 只跑
    // 23-28fps）——GIF 帧率封顶 24，够用；视频录制仍走 ddagrab+NVENC（流畅优先，
    // 反色光标可接受）。gdigrab 坐标是虚拟桌面原点相对值，bounds 直接可用。
    // 音频不进 GIF（容器没有音轨）。
    if opts.gif {
        return Ok(build_gdigrab_args(opts, out));
    }
    let mut filter = format!("ddagrab=0:framerate={}", opts.fps);
    if let Some(b) = opts.bounds {
        let m = crate::capture::monitor_for_bounds(b)
            .ok_or_else(|| format!("选区不在任何显示器上：{}x{}@{},{}", b.w, b.h, b.x, b.y))?;
        // 裁到该显示器内（跨屏选区只录落在该屏上的那部分，越界由 ffmpeg 拒收改为我们夹紧）。
        let x0 = b.x.max(m.rect.x);
        let y0 = b.y.max(m.rect.y);
        let x1 = (b.x + b.w).min(m.rect.x + m.rect.w);
        let y1 = (b.y + b.h).min(m.rect.y + m.rect.h);
        // 宽高对齐偶数：yuv420p 的色度行与亮度行成对，奇数高会让色度行错位 → 彩边
        // （实测窗口高 841 时 R-B 偏差 -12.5，偶数 840 时 -0.6；加 -colorspace /
        // -color_primaries 压不住，实测 -12.5 → -12.0）。这里向下取整到偶数：
        // 少 1 像素，比错位的色度行划算。
        let (w, h) = (even_dim(x1 - x0), even_dim(y1 - y0));
        filter = format!(
            "ddagrab={}:framerate={},hwdownload,format=bgra,crop={}:{}:{}:{}",
            m.index,
            opts.fps,
            w,
            h,
            x0 - m.rect.x,
            y0 - m.rect.y
        );
    }

    // 音频源口径（区域/全屏路径）：**dshow 设备**（麦克风/立体声混音…）。
    // 窗口路径不是这套——那是 WASAPI loopback 抓的**系统声音**（`window_recorder.rs`），
    // 两套口径别混：http 层据此输出 `dshow:<设备名>` / `system_loopback`。
    let audio = opts
        .audio_device
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());

    let mut args: Vec<String> = vec!["-y".into()];
    if let Some(dev) = audio {
        // dshow 音频输入：设备名直接作为单个 argv 元素传，不经 shell，含空格/括号都安全。
        args.push("-f".into());
        args.push("dshow".into());
        args.push("-i".into());
        args.push(format!("audio={dev}"));
    }
    args.push("-filter_complex".into());
    // 显式给滤镜输出打标签并 -map：有第二个输入（音频）时，不标就可能挑错流。
    args.push(format!("{filter}[v]"));
    args.push("-map".into());
    args.push("[v]".into());
    if audio.is_some() {
        args.push("-map".into());
        args.push("0:a".into());
        args.push("-c:a".into());
        args.push("aac".into());
        args.push("-b:a".into());
        args.push("160k".into());
    }
    // 质量档位 → VBR+CQ（映射表见 `QUALITY_TABLE`）：体积跟着画面内容走，
    // 不再固定烧比特。`-maxrate`/`-bufsize` 兜住峰值。
    let q = quality_for(opts.bitrate_k);
    args.extend([
        "-c:v".into(),
        "h264_nvenc".into(),
        "-b:v".into(),
        format!("{}k", q.maxrate_k),
        "-maxrate".into(),
        format!("{}k", q.maxrate_k),
        "-bufsize".into(),
        format!("{}k", q.bufsize_k),
        "-preset".into(),
        "p5".into(),
        "-rc".into(),
        "vbr".into(),
        "-cq".into(),
        q.cq.to_string(),
        // 不设输出 -pix_fmt：让 nv12 由滤镜链自动协商，保证出可播放 MP4。
        "-movflags".into(),
        "+faststart".into(),
        out.to_string_lossy().into_owned(),
    ]);
    Ok(args)
}

/// GIF 中间 MP4 的 gdigrab 参数（真彩光标，见 build_ffmpeg_args 注释）。
/// 帧率沿用用户档位（前端 GIF 档已封 ≤24）；编码用 x264 ultrafast——GDI 采集本身
/// 是瓶颈（~25fps），NVENC 救不了帧率，CPU 软编在这台机器上实测同样够速且兼容性好。
fn build_gdigrab_args(opts: &RecordOptions, out: &Path) -> Vec<String> {
    let mut args = vec![
        "-y".into(),
        "-f".into(),
        "gdigrab".into(),
        "-draw_mouse".into(),
        "1".into(),
        "-framerate".into(),
        opts.fps.to_string(),
    ];
    if let Some(b) = opts.bounds {
        args.push("-offset_x".into());
        args.push(b.x.to_string());
        args.push("-offset_y".into());
        args.push(b.y.to_string());
        args.push("-video_size".into());
        // 采集尺寸对齐偶数：libx264 + yuv420p 遇奇数宽高直接报错（宽高必须能被 2 整除），
        // 且奇数高同样出色度行错位的彩边。
        args.push(format!("{}x{}", even_dim(b.w), even_dim(b.h)));
    }
    // x264 走 CRF（与 NVENC 路径共用 `QUALITY_TABLE` 的同一档质量）。
    // CRF 模式下 `-b:v` 无意义，用 `-maxrate`/`-bufsize` 兜住峰值即可。
    let q = quality_for(opts.bitrate_k);
    args.extend([
        "-i".into(),
        "desktop".into(),
        "-c:v".into(),
        "libx264".into(),
        "-crf".into(),
        q.crf.to_string(),
        "-maxrate".into(),
        format!("{}k", q.maxrate_k),
        "-bufsize".into(),
        format!("{}k", q.bufsize_k),
        "-preset".into(),
        "ultrafast".into(),
        "-tune".into(),
        "zerolatency".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        out.to_string_lossy().into_owned(),
    ]);
    args
}

/// 把多段 MP4 无损拼成一个（concat demuxer + `-c copy`）。
///
/// 各段由**同一组编码参数**产出（同编码器/码率/帧率），所以可以直接 copy 流；
/// concat demuxer 会按各段时长平移时间戳，不会出现时间轴重叠。
fn concat_segments(parts: &[PathBuf], dst: &Path) -> Result<(), String> {
    let ffmpeg = ffmpeg_path().ok_or("未找到 ffmpeg")?;
    let list = dst.with_extension("concat.txt");
    let mut body = String::new();
    for p in parts {
        // concat demuxer 的路径用正斜杠；单引号按 demuxer 约定转义。
        let s = p
            .to_string_lossy()
            .replace('\\', "/")
            .replace('\'', "'\\''");
        body.push_str(&format!("file '{s}'\n"));
    }
    std::fs::write(&list, body).map_err(|e| format!("写拼接清单失败：{e}"))?;
    let out = Command::new(&ffmpeg)
        .arg("-y")
        .args(["-v", "error", "-f", "concat", "-safe", "0", "-i"])
        .arg(&list)
        .args(["-c", "copy", "-movflags", "+faststart"])
        .arg(dst)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("拉起 ffmpeg 拼接失败：{e}"))?;
    let _ = std::fs::remove_file(&list);
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "拼接失败：{}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// 把录好的 MP4 转成 GIF（调色板两遍法，ffmpeg 单命令内完成）：
/// `split` 出两路 → `palettegen` 统计全局 256 色 → `paletteuse` 用 sierra2_4a 抖动映射。
/// 帧率沿用源（前端在 GIF 模式已把帧率封到 ≤24）。`-loop 0` = 无限循环播放。
///
/// **为什么 palettegen 用 `stats_mode=full` 而不是 `diff`（2026-10-16 实测定案）**：
/// `diff` 只把「与前一帧不同」的像素计入直方图。当光标在录制**中途**才进入录制区并随后
/// 静止（真实场景：用户点了录制才把鼠标移到目标上），光标像素只在极少数帧里算作「变化」，
/// 256 色预算被静止的背景占满 → 光标被压成近灰剪影（实测最大彩度 168 → 39，彩点 99 → 0）。
/// `full` 用全帧直方图，光标颜色始终有代表色 → 真彩恢复（168/99）。代价实测 +4% 转码时间、
/// +3% 体积，可接受；这是用户可见的颜色正确性问题，不是性能取舍。
/// why-not `single`（逐帧调色板）：实测产出的帧全空，更差；why-not 保留 diff 另留高彩度色：
/// palettegen 无此旋钮，得自建调色板，成本高。
///
/// `-progress pipe:1` 让 ffmpeg 把进度写成 `out_time_us=...` 行打到 stdout，
/// 我们按源总时长折算百分比回调——长录转码几十秒，没进度用户只会看到「卡住」。
pub(crate) fn mp4_to_gif(
    src: &Path,
    dst: &Path,
    on_progress: &mut dyn FnMut(u8),
) -> Result<(), String> {
    let ffmpeg = ffmpeg_path().ok_or("未找到 ffmpeg")?;
    let total_us = probe_duration_us(src).unwrap_or(0);
    let filter = "[0:v]split[a][b];[a]palettegen=max_colors=256:stats_mode=full[p];\
                  [b][p]paletteuse=dither=sierra2_4a:diff_mode=rectangle";
    let mut child = Command::new(&ffmpeg)
        .arg("-y")
        .args(["-v", "error", "-i"])
        .arg(src)
        .args([
            "-filter_complex",
            filter,
            "-progress",
            "pipe:1",
            "-nostats",
            "-loop",
            "0",
        ])
        .arg(dst)
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("拉起 ffmpeg 转码失败：{e}"))?;

    // stderr 必须有人读：`-v error` 通常很小，但转码失败时可能刷屏，管道写满会死锁。
    let mut err_pipe = child.stderr.take().ok_or("转码进程 stderr 未捕获")?;
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err_pipe.read_to_string(&mut s);
        s
    });

    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(v) = line.strip_prefix("out_time_us=") {
                if let (Ok(us), true) = (v.trim().parse::<i64>(), total_us > 0) {
                    let pct = ((us.max(0) as f64 / total_us as f64) * 100.0).clamp(0.0, 99.0) as u8;
                    on_progress(pct);
                }
            }
        }
    }
    let status = child.wait().map_err(|e| format!("等待转码失败：{e}"))?;
    let err_txt = err_thread.join().unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(err_txt.trim().to_string())
    }
}

/// 读源视频总时长（微秒）——GIF 转码进度折算用。探测失败返回 None（进度就只报 0）。
fn probe_duration_us(src: &Path) -> Option<i64> {
    let ffprobe = ffprobe_path()?;
    let out = Command::new(&ffprobe)
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(src)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some((secs * 1_000_000.0) as i64)
}

/// 探测 ffmpeg 可执行路径。
///
/// 顺序：环境变量 `SNAPPIN_FFMPEG` → `PATH` 中的 `ffmpeg.exe` → exe 同级目录。
/// why-not 写死某台机器的绝对路径：换机即失效，而 PATH 与 exe 同级已覆盖常见装法。
pub fn ffmpeg_path() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("SNAPPIN_FFMPEG") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Some(p);
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let cand = dir.join("ffmpeg.exe");
            if cand.exists() {
                return Some(cand);
            }
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .map(|dir| dir.join("ffmpeg.exe"))
        .filter(|p| p.exists())
}

/// 探测 ffprobe（与 ffmpeg 同目录）。
fn ffprobe_path() -> Option<PathBuf> {
    let ff = ffmpeg_path()?;
    let p = ff.with_file_name("ffprobe.exe");
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

/// 枚举本机 dshow 音频输入设备名（麦克风/立体声混音等）。
/// 设备列表由 ffmpeg 打到 **stderr**，形如 `"麦克风 (Realtek(R) Audio)" (audio)`。
pub fn audio_devices() -> Vec<String> {
    let Some(ffmpeg) = ffmpeg_path() else {
        return Vec::new();
    };
    let Ok(out) = Command::new(&ffmpeg)
        .args([
            "-hide_banner",
            "-list_devices",
            "true",
            "-f",
            "dshow",
            "-i",
            "dummy",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
    else {
        return Vec::new();
    };
    parse_audio_devices(&String::from_utf8_lossy(&out.stderr))
}

/// 从 `-list_devices` 输出里挑出音频设备名（纯函数，便于单测式核对）。
fn parse_audio_devices(txt: &str) -> Vec<String> {
    let mut v = Vec::new();
    for line in txt.lines() {
        let t = line.trim();
        if !t.ends_with("(audio)") {
            continue;
        }
        if let (Some(a), Some(b)) = (t.find('"'), t.rfind('"')) {
            if b > a + 1 {
                v.push(t[a + 1..b].to_string());
            }
        }
    }
    v
}

/// 按设置解析实际要用的音频设备：`enabled=false` → 不录；`configured` 非空 → 用它；
/// 否则取第一个可用设备（没有可用设备 → 不录，而不是让录制失败）。
pub fn resolve_audio_device(enabled: bool, configured: &str) -> Option<String> {
    if !enabled {
        return None;
    }
    let c = configured.trim();
    if !c.is_empty() {
        return Some(c.to_string());
    }
    audio_devices().into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::{even_dim, parse_audio_devices, preset_for, quality_for, QUALITY_PRESETS};

    #[test]
    fn quality_table_matches_presets() {
        // 契约 C-2：前端按 QUALITY_PRESETS 写值，映射表必须与之一一对应。
        assert_eq!(QUALITY_PRESETS[0], (2000, "低"));
        assert_eq!(QUALITY_PRESETS[1], (8000, "中"));
        assert_eq!(QUALITY_PRESETS[2], (20000, "高"));
        for (k, label) in QUALITY_PRESETS {
            assert_eq!(preset_for(k), (k, label));
        }
    }

    #[test]
    fn preset_floor_rule() {
        // 向下取整：不超过 k 的最高档，比最低档还小则取最低档。
        assert_eq!(preset_for(0).1, "低");
        assert_eq!(preset_for(1999).1, "低");
        assert_eq!(preset_for(2000).1, "低");
        assert_eq!(preset_for(2001).1, "中");
        assert_eq!(preset_for(8000).1, "中");
        assert_eq!(preset_for(15000).1, "中"); // 后端默认 15000 → 中
        assert_eq!(preset_for(24000).1, "高"); // 用户实装 24000 → 高
        assert_eq!(preset_for(1_000_000).1, "高");
    }

    #[test]
    fn quality_gets_better_as_preset_rises() {
        let lo = quality_for(2000);
        let mid = quality_for(8000);
        let hi = quality_for(20000);
        // cq/crf 越小质量越好；maxrate 越大体积上限越高。
        assert!(lo.cq > mid.cq && mid.cq > hi.cq);
        assert!(lo.crf > mid.crf && mid.crf > hi.crf);
        assert!(lo.maxrate_k < mid.maxrate_k && mid.maxrate_k < hi.maxrate_k);
        assert_eq!(lo.bufsize_k, lo.maxrate_k * 2);
        assert_eq!(hi.bufsize_k, hi.maxrate_k * 2);
    }

    #[test]
    fn even_dim_never_returns_odd_or_zero() {
        assert_eq!(even_dim(841), 840);
        assert_eq!(even_dim(840), 840);
        assert_eq!(even_dim(1), 2);
        assert_eq!(even_dim(0), 2);
        assert_eq!(even_dim(-3), 2);
    }

    #[test]
    fn parses_only_audio_lines() {
        let txt = "\
[in#0 @ 0x1] \"BisonCam,NB Pro\" (video)
[in#0 @ 0x1]   Alternative name \"@device_pnp_x\"
[in#0 @ 0x1] \"麦克风 (Realtek(R) Audio)\" (audio)
[in#0 @ 0x1] \"立体声混音 (Realtek(R) Audio)\" (none)
[in#0 @ 0x1] \"Line In (USB Audio)\" (audio)
";
        assert_eq!(
            parse_audio_devices(txt),
            vec!["麦克风 (Realtek(R) Audio)", "Line In (USB Audio)"]
        );
    }
}
