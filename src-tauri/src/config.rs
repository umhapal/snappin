//! SnapPin 本地配置：agent 接入 token/端口 + 全部用户设置项。
//! 配置落在 Tauri 的 app_config_dir/config.json，首启自动生成 token。
//!
//! 设计纪律（2026-09-20 定档）：
//! - 每个设置项必须有真实消费点（哪个模块读它），不做"存了没人读"的死项。
//! - 新增设置项 = 加字段 + 给 Default 值 + 前端控件接线；serde 默认缺失字段回 Default，
//!   旧配置文件向后兼容（不迁移、不报错）。

use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 47821;

/// 当前配置文件结构版本（迁移到该版本后，旧的隐式默认值不再被静默改写）。
pub const CURRENT_CONFIG_VERSION: u32 = 1;

/// 录屏帧率合法档位。
pub const FPS_CHOICES: [u32; 4] = [15, 24, 30, 60];

/// 保存文件名格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum NameFormat {
    #[default]
    Timestamp,
    Sequence,
}

// 语言档（`Language`）2026-09-21 删除：本项目没有任何 i18n 基础设施，UI 文案全中文，
// 留着就是「存了没人读」的死项（违反本文件自述纪律）。宁可删掉，不留假开关。
// 旧配置文件里的 `"language": "zhcn"` 会被 serde **忽略**——serde 默认对结构体里
// 不认识的字段就是跳过（只有显式 `#[serde(deny_unknown_fields)]` 才会报错），
// 所以删字段不会让旧配置加载失败。

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    /// 配置文件结构版本（当前 1）。0 = 2026-10-20 之前的旧配置。
    /// 谁在读：`load_or_init` —— 版本 < 1 时做一次性迁移：旧配置里 `record_audio=false` 是
    /// 当年的**默认值**而非用户的选择，统一升级为 true 并写回 version=1；之后用户再显式
    /// 关掉音频会被尊重（不再被迁移覆盖）。为什么要版本号：光看 `record_audio` 的值无法
    /// 区分「用户主动关的」和「一直是默认关的」，强行打开会覆盖真实意图。
    #[serde(default = "legacy_config_version")]
    pub config_version: u32,

    // ---- agent 接入（不可在 UI 里改 token；端口改后需重启生效）----
    /// agent 接入 HTTP 服务的 Bearer token（首启随机生成）。
    pub token: String,
    /// HTTP 服务端口。
    pub port: u16,

    // ---- 系统 ----
    /// 开机自启：写/删注册表 HKCU Run 键（M3 已接线，设置变更即时生效）。
    pub auto_start: bool,
    /// 关闭窗口时隐藏到托盘（true）而非退出（false）。
    pub close_to_tray: bool,
    /// 下次启动以管理员身份运行（true 时未提权则 ShellExecute runas 自重启）。
    pub run_as_admin: bool,

    // ---- 截图 ----
    /// 截图后自动复制到剪贴板。
    pub capture_copy_after: bool,
    /// 截图后自动保存文件（否则只进剪贴板/贴图）。
    pub capture_auto_save: bool,
    /// 智能窗口/元素检测（overlay 命中窗口矩形）。
    pub capture_ui_detect: bool,
    /// 选区实时显示尺寸标签 `x,y W x H px`。
    pub capture_show_size_tag: bool,
    /// 长截图：每步滚轮档数（1 档 = WHEEL_DELTA，钳到 1–5）。速度的粗调——越大每步跳得越远。
    pub longshot_scroll_notches: u32,
    /// 长截图：每步等待上限（毫秒，钳到 100–3000）。画面渲染落定即提前进入下一步，
    /// 所以它是「最慢速度」，不是固定 sleep。
    pub longshot_settle_ms: u64,

    // ---- 贴图 ----
    /// 贴图默认不透明度（1-100）。
    /// 谁在读：`src/Pin.tsx` 挂载时 `get_settings` 拉一次，把它设成贴图内容（img）的初始 opacity。
    pub pin_opacity: u8,
    /// 贴图鼠标滚轮缩放。
    /// 谁在读：`src/Pin.tsx` 的 `onWheel` —— 关掉后普通滚轮不缩放（Ctrl+滚轮调透明度不受影响）。
    pub pin_wheel_zoom: bool,
    /// 贴图带阴影。
    /// 谁在读：`src/Pin.tsx` 容器样式 —— 关掉后贴图四周不再有暗边。
    pub pin_shadow: bool,

    // ---- 外观 ----
    /// 桌面悬浮球：常驻置顶的小圆球（截图/设置/隐藏），见 docs/UI设计规范.md §1.5 real-7。
    pub desktop_ball: bool,
    /// 悬浮球位置（逻辑像素，-1 = 首次启动贴右侧垂直居中）。
    pub ball_x: i32,
    pub ball_y: i32,

    // ---- 保存 ----
    /// 保存目录（空 = 系统图片目录下 SnapPin）。
    pub save_dir: String,
    /// 文件名格式：时间戳 / 序号。
    /// 谁在读：`commands.rs::product_stem` —— finish_capture / capture_long / start_record_impl
    /// 落盘前读它决定基名（timestamp = `SnapPin_<秒>`；sequence = 扫目录取最大序号 +1，补零 4 位）。
    pub save_name_format: NameFormat,
    /// 静态图产物存 PNG（true）还是 JPEG（false）。
    /// 谁在读：`commands.rs::image_ext_and_format` —— finish_capture / capture_long 落盘前读它
    /// 决定扩展名与**真实编码格式**（false 时编出的是真 JPEG，扩展名 .jpg）。
    pub save_format_png: bool,
    /// 截图/录制完成后是否先弹确认阶段（可选路径 / 快速保存）再落盘；false = 直接按 save_dir 落盘。
    pub capture_confirm_save: bool,
    /// 静态图 JPEG 画质（40-100，越大越清晰、文件越大）。PNG 无损时此项不生效。
    /// 谁在读：`capture.rs` —— 抓取/标注后编码 JPEG 时的质量参数（原先硬编码 90，
    /// 用户无法调，是「录屏/截图画质不可调」的一半原因）。
    pub jpeg_quality: u8,

    // ---- 标注 ----
    /// 标注默认画笔颜色（hex）。
    pub annotate_color: String,
    /// 标注默认粗细（1-20）。
    pub annotate_stroke_width: u8,

    // ---- 录制 ----
    /// 录屏帧率档位（15/24/30/60）。
    pub record_fps: u32,
    /// 录屏码率（kbit/s）。
    pub record_bitrate_k: u32,
    /// 录屏开始倒计时秒数（0=不倒计时）。
    /// 谁在读：`src/Overlay.tsx::beginRecord` —— 工具条「开始录制」、G 键、以及热键路径
    /// （后端 `trigger_capture` 武装分支 emit `snappin-arm-start`）都经它；0 = 立即开录。
    pub record_countdown: u8,
    /// 录屏是否录音频（dshow 输入）。
    /// 谁在读：`commands.rs::start_record_impl` / `http.rs::record_start_ep` —— 开录前解析成
    /// `RecordOptions.audio_device`。GIF 模式恒不录（容器没有音轨）。
    pub record_audio: bool,
    /// 音频输入设备名（dshow，如 `麦克风 (Realtek(R) Audio)`）。
    /// 谁在读：同上。**空串 = 自动取第一个可用设备**；没有可用设备则不录（不报错）。
    pub record_audio_device: String,

    // ---- OCR ----
    /// OCR 后自动复制全文到剪贴板。
    /// 谁在读：`commands.rs::auto_copy_ocr` —— ocr_image / ocr_region 识别完成后读它决定是否写剪贴板。
    pub ocr_auto_copy: bool,

    // ---- 剪贴板 ----
    /// 记录剪贴板历史总开关。
    /// 谁在读：`clip.rs::capture_now` 首行 gate —— 关闭后不再记录新复制的内容；
    /// 已有历史保留（面板仍可打开、删除、清空）。
    pub clip_enabled: bool,
    /// 历史条数上限（10-1000，超出删最旧、含其图片文件）。
    /// 谁在读：`clip.rs::ingest` 超限清理；`clip.rs::load_history` 启动截断。
    pub clip_max: u32,
    /// 这些应用里复制的内容不入库（进程名多行文本）；留空=全部记录。
    /// 读：clip.rs::capture_now 来源进程匹配
    pub clip_ignore_apps: String,
    /// 自动识别私钥/服务 Token/AWS Key/JWT 等高置信机密不写入历史。
    /// 读：clip.rs::ingest 敏感跳过
    pub clip_skip_sensitive: bool,
    /// 打开面板时聚焦搜索框。
    /// 读：Clip.tsx
    pub clip_search_focus: bool,
    /// 关闭面板时清空搜索词。
    /// 读：Clip.tsx
    pub clip_search_clear_on_hide: bool,
    /// 点击条目写回剪贴板后自动粘进上个窗口。
    /// 读：Clip.tsx + commands.rs::clip_auto_paste
    pub clip_auto_paste: bool,
    /// 粘贴后自动收起面板。false = 面板留着供连续粘贴，用户手动关（热键/✕）。
    /// 读：commands.rs::clip_paste
    /// （2026-09-30 曾把这里写死成「不自动关」，用户 2026-10-14 报障后改为可配置。）
    pub clip_close_after_paste: bool,
    /// 粘贴触发方式 single/double，仅当 auto_paste 开启有意义。
    /// 读：Clip.tsx
    pub clip_paste_trigger: String,
    /// 支持纯文本粘贴（面板内 Shift+Enter）。
    /// 读：Clip.tsx
    pub clip_paste_plain: bool,
    /// 过期天数，0=永不过期。
    /// 读：clip.rs 过期清理
    pub clip_expire_days: u32,
    /// 点击面板外自动关闭。
    /// 读：Clip.tsx
    pub clip_hide_on_blur: bool,
    /// 列表密度 compact/comfortable 两档行高。
    /// 读：Clip.tsx
    pub clip_list_density: String,

    // ---- 热键 ----
    /// 全局热键重绑定：键 "capture"/"pin"/"record"，值如 "Ctrl+1"/"Ctrl+Shift+R"。
    /// 空 map 或缺某键 = 该动作用内置默认键。
    pub hotkeys: HashMap<String, String>,
    /// 手动禁用全部全局热键（PixPin「禁用快捷键」）。true 时启动与任何配置变更都不注册任何热键。
    /// 谁在读：`hotkeys::sync` 首行 gate——**先 unregister_all 再决定是否注册**（不清空会让旧键残留）。
    pub hotkeys_disabled: bool,
    /// 全屏程序前台时自动禁用热键（PixPin 同名开关）。true 且前台是全屏（D3D 全屏/无边框盖屏）时
    /// 临时注销全部热键，退出全屏自动恢复；`hotkeys_disabled` 优先级更高。
    /// 检测：`system::is_fullscreen_foreground`。
    pub hotkeys_disable_fullscreen: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: CURRENT_CONFIG_VERSION,
            token: gen_token(),
            port: DEFAULT_PORT,
            auto_start: false,
            close_to_tray: false,
            run_as_admin: false,
            capture_copy_after: true,
            capture_auto_save: true,
            capture_ui_detect: true,
            capture_show_size_tag: true,
            longshot_scroll_notches: 1,
            longshot_settle_ms: 400,
            pin_opacity: 100,
            pin_wheel_zoom: true,
            pin_shadow: true,
            desktop_ball: false,
            ball_x: -1,
            ball_y: -1,
            save_dir: String::new(),
            save_name_format: NameFormat::default(),
            save_format_png: true,
            capture_confirm_save: true,
            jpeg_quality: 92,
            annotate_color: "#EF4444".into(),
            annotate_stroke_width: 3,
            record_fps: 60,
            record_bitrate_k: 15000,
            record_countdown: 0,
            record_audio: true,
            record_audio_device: String::new(),
            ocr_auto_copy: true,
            clip_enabled: true,
            clip_max: 100,
            clip_ignore_apps: String::new(),
            clip_skip_sensitive: true,
            clip_search_focus: true,
            clip_search_clear_on_hide: true,
            clip_auto_paste: false,
            clip_close_after_paste: true,
            clip_paste_trigger: "double".into(),
            clip_paste_plain: true,
            clip_expire_days: 0,
            clip_hide_on_blur: true,
            clip_list_density: "comfortable".into(),
            hotkeys: HashMap::new(),
            hotkeys_disabled: false,
            hotkeys_disable_fullscreen: false,
        }
    }
}

/// 生成 32 字符的十六进制随机 token。
fn gen_token() -> String {
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| format!("{:x}", rng.gen_range(0..16u8)))
        .collect()
}

/// 读取配置；不存在或损坏则新建默认并落盘。
/// 部分字段缺失走 serde `#[serde(default)]` 逐字段回填，旧文件无痛升级。
pub fn load_or_init(config_dir: &PathBuf) -> AppConfig {
    let path = config_dir.join("config.json");
    if let Ok(bytes) = std::fs::read(&path) {
        if let Ok(mut cfg) = serde_json::from_slice::<AppConfig>(&bytes) {
            cfg.normalize();
            // 磁盘文件里没有 token（手工/外部工具生成的配置）时，serde 会按 Default 补一个
            // **只在内存里**的随机 token：不落盘的话 HTTP 客户端永远猜不到它，所有受权请求
            // 一律 401（2026-10-14 隔离实例实测踩到）。补写回盘，让「配置文件里的 token」
            // 与本实例实际使用的 token 一致。
            let has_token = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| {
                    v.get("token")
                        .and_then(|t| t.as_str())
                        .map(|s| !s.is_empty())
                })
                .unwrap_or(false);
            let migrated = migrate_legacy_config(&mut cfg);
            if !has_token || migrated {
                cfg.save(&path);
            }
            return cfg;
        }
    }
    let mut cfg = AppConfig::default();
    cfg.normalize();
    let _ = std::fs::create_dir_all(config_dir);
    cfg.save(&path);
    cfg
}

/// 旧配置文件（无 `config_version` 字段）的版本号。
fn legacy_config_version() -> u32 {
    0
}

/// 一次性迁移旧配置；返回是否发生了迁移（迁移后要写回盘）。
///
/// 2026-10-20 之前 `record_audio` 的默认值是 `false`，于是「录屏没有声音」成了
/// 装好即中招的默认状态，而用户在配置里看到 `record_audio:false` 会以为是自己关的。
/// 旧配置（version=0）里的 `false` 一律按「当年的默认值」处理，升级为 `true`；
/// 迁移后版本号变为 1，用户此后显式关掉音频会被尊重，不再被覆盖。
fn migrate_legacy_config(cfg: &mut AppConfig) -> bool {
    if cfg.config_version >= CURRENT_CONFIG_VERSION {
        return false;
    }
    cfg.record_audio = true;
    cfg.config_version = CURRENT_CONFIG_VERSION;
    true
}

impl AppConfig {
    /// 清洗非法值（档位越界、透明度越界等），读入与保存前都跑一遍。
    pub fn normalize(&mut self) {
        if !FPS_CHOICES.contains(&self.record_fps) {
            self.record_fps = 60;
        }
        self.pin_opacity = self.pin_opacity.clamp(1, 100);
        self.annotate_stroke_width = self.annotate_stroke_width.clamp(1, 20);
        self.record_bitrate_k = self.record_bitrate_k.clamp(1000, 100_000);
        self.jpeg_quality = self.jpeg_quality.clamp(40, 100);
        self.record_countdown = self.record_countdown.min(10);
        self.clip_max = self.clip_max.clamp(10, 1000);
        if self.clip_paste_trigger != "single" && self.clip_paste_trigger != "double" {
            self.clip_paste_trigger = "double".into();
        }
        if self.clip_list_density != "compact" && self.clip_list_density != "comfortable" {
            self.clip_list_density = "comfortable".into();
        }
        self.clip_expire_days = self.clip_expire_days.min(3650);
        if !self.annotate_color.starts_with('#') || self.annotate_color.len() != 7 {
            self.annotate_color = "#EF4444".into();
        }
    }

    /// 原子落盘：先写临时文件再改名，避免写一半崩溃留下坏配置。
    pub fn save(&self, path: &PathBuf) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, path);
            }
        }
    }
}
