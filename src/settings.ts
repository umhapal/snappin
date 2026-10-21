// 设置契约：与 src-tauri/src/config.rs 的 AppConfig 一一对应（serde 默认 camel 不开启，
// 字段名保持 snake_case 原样传输）。
// 新增设置项：后端加字段 → 这里同步类型与 DEFAULTS，不设第三处。

export type NameFormat = "timestamp" | "sequence";

// 录制画质三档（与后端 recorder::QUALITY_PRESETS 同值同序）。
// 数字是「质量档位键」不是承诺码率：后端按档位映射到 VBR/CQ，实际体积随画面自适应。
export const QUALITY_PRESETS: { label: string; desc: string; k: number }[] = [
  { label: "低", desc: "省空间", k: 2000 },
  { label: "中", desc: "够用（默认）", k: 8000 },
  { label: "高", desc: "细节多，体积大", k: 20000 },
];

// 旧配置里的自由码率落到三档：取「不超过 k 的最高档」，比最低档还小则取最低档。
// 规则必须与后端 recorder::preset_for 一致，否则界面显示的档与后端实际跑的档不是一档
// （D11 的根因：滑块写 24000、后端落 25000，界面与实况脱节）。
export function presetFor(k: number): number {
  let hit = QUALITY_PRESETS[0].k;
  for (const p of QUALITY_PRESETS) if (p.k <= k) hit = p.k;
  return hit;
}

// 语言档（Language）2026-09-21 删除：后端已删字段，UI 文案全中文，没有 i18n 基础设施。

export interface Settings {
  token: string;
  port: number;
  // 配置文件结构版本（后端 CURRENT_CONFIG_VERSION，当前 1）。前端只读不写：
  // 旧配置（version=0）由后端 load_or_init 做一次性迁移，前端不该自造版本号。
  config_version: number;

  // 系统
  auto_start: boolean;
  close_to_tray: boolean; // 关闭窗口=隐藏到托盘
  run_as_admin: boolean; // 重启时以管理员运行

  // 截图
  capture_copy_after: boolean;
  capture_auto_save: boolean;
  capture_ui_detect: boolean;
  capture_show_size_tag: boolean;
  capture_confirm_save: boolean; // 保存前确认：true=弹保存对话框可选路径，false=直接快速保存
  longshot_scroll_notches: number; // 长截图每步滚轮档数（1-5）：越大每步跳得越远
  longshot_settle_ms: number; // 长截图每步等待上限（毫秒，100-3000）：画面落定即提前下一步

  // 外观
  desktop_ball: boolean; // 桌面悬浮球
  ball_x: number;
  ball_y: number;

  // 贴图
  pin_opacity: number;
  pin_wheel_zoom: boolean;
  pin_shadow: boolean;

  // 保存
  save_dir: string;
  save_name_format: NameFormat;
  save_format_png: boolean;
  jpeg_quality: number; // JPEG 截图质量（40-100，越大越清晰体积越大）；后端 clamp 后回写

  // 标注
  annotate_color: string;
  annotate_stroke_width: number;

  // 录制
  record_fps: number;
  record_bitrate_k: number;
  record_countdown: number;
  record_audio: boolean; // 录屏时录音频（dshow 输入；GIF 模式恒不录）
  record_audio_device: string; // 音频设备名；空 = 自动取第一个可用设备

  // OCR
  ocr_auto_copy: boolean;

  // 剪贴板
  clip_enabled: boolean; // 记录剪贴板历史总开关
  clip_max: number; // 历史条数上限（10-1000，默认 100）
  clip_ignore_apps: string; // 这些应用里复制的内容不入库（进程名多行）；留空=全部记录
  clip_skip_sensitive: boolean; // 自动识别私钥/Token 等机密不写入历史
  clip_search_focus: boolean; // 面板被用户激活后把光标放进搜索框（打开面板时不再抢焦）
  clip_search_clear_on_hide: boolean; // 关闭面板时清空搜索词
  clip_auto_paste: boolean; // 点击条目后自动粘进上个窗口
  clip_close_after_paste: boolean; // 粘贴后自动收起面板（关=留着连续粘贴，手动关）
  clip_paste_trigger: "single" | "double"; // 粘贴触发方式（仅 auto_paste 开启时有意义）
  clip_paste_plain: boolean; // 支持纯文本粘贴（面板内 Shift+Enter）
  clip_expire_days: number; // 过期天数，0=永不过期
  clip_hide_on_blur: boolean; // 点击面板外自动关闭
  clip_list_density: "compact" | "comfortable"; // 列表密度两档行高

  // 快捷键重绑定：键如 "capture"/"pin"/"record"，值如 "Ctrl+1"；空/缺失 = 用内置默认
  hotkeys: Record<string, string>;
  hotkeys_disabled: boolean; // 手动禁用全部全局热键
  hotkeys_disable_fullscreen: boolean; // 全屏程序前台时自动禁用热键，退出全屏恢复
}

export const DEFAULTS: Settings = {
  token: "",
  port: 47821,
  config_version: 1,
  auto_start: false,
  close_to_tray: false,
  run_as_admin: false,
  capture_copy_after: true,
  capture_auto_save: true,
  capture_ui_detect: true,
  capture_show_size_tag: true,
  capture_confirm_save: true,
  longshot_scroll_notches: 1,
  longshot_settle_ms: 400,
  desktop_ball: false,
  ball_x: -1,
  ball_y: -1,
  pin_opacity: 100,
  pin_wheel_zoom: true,
  pin_shadow: true,
  save_dir: "",
  save_name_format: "timestamp",
  save_format_png: true,
  jpeg_quality: 92,
  annotate_color: "#EF4444",
  annotate_stroke_width: 3,
  record_fps: 60,
  // 码率只剩三档（后端 QUALITY_PRESETS 同值），默认取「中」；自由值会被设置页落回三档。
  record_bitrate_k: 8000,
  record_countdown: 0,
  // 默认开：录屏没声音曾被当成默认值（后端 config_version=1 已把旧配置的 false 迁成 true）。
  record_audio: true,
  record_audio_device: "",
  ocr_auto_copy: true,
  clip_enabled: true,
  clip_max: 100,
  clip_ignore_apps: "",
  clip_skip_sensitive: true,
  clip_search_focus: true,
  clip_search_clear_on_hide: true,
  clip_auto_paste: false,
  clip_close_after_paste: true,
  clip_paste_trigger: "double",
  clip_paste_plain: true,
  clip_expire_days: 0,
  clip_hide_on_blur: true,
  clip_list_density: "comfortable",
  hotkeys: {},
  hotkeys_disabled: false,
  hotkeys_disable_fullscreen: false,
};
