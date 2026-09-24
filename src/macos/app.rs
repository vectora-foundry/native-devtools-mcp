// The `cocoa` crate is deprecated in favor of `objc2`; this module still uses it.
#![allow(deprecated)]

use cocoa::base::{id, nil};
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_graphics::window::{
    kCGNullWindowID, kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly,
    kCGWindowOwnerPID, CGWindowListCopyWindowInfo,
};
use objc::{class, msg_send, sel, sel_impl};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::c_void;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub bundle_id: Option<String>,
    pub pid: i32,
    pub is_active: bool,
    pub is_hidden: bool,
    /// Whether this is a regular user-facing app (appears in Dock).
    /// Background agents and accessory apps are not user-facing.
    #[serde(skip)]
    pub is_user_app: bool,
}

/// Returns NSRunningApplication handles by merging two sources:
/// 1. `NSWorkspace.runningApplications` — may contain stale entries and miss recently
///    launched apps (requires run loop events to update, which our server doesn't process)
/// 2. `CGWindowListCopyWindowInfo` — always fresh, but only includes apps with windows
///
/// Apps from source 1 are filtered by `is_app_alive` to remove stale entries.
/// PIDs from source 2 that aren't in source 1 are looked up via
/// `NSRunningApplication.runningApplicationWithProcessIdentifier:` and added.
unsafe fn get_running_apps() -> Vec<id> {
    let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];
    let running_apps: id = msg_send![workspace, runningApplications];
    let count: usize = msg_send![running_apps, count];

    let mut apps: Vec<id> = Vec::new();
    let mut seen_pids: HashSet<i32> = HashSet::new();

    // Source 1: NSWorkspace (filter dead processes)
    for i in 0..count {
        let app: id = msg_send![running_apps, objectAtIndex: i];
        if is_app_alive(app) {
            let pid: i32 = msg_send![app, processIdentifier];
            seen_pids.insert(pid);
            apps.push(app);
        }
    }

    // Source 2: CGWindowList (catch apps NSWorkspace hasn't registered yet)
    for pid in get_window_owner_pids() {
        if pid > 0 && !seen_pids.contains(&pid) {
            let app: id = msg_send![
                class!(NSRunningApplication),
                runningApplicationWithProcessIdentifier: pid
            ];
            if app != nil {
                seen_pids.insert(pid);
                apps.push(app);
            }
        }
    }

    apps
}

/// Returns unique owner PIDs from all on-screen windows via CGWindowListCopyWindowInfo.
unsafe fn get_window_owner_pids() -> HashSet<i32> {
    let mut pids = HashSet::new();

    let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let ptr = CGWindowListCopyWindowInfo(options, kCGNullWindowID);
    if ptr.is_null() {
        return pids;
    }

    let list: CFArray<*const c_void> = CFArray::wrap_under_create_rule(ptr);
    for i in 0..list.len() {
        let dict: CFDictionary<*const c_void, *const c_void> =
            CFDictionary::wrap_under_get_rule(*list.get_unchecked(i) as *const _);

        if let Some(val) = dict.find(kCGWindowOwnerPID as *const c_void) {
            let num: CFNumber =
                core_foundation::base::CFType::wrap_under_get_rule(*val as *const _)
                    .downcast_into()
                    .unwrap();
            if let Some(pid) = num.to_i32() {
                pids.insert(pid);
            }
        }
    }

    pids
}

/// Checks if the process behind an NSRunningApplication is still alive.
///
/// macOS keeps stale entries in `runningApplications` for several seconds after
/// termination. `NSRunningApplication.isTerminated` requires run loop events to
/// update, which our server doesn't process, so we check liveness via `kill(pid, 0)`.
unsafe fn is_app_alive(app: id) -> bool {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    let pid: i32 = msg_send![app, processIdentifier];
    kill(pid, 0) == 0
}

/// List all running applications
pub fn list_apps() -> Vec<AppInfo> {
    let mut apps = Vec::new();

    unsafe {
        for app in get_running_apps() {
            if !is_app_alive(app) {
                continue;
            }

            // Get localized name
            let name_ns: id = msg_send![app, localizedName];
            let name = if name_ns != nil {
                nsstring_to_string(name_ns)
            } else {
                continue;
            };

            // Get bundle identifier
            let bundle_id_ns: id = msg_send![app, bundleIdentifier];
            let bundle_id = if bundle_id_ns != nil {
                Some(nsstring_to_string(bundle_id_ns))
            } else {
                None
            };

            // Get PID
            let pid: i32 = msg_send![app, processIdentifier];

            // Get active state
            let is_active: bool = msg_send![app, isActive];

            // Get hidden state
            let is_hidden: bool = msg_send![app, isHidden];

            // Get activation policy to distinguish user-facing apps from background agents
            // NSApplicationActivationPolicyRegular = 0
            let activation_policy: i64 = msg_send![app, activationPolicy];
            let is_user_app = activation_policy == 0;

            // Filter to user-facing apps (those with a name)
            if !name.is_empty() {
                apps.push(AppInfo {
                    name,
                    bundle_id,
                    pid,
                    is_active,
                    is_hidden,
                    is_user_app,
                });
            }
        }
    }

    apps
}

/// Activate (focus) an application by name
pub fn activate_app(app_name: &str) -> bool {
    unsafe {
        for app in get_running_apps() {
            let name_ns: id = msg_send![app, localizedName];

            if name_ns != nil {
                let name = nsstring_to_string(name_ns);
                if name.to_lowercase().contains(&app_name.to_lowercase()) {
                    let _: bool = msg_send![app, activateWithOptions: 1u64]; // NSApplicationActivateIgnoringOtherApps
                    return true;
                }
            }
        }
    }

    false
}

/// Activate an application by PID
pub fn activate_app_by_pid(pid: i32) -> bool {
    unsafe {
        let app: id = msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid
        ];

        if app != nil {
            let _: bool = msg_send![app, activateWithOptions: 1u64];
            return true;
        }
    }

    false
}

/// Check if a user-facing application is currently running (case-insensitive name match).
///
/// Only considers apps with NSApplicationActivationPolicyRegular (i.e., apps that
/// appear in the Dock). Ignores background agents, helpers, and accessory processes.
pub fn is_app_running(app_name: &str) -> bool {
    unsafe {
        let needle = app_name.to_lowercase();
        for app in get_running_apps() {
            if !is_app_alive(app) {
                continue;
            }

            // Only consider user-facing apps (NSApplicationActivationPolicyRegular = 0)
            let activation_policy: i64 = msg_send![app, activationPolicy];
            if activation_policy != 0 {
                continue;
            }

            let name_ns: id = msg_send![app, localizedName];
            if name_ns != nil {
                let name = nsstring_to_string(name_ns);
                if name.to_lowercase().contains(&needle) {
                    return true;
                }
            }
        }
    }
    false
}

/// Build the `open` [`Command`] for launching an application.
///
/// Extracted so the argument list can be inspected in unit tests without
/// actually executing `open`. The caller is responsible for running the
/// returned command.
///
/// * `app_name`   — app to launch (passed as `open -a <app_name>`)
/// * `args`       — optional extra CLI args forwarded after `--args`
/// * `background` — when `true`, prepends `-g` so the app is launched
///   without stealing foreground focus (`open -g -a …`)
pub fn build_launch_command(
    app_name: &str,
    args: &[String],
    background: bool,
) -> std::process::Command {
    let mut cmd = std::process::Command::new("open");
    if background {
        cmd.arg("-g");
    }
    cmd.arg("-a").arg(app_name);
    if !args.is_empty() {
        cmd.arg("--args");
        cmd.args(args);
    }
    cmd
}

/// Launch an application by name using `open -a`.
///
/// Finds the app in standard locations (/Applications, ~/Applications, etc.)
/// and launches it. If args is non-empty, passes them via `--args`.
/// If the app is already running and no args are given, it is brought to the front.
///
/// When `background` is `true`, the `-g` flag is passed to `open` so the
/// app launches without stealing foreground focus. Recommended whenever the
/// next action will use CDP or AX dispatch, both of which are focus-preserving.
pub fn launch_app(app_name: &str, args: &[String], background: bool) -> Result<(), String> {
    let mut cmd = build_launch_command(app_name, args, background);

    let output = cmd
        .output()
        .map_err(|e| format!("Failed to run 'open' command: {}", e))?;

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
/// By default performs a graceful termination (NSRunningApplication.terminate).
/// If `force` is true, uses forceTerminate which kills immediately without cleanup.
/// Returns the number of app instances terminated.
pub fn quit_app(app_name: &str, force: bool) -> Result<u32, String> {
    let mut terminated = 0u32;

    unsafe {
        let needle = app_name.to_lowercase();
        for app in get_running_apps() {
            if !is_app_alive(app) {
                continue;
            }

            let name_ns: id = msg_send![app, localizedName];
            if name_ns != nil {
                let name = nsstring_to_string(name_ns);
                if name.to_lowercase().contains(&needle) {
                    let success: bool = if force {
                        msg_send![app, forceTerminate]
                    } else {
                        msg_send![app, terminate]
                    };
                    if success {
                        terminated += 1;
                    }
                }
            }
        }
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

/// Known Chrome-family bundle identifiers.
const CHROME_BUNDLE_IDS: &[&str] = &[
    "com.google.Chrome",
    "com.google.Chrome.canary",
    "com.brave.Browser",
    "com.microsoft.edgemac",
    "company.thebrowser.Browser",
    "org.chromium.Chromium",
];

/// Check if an app is a Chrome-family browser by its bundle ID.
pub fn is_chrome_browser(bundle_id: Option<&str>, _app_name: &str) -> bool {
    bundle_id.is_some_and(|bid| CHROME_BUNDLE_IDS.contains(&bid))
}

/// Check if a running app is an Electron app by inspecting its bundle for
/// `Contents/Frameworks/Electron Framework.framework`.
pub fn is_electron_app_by_pid(pid: i32) -> bool {
    bundle_path_for_pid(pid)
        .map(|p| is_electron_bundle(&p))
        .unwrap_or(false)
}

/// Check if a non-running app is Electron by searching standard application
/// directories for `<app_name>.app` and inspecting the bundle.
pub fn is_electron_app_by_name(app_name: &str) -> bool {
    find_app_bundle(app_name)
        .map(|p| is_electron_bundle(&p))
        .unwrap_or(false)
}

fn is_electron_bundle(bundle_path: &str) -> bool {
    std::path::Path::new(bundle_path)
        .join("Contents/Frameworks/Electron Framework.framework")
        .exists()
}

/// Get the `.app` bundle path for a running app by PID.
fn bundle_path_for_pid(pid: i32) -> Option<String> {
    unsafe {
        let app: id = msg_send![
            class!(NSRunningApplication),
            runningApplicationWithProcessIdentifier: pid
        ];
        if app == nil {
            return None;
        }
        let url: id = msg_send![app, bundleURL];
        if url == nil {
            return None;
        }
        let path: id = msg_send![url, path];
        if path == nil {
            return None;
        }
        Some(nsstring_to_string(path))
    }
}

/// Search standard application directories for `<app_name>.app`.
fn find_app_bundle(app_name: &str) -> Option<String> {
    let dirs = [
        "/Applications",
        "/System/Applications",
        "/System/Applications/Utilities",
    ];
    let home_apps = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join("Applications"));

    for dir in dirs
        .iter()
        .map(std::path::PathBuf::from)
        .chain(home_apps.into_iter())
    {
        let candidate = dir.join(format!("{}.app", app_name));
        if candidate.exists() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

unsafe fn nsstring_to_string(ns_string: id) -> String {
    let utf8_ptr: *const i8 = msg_send![ns_string, UTF8String];
    if utf8_ptr.is_null() {
        String::new()
    } else {
        std::ffi::CStr::from_ptr(utf8_ptr)
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that quit_app + list_apps doesn't return stale entries.
    ///
    /// Requires Calculator.app — run with: cargo test test_no_stale_entries -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_no_stale_entries_after_quit() {
        let app = "Calculator";

        // Ensure Calculator is running
        launch_app(app, &[], false).ok();
        std::thread::sleep(std::time::Duration::from_secs(2));

        let apps = list_apps();
        let found = apps.iter().find(|a| a.name == app);
        assert!(
            found.is_some(),
            "Calculator should appear in list_apps after launch"
        );
        let old_pid = found.unwrap().pid;
        println!("Calculator running at PID {}", old_pid);

        // Force quit to ensure immediate termination
        let result = quit_app(app, true);
        assert!(result.is_ok(), "quit_app should succeed");
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Stale entry should be filtered by is_app_alive
        let apps = list_apps();
        let stale = apps.iter().find(|a| a.name == app && a.pid == old_pid);
        assert!(
            stale.is_none(),
            "Stale Calculator entry (PID {}) should be filtered from list_apps",
            old_pid
        );

        // is_app_running should return false
        assert!(
            !is_app_running(app),
            "is_app_running should return false after quit"
        );

        // quit_app on an already-dead app should return an error
        let result = quit_app(app, false);
        assert!(
            result.is_err(),
            "quit_app should error when app is not running"
        );

        // Relaunch and verify it appears (via CGWindowList supplement)
        launch_app(app, &[], false).ok();
        std::thread::sleep(std::time::Duration::from_secs(2));

        let apps = list_apps();
        let new = apps.iter().find(|a| a.name == app);
        assert!(
            new.is_some(),
            "Calculator should appear in list_apps after relaunch"
        );
        let new_pid = new.unwrap().pid;
        assert_ne!(
            old_pid, new_pid,
            "Relaunched Calculator should have a new PID"
        );
        println!("Calculator relaunched at PID {}", new_pid);

        // Cleanup
        quit_app(app, true).ok();
    }

    // MARK: - is_chrome_browser tests

    #[test]
    fn test_chrome_bundle_ids_detected() {
        for bid in CHROME_BUNDLE_IDS {
            assert!(
                is_chrome_browser(Some(bid), "irrelevant"),
                "Expected ChromeBrowser for bundle_id={}",
                bid,
            );
        }
    }

    #[test]
    fn test_non_chrome_bundle_id() {
        assert!(!is_chrome_browser(Some("com.apple.Safari"), "Safari"));
    }

    #[test]
    fn test_no_bundle_id() {
        assert!(!is_chrome_browser(None, "anything"));
    }

    // MARK: - is_electron_bundle tests

    #[test]
    fn test_electron_bundle_detected() {
        let dir = tempfile::tempdir().unwrap();
        let framework_path = dir
            .path()
            .join("Contents/Frameworks/Electron Framework.framework");
        std::fs::create_dir_all(&framework_path).unwrap();

        assert!(is_electron_bundle(dir.path().to_str().unwrap()));
    }

    #[test]
    fn test_non_electron_bundle() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_electron_bundle(dir.path().to_str().unwrap()));
    }

    #[test]
    fn test_nonexistent_path_not_electron() {
        assert!(!is_electron_bundle("/nonexistent/path"));
    }

    // MARK: - build_launch_command tests

    fn cmd_args(cmd: &std::process::Command) -> Vec<&std::ffi::OsStr> {
        cmd.get_args().collect()
    }

    #[test]
    fn build_launch_command_foreground_has_no_dash_g() {
        let cmd = build_launch_command("Calculator", &[], false);
        let args = cmd_args(&cmd);
        assert!(
            !args.contains(&std::ffi::OsStr::new("-g")),
            "Expected no -g flag when background=false, got: {:?}",
            args
        );
        assert!(args.contains(&std::ffi::OsStr::new("-a")));
        assert!(args.contains(&std::ffi::OsStr::new("Calculator")));
    }

    #[test]
    fn build_launch_command_background_includes_dash_g() {
        let cmd = build_launch_command("Calculator", &[], true);
        let args = cmd_args(&cmd);
        assert!(
            args.contains(&std::ffi::OsStr::new("-g")),
            "Expected -g flag when background=true, got: {:?}",
            args
        );
        assert!(args.contains(&std::ffi::OsStr::new("-a")));
        assert!(args.contains(&std::ffi::OsStr::new("Calculator")));
    }

    #[test]
    fn build_launch_command_background_dash_g_precedes_dash_a() {
        let cmd = build_launch_command("MyApp", &[], true);
        let args = cmd_args(&cmd);
        let g_pos = args
            .iter()
            .position(|a| *a == std::ffi::OsStr::new("-g"))
            .expect("-g must be present when background=true");
        let a_pos = args
            .iter()
            .position(|a| *a == std::ffi::OsStr::new("-a"))
            .expect("-a must always be present");
        assert!(
            g_pos < a_pos,
            "-g ({}) must come before -a ({}) in open invocation",
            g_pos,
            a_pos
        );
    }

    #[test]
    fn build_launch_command_background_with_extra_args() {
        let extra = vec!["--remote-debugging-port=9222".to_string()];
        let cmd = build_launch_command("Electron App", &extra, true);
        let args = cmd_args(&cmd);
        assert!(args.contains(&std::ffi::OsStr::new("-g")));
        assert!(args.contains(&std::ffi::OsStr::new("--args")));
        assert!(args.contains(&std::ffi::OsStr::new("--remote-debugging-port=9222")));
    }
}
