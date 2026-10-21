//! Windows 系统集成：开机自启（注册表 HKCU Run 键）与管理员自提升。
//! 全部失败路径记日志、绝不 panic；非 Windows 目标下为安全空实现。

/// 注册表 Run 键里本应用的值名。
const RUN_VALUE_NAME: &str = "SnapPin";

/// 当前可执行文件路径（用于写自启值与自提升重启）。
fn current_exe_string() -> Option<String> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
}

/// 应用开机自启偏好：`enable` 为 true 写 HKCU Run 值，false 删除。
/// 幂等，失败仅记日志。
pub fn set_autostart(enable: bool) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use std::process::Command;
        // 隐藏子进程控制台窗口（GUI 应用里 reg.exe 会闪黑框）。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

        if enable {
            let Some(exe) = current_exe_string() else {
                eprintln!("[snappin] 开机自启：取不到 exe 路径，跳过");
                return;
            };
            // 值内容需带引号包裹整条路径，避免路径含空格被截断。
            let data = format!("\"{exe}\"");
            let out = Command::new("reg")
                .args([
                    "add",
                    RUN_KEY,
                    "/v",
                    RUN_VALUE_NAME,
                    "/t",
                    "REG_SZ",
                    "/d",
                    &data,
                    "/f",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    eprintln!("[snappin] 开机自启已开启 → {data}")
                }
                Ok(o) => eprintln!(
                    "[snappin] 开机自启写入失败: {}",
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => eprintln!("[snappin] 开机自启 reg add 执行失败: {e}"),
            }
        } else {
            let out = Command::new("reg")
                .args(["delete", RUN_KEY, "/v", RUN_VALUE_NAME, "/f"])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            match out {
                // 值不存在时 reg delete 返回非零，属正常（本就没开），不当错误。
                Ok(o) if o.status.success() => eprintln!("[snappin] 开机自启已关闭"),
                Ok(_) => eprintln!("[snappin] 开机自启：Run 值不存在，无需删除"),
                Err(e) => eprintln!("[snappin] 开机自启 reg delete 执行失败: {e}"),
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = enable;
    }
}

/// 是否已以管理员（提权）身份运行。取不到时保守返回 false。
#[cfg(windows)]
pub fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

/// 若 config.run_as_admin 为真且当前未提权，则以管理员身份重启自身并退出当前实例。
/// 该函数在 `run()` 最早期调用（还未建窗），成功重启即 `process::exit(0)`。
/// 已提权 / 未开启 / 读不到配置 / 提升失败：直接返回，程序照常以当前权限运行。
#[cfg(windows)]
pub fn maybe_elevate() {
    // 直接从磁盘读配置，避免为提权判断先建 Tauri app。
    if !wants_admin_from_disk() {
        return;
    }
    if is_elevated() {
        return;
    }
    let Some(exe) = current_exe_string() else {
        eprintln!("[snappin] 管理员重启：取不到 exe 路径，以当前权限继续");
        return;
    };
    match elevate_relaunch(&exe) {
        Ok(()) => {
            // 已拉起提权实例，退出当前非提权实例（单实例插件会让新实例接管）。
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[snappin] 管理员重启失败({e})，以当前权限继续");
        }
    }
}

#[cfg(not(windows))]
pub fn maybe_elevate() {}

#[cfg(not(windows))]
pub fn is_elevated() -> bool {
    false
}

/// ShellExecuteW "runas" 拉起提权实例。返回 Err 表示用户拒绝 UAC 或调用失败。
#[cfg(windows)]
fn elevate_relaunch(exe: &str) -> Result<(), String> {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let verb = HSTRING::from("runas");
    let file = HSTRING::from(exe);
    // ShellExecuteW 成功返回值 > 32；<=32 为错误码。
    let h = unsafe {
        ShellExecuteW(
            HWND::default(),
            &verb,
            &file,
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let code = h.0 as isize;
    if code > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecuteW 返回 {code}"))
    }
}

/// 前台是否处于全屏程序（供热键守卫判断是否临时注销热键）。
///
/// 主判据 `SHQueryUserNotificationState`：D3D 独占全屏返回 `QUNS_RUNNING_D3D_FULL_SCREEN`。
/// 但无边框全屏（borderless，常见于游戏/视频窗）不触发它 → 再加一层兜底：前台窗口矩形与
/// 它所在显示器的 `rcMonitor` 完全重合。
///
/// 为什么两者都要：只用 API 会漏放无边框全屏；只用矩形会把「最大化窗口」误判成全屏
/// ——DWM 扩展框（WS_CAPTION/WS_THICKFRAME 仍在，`GetWindowRect` 会把不可见边框算进来）
/// 使最大化窗口的 rect 比 rcMonitor **每边大 8px**，所以「完全重合（容差 1px）」正好把它排除；
/// 无边框全屏（无 caption/thickframe）才是精确等于 rcMonitor。
#[cfg(windows)]
pub fn is_fullscreen_foreground() -> bool {
    use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};
    unsafe {
        if let Ok(state) = SHQueryUserNotificationState() {
            if state == QUNS_RUNNING_D3D_FULL_SCREEN {
                return true;
            }
        }
    }
    foreground_covers_monitor()
}

#[cfg(not(windows))]
pub fn is_fullscreen_foreground() -> bool {
    false
}

/// 兜底：前台窗口矩形是否精确盖满它所在显示器（DWM 扩展框口径下的「无边框全屏」）。
#[cfg(windows)]
fn foreground_covers_monitor() -> bool {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect, IsIconic};
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || IsIconic(fg).as_bool() {
            return false;
        }
        // 桌面外壳与本进程窗口不算「全屏程序」：桌面的 Progman/WorkerW 矩形就是整屏，
        // 用户点一下桌面、按 Win+D、或看自己的覆盖层时都会被误判成全屏 →
        // 守卫把全局热键全部注销（2026-10-15 审查发现）。
        if is_shell_or_self_window(fg) {
            return false;
        }
        let mut wr = RECT::default();
        if GetWindowRect(fg, &mut wr).is_err() {
            return false;
        }
        let mon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        if mon.0.is_null() {
            return false;
        }
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(mon, &mut mi).as_bool() {
            return false;
        }
        let m = mi.rcMonitor;
        (wr.left - m.left).abs() <= 1
            && (wr.top - m.top).abs() <= 1
            && (wr.right - m.right).abs() <= 1
            && (wr.bottom - m.bottom).abs() <= 1
    }
}

/// 桌面外壳窗口或本进程窗口：矩形可能是整屏，但不代表「用户在全屏程序里」。
#[cfg(windows)]
fn is_shell_or_self_window(hwnd: windows::Win32::Foundation::HWND) -> bool {
    if crate::window::current_process_owns(hwnd) {
        return true;
    }
    matches!(
        crate::window::class_name(hwnd).as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

/// 从磁盘配置文件读 run_as_admin；路径按 Tauri app_config_dir 约定：
/// %APPDATA%\<identifier>\config.json（identifier=com.snappin.app，roaming）。
#[cfg(windows)]
fn wants_admin_from_disk() -> bool {
    let Ok(appdata) = std::env::var("APPDATA") else {
        return false;
    };
    let path = std::path::Path::new(&appdata)
        .join("com.snappin.app")
        .join("config.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return false;
    };
    // 只关心一个布尔位，用宽松解析，任何失败都当 false。
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(v) => v
            .get("run_as_admin")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        Err(_) => false,
    }
}
