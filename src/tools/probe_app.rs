use crate::platform;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppKind {
    Native,
    ElectronApp,
    ChromeBrowser,
}

#[derive(Debug, Deserialize)]
pub struct ProbeAppParams {
    pub app_name: String,
}

#[derive(Debug, Serialize)]
pub struct ProbeAppResult {
    pub name: String,
    pub kind: AppKind,
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
}

/// Classify a running app using platform-specific detection.
///
/// Priority: ChromeBrowser > ElectronApp > Native.
pub fn classify_running_app(pid: i32, bundle_id: Option<&str>, app_name: &str) -> AppKind {
    if platform::is_chrome_browser(bundle_id, app_name) {
        return AppKind::ChromeBrowser;
    }
    if platform::is_electron_app_by_pid(pid) {
        return AppKind::ElectronApp;
    }
    AppKind::Native
}

/// Classify a non-running app by searching install directories.
///
/// Priority: ElectronApp > Native (Chrome detection requires a running process).
pub fn classify_installed_app(app_name: &str) -> AppKind {
    if platform::is_electron_app_by_name(app_name) {
        return AppKind::ElectronApp;
    }
    AppKind::Native
}

/// Probe an app: classify its kind and return metadata.
pub fn probe_app(params: ProbeAppParams) -> CallToolResult {
    let apps = platform::list_apps();
    let needle = params.app_name.to_lowercase();
    let running_app = apps
        .iter()
        .find(|a| a.name.to_lowercase() == needle)
        .or_else(|| {
            apps.iter()
                .find(|a| a.name.to_lowercase().contains(&needle))
        });

    let result = if let Some(app) = running_app {
        let kind = classify_running_app(app.pid, app.bundle_id.as_deref(), &app.name);
        ProbeAppResult {
            name: app.name.clone(),
            kind,
            running: true,
            pid: Some(app.pid),
            bundle_id: app.bundle_id.clone(),
        }
    } else {
        let kind = classify_installed_app(&params.app_name);
        ProbeAppResult {
            name: params.app_name,
            kind,
            running: false,
            pid: None,
            bundle_id: None,
        }
    };

    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&result).unwrap_or_else(|e| format!("Serialize error: {}", e)),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    // MARK: - AppKind serialization tests

    #[test]
    fn app_kind_serializes_correctly() {
        assert_eq!(
            serde_json::to_string(&AppKind::Native).unwrap(),
            "\"Native\""
        );
        assert_eq!(
            serde_json::to_string(&AppKind::ElectronApp).unwrap(),
            "\"ElectronApp\""
        );
        assert_eq!(
            serde_json::to_string(&AppKind::ChromeBrowser).unwrap(),
            "\"ChromeBrowser\""
        );
    }

    // MARK: - ProbeAppResult serialization tests

    #[test]
    fn result_omits_none_fields() {
        let result = ProbeAppResult {
            name: "Safari".to_string(),
            kind: AppKind::Native,
            running: false,
            pid: None,
            bundle_id: None,
        };

        let json: serde_json::Value = serde_json::to_value(&result).unwrap();
        assert!(!json.as_object().unwrap().contains_key("pid"));
        assert!(!json.as_object().unwrap().contains_key("bundle_id"));
    }

    #[test]
    fn result_includes_present_fields() {
        let result = ProbeAppResult {
            name: "Signal".to_string(),
            kind: AppKind::ElectronApp,
            running: true,
            pid: Some(12345),
            bundle_id: Some("org.whispersystems.signal-desktop".to_string()),
        };

        let json: serde_json::Value = serde_json::to_value(&result).unwrap();
        assert_eq!(json["name"], "Signal");
        assert_eq!(json["kind"], "ElectronApp");
        assert_eq!(json["running"], true);
        assert_eq!(json["pid"], 12345);
        assert_eq!(json["bundle_id"], "org.whispersystems.signal-desktop");
    }

    // MARK: - classify_running_app tests

    #[cfg(target_os = "macos")]
    #[test]
    fn classify_chrome_by_bundle_id() {
        assert_eq!(
            classify_running_app(1, Some("com.google.Chrome"), "Google Chrome"),
            AppKind::ChromeBrowser
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn classify_all_chrome_variants() {
        for bid in &[
            "com.google.Chrome",
            "com.google.Chrome.canary",
            "com.brave.Browser",
            "com.microsoft.edgemac",
            "company.thebrowser.Browser",
            "org.chromium.Chromium",
        ] {
            assert_eq!(
                classify_running_app(1, Some(bid), "SomeBrowser"),
                AppKind::ChromeBrowser,
                "Expected ChromeBrowser for bundle_id={}",
                bid,
            );
        }
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn classify_chrome_by_exe_name() {
        assert_eq!(
            classify_running_app(1, None, "chrome"),
            AppKind::ChromeBrowser
        );
        assert_eq!(
            classify_running_app(1, None, "msedge"),
            AppKind::ChromeBrowser
        );
        assert_eq!(
            classify_running_app(1, None, "brave"),
            AppKind::ChromeBrowser
        );
    }

    #[test]
    fn classify_native_fallback() {
        assert_eq!(
            classify_running_app(1, Some("com.apple.Safari"), "Safari"),
            AppKind::Native
        );
    }

    #[test]
    fn classify_no_bundle_id_native() {
        assert_eq!(classify_running_app(1, None, "notepad"), AppKind::Native);
    }
}
