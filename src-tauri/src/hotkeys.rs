//! 全局热键重绑定：把 config.hotkeys 里的 "Ctrl+1" 等字符串解析为 Shortcut，
//! 缺失/非法回内置默认；启动与配置变更时先 unregister_all 再按解析结果注册，
//! 单键冲突/注册失败回退该动作默认键并记日志。注册结果写入 HotkeyState 供
//! 热键 handler 反查「按下的是哪个动作」。

use crate::config::AppConfig;
use std::sync::Mutex;
use tauri::{AppHandle, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut};

/// 内置默认键：截图 Ctrl+1 / 贴图 Ctrl+2 / 录屏 Ctrl+Shift+R。
pub const DEFAULT_CAPTURE: &str = "Ctrl+1";
pub const DEFAULT_PIN: &str = "Ctrl+2";
pub const DEFAULT_RECORD: &str = "Ctrl+Shift+R";
pub const DEFAULT_CLIP: &str = "Ctrl+Shift+V";

/// 支持的动作 id（与前端 config.hotkeys 的键一致）及其内置默认键。
/// 默认键为空 = 默认不占任何键（用户想用就在设置页自定义）——「暂停/继续录制」就是这种，
/// 全局占键会抢别人程序的热键，不给默认值。
pub const ACTIONS: [(&str, &str); 5] = [
    ("capture", DEFAULT_CAPTURE),
    ("pin", DEFAULT_PIN),
    ("record", DEFAULT_RECORD),
    ("clip", DEFAULT_CLIP),
    ("record_pause", ""),
];

/// 已注册热键表：(Shortcut, 动作 id)。handler 反查按下的 Shortcut 属于哪个动作。
#[derive(Default)]
pub struct HotkeyState(pub Mutex<Vec<(Shortcut, String)>>);

impl HotkeyState {
    /// 反查某个按下的 Shortcut 对应的动作 id。
    pub fn action_for(&self, sc: &Shortcut) -> Option<String> {
        let g = self.0.lock().ok()?;
        g.iter().find(|(s, _)| s == sc).map(|(_, a)| a.clone())
    }
}

/// 单个动作的注册结果，经 `re_register_hotkeys` 回给前端。
/// `requested`=用户想要的键（空=没配走默认），`effective`=真正生效的键（空=彻底没注册上）。
/// 注册失败曾**静默回退默认键**，UI 却显示用户填的键——「热键改了没反应」的根因
/// （2026-09-24 用户实报「剪贴板快捷键不能更换」），失败必须可见。
#[derive(serde::Serialize, Clone, Debug)]
pub struct HotkeyRegResult {
    pub action: String,
    pub requested: String,
    pub effective: String,
    pub ok: bool,
    pub error: Option<String>,
}

/// 把 "Ctrl+Shift+R" 之类字符串解析为 Shortcut。
/// 修饰符支持 Ctrl/Control、Shift、Alt/Option、Super/Win/Meta/Cmd/Command（大小写不敏感）；
/// 主键支持单个字母/数字、F1–F24 及常见功能键名。无法识别返回 None。
pub fn parse_shortcut(s: &str) -> Option<Shortcut> {
    let mut mods = Modifiers::empty();
    let mut key: Option<Code> = None;
    for part in s.split('+') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        match p.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= Modifiers::CONTROL,
            "shift" => mods |= Modifiers::SHIFT,
            "alt" | "option" => mods |= Modifiers::ALT,
            "super" | "win" | "meta" | "cmd" | "command" => mods |= Modifiers::SUPER,
            other => {
                // 主键只允许出现一次；多写视为非法。
                if key.is_some() {
                    return None;
                }
                key = Some(parse_code(other)?);
            }
        }
    }
    let code = key?;
    let m = if mods.is_empty() { None } else { Some(mods) };
    Some(Shortcut::new(m, code))
}

/// 主键名 → Code。单字符字母/数字直接映射，其余走功能键名表。
fn parse_code(s: &str) -> Option<Code> {
    if s.chars().count() == 1 {
        let c = s.chars().next().unwrap();
        if c.is_ascii_alphabetic() {
            return letter_code(c.to_ascii_uppercase());
        }
        if c.is_ascii_digit() {
            return digit_code(c);
        }
    }
    match s {
        "f1" => Some(Code::F1),
        "f2" => Some(Code::F2),
        "f3" => Some(Code::F3),
        "f4" => Some(Code::F4),
        "f5" => Some(Code::F5),
        "f6" => Some(Code::F6),
        "f7" => Some(Code::F7),
        "f8" => Some(Code::F8),
        "f9" => Some(Code::F9),
        "f10" => Some(Code::F10),
        "f11" => Some(Code::F11),
        "f12" => Some(Code::F12),
        // F13-F24：global-hotkey 全支持（Windows VK_F13..VK_F24），编辑器 e.code
        // 也产 "F13" 形——缺这些键位会让用户选了 F 区键后静默回退默认（同「改不了」暗根因）。
        "f13" => Some(Code::F13),
        "f14" => Some(Code::F14),
        "f15" => Some(Code::F15),
        "f16" => Some(Code::F16),
        "f17" => Some(Code::F17),
        "f18" => Some(Code::F18),
        "f19" => Some(Code::F19),
        "f20" => Some(Code::F20),
        "f21" => Some(Code::F21),
        "f22" => Some(Code::F22),
        "f23" => Some(Code::F23),
        "f24" => Some(Code::F24),
        "space" => Some(Code::Space),
        "enter" | "return" => Some(Code::Enter),
        "tab" => Some(Code::Tab),
        "esc" | "escape" => Some(Code::Escape),
        "backspace" => Some(Code::Backspace),
        "up" | "arrowup" => Some(Code::ArrowUp),
        "down" | "arrowdown" => Some(Code::ArrowDown),
        "left" | "arrowleft" => Some(Code::ArrowLeft),
        "right" | "arrowright" => Some(Code::ArrowRight),
        "printscreen" | "prtsc" | "prtscn" => Some(Code::PrintScreen),
        "insert" | "ins" => Some(Code::Insert),
        "delete" | "del" => Some(Code::Delete),
        "home" => Some(Code::Home),
        "end" => Some(Code::End),
        "pageup" | "pgup" => Some(Code::PageUp),
        "pagedown" | "pgdn" => Some(Code::PageDown),
        "-" | "minus" => Some(Code::Minus),
        "=" | "equal" => Some(Code::Equal),
        "`" | "backquote" => Some(Code::Backquote),
        // 标点/小键盘：编辑器改用 e.code 物理键名后（Shift+/ 产 "?" 这类符号键值过不了
        // 解析 = 又一条「热键改了没反应」暗根因），这些名字必须能解析。
        "/" | "slash" => Some(Code::Slash),
        "\\" | "backslash" => Some(Code::Backslash),
        "," | "comma" => Some(Code::Comma),
        "." | "period" => Some(Code::Period),
        ";" | "semicolon" => Some(Code::Semicolon),
        "'" | "quote" => Some(Code::Quote),
        "[" | "bracketleft" => Some(Code::BracketLeft),
        "]" | "bracketright" => Some(Code::BracketRight),
        "numpad0" => Some(Code::Numpad0),
        "numpad1" => Some(Code::Numpad1),
        "numpad2" => Some(Code::Numpad2),
        "numpad3" => Some(Code::Numpad3),
        "numpad4" => Some(Code::Numpad4),
        "numpad5" => Some(Code::Numpad5),
        "numpad6" => Some(Code::Numpad6),
        "numpad7" => Some(Code::Numpad7),
        "numpad8" => Some(Code::Numpad8),
        "numpad9" => Some(Code::Numpad9),
        "numpadadd" => Some(Code::NumpadAdd),
        "numpadsubtract" => Some(Code::NumpadSubtract),
        "numpadmultiply" => Some(Code::NumpadMultiply),
        "numpaddivide" => Some(Code::NumpadDivide),
        _ => None,
    }
}

/// 'A'..'Z' → Code::KeyA..KeyZ。
fn letter_code(c: char) -> Option<Code> {
    Some(match c {
        'A' => Code::KeyA,
        'B' => Code::KeyB,
        'C' => Code::KeyC,
        'D' => Code::KeyD,
        'E' => Code::KeyE,
        'F' => Code::KeyF,
        'G' => Code::KeyG,
        'H' => Code::KeyH,
        'I' => Code::KeyI,
        'J' => Code::KeyJ,
        'K' => Code::KeyK,
        'L' => Code::KeyL,
        'M' => Code::KeyM,
        'N' => Code::KeyN,
        'O' => Code::KeyO,
        'P' => Code::KeyP,
        'Q' => Code::KeyQ,
        'R' => Code::KeyR,
        'S' => Code::KeyS,
        'T' => Code::KeyT,
        'U' => Code::KeyU,
        'V' => Code::KeyV,
        'W' => Code::KeyW,
        'X' => Code::KeyX,
        'Y' => Code::KeyY,
        'Z' => Code::KeyZ,
        _ => return None,
    })
}

/// '0'..'9' → Code::Digit0..Digit9。
fn digit_code(c: char) -> Option<Code> {
    Some(match c {
        '0' => Code::Digit0,
        '1' => Code::Digit1,
        '2' => Code::Digit2,
        '3' => Code::Digit3,
        '4' => Code::Digit4,
        '5' => Code::Digit5,
        '6' => Code::Digit6,
        '7' => Code::Digit7,
        '8' => Code::Digit8,
        '9' => Code::Digit9,
        _ => return None,
    })
}

/// 先 unregister_all，再按 config 注册全部动作热键；把最终生效的 (Shortcut, 动作)
/// 一次性 unregister_all 后按 config 注册全部动作，结果写入 HotkeyState 并返回；
/// 每个动作：优先配置值，解析失败/注册失败退默认键并把原因写进结果，**不再静默**。
pub fn register_all(app: &AppHandle, cfg: &AppConfig) -> Vec<HotkeyRegResult> {
    let gs = app.global_shortcut();
    if let Err(e) = gs.unregister_all() {
        eprintln!("[snappin] 热键 unregister_all 失败: {e}");
    }

    let mut table: Vec<(Shortcut, String)> = Vec::new();
    let mut results: Vec<HotkeyRegResult> = Vec::new();
    for (action, default_str) in ACTIONS {
        let requested = cfg.hotkeys.get(action).cloned().unwrap_or_default();
        // 可选动作（默认键为空且用户没配）：直接跳过，**不算失败**——否则设置页会给它
        // 报一条「默认键 \"\" 无法解析」的假错误。
        if requested.is_empty() && default_str.is_empty() {
            results.push(HotkeyRegResult {
                action: action.to_string(),
                requested,
                effective: String::new(),
                ok: true,
                error: None,
            });
            continue;
        }
        let default_sc = parse_shortcut(default_str);
        let mut effective = String::new();
        let mut error: Option<String> = None;

        // 首选：配置里的自定义键（解析成功 + 注册成功才算生效）。
        if !requested.is_empty() {
            match parse_shortcut(&requested) {
                Some(sc) => match gs.register(sc) {
                    Ok(()) => {
                        table.push((sc, action.to_string()));
                        effective = requested.clone();
                    }
                    Err(e) => {
                        eprintln!("[snappin] 热键 {action} 注册失败({requested:?}): {e}");
                        error = Some(format!(
                            "「{requested}」注册失败（可能被其他程序占用）：{e}"
                        ));
                    }
                },
                None => {
                    eprintln!("[snappin] 热键 {action} 配置值 \"{requested}\" 无法解析，回退默认键 {default_str}");
                    error = Some(format!("「{requested}」不被支持（无法解析），回退默认"));
                }
            }
        }

        // 自定义键没生效（或压根没配）→ 注册内置默认键。
        if effective.is_empty() {
            match default_sc {
                Some(dsc) => match gs.register(dsc) {
                    Ok(()) => {
                        table.push((dsc, action.to_string()));
                        effective = default_str.to_string();
                    }
                    Err(e) => {
                        eprintln!("[snappin] 热键 {action} 默认键注册失败: {e}");
                        error
                            .get_or_insert_with(|| format!("默认键 {default_str} 也注册失败：{e}"));
                    }
                },
                None => {
                    eprintln!("[snappin] 热键 {action} 默认键 \"{default_str}\" 无法解析，跳过");
                    error.get_or_insert_with(|| format!("默认键 {default_str} 无法解析"));
                }
            }
        }

        results.push(HotkeyRegResult {
            action: action.to_string(),
            requested,
            effective,
            ok: error.is_none(),
            error,
        });
    }

    if let Some(state) = app.try_state::<HotkeyState>() {
        if let Ok(mut g) = state.0.lock() {
            *g = table;
        }
    } else {
        eprintln!("[snappin] HotkeyState 未托管，热键反查表未写入");
    }
    results
}

/// 注销全部热键并**清空反查表**（守卫命中时用）。
/// 只注销不清表会留下陈旧映射：键被系统回收后若被别的快捷键复用，
/// `action_for` 仍会认领它，按下会触发错误动作。
fn clear(app: &AppHandle) {
    let gs = app.global_shortcut();
    if let Err(e) = gs.unregister_all() {
        eprintln!("[snappin] 热键 unregister_all 失败: {e}");
    }
    if let Some(state) = app.try_state::<HotkeyState>() {
        if let Ok(mut g) = state.0.lock() {
            *g = Vec::new();
        }
    }
}

/// 按 config + 热键守卫状态同步注册：**先 unregister_all 再决定是否注册**（合同要求）。
///
/// 守卫优先级：`hotkeys_disabled`（手动禁用）> `hotkeys_disable_fullscreen`（全屏守卫）。
/// 命中任一 → 全部注销并清空反查表，返回空结果（不当失败，前端不该看到假错误）。
/// 未命中 → 交给 `register_all`（它内部也会先 unregister_all，幂等）。
///
/// 调用点：启动、`re_register_hotkeys`、`update_settings`、托盘两个勾选项、全屏守卫轮询。
pub fn sync(app: &AppHandle, cfg: &AppConfig) -> Vec<HotkeyRegResult> {
    if cfg.hotkeys_disabled {
        clear(app);
        eprintln!("[snappin] 热键已禁用（手动开关 hotkeys_disabled=true）");
        return Vec::new();
    }
    if cfg.hotkeys_disable_fullscreen && crate::system::is_fullscreen_foreground() {
        clear(app);
        eprintln!("[snappin] 热键临时禁用（前台为全屏程序，退出全屏自动恢复）");
        return Vec::new();
    }
    register_all(app, cfg)
}
