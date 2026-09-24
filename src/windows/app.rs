//! Application enumeration and focus for Windows.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, TRUE};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    TerminateProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindow, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW, SetForegroundWindow,
    ShowWindow, GW_OWNER, SW_RESTORE, SW_SHOW, WM_CLOSE,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: Option<String>, // Not applicable on Windows, kept for API compatibility
    pub pid: i32,
    pub is_active: bool,
    pub is_hidden: bool,
    /// Whether this is a regular user-facing app. On Windows, all enumerated
    /// apps have visible windows, so this is always true.
    #[serde(skip)]
    pub is_user_app: bool,
}

struct AppEnumData {
    apps: HashMap<u32, AppInfo>, // pid -> AppInfo
    foreground_pid: u32,
}

/// List all running applications (processes with visible windows).
///
/// This matches macOS behavior where we list GUI applications rather than
/// all processes. An app is considered running if it has at least one
/// visible top-level window.
pub fn list_apps() -> Vec<AppInfo> {
    // Get the foreground window's PID to determine active app
    let foreground_pid = unsafe {
        let fg = GetForegroundWindow();
        if !fg.is_invalid() {
            let mut pid = 0u32;
            GetWindowThreadProcessId(fg, Some(&mut pid));
            pid
        } else {
            0
        }
    };

    let mut data = AppEnumData {
        apps: HashMap::new(),
        foreground_pid,
    };

    unsafe {
        let _ = EnumWindows(
            Some(app_enum_callback),
            LPARAM(&mut data as *mut _ as isize),
        );
    }

    data.apps.into_values().collect()
}

unsafe extern "system" fn app_enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let data = &mut *(lparam.0 as *mut AppEnumData);

    // Skip invisible windows
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }

    // Skip windows with no title (not a main window)
    let title_len = GetWindowTextLengthW(hwnd);
    if title_len == 0 {
        return TRUE;
    }

    // Skip windows that have an owner (popup windows, tooltips, etc.)
    if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
        if !owner.is_invalid() {
            return TRUE;
        }
    }

    // Get owner process ID
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));

    // Skip if we already have this PID
    if data.apps.contains_key(&pid) {
        return TRUE;
    }

    // Get process name
    if let Some(name) = get_process_name(pid) {
        let is_active = pid == data.foreground_pid;

        data.apps.insert(
            pid,
            AppInfo {
                name,
                bundle_id: None, // Windows doesn't have bundle IDs
                pid: pid as i32,
                is_active,
                is_hidden: false, // Windows doesn't have a hidden concept like macOS
                is_user_app: true, // All enumerated apps have visible windows
            },
        );
    }

    TRUE
}

/// Get the executable name for a process ID.
pub fn get_process_name(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut buf: Vec<u16> = vec![0; 260];
        let mut size = buf.len() as u32;

        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );

        let _ = CloseHandle(handle);

        if result.is_ok() && size > 0 {
            let path = OsString::from_wide(&buf[..size as usize])
                .to_string_lossy()
                .into_owned();
            // Extract just the filename without extension for cleaner display
            path.rsplit('\\')
                .next()
                .map(|s| s.strip_suffix(".exe").unwrap_or(s).to_string())
        } else {
            None
        }
    }
}

/// Activate (focus) an application by name.
///
/// Finds the first window belonging to a process whose name contains
/// the given substring (case-insensitive) and brings it to the foreground.
pub fn activate_app(app_name: &str) -> bool {
    let needle = app_name.to_lowercase();

    struct ActivateData {
        needle: String,
        found: bool,
    }

    let mut data = ActivateData {
        needle,
        found: false,
    };

    unsafe extern "system" fn activate_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam.0 as *mut ActivateData);

        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }

        let title_len = GetWindowTextLengthW(hwnd);
        if title_len == 0 {
            return TRUE;
        }

        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.is_invalid() {
                return TRUE;
            }
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));

        if let Some(name) = get_process_name_static(pid) {
            if name.to_lowercase().contains(&data.needle) {
                focus_hwnd(hwnd);
                data.found = true;
                return BOOL(0); // Stop enumeration
            }
        }

        TRUE
    }

    unsafe {
        let _ = EnumWindows(
            Some(activate_callback),
            LPARAM(&mut data as *mut _ as isize),
        );
    }

    data.found
}

// Static version for use in callback
fn get_process_name_static(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut buf: Vec<u16> = vec![0; 260];
        let mut size = buf.len() as u32;

        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );

        let _ = CloseHandle(handle);

        if result.is_ok() && size > 0 {
            let path = OsString::from_wide(&buf[..size as usize])
                .to_string_lossy()
                .into_owned();
            path.rsplit('\\')
                .next()
                .map(|s| s.strip_suffix(".exe").unwrap_or(s).to_string())
        } else {
            None
        }
    }
}

/// Activate an application by PID.
///
/// Finds the first visible window belonging to the given process and brings
/// it to the foreground.
pub fn activate_app_by_pid(pid: i32) -> bool {
    struct ActivateByPidData {
        target_pid: u32,
        found: bool,
    }

    let mut data = ActivateByPidData {
        target_pid: pid as u32,
        found: false,
    };

    unsafe extern "system" fn activate_by_pid_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let data = &mut *(lparam.0 as *mut ActivateByPidData);

        if !IsWindowVisible(hwnd).as_bool() {
            return TRUE;
        }

        let title_len = GetWindowTextLengthW(hwnd);
        if title_len == 0 {
            return TRUE;
        }

        if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
            if !owner.is_invalid() {
                return TRUE;
            }
        }

        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));

        if pid == data.target_pid {
            focus_hwnd(hwnd);
            data.found = true;
            return BOOL(0); // Stop enumeration
        }

        TRUE
    }

    unsafe {
        let _ = EnumWindows(
            Some(activate_by_pid_callback),
            LPARAM(&mut data as *mut _ as isize),
        );
    }

    data.found
}

/// Check if an application is currently running (case-insensitive name match).
pub fn is_app_running(app_name: &str) -> bool {
    let needle = app_name.to_lowercase();
    list_apps()
        .iter()
        .any(|app| app.name.to_lowercase().contains(&needle))
}

/// Launch an application by name.
///
/// Uses `cmd /c start "" "app_name"` which searches PATH and App Paths registry.
/// For apps not in PATH, provide the full executable path.
/// If args is non-empty, they are appended after the app name.
///
/// The `_background` parameter is accepted for API parity with the macOS
/// implementation but is a no-op on Windows (Windows uses different launch
/// semantics; background launching is not supported here).
pub fn launch_app(app_name: &str, args: &[String], _background: bool) -> Result<(), String> {
    let mut cmd_args = vec!["/C", "start", "", app_name];
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    cmd_args.extend(arg_refs);

    let output = std::process::Command::new("cmd")
        .args(&cmd_args)
        .output()
        .map_err(|e| format!("Failed to run start command: {}", e))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "Failed to launch '{}': {}",
            app_name,
            stderr.trim()
        ))
    }
}

/// Quit an application by name.
///
/// Graceful: sends WM_CLOSE to all matching windows.
/// Force: calls TerminateProcess on the matching PIDs.
/// Returns the number of app instances terminated.
pub fn quit_app(app_name: &str, force: bool) -> Result<u32, String> {
    let needle = app_name.to_lowercase();
    let mut terminated = 0u32;

    if force {
        // Force kill: find PIDs and terminate processes
        let mut killed_pids = std::collections::HashSet::new();
        let apps = list_apps();
        for app in &apps {
            if app.name.to_lowercase().contains(&needle) && killed_pids.insert(app.pid) {
                unsafe {
                    if let Ok(handle) = OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                        false,
                        app.pid as u32,
                    ) {
                        let _ = TerminateProcess(handle, 1);
                        let _ = CloseHandle(handle);
                        terminated += 1;
                    }
                }
            }
        }
    } else {
        // Graceful: enumerate windows and send WM_CLOSE to matching ones
        struct QuitData {
            needle: String,
            terminated: u32,
        }

        let mut data = QuitData {
            needle,
            terminated: 0,
        };

        unsafe extern "system" fn quit_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let data = &mut *(lparam.0 as *mut QuitData);

            if !IsWindowVisible(hwnd).as_bool() {
                return TRUE;
            }

            let title_len = GetWindowTextLengthW(hwnd);
            if title_len == 0 {
                return TRUE;
            }

            if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
                if !owner.is_invalid() {
                    return TRUE;
                }
            }

            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));

            if let Some(name) = get_process_name_static(pid) {
                if name.to_lowercase().contains(&data.needle) {
                    let _ = PostMessageW(hwnd, WM_CLOSE, None, None);
                    data.terminated += 1;
                }
            }

            TRUE
        }

        unsafe {
            let _ = EnumWindows(Some(quit_callback), LPARAM(&mut data as *mut _ as isize));
        }

        terminated = data.terminated;
    }

    if terminated > 0 {
        Ok(terminated)
    } else {
        Err(format!(
            "No running app found matching '{}'. Use list_apps to find the correct app name.",
            app_name
        ))
    }
}

/// Known Chrome-family executable names (without `.exe`).
const CHROME_EXE_NAMES: &[&str] = &["chrome", "msedge", "brave", "chromium", "arc"];

/// Check if an app is a Chrome-family browser.
/// On Windows, matches against known executable names (bundle_id is always None).
pub fn is_chrome_browser(_bundle_id: Option<&str>, app_name: &str) -> bool {
    let lower = app_name.to_lowercase();
    CHROME_EXE_NAMES.iter().any(|&name| lower == name)
}

/// Check if a running app is an Electron app by inspecting its install directory
/// for `resources/electron.asar` next to the executable.
pub fn is_electron_app_by_pid(pid: i32) -> bool {
    get_process_exe_dir(pid as u32)
        .map(|dir| is_electron_dir(&dir))
        .unwrap_or(false)
}

/// Check if a non-running app is Electron by searching standard install locations.
pub fn is_electron_app_by_name(app_name: &str) -> bool {
    for dir in app_search_dirs() {
        // Check <dir>/<app_name>/resources/electron.asar
        let candidate = dir.join(app_name);
        if is_electron_dir(&candidate) {
            return true;
        }
        // Also check <dir>/<app_name>/app-*/resources/electron.asar (Squirrel installs)
        if candidate.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&candidate) {
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    if name.to_string_lossy().starts_with("app-") {
                        if is_electron_dir(&entry.path()) {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

fn is_electron_dir(dir: &std::path::Path) -> bool {
    dir.join("resources").join("electron.asar").exists()
}

/// Get the directory containing the executable for a process.
fn get_process_exe_dir(pid: u32) -> Option<std::path::PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf: Vec<u16> = vec![0; 260];
        let mut size = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);

        if result.is_ok() && size > 0 {
            let path = OsString::from_wide(&buf[..size as usize])
                .to_string_lossy()
                .into_owned();
            std::path::Path::new(&path)
                .parent()
                .map(|p| p.to_path_buf())
        } else {
            None
        }
    }
}

/// Standard directories to search for installed apps on Windows.
fn app_search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec![
        std::path::PathBuf::from(r"C:\Program Files"),
        std::path::PathBuf::from(r"C:\Program Files (x86)"),
    ];
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        dirs.push(std::path::PathBuf::from(local).join("Programs"));
    }
    dirs
}

/// Raise windows by PID. On Windows this is a no-op because `activate_app_by_pid`
/// already uses `BringWindowToTop` + `SetForegroundWindow` which reliably raise windows.
pub fn raise_windows(_pid: i32) -> bool {
    true
}

/// Bring one specific window (by its HWND-based ID) to the foreground.
///
/// Returns `true` when the window is the foreground window afterwards.
/// `SetForegroundWindow` can be refused by focus-stealing prevention, so
/// the caller must not assume success.
pub fn raise_window(window: &super::window::WindowInfo) -> bool {
    let hwnd = super::window::hwnd_from_id(window.id);
    focus_hwnd(hwnd);
    unsafe { GetForegroundWindow() == hwnd }
}

/// Focus a window by its handle.
///
/// Uses multiple techniques to bring a window to the foreground since
/// Windows restricts SetForegroundWindow in certain conditions.
fn focus_hwnd(hwnd: HWND) {
    unsafe {
        // Get thread IDs for attachment
        let foreground_hwnd = GetForegroundWindow();
        let foreground_thread = GetWindowThreadProcessId(foreground_hwnd, None);
        let target_thread = GetWindowThreadProcessId(hwnd, None);
        let current_thread = GetCurrentThreadId();

        // Attach to foreground thread to bypass focus-stealing prevention
        let attached_to_foreground =
            if foreground_thread != current_thread && foreground_thread != 0 {
                AttachThreadInput(current_thread, foreground_thread, true).as_bool()
            } else {
                false
            };

        let attached_to_target =
            if target_thread != current_thread && target_thread != foreground_thread {
                AttachThreadInput(current_thread, target_thread, true).as_bool()
            } else {
                false
            };

        // Restore if minimized
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            // Ensure window is visible
            let _ = ShowWindow(hwnd, SW_SHOW);
        }

        // Bring to top and set foreground
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);

        // Detach threads
        if attached_to_target {
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        if attached_to_foreground {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
    }
}
