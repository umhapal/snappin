//! SnapPin 主进程装配：托盘常驻、单实例、全局快捷键、内置 HTTP、截图/录屏命令。
//! DPI：依赖 Tauri 内嵌 manifest 的 Per-Monitor-V2 感知，capture 拿到物理像素。

mod capture;
mod clip;
mod commands;
mod config;
mod hotkeys;
mod http;
mod longshot;
mod loopback;
mod ocr;
mod pin;
mod recorder;
mod system;
mod uia;
mod window;
mod window_recorder;
mod window_target;

use base64::Engine;
use commands::RecordState;
use pin::PinState;
use std::sync::LazyLock;
use tauri::{
    menu::{CheckMenuItem, Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_global_shortcut::ShortcutState;

/// 把 `log` crate 的日志落到 stderr。
/// 为什么必须有：wry 的建窗/建 WebView 失败**只走 `log::error!`**（见
/// tauri-runtime-wry `Message::CreateWindow` 分支），没有 logger 时完全静默——
/// 命令返回 Ok、窗口不存在、无任何报错。2026-09-21 排查 overlay 建不出来时正是被这层吞掉。
/// 级别用 `SNAPPIN_LOG=error|warn|info|debug` 控制，默认 warn。
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, record: &log::Record) {
        eprintln!(
            "[{}] {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
    }
    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

/// WebView2 浏览器参数——**所有窗口必须传同一份**。
///
/// 为什么（2026-09-21 定案）：WebView2 对同一个 user data folder 只允许一套参数，
/// 参数不一致时 `CreateCoreWebView2EnvironmentWithOptions` 直接失败（0x8007139F）；
/// 而 wry 的建窗失败只走 `log::error!`（见 tauri-runtime-wry `Message::CreateWindow` 分支），
/// 表现为「命令返回 Ok、窗口永远不出现、零报错」。此前 CDP 端口只写在
/// `tauri.conf.json` 的主窗上，overlay / pin 用 wry 默认参数 → 与主窗不一致 →
/// 这两个窗口全部建不出来（用户报「Ctrl+1 / F1 没反应」的真因）。
///
/// 哨兵文件 `%TEMP%\snappin-cdp-flag` 存在时才带 `--remote-debugging-port=9333`；
/// 启动时解析一次并缓存，避免中途删哨兵导致已建环境与新窗口的参数漂移。
static BROWSER_ARGS: LazyLock<String> = LazyLock::new(|| {
    let base = "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";
    if std::env::temp_dir().join("snappin-cdp-flag").exists() {
        format!("{base} --remote-debugging-port=9333")
    } else {
        base.to_string()
    }
});

/// 本进程统一的 WebView2 参数（主窗 / overlay / pin 三处都用它）。
pub fn browser_args() -> &'static str {
    BROWSER_ARGS.as_str()
}

fn show_main(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    }
}

/// 取当前武装的录制参数（overlay 里选区已提交时由前端推上来）。
///
/// **只在 overlay 可见时有效**（2026-09-21 实测定案）：`close_overlay` 走的是 `win.hide()`，
/// 窗口不销毁 → React 不卸载 → Overlay 里 `arm_record` 那个 effect 的 cleanup（`disarm_record`）
/// 不会执行，武装态就残留下来。残留的后果是用户下次按截图键时，热键优先分支拿旧选区
/// **直接开录**（现象：截图键不弹 overlay、却悄悄录出一段），录屏热键同理不落全屏。
/// 这里按「overlay 可见」判定，一次覆盖所有把 overlay 藏起来的路径（截图完成 / 贴图 / 长截图 / 录制收尾）。
fn armed_record(app: &tauri::AppHandle) -> Option<commands::ArmInfo> {
    let overlay_visible = app
        .get_webview_window("overlay")
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
    if !overlay_visible {
        return None;
    }
    app.state::<commands::RecordArmState>()
        .0
        .lock()
        .ok()
        .and_then(|g| g.clone())
}

/// 录制是否在进行中。
fn is_recording(app: &tauri::AppHandle) -> bool {
    app.state::<RecordState>()
        .0
        .lock()
        .map(|g| g.is_some())
        .unwrap_or(false)
}

/// 开始**全屏**录制，并把状态推给 overlay 页面。
///
/// 选区录制不从这里走：选区/帧率/码率只有前端知道，统一由 Overlay 的 `beginRecord()`
/// 发起（含 `record_countdown` 倒计时），后端只发 `snappin-arm-start` 信号（见该函数）。
fn start_recording(app: &tauri::AppHandle) -> Result<String, String> {
    let path = commands::start_record_impl(
        app,
        app.state::<RecordState>().inner(),
        app.state::<commands::ConfigState>().inner(),
        None,
        None,
        None,
        None,
    )?;
    // 让 overlay 页面同步出录制条（热键开的录制不是页面自己发起的，它不知道）。
    let _ = app.emit_to(
        "overlay",
        "snappin-record",
        serde_json::json!({ "recording": true, "gif": false }),
    );
    Ok(path)
}

/// 通知 overlay「该开录了」：前端 `beginRecord()` 收到后按 `record_countdown` 倒计时再开录。
///
/// 为什么不让后端直接 `start_record`：倒计时要在选区上显示大号秒数，而选区/录制模式/帧率/
/// 码率**只有前端知道**；后端多发一份参数就会与前端的武装状态漂移。事件不带参数，
/// 前端用自己那份 state 即可（契约见 Overlay.tsx::beginRecord）。
/// 武装态本身已由 `armed_record` 保证「overlay 可见且有选区」，所以这条事件一定能被页面收到。
fn notify_overlay_arm_start(app: &tauri::AppHandle) {
    if let Err(e) = app.emit_to("overlay", "snappin-arm-start", ()) {
        eprintln!("[snappin] 通知 overlay 开始录制失败: {e}");
    }
}

/// 暂停/继续录制（热键动作 `record_pause` 与录制条按钮同义，2026-10-14 新增）。
///
/// **必须离开主线程**：`Recorder::pause` 要往 ffmpeg stdin 写 `q` 并 `child.wait()`
/// 等它退出（实测百毫秒级），热键 handler 在主线程上跑，同步调用会卡住整个 UI。
/// 收尾/停止路径同理（见 `stop_recording`）。
fn toggle_record_pause(app: &tauri::AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        let (paused, err) = {
            let state = app.state::<RecordState>();
            let mut guard = match state.0.lock() {
                Ok(g) => g,
                Err(e) => {
                    eprintln!("[snappin] 暂停热键取状态失败: {e}");
                    return;
                }
            };
            let Some(rec) = guard.as_mut() else {
                eprintln!("[snappin] 暂停热键：当前没有录制，忽略");
                return;
            };
            let now_paused = !rec.is_paused();
            let r = if now_paused {
                rec.pause()
            } else {
                rec.resume()
            };
            match r {
                Ok(()) => (Some(now_paused), None),
                Err(e) => (None, Some(e)),
            }
        };
        match (paused, err) {
            (Some(p), None) => {
                eprintln!(
                    "[snappin] 暂停热键：{}",
                    if p { "已暂停" } else { "已继续" }
                );
                // 前端据此切换按钮文案与计时基准（它看不到后端状态，必须由事件驱动）。
                let _ = app.emit_to(
                    "overlay",
                    "snappin-rec-paused",
                    serde_json::json!({ "paused": p }),
                );
            }
            (_, Some(e)) => eprintln!("[snappin] 暂停热键失败: {e}"),
            _ => {}
        }
    });
}

/// 停止录制并把状态推给 overlay 页面。
///
/// **收尾必须离开主线程**（2026-09-21 用户实测「停止后卡死/无法立刻再截图」）：
/// `Recorder::stop` 要等 ffmpeg 退出，GIF 还要同步跑调色板转码（长录几十秒）。
/// 本函数由热键/托盘在**主线程**上调用，所以先把状态取走（立即让 is_recording=false），
/// 再 spawn 线程做收尾与转码，完成后 emit 结果事件。
fn stop_recording(app: &tauri::AppHandle) -> Result<String, String> {
    let rec = {
        let state = app.state::<RecordState>();
        let mut guard = state.0.lock().map_err(|e| e.to_string())?;
        guard.take()
    };
    let rec = rec.ok_or_else(|| "当前没有录制".to_string())?;
    let app2 = app.clone();
    std::thread::spawn(move || {
        match rec.stop(|pct| commands::emit_transcode(&app2, true, pct)) {
            Ok(p) => {
                commands::emit_transcode(&app2, false, 100);
                let p = p.to_string_lossy().to_string();
                eprintln!("[snappin] 录制收尾完成 → {p}");
                let _ = app2.emit_to(
                    "overlay",
                    "snappin-record",
                    serde_json::json!({ "recording": false, "path": p }),
                );
            }
            Err(e) => {
                commands::emit_transcode(&app2, false, 100);
                eprintln!("[snappin] 录制收尾失败: {e}");
                let _ = app2.emit_to(
                    "overlay",
                    "snappin-record",
                    serde_json::json!({ "recording": false, "error": e }),
                );
            }
        }
        // 收尾结束（成败都算已停）→ 恢复悬浮球（A 方案 2026-09-23）。
        commands::spawn_ball_restore(app2);
    });
    Ok(String::new())
}

/// 截图动作：热键与托盘菜单共用同一路径。
///
/// 优先级（2026-09-21 用户点名要求，PixPin 同款手感）：
/// 录制中 → 停止；overlay 已武装（选区已定）→ 开始录制；否则 → 打开 overlay 框选。
pub fn trigger_capture(app: &tauri::AppHandle) {
    let app = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        if is_recording(&app) {
            match stop_recording(&app) {
                Ok(p) => eprintln!("[snappin] 截图热键：停止录制 → {p}"),
                Err(e) => eprintln!("[snappin] 截图热键停止录制失败: {e}"),
            }
            return;
        }
        if armed_record(&app).is_some() {
            // 武装态 = overlay 开着且选区已定：**必须走前端统一入口**，
            // 否则 `record_countdown` 倒计时会被绕过（后端直接开录，页面上什么都没有）。
            notify_overlay_arm_start(&app);
            return;
        }
        if let Err(e) = commands::open_overlay(app.clone()) {
            eprintln!("[snappin] 打开 overlay 失败: {e}");
        }
    });
}

/// 贴图动作（热键 Ctrl+2 / 托盘菜单「贴图」）：把**剪贴板里的图片**钉成置顶小窗。
///
/// 为什么是这个语义：PixPin 的贴图键就是「贴剪贴板里的图」；本项目此前这里是空实现
/// （只 eprintln），用户按下没任何反应——属于「界面承诺、代码不兑现」。
/// 位置取当前光标处；剪贴板没有图片时打日志说明原因（不静默）。
/// 建窗必须回主线程（同 pin_image 的约束）。
pub(crate) fn trigger_pin(app: &tauri::AppHandle) {
    let img = match capture::clipboard_image() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[snappin] 贴图：{e}");
            return;
        }
    };
    let png = match capture::png_bytes(&img) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[snappin] 贴图：PNG 编码失败 {e}");
            return;
        }
    };
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
    let (x, y) = cursor_pos();
    let app2 = app.clone();
    let _ = app.clone().run_on_main_thread(move || {
        match commands::pin_image(app2.clone(), app2.state(), b64, x, y) {
            Ok(label) => eprintln!("[snappin] 贴图：剪贴板图片 → {label} @{x},{y}"),
            Err(e) => eprintln!("[snappin] 贴图失败: {e}"),
        }
    });
}

/// 当前光标位置（物理像素，虚拟桌面坐标）。
fn cursor_pos() -> (i32, i32) {
    let mut p = windows::Win32::Foundation::POINT { x: 0, y: 0 };
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut p);
    }
    (p.x, p.y)
}

/// 桌面悬浮球开关（托盘菜单）：开着就关掉，关着就显示；状态写回配置。
fn toggle_ball(app: &tauri::AppHandle) {
    let open = app.get_webview_window("ball").is_some();
    let res = if open {
        commands::close_ball(app.clone())
    } else {
        commands::open_ball(app.clone())
    };
    match res {
        Ok(()) => {
            if let Some(state) = app.try_state::<commands::ConfigState>() {
                if let Ok(mut cfg) = state.0.lock() {
                    cfg.desktop_ball = !open;
                    let dir = app
                        .path()
                        .app_config_dir()
                        .unwrap_or_else(|_| std::env::temp_dir().join("snappin"));
                    let _ = cfg.save(&dir.join("config.json"));
                }
            }
            eprintln!(
                "[snappin] 桌面悬浮球: {}",
                if open { "已隐藏" } else { "已显示" }
            );
        }
        Err(e) => eprintln!("[snappin] 悬浮球切换失败: {e}"),
    }
}

/// 托盘勾选项：把配置里的布尔位取反、落盘、按新值重同步热键，返回新值。
///
/// 为什么返回值要写回勾选态：muda 的 `CheckMenuItem` 勾选态由应用维护（点一下不会自动翻转），
/// 不显式 `set_checked` 会出现「菜单勾了但配置没变」或反之的假同步。
fn toggle_hotkey_flag(
    app: &tauri::AppHandle,
    get: fn(&crate::config::AppConfig) -> bool,
    set: fn(&mut crate::config::AppConfig, bool),
) -> bool {
    let Some(state) = app.try_state::<commands::ConfigState>() else {
        return false;
    };
    let newval;
    // 锁内只改配置并落盘；**不能**在锁内调 hotkeys::sync / set_checked —— 两者都会 marshal 回
    // 主线程并阻塞等待，而主线程正在跑这个托盘事件闭包，拿别的线程持有这把锁就会死锁
    // （2026-10-15 审查发现）。改成「克隆快照 → 放锁 → 同步」。
    let snapshot = {
        let Ok(mut cfg) = state.0.lock() else {
            return false;
        };
        newval = !get(&cfg);
        set(&mut cfg, newval);
        let dir = app
            .path()
            .app_config_dir()
            .unwrap_or_else(|_| std::env::temp_dir().join("snappin"));
        let _ = cfg.save(&dir.join("config.json"));
        cfg.clone()
    };
    sync_hotkey_surfaces(app, &snapshot);
    newval
}

/// 托盘两个勾选项句柄（setup 时托管）：非托盘入口（设置页 / `PUT /v1/settings` / 重置设置）
/// 改了开关后必须回写勾选态，否则菜单显示旧状态、用户再点一下会把真实配置翻反
/// （2026-10-15 审查发现：只有托盘入口自己 set_checked，其他入口全脱钩）。
pub struct HotkeyTrayItems {
    pub disable: CheckMenuItem<tauri::Wry>,
    pub fullscreen: CheckMenuItem<tauri::Wry>,
}

/// 热键变更的**唯一出口**：注册/注销热键 + 回写托盘勾选态。
///
/// 为什么必须是一个出口：守卫开关有 6 个写入点（启动 / 托盘勾选 / `update_settings` /
/// `PUT /v1/settings` / `re_register_hotkeys` / 重置设置），任何一处漏写勾选态都会让托盘
/// 与真实配置脱钩。调用方**不得持 `ConfigState` 锁**（见上：sync 会 marshal 回主线程阻塞）。
pub fn sync_hotkey_surfaces(
    app: &tauri::AppHandle,
    cfg: &crate::config::AppConfig,
) -> Vec<hotkeys::HotkeyRegResult> {
    let res = hotkeys::sync(app, cfg);
    if let Some(items) = app.try_state::<HotkeyTrayItems>() {
        let _ = items.disable.set_checked(cfg.hotkeys_disabled);
        let _ = items.fullscreen.set_checked(cfg.hotkeys_disable_fullscreen);
    }
    res
}

/// 录屏热键：录制中则停；否则有武装选区按选区录，没有则全屏录。
pub(crate) fn toggle_record(app: &tauri::AppHandle) {
    if is_recording(app) {
        match stop_recording(app) {
            Ok(p) => eprintln!("[snappin] 录屏已停止 → {p}"),
            Err(e) => eprintln!("[snappin] 录屏错误: {e}"),
        }
        return;
    }
    // 武装态（overlay 开着 + 选区已定）：与截图热键同一条路——交给前端 beginRecord()，
    // 让 `record_countdown` 倒计时对两条热键一视同仁。全屏录制没有 overlay 可显示倒计时，
    // 所以只有这一条分支走前端。
    if armed_record(app).is_some() {
        notify_overlay_arm_start(app);
        return;
    }
    match start_recording(app) {
        Ok(p) => eprintln!("[snappin] 录屏开始（全屏） → {p}"),
        Err(e) => eprintln!("[snappin] 录屏错误: {e}"),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 日志：默认 warn，SNAPPIN_LOG=debug 看 wry/tao 内部细节（建窗失败的真正原因在这里）。
    let level = match std::env::var("SNAPPIN_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "error" => log::LevelFilter::Error,
        "info" => log::LevelFilter::Info,
        "debug" | "trace" => log::LevelFilter::Debug,
        _ => log::LevelFilter::Warn,
    };
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);

    // 管理员自提升：若磁盘配置 run_as_admin 且当前未提权，则以 runas 重启并退出本实例。
    // 放在建窗之前，避免非提权窗口先闪出来。
    system::maybe_elevate();

    let builder = tauri::Builder::default();
    // 单实例锁：同一 identifier 的第二实例会被插件直接 exit(0)（并激活已有实例的主窗）。
    // `SNAPPIN_NO_SINGLE_INSTANCE` 置位时**跳过注册**——与 SNAPPIN_OVERLAY_BOUNDS /
    // SNAPPIN_OVERLAY_NO_READY / SNAPPIN_UIA_DEBUG 同一档「无头验证钩子」：让隔离实例
    // 能在用户托盘实例仍在跑时共存（否则套件只能等用户退出托盘，验证无法无人值守）。
    // why-not 用不同 identifier 的独立构建：identifier 编译进二进制，为测试另编一份
    // = 测的不是同一份产物；一个运行时开关更省且不改变被测代码路径。
    let builder = if std::env::var_os("SNAPPIN_NO_SINGLE_INSTANCE").is_some() {
        builder
    } else {
        builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
    };
    builder
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if event.state() != ShortcutState::Pressed {
                        return;
                    }
                    // 热键已可重绑定：按下的 Shortcut 反查动作 id，再走与托盘菜单相同的路径。
                    let action = app
                        .try_state::<hotkeys::HotkeyState>()
                        .and_then(|s| s.action_for(shortcut));
                    match action.as_deref() {
                        Some("capture") => trigger_capture(app),
                        Some("pin") => trigger_pin(app),
                        Some("record") => toggle_record(app),
                        Some("clip") => {
                            let _ = crate::commands::toggle_clip_history(app.clone());
                        }
                        Some("record_pause") => toggle_record_pause(app),
                        _ => {}
                    }
                })
                .build(),
        )
        .manage(RecordState::default())
        .manage(PinState::default())
        .manage(clip::ClipState::default())
        .manage(hotkeys::HotkeyState::default())
        .manage(commands::RecordArmState::default())
        .invoke_handler(tauri::generate_handler![
            commands::capture_screen_data,
            commands::finish_capture,
            commands::pick_save_path,
            commands::save_file_as,
            commands::remove_file,
            commands::open_overlay,
            commands::overlay_ready,
            commands::close_overlay,
            commands::show_overlay,
            commands::rec_input_passthrough,
            commands::debug_trigger_hotkey,
            commands::start_record,
            commands::stop_record,
            commands::pause_record,
            commands::resume_record,
            commands::list_audio_devices,
            commands::list_windows,
            commands::ui_element_at,
            commands::ui_chain_at,
            commands::pin_image,
            commands::take_pin_payload,
            commands::close_pin,
            commands::ocr_image,
            commands::capture_long,
            commands::get_settings,
            commands::update_settings,
            commands::reset_settings,
            commands::re_register_hotkeys,
            commands::arm_record,
            commands::disarm_record,
            commands::ocr_region,
            commands::open_ball,
            commands::close_ball,
            commands::set_ball_pos,
            commands::show_main_window,
            commands::trigger_capture_now,
            commands::toggle_clip_history,
            commands::clip_list,
            commands::clip_copy_back,
            commands::clip_remove,
            commands::clip_clear,
            commands::clip_toggle_pin,
            commands::clip_export,
            commands::clip_paste,
            commands::copy_product,
            // D12：确认后才落盘——`finish_capture` 只写 `.pending/` 暂存件，
            // 用户点「快速保存」才由 `commit_capture` 搬进最终目录。
            commands::commit_capture,
            // 贴图产物此前没有任何保存入口（只在内存 PinState 里）。
            commands::pin_save,
            // D13：剪贴板面板默认不抢焦；用户主动交互时调它把键盘焦点给面板。
            commands::clip_activate_panel,
            // D9：动图条目的本体（多帧）懒加载用。
            clip::clip_animated,
        ])
        .setup(move |app| {
            // 主窗在代码里建（而非 tauri.conf.json 的 app.windows）：只有代码建窗才能让
            // 主窗与 overlay / pin 用**同一份** WebView2 参数（见 browser_args 注释）。
            // 启动即驻托盘（2026-09-23 用户定档）：手动打开与开机自启都直接进托盘，
            // 主界面只经右键托盘菜单「显示主界面」打开（二次启动激活走 single_instance→show_main）。
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("SnapPin")
                .inner_size(800.0, 600.0)
                .visible(false)
                .additional_browser_args(browser_args())
                .build()?;

            let config_dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("snappin"));
            let cfg = config::load_or_init(&config_dir);
            eprintln!("[snappin] 配置目录: {}", config_dir.display());
            // 共享配置状态：HTTP 线程拿克隆、Tauri 命令经 State<ConfigState> 读写。
            let config_state = std::sync::Arc::new(std::sync::Mutex::new(cfg.clone()));
            app.manage(commands::ConfigState(config_state.clone()));
            // 按配置注册全局热键（缺省回内置默认；冲突回退并**打印失败原因**——
            // 静默回退曾是「热键改了没反应」根因，2026-09-24 起注册结果全可见）。
            // 走 sync 而非 register_all：它先套热键守卫（手动禁用 / 全屏自动禁用）。
            for r in hotkeys::sync(app.handle(), &cfg) {
                if !r.ok {
                    eprintln!(
                        "[snappin] 启动热键 {} 未生效: {}",
                        r.action,
                        r.error.as_deref().unwrap_or("未知原因")
                    );
                }
            }
            system::set_autostart(cfg.auto_start);
            // 画质是进程级状态（capture.rs 的静态量）：启动时按配置同步一次，
            // 否则桌面 UI 路径会一直用默认值 92，设置的 JPEG 画质只在 HTTP 路径生效。
            capture::apply_config(&cfg);
            // 清掉上次运行遗留的暂存件（`.pending/` 里 1 小时前的）：进程被强杀时
            // 暂存件会留下，而它就躺在用户的产物目录里。
            commands::purge_stale_pending(app.handle());
            // 产物目录进 asset 协议白名单（保存确认阶段 mp4/gif 回放预览）。
            commands::allow_asset_captures(app.handle());
            http::spawn(cfg.clone(), app.handle().clone());
            // 桌面悬浮球：上次开着就继续开着（P1，见 docs/UI设计规范.md §1.5 real-7）。
            if cfg.desktop_ball {
                if let Err(e) = commands::open_ball(app.handle().clone()) {
                    eprintln!("[snappin] 悬浮球创建失败: {e}");
                }
            }
            // 预热 UIA 常驻线程：把 COM/CUIAutomation 的冷启动成本挪到启动阶段，
            // 免得用户第一次悬停时首个查询吃满超时（见 uia::warmup）。
            uia::warmup();

            // 剪贴板历史：注册捕获 AppHandle + 主窗挂 WM_CLIPBOARDUPDATE 监听
            // + 启动时恢复持久化历史。
            clip::register_capture_app(app.handle().clone());
            if let Some(main) = app.get_webview_window("main") {
                clip::init_listener(&main);
            }
            if let Some(state) = app.try_state::<clip::ClipState>() {
                if let Ok(mut g) = state.inner().0.lock() {
                    *g = clip::load_history(app.handle());
                }
            }

            let show_i = MenuItem::with_id(app, "show", "显示主界面", true, None::<&str>)?;
            let capture_i = MenuItem::with_id(app, "capture", "截屏", true, None::<&str>)?;
            let pin_i = MenuItem::with_id(app, "pin", "贴图", true, None::<&str>)?;
            let clip_i = MenuItem::with_id(app, "clip", "剪贴板历史", true, None::<&str>)?;
            let ball_i = MenuItem::with_id(app, "ball", "桌面悬浮球", true, None::<&str>)?;
            // 热键守卫两个勾选项（PixPin 同款），位置：悬浮球后、退出前。
            // 初始勾选态取自持久化配置——重启后与设置页保持一致。
            let disable_hk_i = CheckMenuItem::with_id(
                app,
                "hotkeys_disabled",
                "禁用快捷键",
                true,
                cfg.hotkeys_disabled,
                None::<&str>,
            )?;
            let fsc_hk_i = CheckMenuItem::with_id(
                app,
                "hotkeys_disable_fullscreen",
                "全屏程序时自动禁用快捷键",
                true,
                cfg.hotkeys_disable_fullscreen,
                None::<&str>,
            )?;
            let quit_i = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &show_i,
                    &capture_i,
                    &pin_i,
                    &clip_i,
                    &ball_i,
                    &disable_hk_i,
                    &fsc_hk_i,
                    &quit_i,
                ],
            )?;
            // 勾选项句柄交给托管状态：非托盘入口（设置页 / PUT /v1/settings / 重置）改开关
            // 后由 sync_hotkey_surfaces 统一回写勾选态，托盘入口不再自己 set_checked。
            app.manage(HotkeyTrayItems {
                disable: disable_hk_i.clone(),
                fullscreen: fsc_hk_i.clone(),
            });
            let _tray = TrayIconBuilder::with_id("main-tray")
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("SnapPin")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "show" => show_main(app),
                    "capture" => trigger_capture(app),
                    "pin" => trigger_pin(app),
                    "clip" => {
                        let _ = commands::toggle_clip_history(app.clone());
                    }
                    "ball" => toggle_ball(app),
                    "hotkeys_disabled" => {
                        toggle_hotkey_flag(
                            app,
                            |c| c.hotkeys_disabled,
                            |c, v| c.hotkeys_disabled = v,
                        );
                    }
                    "hotkeys_disable_fullscreen" => {
                        toggle_hotkey_flag(
                            app,
                            |c| c.hotkeys_disable_fullscreen,
                            |c, v| c.hotkeys_disable_fullscreen = v,
                        );
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                // 不挂 on_tray_icon_event：左键点托盘不做任何事，打开界面唯一入口 =
                // 右键菜单「显示主界面」（2026-09-23 用户定档）。
                .build(app)?;

            // 全屏守卫轮询：开关可随时从设置页/托盘变，故常驻每秒查一次前台全屏状态，
            // **状态变化才 sync**（不变就不重注册，避免每秒注销+注册热键）。
            {
                let handle = app.handle().clone();
                std::thread::spawn(move || {
                    let mut last_active = false;
                    loop {
                        std::thread::sleep(std::time::Duration::from_millis(1000));
                        // **快照式取配置**：`hotkeys::sync` 与 `set_checked` 都会 marshal 回主线程
                        // 并阻塞等待（global-shortcut 插件与 tray 菜单的 run_main_thread! =
                        // marshal + rx.recv），持锁等主线程时若主线程正来抢这把锁（托盘点击行、
                        // 设置命令行）→ 全进程死锁（2026-10-15 审查发现）。先克隆、放锁，再同步。
                        // 单轮 panic 不能杀掉守卫线程（否则用户看到的是「全屏守卫莫名失效」）：
                        // panic 隔离在本轮内，下一轮继续。
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let snapshot = match handle.try_state::<commands::ConfigState>() {
                                Some(s) => match s.0.lock() {
                                    Ok(c) => c.clone(),
                                    Err(_) => return,
                                },
                                None => return,
                            };
                            let active = snapshot.hotkeys_disable_fullscreen
                                && system::is_fullscreen_foreground();
                            if active != last_active {
                                last_active = active;
                                sync_hotkey_surfaces(&handle, &snapshot);
                            }
                        }));
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            // 关闭到托盘：仅拦主窗的关闭请求；close_to_tray=true 则隐藏，否则真退出。
            // 托盘「退出」走 app.exit(0)，不经此路径，故强制退出不受影响。
            if window.label() != "main" {
                return;
            }
            if let WindowEvent::CloseRequested { api, .. } = event {
                let hide = window
                    .app_handle()
                    .try_state::<commands::ConfigState>()
                    .and_then(|s| s.0.lock().ok().map(|c| c.close_to_tray))
                    .unwrap_or(false);
                if hide {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
