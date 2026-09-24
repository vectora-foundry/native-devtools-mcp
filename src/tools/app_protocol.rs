use crate::app_protocol::AppProtocolClient;
use rmcp::model::{CallToolResult, ContentBlock};
use rmcp::service::{Peer, RoleServer};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::RwLock;

pub type SharedClient = Arc<RwLock<Option<AppProtocolClient>>>;

/// Identity validation result
#[derive(Debug, PartialEq)]
pub enum IdentityValidationResult {
    /// Validation passed
    Ok,
    /// Bundle ID mismatch
    BundleIdMismatch {
        expected: String,
        actual: String,
        actual_app_name: String,
    },
    /// App name mismatch
    AppNameMismatch {
        expected: String,
        actual: String,
        actual_bundle_id: String,
    },
}

/// Validates bundle ID (exact, case-sensitive match)
pub fn validate_bundle_id(expected: &str, actual: &str) -> bool {
    expected == actual
}

/// Validates app name (case-insensitive, whitespace-trimmed)
pub fn validate_app_name(expected: &str, actual: &str) -> bool {
    expected.trim().eq_ignore_ascii_case(actual.trim())
}

/// Validates app identity against expected values from runtime info
pub fn validate_identity(
    expected_bundle_id: Option<&str>,
    expected_app_name: Option<&str>,
    info: &serde_json::Value,
) -> IdentityValidationResult {
    let actual_bundle_id = info.get("bundleId").and_then(|v| v.as_str()).unwrap_or("");
    let actual_app_name = info
        .get("appName")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    // Validate bundle ID if expected
    if let Some(expected) = expected_bundle_id {
        if !validate_bundle_id(expected, actual_bundle_id) {
            return IdentityValidationResult::BundleIdMismatch {
                expected: expected.to_string(),
                actual: actual_bundle_id.to_string(),
                actual_app_name: actual_app_name.to_string(),
            };
        }
    }

    // Validate app name if expected
    if let Some(expected) = expected_app_name {
        if !validate_app_name(expected, actual_app_name) {
            return IdentityValidationResult::AppNameMismatch {
                expected: expected.to_string(),
                actual: actual_app_name.to_string(),
                actual_bundle_id: actual_bundle_id.to_string(),
            };
        }
    }

    IdentityValidationResult::Ok
}

#[derive(Debug, Deserialize)]
pub struct AppConnectParams {
    pub url: String,
    #[serde(default)]
    pub expected_bundle_id: Option<String>,
    #[serde(default)]
    pub expected_app_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppGetTreeParams {
    #[serde(default)]
    pub depth: Option<i32>,
    #[serde(default)]
    pub root_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppQueryParams {
    pub selector: String,
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Deserialize)]
pub struct AppClickParams {
    pub element_id: String,
    #[serde(default)]
    pub click_count: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct AppTypeParams {
    pub text: String,
    #[serde(default)]
    pub element_id: Option<String>,
    #[serde(default)]
    pub clear_first: bool,
}

#[derive(Debug, Deserialize)]
pub struct AppPressKeyParams {
    pub key: String,
    #[serde(default)]
    pub modifiers: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppFocusParams {
    pub element_id: String,
}

#[derive(Debug, Deserialize)]
pub struct AppScreenshotParams {
    #[serde(default)]
    pub element_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppGetElementParams {
    pub element_id: String,
}

#[derive(Debug, Deserialize)]
pub struct AppFocusWindowParams {
    pub window_id: String,
}

const RELIST_HINT: &str = "Re-list tools to see app_* tools if your client doesn't auto-refresh.";

pub async fn app_connect(
    params: AppConnectParams,
    client: SharedClient,
    peer: Peer<RoleServer>,
) -> CallToolResult {
    // Check if there's an existing connection (for error messages)
    let has_existing = client.read().await.is_some();

    let new_client = match AppProtocolClient::connect(&params.url).await {
        Ok(c) => c,
        Err(e) => {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "Failed to connect: {}",
                e
            ))])
        }
    };

    // Get runtime info to validate identity
    let info = match new_client.get_runtime_info().await {
        Ok(info) => info,
        Err(e) => {
            // If we can't get info but have expectations, fail (preserve existing connection)
            if params.expected_bundle_id.is_some() || params.expected_app_name.is_some() {
                new_client.close(); // Clean up the new connection
                let existing_note = if has_existing {
                    " Existing connection preserved."
                } else {
                    ""
                };
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Failed to get app info for validation: {}.{}",
                    e, existing_note
                ))]);
            }
            // No expectations, proceed without info
            *client.write().await = Some(new_client);
            let _ = peer.notify_tool_list_changed().await;
            return CallToolResult::success(vec![ContentBlock::text(format!(
                "Connected to {}. App debug tools (app_*) are now available. {}",
                params.url, RELIST_HINT
            ))]);
        }
    };

    // Validate identity using the extracted helper
    let validation_result = validate_identity(
        params.expected_bundle_id.as_deref(),
        params.expected_app_name.as_deref(),
        &info,
    );

    match validation_result {
        IdentityValidationResult::Ok => {}
        IdentityValidationResult::BundleIdMismatch {
            expected,
            actual,
            actual_app_name,
        } => {
            new_client.close(); // Clean up the new connection
            let existing_note = if has_existing {
                " Existing connection preserved."
            } else {
                ""
            };
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "Identity mismatch: connected to \"{}\" (bundleId \"{}\"), but expected bundleId \"{}\".{}",
                actual_app_name, actual, expected, existing_note
            ))]);
        }
        IdentityValidationResult::AppNameMismatch {
            expected,
            actual,
            actual_bundle_id,
        } => {
            new_client.close(); // Clean up the new connection
            let existing_note = if has_existing {
                " Existing connection preserved."
            } else {
                ""
            };
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "Identity mismatch: connected to \"{}\" (bundleId \"{}\"), but expected app name \"{}\".{}",
                actual, actual_bundle_id, expected, existing_note
            ))]);
        }
    }

    // Validation passed (or no expectations), store client
    *client.write().await = Some(new_client);
    let _ = peer.notify_tool_list_changed().await;

    let msg = format!(
        "Connected. App debug tools (app_*) are now available. {}\n\n{}",
        RELIST_HINT,
        serde_json::to_string_pretty(&info).unwrap_or_default()
    );
    CallToolResult::success(vec![ContentBlock::text(msg)])
}

pub async fn app_disconnect(client: SharedClient, peer: Peer<RoleServer>) -> CallToolResult {
    if client.write().await.take().is_some() {
        let _ = peer.notify_tool_list_changed().await;
        CallToolResult::success(vec![ContentBlock::text(
            "Disconnected. App debug tools (app_*) are no longer available.",
        )])
    } else {
        CallToolResult::error(vec![ContentBlock::text("Not connected to any app")])
    }
}

/// Helper to get a cloned client, releasing the lock before async operations
async fn get_client(shared: &SharedClient) -> Option<AppProtocolClient> {
    shared.read().await.clone()
}

pub async fn app_get_info(client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client.get_runtime_info().await {
        Ok(info) => CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&info).unwrap_or_else(|_| "{}".to_string()),
        )]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_get_tree(params: AppGetTreeParams, client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client
        .get_tree(params.depth, params.root_id.as_deref())
        .await
    {
        Ok(tree) => CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&tree).unwrap_or_else(|_| "{}".to_string()),
        )]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_query(params: AppQueryParams, client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    let result = if params.all {
        client.query_selector_all(&params.selector).await
    } else {
        client.query_selector(&params.selector).await
    };

    match result {
        Ok(elements) => CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&elements).unwrap_or_else(|_| "{}".to_string()),
        )]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_get_element(params: AppGetElementParams, client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client.get_element(&params.element_id).await {
        Ok(element) => CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&element).unwrap_or_else(|_| "{}".to_string()),
        )]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_click(params: AppClickParams, client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client.click(&params.element_id, params.click_count).await {
        Ok(_) => CallToolResult::success(vec![ContentBlock::text(format!(
            "Clicked element: {}",
            params.element_id
        ))]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_type(params: AppTypeParams, client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client
        .type_text(
            &params.text,
            params.element_id.as_deref(),
            params.clear_first,
        )
        .await
    {
        Ok(_) => {
            CallToolResult::success(vec![ContentBlock::text(format!("Typed: {}", params.text))])
        }
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_press_key(params: AppPressKeyParams, client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client.press_key(&params.key, params.modifiers).await {
        Ok(_) => {
            CallToolResult::success(vec![ContentBlock::text(format!("Pressed: {}", params.key))])
        }
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_focus(params: AppFocusParams, client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client.focus(&params.element_id).await {
        Ok(_) => CallToolResult::success(vec![ContentBlock::text(format!(
            "Focused element: {}",
            params.element_id
        ))]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_screenshot(params: AppScreenshotParams, client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client.get_screenshot(params.element_id.as_deref()).await {
        Ok(result) => {
            // Extract base64 data from result
            if let Some(data) = result.get("data").and_then(|v| v.as_str()) {
                let width = result.get("width").and_then(|v| v.as_i64()).unwrap_or(0);
                let height = result.get("height").and_then(|v| v.as_i64()).unwrap_or(0);

                CallToolResult::success(vec![
                    ContentBlock::text(format!("Screenshot: {}x{}", width, height)),
                    ContentBlock::image(data, "image/png"),
                ])
            } else {
                CallToolResult::error(vec![ContentBlock::text("Invalid screenshot response")])
            }
        }
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_list_windows(client: SharedClient) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client.list_windows().await {
        Ok(windows) => CallToolResult::success(vec![ContentBlock::text(
            serde_json::to_string_pretty(&windows).unwrap_or_else(|_| "{}".to_string()),
        )]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}

pub async fn app_focus_window(
    params: AppFocusWindowParams,
    client: SharedClient,
) -> CallToolResult {
    let Some(client) = get_client(&client).await else {
        return CallToolResult::error(vec![ContentBlock::text(
            "Not connected. Use app_connect first.",
        )]);
    };

    match client.focus_window(&params.window_id).await {
        Ok(_) => CallToolResult::success(vec![ContentBlock::text(format!(
            "Focused window: {}",
            params.window_id
        ))]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Failed: {}", e))]),
    }
}
