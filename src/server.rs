use crate::app_protocol::AppProtocolClient;
use crate::tools::{
    app_protocol as app_tools, find_image, image_cache::ImageCache, input as input_tools,
    load_image, navigation, screenshot, screenshot_cache::ScreenshotCache,
};
use base64::Engine;
use rmcp::model::ContentBlock;
use rmcp::{
    handler::server::ServerHandler,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
        ToolAnnotations,
    },
    service::{RequestContext, RoleServer},
    ErrorData as McpError,
};
use serde_json::Value;
use std::borrow::Cow;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::android::AndroidDevice;

/// Serialize a value to pretty-printed JSON, returning a formatted error on failure.
fn to_json_pretty(value: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|e| format!("Failed to serialize: {}", e))
}

/// Extract a required string field from a JSON value.
fn parse_string_field(args: &Value, field: &str) -> Result<String, McpError> {
    args.get(field)
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| McpError::invalid_params(format!("missing required param: {}", field), None))
}

/// Extract required `x` and `y` number fields from a JSON value.
fn parse_xy(args: &Value) -> Result<(f64, f64), McpError> {
    let x = args
        .get("x")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| McpError::invalid_params("missing required param: x", None))?;
    let y = args
        .get("y")
        .and_then(|v| v.as_f64())
        .ok_or_else(|| McpError::invalid_params("missing required param: y", None))?;
    Ok((x, y))
}

/// MCP protocol version this server answers `initialize` with.
const SERVER_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2024_11_05;

fn json_to_object(value: Value) -> rmcp::model::JsonObject {
    match value {
        Value::Object(map) => map,
        _ => Default::default(),
    }
}

// ============================================================================
// Tool safety-hint annotations
//
// Each tool is tagged with the MCP `ToolAnnotations` hints
// (readOnlyHint, destructiveHint, idempotentHint, openWorldHint) so clients
// can reason about safety before invoking. These are *hints* per the MCP spec.
// ============================================================================

/// Read-only, idempotent, closed-world (queries: screenshots, snapshots, finds).
fn annotate_read_only() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(true)
        .idempotent(true)
        .destructive(false)
        .open_world(false)
}

/// Non-destructive state change on a closed world (clicks, typing, scrolling,
/// focusing, launching a local app, connecting to a local debug server).
fn annotate_state_change() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .idempotent(false)
        .destructive(false)
        .open_world(false)
}

/// Destructive tool on a closed world (quit app, close tab).
fn annotate_destructive() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .idempotent(false)
        .destructive(true)
        .open_world(false)
}

/// Non-destructive state change that reaches an open world (e.g. web
/// navigation, arbitrary URL loads).
fn annotate_open_world_state_change() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .idempotent(false)
        .destructive(false)
        .open_world(true)
}

/// Arbitrary code evaluation in an open world (JS eval in a browser page).
fn annotate_open_world_destructive() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .idempotent(false)
        .destructive(true)
        .open_world(true)
}

/// Apply the given annotation to every tool in `tools` whose name appears in
/// `names`. Missing names are silently ignored because the conditional tool
/// groups (app_*, android_*, cdp_*, hover, recording) aren't always present.
/// The `test_every_tool_has_annotations` test catches any tool left without
/// an annotation.
fn annotate_tools(tools: &mut [Tool], names: &[&str], annotation: ToolAnnotations) {
    for name in names {
        if let Some(tool) = tools.iter_mut().find(|t| t.name.as_ref() == *name) {
            tool.annotations = Some(annotation.clone());
        }
    }
}

#[derive(Clone)]
pub struct MacOSDevToolsServer {
    app_client: Arc<RwLock<Option<AppProtocolClient>>>,
    screenshot_cache: Arc<RwLock<ScreenshotCache>>,
    image_cache: Arc<RwLock<ImageCache>>,
    android_device: Arc<RwLock<Option<AndroidDevice>>>,
    hover_tracker: Arc<RwLock<Option<crate::tools::hover_tracker::HoverTracker>>>,
    screen_recorder: Arc<RwLock<Option<crate::tools::screen_recorder::ScreenRecorder>>>,
    #[cfg(feature = "cdp")]
    cdp_client: Arc<RwLock<Option<crate::cdp::CdpClient>>>,
    #[cfg(target_os = "macos")]
    ax_session: Arc<crate::tools::ax_session::AxSession>,
}

impl Default for MacOSDevToolsServer {
    fn default() -> Self {
        Self::new()
    }
}

impl MacOSDevToolsServer {
    pub fn new() -> Self {
        Self {
            app_client: Arc::new(RwLock::new(None)),
            screenshot_cache: Arc::new(RwLock::new(ScreenshotCache::default())),
            image_cache: Arc::new(RwLock::new(ImageCache::default())),
            android_device: Arc::new(RwLock::new(None)),
            hover_tracker: Arc::new(RwLock::new(None)),
            screen_recorder: Arc::new(RwLock::new(None)),
            #[cfg(feature = "cdp")]
            cdp_client: Arc::new(RwLock::new(None)),
            #[cfg(target_os = "macos")]
            ax_session: Arc::new(crate::tools::ax_session::AxSession::new()),
        }
    }

    async fn is_connected(&self) -> bool {
        self.app_client.read().await.is_some()
    }

    async fn is_android_connected(&self) -> bool {
        self.android_device.read().await.is_some()
    }

    async fn is_hover_tracking(&self) -> bool {
        self.hover_tracker.read().await.is_some()
    }

    async fn is_recording(&self) -> bool {
        self.screen_recorder.read().await.is_some()
    }

    #[cfg(feature = "cdp")]
    async fn is_cdp_connected(&self) -> bool {
        self.cdp_client.read().await.is_some()
    }

    /// Acquire the android device lock and call `f` with a mutable reference.
    /// Returns a "not connected" error result if no device is connected.
    async fn with_android_device<F>(&self, f: F) -> CallToolResult
    where
        F: FnOnce(&mut AndroidDevice) -> CallToolResult,
    {
        let mut guard = self.android_device.write().await;
        match guard.as_mut() {
            Some(device) => f(device),
            None => CallToolResult::error(vec![ContentBlock::text(
                "No Android device connected. Use android_connect first.",
            )]),
        }
    }

    /// Get tools available based on connection state.
    /// Base tools and app_connect are always available.
    /// Other app_* tools are only available when connected.
    ///
    /// CDP tools are always listed (independent of `cdp_connected`) so the
    /// tool surface does not mutate mid-session — clients that prompt-cache
    /// the tool list stay warm. Each CDP tool handler returns a clean
    /// "No CDP connection" error when called without an active connection.
    /// The `cdp_connected` parameter is accepted for API stability but is
    /// no longer used to gate visibility.
    pub fn get_tools(
        app_connected: bool,
        android_connected: bool,
        cdp_connected: bool,
        hover_tracking: bool,
        recording: bool,
    ) -> Vec<Tool> {
        let _ = cdp_connected;
        let mut tools = Self::get_base_tools();
        tools.push(Self::get_app_connect_tool());
        if app_connected {
            tools.extend(Self::get_app_tools());
        }
        tools.extend(Self::get_android_base_tools());
        if android_connected {
            tools.extend(Self::get_android_tools());
        }
        #[cfg(feature = "cdp")]
        {
            tools.push(Self::get_cdp_connect_tool());
            tools.extend(Self::get_cdp_tools());
        }
        tools.extend(Self::get_hover_tracking_tools(hover_tracking));
        tools.extend(Self::get_recording_tools(recording));
        Self::apply_tool_annotations(&mut tools);
        tools
    }

    /// Attach MCP safety-hint annotations (readOnlyHint, destructiveHint,
    /// idempotentHint, openWorldHint) to every tool in the list.
    ///
    /// Classification keys off tool *name* (not description or schema) so
    /// it's stable across schema edits. Tool names absent from `tools`
    /// (conditional groups gated by connection state) are ignored —
    /// `test_every_tool_has_annotations` catches any unclassified tool.
    fn apply_tool_annotations(tools: &mut [Tool]) {
        // Read-only queries: screenshots, snapshots, finds, metadata.
        annotate_tools(
            tools,
            &[
                "take_screenshot",
                "list_windows",
                "list_apps",
                "get_displays",
                "find_text",
                "element_at_point",
                "find_image",
                "probe_app",
                "android_list_devices",
                "app_get_info",
                "app_get_tree",
                "app_query",
                "app_get_element",
                "app_list_windows",
                "app_screenshot",
                "android_screenshot",
                "android_find_text",
                "android_list_apps",
                "android_get_display_info",
                "android_get_current_activity",
            ],
            annotate_read_only(),
        );

        // Non-destructive state changes: clicks, typing, launches, sessions.
        annotate_tools(
            tools,
            &[
                "focus_window",
                "launch_app",
                "click",
                "move_mouse",
                "drag",
                "scroll",
                "type_text",
                "press_key",
                "load_image",
                "app_connect",
                "android_connect",
                "start_hover_tracking",
                "start_recording",
                "app_disconnect",
                "app_click",
                "app_type",
                "app_press_key",
                "app_focus",
                "app_focus_window",
                "android_disconnect",
                "android_click",
                "android_swipe",
                "android_type_text",
                "android_press_key",
                "android_launch_app",
                "get_hover_events",
                "stop_hover_tracking",
                "stop_recording",
            ],
            annotate_state_change(),
        );

        // take_ax_snapshot is state-changing on both platforms: macOS bumps
        // the session generation on every call (invalidating prior uids);
        // Windows advertises the same posture for a uniform client-safety
        // contract even though the underlying UIA read is still pure.
        annotate_tools(tools, &["take_ax_snapshot"], annotate_state_change());

        #[cfg(target_os = "macos")]
        {
            annotate_tools(
                tools,
                &["ax_click", "ax_set_value", "ax_select"],
                annotate_state_change(),
            );
        }

        annotate_tools(tools, &["quit_app"], annotate_destructive());

        #[cfg(feature = "cdp")]
        {
            annotate_tools(
                tools,
                &[
                    "cdp_take_dom_snapshot",
                    "cdp_summarize_page",
                    "cdp_find_elements",
                    "cdp_get_element_context",
                    "cdp_list_pages",
                    "cdp_element_at_point",
                    "cdp_wait_for",
                    "cdp_wait_for_page_change",
                ],
                annotate_read_only(),
            );
            annotate_tools(
                tools,
                &[
                    "cdp_connect",
                    "cdp_disconnect",
                    "cdp_click",
                    "cdp_hover",
                    "cdp_fill",
                    "cdp_press_key",
                    "cdp_handle_dialog",
                    "cdp_type_text",
                    "cdp_select_page",
                ],
                annotate_state_change(),
            );
            annotate_tools(
                tools,
                &["cdp_navigate", "cdp_new_page"],
                annotate_open_world_state_change(),
            );
            annotate_tools(
                tools,
                &["cdp_evaluate_script"],
                annotate_open_world_destructive(),
            );
            annotate_tools(tools, &["cdp_close_page"], annotate_destructive());
        }
    }

    /// Tools that are always available (system tools, CGEvent tools, etc.)
    fn get_base_tools() -> Vec<Tool> {
        #[allow(unused_mut)]
        let mut tools = vec![
            Tool::new(
                "take_screenshot",
                "Capture a screenshot of the screen, a specific window, or a region. Returns a base64-encoded image, JSON metadata for coordinate conversion, and OCR text annotations including clickable coordinates.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "mode": {
                            "type": "string",
                            "enum": ["screen", "window", "region"],
                            "description": "Capture mode (default: 'window'). Prefer 'window' with app_name for focused screenshots. Only use 'screen' when you need to see multiple windows or the desktop.",
                            "default": "window"
                        },
                        "window_id": {
                            "type": "integer",
                            "description": "Window ID to capture (required for mode='window')"
                        },
                        "app_name": {
                            "type": "string",
                            "description": "Application name to capture (for mode='window', alternative to window_id)"
                        },
                        "x": {
                            "type": "number",
                            "description": "X coordinate of region (required for mode='region')"
                        },
                        "y": {
                            "type": "number",
                            "description": "Y coordinate of region (required for mode='region')"
                        },
                        "width": {
                            "type": "number",
                            "description": "Width of region (required for mode='region')"
                        },
                        "height": {
                            "type": "number",
                            "description": "Height of region (required for mode='region')"
                        },
                        "include_ocr": {
                            "type": "boolean",
                            "description": "Include OCR text detection with clickable coordinates (default: true)",
                            "default": true
                        }
                    }
                }))),
            ),
            Tool::new(
                "list_windows",
                "List all visible windows on screen with their IDs, titles, app names, and bounds.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app_name": {
                            "type": "string",
                            "description": "Filter windows by application name (optional)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "list_apps",
                "List all running applications with their names, bundle IDs, and PIDs.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app_name": {
                            "type": "string",
                            "description": "Filter by application name (case-insensitive substring match)"
                        },
                        "user_apps_only": {
                            "type": "boolean",
                            "description": "Only return user-facing apps (excludes system agents, helpers, and daemons)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "focus_window",
                "Bring a window or application to the front. Specify window_id, app_name, or pid.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "window_id": {
                            "type": "integer",
                            "description": "Window ID to focus"
                        },
                        "app_name": {
                            "type": "string",
                            "description": "Application name to focus"
                        },
                        "pid": {
                            "type": "integer",
                            "description": "Process ID to focus"
                        }
                    }
                }))),
            ),
            Tool::new(
                "launch_app",
                "Launch an application by name. On macOS, finds apps in /Applications and other standard locations. If the app is already running and no args are provided, brings it to the front. If args are provided and the app is already running, returns an error — use quit_app first.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["app_name"],
                    "properties": {
                        "app_name": {
                            "type": "string",
                            "description": "Application name to launch (e.g., 'Calculator', 'Safari')"
                        },
                        "args": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "CLI arguments to pass to the app (e.g., ['--remote-debugging-port=9222']). Only applied on fresh launch — if the app is already running, returns an error."
                        },
                        "background": {
                            "type": "boolean",
                            "description": "If true, launch without bringing the app to the foreground (uses `open -g` on macOS). Recommended when the next action will use CDP or AX dispatch, which are focus-preserving."
                        }
                    }
                }))),
            ),
            Tool::new(
                "quit_app",
                "Quit a running application by name. Graceful by default (app can save state). Use force=true to kill immediately.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["app_name"],
                    "properties": {
                        "app_name": {
                            "type": "string",
                            "description": "Application name to quit (e.g., 'Calculator', 'Safari')"
                        },
                        "force": {
                            "type": "boolean",
                            "description": "Force kill instead of graceful quit (default: false)"
                        }
                    }
                }))),
            ),
            // System-level input tools (CGEvent on macOS, SendInput on Windows)
            Tool::new(
                "click",
                "Click at screen coordinates. Pass exactly one coordinate variant — the runtime \
                 rejects mixes. Variants: \
                 (1) 'screenshot-pixels' (PREFERRED after take_screenshot) — screenshot_x, \
                 screenshot_y, screenshot_origin_x, screenshot_origin_y, screenshot_scale from \
                 take_screenshot metadata; \
                 (2) 'screen' — absolute screen x, y (use with find_text results); \
                 (3) 'window-relative' — window_x, window_y, window_id from list_windows; \
                 (4) 'screenshot-pixels-legacy' (DEPRECATED) — screenshot_x, screenshot_y, \
                 screenshot_window_id. \
                 Works with any app (egui, Electron, etc.). Requires Accessibility permission on macOS.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "x": {
                            "type": "number",
                            "description": "[screen variant] Absolute screen X coordinate. Use with find_text results."
                        },
                        "y": {
                            "type": "number",
                            "description": "[screen variant] Absolute screen Y coordinate. Use with find_text results."
                        },
                        "window_x": {
                            "type": "number",
                            "description": "[window-relative variant] X relative to window top-left. Pair with window_y and window_id."
                        },
                        "window_y": {
                            "type": "number",
                            "description": "[window-relative variant] Y relative to window top-left. Pair with window_x and window_id."
                        },
                        "window_id": {
                            "type": "integer",
                            "description": "[window-relative variant] Target window ID (from list_windows)."
                        },
                        "screenshot_x": {
                            "type": "number",
                            "description": "[screenshot-pixels / screenshot-pixels-legacy] X pixel inside the screenshot image."
                        },
                        "screenshot_y": {
                            "type": "number",
                            "description": "[screenshot-pixels / screenshot-pixels-legacy] Y pixel inside the screenshot image."
                        },
                        "screenshot_origin_x": {
                            "type": "number",
                            "description": "[screenshot-pixels, PREFERRED] screenshot_origin_x from take_screenshot metadata."
                        },
                        "screenshot_origin_y": {
                            "type": "number",
                            "description": "[screenshot-pixels, PREFERRED] screenshot_origin_y from take_screenshot metadata."
                        },
                        "screenshot_scale": {
                            "type": "number",
                            "description": "[screenshot-pixels, PREFERRED] screenshot_scale from take_screenshot metadata."
                        },
                        "screenshot_window_id": {
                            "type": "integer",
                            "description": "[screenshot-pixels-legacy, DEPRECATED] Window ID the screenshot was taken from. Prefer screenshot_origin_x/y + screenshot_scale."
                        },
                        "button": {
                            "type": "string",
                            "enum": ["left", "right", "center"],
                            "description": "Mouse button (default: left)"
                        },
                        "click_count": {
                            "type": "integer",
                            "description": "Number of clicks (1=single, 2=double)",
                            "default": 1
                        }
                    }
                }))),
            ),
            Tool::new(
                "move_mouse",
                "Move mouse cursor to screen coordinates. Requires Accessibility permission on macOS.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["x", "y"],
                    "properties": {
                        "x": {
                            "type": "number",
                            "description": "Screen X coordinate"
                        },
                        "y": {
                            "type": "number",
                            "description": "Screen Y coordinate"
                        }
                    }
                }))),
            ),
            Tool::new(
                "drag",
                "Drag from one point to another. Requires Accessibility permission on macOS.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["start_x", "start_y", "end_x", "end_y"],
                    "properties": {
                        "start_x": {
                            "type": "number",
                            "description": "Start X coordinate"
                        },
                        "start_y": {
                            "type": "number",
                            "description": "Start Y coordinate"
                        },
                        "end_x": {
                            "type": "number",
                            "description": "End X coordinate"
                        },
                        "end_y": {
                            "type": "number",
                            "description": "End Y coordinate"
                        },
                        "button": {
                            "type": "string",
                            "enum": ["left", "right", "center"],
                            "description": "Mouse button (default: left)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "scroll",
                "Scroll at a position. Requires Accessibility permission on macOS.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["x", "y", "delta_y"],
                    "properties": {
                        "x": {
                            "type": "number",
                            "description": "Screen X coordinate to scroll at"
                        },
                        "y": {
                            "type": "number",
                            "description": "Screen Y coordinate to scroll at"
                        },
                        "delta_x": {
                            "type": "integer",
                            "description": "Horizontal scroll amount (positive=right)",
                            "default": 0
                        },
                        "delta_y": {
                            "type": "integer",
                            "description": "Vertical scroll amount (negative=up, positive=down)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "type_text",
                "Type text at the current cursor position. Works with any app. Requires Accessibility permission on macOS.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Text to type"
                        }
                    }
                }))),
            ),
            Tool::new(
                "press_key",
                "Press a key combination. Works with any app. Requires Accessibility permission on macOS.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["key"],
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to press (e.g., 'return', 'tab', 'escape', 'a', 'f1', 'left', 'up')"
                        },
                        "modifiers": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Modifier keys: 'shift', 'control', 'option', 'command'",
                            "default": []
                        }
                    }
                }))),
            ),
            Tool::new(
                "get_displays",
                "Get information about all connected displays including bounds, scale factors, and resolution.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "find_text",
                "PREFERRED for clicking buttons/labels by name. Finds text on screen using the platform accessibility API (macOS Accessibility, Windows UI Automation) with OCR fallback, and returns screen coordinates ready for the click tool. Use this instead of visually estimating coordinates from screenshots. Can be scoped to a specific app window for faster, more precise results. Note: accessibility results use semantic element names (e.g., 'All Clear' instead of 'AC', 'Subtract' instead of '\u{2212}'), so search by meaning rather than displayed symbol. When no matches are found, the response includes an available_elements array listing all UI element names in the target window — use this to find the correct name and retry. Requires macOS 10.15+ or Windows 10 1903+.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Text to search for (case-insensitive substring match). Matches against accessibility element names first (e.g., 'All Clear', 'Subtract'), then falls back to OCR on visible text."
                        },
                        "app_name": {
                            "type": "string",
                            "description": "Application name to scope the search to a specific app's window (e.g., 'Calculator'). Faster and avoids false matches from other windows."
                        },
                        "window_id": {
                            "type": "integer",
                            "description": "Window ID to scope the search to a specific window"
                        },
                        "display_id": {
                            "type": "integer",
                            "description": "Display ID to search on. Use get_displays to list available displays. If omitted, searches the main display. Ignored when window_id or app_name is provided."
                        },
                        "uses_language_correction": {
                            "type": "boolean",
                            "description": "Enable language correction for better word accuracy in OCR fallback. Default is false, which is better for UI automation (buttons, labels, single characters). Has no effect when results come from the accessibility API."
                        }
                    }
                }))),
            ),
            Tool::new(
                "element_at_point",
                "Given screen coordinates (x, y), return the accessibility element at that point (name, role, label, value, bounds, pid, app_name). Optional app_name param to scope the lookup to a specific application for faster, more precise results.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["x", "y"],
                    "properties": {
                        "x": {
                            "type": "number",
                            "description": "Screen X coordinate"
                        },
                        "y": {
                            "type": "number",
                            "description": "Screen Y coordinate"
                        },
                        "app_name": {
                            "type": "string",
                            "description": "Application name to scope the lookup to a specific app (e.g., 'Calculator'). Faster and avoids ambiguity at window edges."
                        }
                    }
                }))),
            ),
            Tool::new(
                "find_image",
                "Find a template image within a screenshot using template matching. Returns precise click coordinates for non-text UI elements like icons and shapes. Use screenshot_id from take_screenshot or provide screenshot_image_base64. Use template_id from load_image or provide template_image_base64.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "screenshot_id": {
                            "type": "string",
                            "description": "Screenshot ID from a previous take_screenshot call (preferred)"
                        },
                        "screenshot_image_base64": {
                            "type": "string",
                            "description": "Base64-encoded screenshot image (used if no screenshot_id)"
                        },
                        "template_id": {
                            "type": "string",
                            "description": "Image ID from a previous load_image call (preferred over template_image_base64)"
                        },
                        "template_image_base64": {
                            "type": "string",
                            "description": "Base64-encoded template image to find (used if no template_id)"
                        },
                        "mask_id": {
                            "type": "string",
                            "description": "Image ID from a previous load_image call for the mask"
                        },
                        "mask_image_base64": {
                            "type": "string",
                            "description": "Base64-encoded mask image (optional; white=match, black=ignore)"
                        },
                        "mode": {
                            "type": "string",
                            "enum": ["fast", "accurate"],
                            "description": "Matching mode: 'fast' (default) for quick searches, 'accurate' for thorough matching",
                            "default": "fast"
                        },
                        "threshold": {
                            "type": "number",
                            "description": "Minimum match score 0.0-1.0 (default: 0.75)"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum matches to return (default: 3 fast, 5 accurate)"
                        },
                        "scales": {
                            "type": "object",
                            "description": "Scale search range {min, max, step}",
                            "properties": {
                                "min": { "type": "number", "default": 0.8 },
                                "max": { "type": "number", "default": 1.2 },
                                "step": { "type": "number", "default": 0.1 }
                            }
                        },
                        "search_region": {
                            "type": "object",
                            "description": "Limit search to region {x, y, w, h} in screenshot pixels",
                            "properties": {
                                "x": { "type": "integer" },
                                "y": { "type": "integer" },
                                "w": { "type": "integer" },
                                "h": { "type": "integer" }
                            }
                        },
                        "stride": {
                            "type": "integer",
                            "description": "Search step size (default: 2 fast, 1 accurate)"
                        },
                        "rotations": {
                            "type": "array",
                            "items": { "type": "number" },
                            "description": "Rotations to try in degrees (only 0, 90, 180, 270 supported)"
                        },
                        "return_screen_coords": {
                            "type": "boolean",
                            "description": "Include screen coordinates for clicking (default: true)",
                            "default": true
                        }
                    }
                }))),
            ),
            Tool::new(
                "load_image",
                "Load an image from a local file path and cache it for use with find_image. Returns an image_id that can be passed to find_image as template_id or mask_id. This avoids manually base64-encoding images.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["path"],
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Local filesystem path to the image file"
                        },
                        "id_prefix": {
                            "type": "string",
                            "description": "Optional prefix for the generated ID (e.g., 'template', 'mask')"
                        },
                        "max_width": {
                            "type": "integer",
                            "description": "Maximum width to downscale to (maintains aspect ratio)"
                        },
                        "max_height": {
                            "type": "integer",
                            "description": "Maximum height to downscale to (maintains aspect ratio)"
                        },
                        "as_mask": {
                            "type": "boolean",
                            "description": "If true, convert to single-channel grayscale mask",
                            "default": false
                        },
                        "return_base64": {
                            "type": "boolean",
                            "description": "If true, include base64-encoded image data in response",
                            "default": false
                        }
                    }
                }))),
            ),
            Tool::new(
                "take_ax_snapshot",
                "Take an accessibility tree snapshot of an application. Returns a \
                 structured text representation with unique element IDs, roles, names, \
                 state attributes, and (on macOS) per-element bounding boxes. Works for \
                 any app without requiring a debug port. \
                 macOS note: this tool mutates server state — each call bumps a \
                 monotonic generation and invalidates every prior uid for ax_click / \
                 ax_set_value / ax_select consumption. Snapshot IDs on macOS look like \
                 'a42g3' (generation-tagged); Windows IDs remain bare 'a42'. \
                 Re-snapshot immediately before any ax_click / ax_set_value / ax_select \
                 call.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "app_name": {
                            "type": "string",
                            "description": "Application name (defaults to frontmost app if omitted)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "probe_app",
                "Probe an application to determine its type (Native, ElectronApp, or ChromeBrowser). Works whether the app is running or not. Use this to decide between native automation (take_ax_snapshot, click, find_text) and CDP-based tools (cdp_connect, cdp_find_elements, cdp_take_dom_snapshot).",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["app_name"],
                    "properties": {
                        "app_name": {
                            "type": "string",
                            "description": "Application name to probe (e.g., 'Signal', 'Google Chrome', 'Safari')"
                        }
                    }
                }))),
            ),
        ];
        #[cfg(target_os = "macos")]
        {
            tools.push(Self::get_ax_click_tool());
            tools.push(Self::get_ax_set_value_tool());
            tools.push(Self::get_ax_select_tool());
        }
        tools
    }

    #[cfg(target_os = "macos")]
    fn get_ax_click_tool() -> Tool {
        Tool::new(
            "ax_click",
            "macOS only. Dispatch AXPress against a UI element identified by its uid \
             from the most recent take_ax_snapshot (e.g. \"a42g3\"). The 'g<gen>' \
             suffix is a generation tag — any fresh take_ax_snapshot invalidates all \
             prior uids, so always snapshot immediately before the uid is used. Does \
             not move the mouse cursor and does not steal focus from the frontmost \
             app. On failure the tool returns an error whose JSON body includes \
             { error: { code, message, fallback: { x, y } | null } }; when fallback is \
             populated you can retry via click(x, y).",
            Arc::new(json_to_object(serde_json::json!({
                "type": "object",
                "required": ["uid"],
                "properties": {
                    "uid": {
                        "type": "string",
                        "description": "Element uid from the most recent take_ax_snapshot, e.g. \"a42g3\". Must match the current snapshot generation."
                    }
                }
            }))),
        )
    }

    #[cfg(target_os = "macos")]
    fn get_ax_set_value_tool() -> Tool {
        Tool::new(
            "ax_set_value",
            "macOS only. Write to an element's kAXValueAttribute — value assignment, \
             not key-event typing. Use for AXTextField / AXTextArea / AXSearchField and \
             similar text widgets. Does NOT fire keydown/keyup, does NOT participate in \
             IME/composition, does NOT populate the app's undo stack, and will not work \
             on rich editors that refuse AXValue writes. On not_dispatchable, the caller \
             should fall back to a two-step sequence: click(fallback.x, fallback.y) to \
             focus, then type_text(text) for key-event input.",
            Arc::new(json_to_object(serde_json::json!({
                "type": "object",
                "required": ["uid", "text"],
                "properties": {
                    "uid": {
                        "type": "string",
                        "description": "Element uid from the most recent take_ax_snapshot, e.g. \"a42g3\"."
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to assign via kAXValueAttribute."
                    }
                }
            }))),
        )
    }

    #[cfg(target_os = "macos")]
    fn get_ax_select_tool() -> Tool {
        Tool::new(
            "ax_select",
            "macOS only. Select a row inside an NSOutlineView / NSTableView (sidebars, \
             rule lists, file browsers) by writing AXSelectedRows on the enclosing \
             outline or table. Accepts a uid pointing at the row, a cell inside the row, \
             or any descendant — the tool walks up to the enclosing AXRow then the \
             enclosing AXOutline / AXTable. Does not move the cursor and does not steal \
             focus. Use ax_select instead of ax_click for row targets: rows typically \
             refuse AXPress (returning not_dispatchable or AX error -25205), and a \
             coordinate click steals focus. On failure the tool returns { error: { code, \
             message, fallback: { x, y } | null } } with codes snapshot_expired, \
             uid_not_found, no_row_ancestor, no_outline_container, not_dispatchable, or \
             ax_error. The fallback centre falls back from the row bbox to the \
             originally-targeted element bbox so the caller can still click(x, y) if \
             desired.",
            Arc::new(json_to_object(serde_json::json!({
                "type": "object",
                "required": ["uid"],
                "properties": {
                    "uid": {
                        "type": "string",
                        "description": "Element uid from the most recent take_ax_snapshot. Points at the row itself, a cell within the row, or any descendant."
                    }
                }
            }))),
        )
    }

    /// The app_connect tool - always available to initiate connections
    fn get_app_connect_tool() -> Tool {
        Tool::new(
            "app_connect",
            "Connect to an app's debug server via WebSocket. The app must have AppDebugKit embedded.",
            Arc::new(json_to_object(serde_json::json!({
                "type": "object",
                "required": ["url"],
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "WebSocket URL (e.g., ws://127.0.0.1:9222)"
                    },
                    "expected_bundle_id": {
                        "type": "string",
                        "description": "Expected bundle ID (e.g., com.example.MyApp). Connection fails if mismatch."
                    },
                    "expected_app_name": {
                        "type": "string",
                        "description": "Expected app name (case-insensitive). Connection fails if mismatch."
                    }
                }
            }))),
        )
    }

    /// App debug tools - only available when connected to an app
    fn get_app_tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "app_disconnect",
                "Disconnect from the app's debug server.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "app_get_info",
                "Get runtime info from the connected app (name, bundle ID, version, etc.).",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "app_get_tree",
                "Get the view hierarchy from the connected app.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "depth": {
                            "type": "integer",
                            "description": "Max depth to traverse (-1 for unlimited)",
                            "default": 5
                        },
                        "root_id": {
                            "type": "string",
                            "description": "Element ID to start from (optional, defaults to key window)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "app_query",
                "Find elements matching a CSS-like selector in the connected app.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["selector"],
                    "properties": {
                        "selector": {
                            "type": "string",
                            "description": "CSS-like selector (#id, .ClassName, [prop=value])"
                        },
                        "all": {
                            "type": "boolean",
                            "description": "Return all matches (default: first only)",
                            "default": false
                        }
                    }
                }))),
            ),
            Tool::new(
                "app_get_element",
                "Get detailed information about an element by ID.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["element_id"],
                    "properties": {
                        "element_id": {
                            "type": "string",
                            "description": "Element ID to get details for"
                        }
                    }
                }))),
            ),
            Tool::new(
                "app_click",
                "Click an element in the connected app by ID.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["element_id"],
                    "properties": {
                        "element_id": {
                            "type": "string",
                            "description": "Element ID to click"
                        },
                        "click_count": {
                            "type": "integer",
                            "description": "Number of clicks (1 for single, 2 for double)",
                            "default": 1
                        }
                    }
                }))),
            ),
            Tool::new(
                "app_type",
                "Type text into an element in the connected app.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Text to type"
                        },
                        "element_id": {
                            "type": "string",
                            "description": "Element ID to type into (uses focused element if omitted)"
                        },
                        "clear_first": {
                            "type": "boolean",
                            "description": "Clear existing text first",
                            "default": false
                        }
                    }
                }))),
            ),
            Tool::new(
                "app_press_key",
                "Press a key or key combination in the connected app.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["key"],
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to press (e.g., 'Return', 'Tab', 'Escape')"
                        },
                        "modifiers": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Modifier keys: 'shift', 'control', 'option', 'command'",
                            "default": []
                        }
                    }
                }))),
            ),
            Tool::new(
                "app_focus",
                "Focus an element in the connected app (make it first responder).",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["element_id"],
                    "properties": {
                        "element_id": {
                            "type": "string",
                            "description": "Element ID to focus"
                        }
                    }
                }))),
            ),
            Tool::new(
                "app_screenshot",
                "Take a screenshot of an element or window in the connected app.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "element_id": {
                            "type": "string",
                            "description": "Element ID to capture (whole window if omitted)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "app_list_windows",
                "List all windows in the connected app.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "app_focus_window",
                "Focus a window in the connected app (make it key and main window).",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["window_id"],
                    "properties": {
                        "window_id": {
                            "type": "string",
                            "description": "Window ID to focus (e.g., 'window-1')"
                        }
                    }
                }))),
            ),
        ]
    }

    /// Android tools that are always available (device discovery and connection)
    fn get_android_base_tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "android_list_devices",
                "List all Android devices connected via ADB.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "android_connect",
                "Connect to an Android device by its serial number. Use android_list_devices to find available devices.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["serial"],
                    "properties": {
                        "serial": {
                            "type": "string",
                            "description": "Device serial number (e.g., 'emulator-5554' or a USB device serial)"
                        }
                    }
                }))),
            ),
        ]
    }

    /// Android tools available only when a device is connected
    fn get_android_tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "android_disconnect",
                "Disconnect from the current Android device.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "android_screenshot",
                "Take a screenshot of the Android device screen. Returns a base64-encoded JPEG image.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "file_path": {
                            "type": "string",
                            "description": "Optional file path to save the screenshot PNG to instead of returning it inline."
                        }
                    }
                }))),
            ),
            Tool::new(
                "android_click",
                "Tap at screen coordinates on the Android device.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["x", "y"],
                    "properties": {
                        "x": {
                            "type": "number",
                            "description": "Screen X coordinate"
                        },
                        "y": {
                            "type": "number",
                            "description": "Screen Y coordinate"
                        }
                    }
                }))),
            ),
            Tool::new(
                "android_swipe",
                "Swipe from one point to another on the Android device.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["start_x", "start_y", "end_x", "end_y"],
                    "properties": {
                        "start_x": {
                            "type": "number",
                            "description": "Start X coordinate"
                        },
                        "start_y": {
                            "type": "number",
                            "description": "Start Y coordinate"
                        },
                        "end_x": {
                            "type": "number",
                            "description": "End X coordinate"
                        },
                        "end_y": {
                            "type": "number",
                            "description": "End Y coordinate"
                        },
                        "duration_ms": {
                            "type": "integer",
                            "description": "Duration of the swipe in milliseconds (optional, default is instant)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "android_type_text",
                "Type text on the Android device. Special characters are automatically escaped.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Text to type"
                        }
                    }
                }))),
            ),
            Tool::new(
                "android_press_key",
                "Press a key on the Android device by keycode name (e.g., 'KEYCODE_HOME') or numeric code.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["key"],
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "Key to press (e.g., 'KEYCODE_HOME', 'KEYCODE_BACK', 'KEYCODE_ENTER', or a numeric keycode)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "android_find_text",
                "Find UI elements on the Android device screen that match the given text. Uses uiautomator to dump the view hierarchy and search for matching elements. Returns coordinates for clicking. When no matches are found, the response includes an available_elements array listing all UI element names on screen — use this to find the correct name and retry.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "Text to search for (case-insensitive substring match against text and content-desc attributes)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "android_list_apps",
                "List installed apps on the Android device.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "user_apps_only": {
                            "type": "boolean",
                            "description": "Only return user-installed (third-party) apps. Default is false (all packages)."
                        }
                    }
                }))),
            ),
            Tool::new(
                "android_launch_app",
                "Launch an app on the Android device by its package name.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["package_name"],
                    "properties": {
                        "package_name": {
                            "type": "string",
                            "description": "Package name to launch (e.g., 'com.android.settings')"
                        }
                    }
                }))),
            ),
            Tool::new(
                "android_get_display_info",
                "Get display information (size and density) from the Android device.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "android_get_current_activity",
                "Get the currently resumed activity on the Android device.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
        ]
    }

    /// Hover tracking tools. `start_hover_tracking` is always visible.
    /// `get_hover_events` and `stop_hover_tracking` only appear while tracking is active.
    fn get_hover_tracking_tools(tracking_active: bool) -> Vec<Tool> {
        let mut tools = vec![Tool::new(
            "start_hover_tracking",
            "Start tracking hover state changes. Polls cursor position and accessibility element at configurable intervals, recording transitions. Use get_hover_events to retrieve recorded events, and stop_hover_tracking to end the session. Only one tracking session can be active at a time.",
            Arc::new(json_to_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "app_name": {
                        "type": "string",
                        "description": "Scope element lookup to a specific application (e.g., 'Safari'). Faster and avoids ambiguity."
                    },
                    "poll_interval_ms": {
                        "type": "integer",
                        "description": "Polling interval in milliseconds (default: 100)",
                        "default": 100
                    },
                    "max_duration_ms": {
                        "type": "integer",
                        "description": "Auto-stop after this many milliseconds (default: 60000 = 60s)",
                        "default": 60000
                    },
                    "min_dwell_ms": {
                        "type": "integer",
                        "description": "Minimum time (ms) cursor must stay on a new element before recording a transition. Filters out pass-through elements during fast mouse movement. 0 = record every change immediately. (default: 300)",
                        "default": 300
                    }
                }
            }))),
        )];
        if tracking_active {
            tools.push(Tool::new(
                "get_hover_events",
                "Retrieve and drain buffered hover events since the last call. Returns a JSON array of transition events, each with cursor position, element info, timestamp, and dwell time. Events are consumed — subsequent calls return only new events.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ));
            tools.push(Tool::new(
                "stop_hover_tracking",
                "Stop hover tracking and return any remaining buffered events. Ends the background polling task.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ));
        }
        tools
    }

    /// Screen recording tools. `start_recording` always visible,
    /// `stop_recording` only while recording is active.
    fn get_recording_tools(recording_active: bool) -> Vec<Tool> {
        let mut tools = vec![Tool::new(
            "start_recording",
            "Start recording the frontmost app's window at ~5fps. Writes timestamped JPEG frames to the specified output directory. Use stop_recording to end the session and get the frame list.",
            Arc::new(json_to_object(serde_json::json!({
                "type": "object",
                "properties": {
                    "output_dir": {
                        "type": "string",
                        "description": "Directory to write JPEG frames to (created if needed)"
                    },
                    "fps": {
                        "type": "integer",
                        "description": "Frames per second (default: 5)",
                        "default": 5
                    },
                    "max_duration_ms": {
                        "type": "integer",
                        "description": "Auto-stop after this many milliseconds (default: 60000 = 1 min)",
                        "default": 60000
                    }
                },
                "required": ["output_dir"]
            }))),
        )];
        if recording_active {
            tools.push(Tool::new(
                "stop_recording",
                "Stop screen recording and return all frame metadata as a JSON array.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ));
        }
        tools
    }

    #[cfg(feature = "cdp")]
    fn get_cdp_connect_tool() -> Tool {
        Tool::new(
            "cdp_connect",
            "Connect to a Chrome or Electron app via its remote debugging port. The app must be launched with --remote-debugging-port=PORT and --user-data-dir=PATH (Chrome 136+ requires a non-default profile for the debug port to open). After connecting, use cdp_summarize_page for page inventory, cdp_find_elements for targeted discovery, and cdp_get_element_context to expand an ambiguous match.",
            Arc::new(json_to_object(serde_json::json!({
                "type": "object",
                "required": ["port"],
                "properties": {
                    "port": {
                        "type": "integer",
                        "description": "The remote debugging port number"
                    }
                }
            }))),
        )
    }

    #[cfg(feature = "cdp")]
    const UID_DESC: &'static str =
        "Element UID from cdp_take_dom_snapshot or cdp_find_elements (d-prefixed)";

    fn get_cdp_tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "cdp_disconnect",
                "Disconnect from the Chrome/Electron app. CDP tools remain listed but will return a 'not connected' error until cdp_connect is called again.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "cdp_take_dom_snapshot",
                "Take a full DOM snapshot of the selected browser page. Returns all interactive elements with UIDs prefixed 'd' (e.g., d1, d2). Use when you need the complete page structure — captures contenteditable editors, placeholder inputs, and custom widgets. For targeted lookups, prefer cdp_find_elements instead. UIDs are valid for cdp_click, cdp_fill, and other action tools.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "max_nodes": {
                            "type": "integer",
                            "description": "Maximum number of nodes to return (default: 500)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_summarize_page",
                "Return a compact page summary: URL, title, current page generation, and an inventory of interactive elements grouped by role with a few sample labels. Does not return element UIDs and does not overwrite the current DOM UID snapshot. Use this for orientation before issuing targeted cdp_find_elements queries.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "cdp_find_elements",
                "PREFERRED discovery tool. Search the live DOM for interactive elements matching a text query across labels, visible text, values, placeholders, titles, alt text, and test ids. Returns UIDs prefixed 'd' (e.g., d1, d2), match provenance, visible/accessibility text evidence, parent context for disambiguation, viewport geometry, warnings, and a page-level inventory grouped by role. Always try this first — it gives focused results without flooding context. Use cdp_take_dom_snapshot only if you need the full page structure. UIDs are valid for cdp_click, cdp_fill, and other action tools.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["query"],
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Text to search for across element labels, visible text, values, placeholders, titles, alt text, and test ids"
                        },
                        "role": {
                            "type": "string",
                            "description": "Optional role filter (e.g., 'textbox', 'button')"
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum matches to return (default: 10)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_get_element_context",
                "Expand a UID returned by the most recent cdp_find_elements or cdp_take_dom_snapshot call. Returns the stored match evidence, nearby snapshot matches, and bounded live DOM context around the element (ancestors, siblings, and children). Use this when targeted search returns multiple plausible matches or the match needs local context before acting.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["uid"],
                    "properties": {
                        "uid": {
                            "type": "string",
                            "description": (Self::UID_DESC)
                        },
                        "ancestor_depth": {
                            "type": "integer",
                            "description": "Maximum ancestor levels to include (default: 3, max: 8)"
                        },
                        "sibling_limit": {
                            "type": "integer",
                            "description": "Number of preceding/following siblings to include around the element (default: 2, max: 10)"
                        },
                        "child_limit": {
                            "type": "integer",
                            "description": "Maximum direct children to summarize (default: 8, max: 50)"
                        },
                        "max_chars": {
                            "type": "integer",
                            "description": "Maximum text characters per summarized node (default: 240, range: 40-1000)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_evaluate_script",
                "Evaluate a JavaScript function in the selected browser page. Arbitrary script execution may mutate page or external state and is approval-gated by clients; prefer cdp_find_elements/cdp_take_dom_snapshot for discovery and typed cdp_* action tools for UI changes. Returns the response as JSON. Example without arguments: '() => document.title'. Example with element arguments: pass UIDs from cdp_take_dom_snapshot or cdp_find_elements via args to reference DOM elements, e.g., '(el) => el.innerText' with args=[{uid: 'd5'}].",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["function"],
                    "properties": {
                        "function": {
                            "type": "string",
                            "description": "JavaScript function to evaluate (e.g., '() => document.title' or '(el) => el.innerText')"
                        },
                        "args": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "uid": { "type": "string", "description": (Self::UID_DESC) }
                                }
                            },
                            "description": "Optional element arguments from snapshot UIDs"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_click",
                "Click a DOM element by its UID from a cdp_take_dom_snapshot or cdp_find_elements result. Scrolls the element into view automatically and clicks its center. More reliable than coordinate-based clicking for web content.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["uid"],
                    "properties": {
                        "uid": {
                            "type": "string",
                            "description": (Self::UID_DESC)
                        },
                        "dbl_click": {
                            "type": "boolean",
                            "description": "Double-click instead of single click (default: false)"
                        },
                        "include_snapshot": {
                            "type": "boolean",
                            "description": "Appends a DOM snapshot (d-prefixed UIDs) to the response (default: false)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_list_pages",
                "List all open pages (tabs) in the connected browser. Returns page indices and URLs. The currently selected page is marked with *. Use cdp_select_page to switch between pages.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {}
                }))),
            ),
            Tool::new(
                "cdp_select_page",
                "Select a browser page (tab) by index as context for subsequent CDP operations. Call cdp_list_pages first to see available pages and their indices.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["page_idx"],
                    "properties": {
                        "page_idx": {
                            "type": "integer",
                            "description": "Page index from cdp_list_pages"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_hover",
                "Hover over the provided element.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["uid"],
                    "properties": {
                        "uid": {
                            "type": "string",
                            "description": (Self::UID_DESC)
                        },
                        "include_snapshot": {
                            "type": "boolean",
                            "description": "Appends a DOM snapshot (d-prefixed UIDs) to the response (default: false)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_fill",
                "Type text into an input, text area, rich contenteditable editor, or select an option from a <select> element. Rich editors are focused inside the page and receive CDP keyboard insertion so framework state updates.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["uid", "value"],
                    "properties": {
                        "uid": {
                            "type": "string",
                            "description": (Self::UID_DESC)
                        },
                        "value": {
                            "type": "string",
                            "description": "The value to fill in"
                        },
                        "include_snapshot": {
                            "type": "boolean",
                            "description": "Appends a DOM snapshot (d-prefixed UIDs) to the response (default: false)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_press_key",
                "Press a key or key combination. Use this when other input methods like cdp_fill cannot be used (e.g., keyboard shortcuts, navigation keys, or special key combinations).",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["key"],
                    "properties": {
                        "key": {
                            "type": "string",
                            "description": "A key or combination (e.g., 'Enter', 'Control+A', 'Control++', 'Control+Shift+R'). Modifiers: Control, Shift, Alt, Meta"
                        },
                        "include_snapshot": {
                            "type": "boolean",
                            "description": "Appends a DOM snapshot (d-prefixed UIDs) to the response (default: false)"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_handle_dialog",
                "If a browser dialog was opened, use this command to handle it.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["action"],
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["accept", "dismiss"],
                            "description": "Whether to accept or dismiss the dialog"
                        },
                        "prompt_text": {
                            "type": "string",
                            "description": "Optional text to enter into a prompt dialog before accepting"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_navigate",
                "Navigate the currently selected page to a URL, or go back, forward, or reload.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "Target URL (required when type is 'url')"
                        },
                        "type": {
                            "type": "string",
                            "enum": ["url", "back", "forward", "reload"],
                            "description": "Navigation type. Default: 'url'"
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Maximum wait time in milliseconds for page load (default: 10000). If the page takes longer, navigation is assumed successful."
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_new_page",
                "Create a new page (tab) and navigate it to the given URL. The new page becomes the selected page.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["url"],
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "URL to load in the new page"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_close_page",
                "Close a page (tab) by its index. The last open page cannot be closed.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["page_idx"],
                    "properties": {
                        "page_idx": {
                            "type": "integer",
                            "description": "The index of the page to close. Call cdp_list_pages to list pages."
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_wait_for",
                "Wait for the specified text to appear on the selected page. Resolves when any value appears.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "array",
                            "items": { "type": "string" },
                            "minItems": 1,
                            "description": "Non-empty list of texts. Resolves when any value appears on the page."
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Maximum wait time in milliseconds (default: 10000)"
                        },
                        "include_snapshot": {
                            "type": "boolean",
                            "description": "Appends a DOM snapshot (d-prefixed UIDs) to the response after the text appears (default: false). When false, only a short 'text appeared after Xms' line is returned."
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_wait_for_page_change",
                "Wait until the selected page or a scoped DOM element has a semantic visible-text/editor-value change. Use this for unknown incoming messages, replies, notifications, or page updates after you have chosen the best scope_uid with cdp_find_elements/cdp_get_element_context. This blocks inside one tool call and returns compact before/after deltas.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "scope_uid": {
                            "type": "string",
                            "description": "Optional d-prefixed uid to watch. Prefer a stable container such as a message list, inbox list, thread, panel, or editor. Omit only when a page-level wait is intentional."
                        },
                        "condition": {
                            "type": "string",
                            "description": "Coarse condition label for the wait, e.g. semantic_delta, new_visible_text, item_count_changed. Currently evaluated as semantic_delta and echoed in the result."
                        },
                        "goal": {
                            "type": "string",
                            "description": "Short natural-language goal to echo in the result so the next model turn can judge relevance, e.g. new incoming message in Note to Self."
                        },
                        "timeout": {
                            "type": "integer",
                            "description": "Maximum wait time in milliseconds (default: 55000, capped at 55000 to stay within the Clickweave MCP client timeout)"
                        },
                        "poll_interval_ms": {
                            "type": "integer",
                            "description": "Backup polling interval in milliseconds for changes MutationObserver cannot see (default: 500, clamped to 100-5000)"
                        },
                        "stable_ms": {
                            "type": "integer",
                            "description": "Debounce/stability window after a wake-up before comparing semantic text (default: 500, clamped to 100-2000)"
                        },
                        "include_snapshot": {
                            "type": "boolean",
                            "description": "Appends a compact DOM snapshot after the wait returns (default: false). The normal response already includes before/after text tails and deltas."
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_type_text",
                "Type text using keyboard into a previously focused input. Use cdp_fill for form fields; use this for character-by-character keyboard input.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["text"],
                    "properties": {
                        "text": {
                            "type": "string",
                            "description": "The text to type"
                        },
                        "submit_key": {
                            "type": "string",
                            "description": "Optional key to press after typing (e.g., 'Enter', 'Tab', 'Escape')"
                        }
                    }
                }))),
            ),
            Tool::new(
                "cdp_element_at_point",
                "Given screen coordinates (x, y) in points, hit-test the DOM element at that \
                 position. Always returns the element's backend_node_id. If a current DOM \
                 snapshot already contains that node, the response also includes its d-prefixed \
                 UID, role, and name; otherwise uid is null and a note points at \
                 cdp_take_dom_snapshot / cdp_find_elements. Requires an active CDP connection. \
                 Coordinates use the same screen-point system as element_at_point and click.",
                Arc::new(json_to_object(serde_json::json!({
                    "type": "object",
                    "required": ["x", "y"],
                    "properties": {
                        "x": {
                            "type": "number",
                            "description": "Screen X coordinate in points"
                        },
                        "y": {
                            "type": "number",
                            "description": "Screen Y coordinate in points"
                        }
                    }
                }))),
            ),
        ]
    }
}

impl ServerHandler for MacOSDevToolsServer {
    fn get_info(&self) -> ServerConfig {
        let mut instructions = String::from(
            "Native DevTools MCP server for automating desktop apps (macOS/Windows) and Android devices.\n\n\
             WHICH TOOLS TO USE:\n\
             - Desktop apps (coordinate-based, cross-platform): no prefix (click, find_text, take_screenshot, type_text, etc.). Moves the cursor; steals focus.\n",
        );

        #[cfg(target_os = "macos")]
        {
            instructions.push_str(
                "- Desktop apps (element-precise, macOS only): ax_* (ax_click, ax_set_value, ax_select) — focus-preserving dispatch against uids from take_ax_snapshot.\n",
            );
        }

        instructions.push_str(
            "- Android devices: android_* (android_click, android_find_text, etc.)\n\
             - App debug protocol: app_* — only when given a WebSocket URL to connect to.\n\
             NEVER mix these — desktop tools do not work on Android and vice versa.\n\n\
             == DESKTOP (macOS/Windows) ==\n\n\
             CLICKING BY TEXT (PREFERRED): Use find_text to locate UI elements by name, \
             then click at the returned coordinates.\n\
             Example: find_text(text='Submit') → click(x=..., y=...).\n\n\
             CLICKING BY VISUAL POSITION: Use take_screenshot with include_ocr=true. \
             The OCR results include screen coordinates you can click directly. \
             For positions not covered by OCR, use the screenshot metadata \
             (origin_x, origin_y, scale) to convert pixel positions.\n\n\
             Always call focus_window before clicking to ensure the target window receives input.\n\n\
             Screenshot best practice: Use take_screenshot with app_name (e.g., app_name='Code') \
             to capture a specific window. Avoid mode='screen' unless you need to see multiple windows.\n\n",
        );

        #[cfg(target_os = "macos")]
        {
            instructions.push_str(
                "ELEMENT-PRECISE AUTOMATION (macOS, PREFERRED for native apps): \
                 Call take_ax_snapshot(app_name='...') to get a tree of elements tagged \
                 with generation-stamped uids like 'a42g3'. Then pick the dispatch \
                 primitive that matches the target: ax_click(uid) dispatches AXPress for \
                 buttons, menu items, and anything pressable; ax_set_value(uid, text) \
                 writes kAXValueAttribute on text fields; ax_select(uid) writes \
                 AXSelectedRows for NSOutlineView / NSTableView row selection (sidebars, \
                 rule lists, file browsers) — rows typically refuse AXPress so ax_click \
                 returns not_dispatchable or AX error -25205 against them. IMPORTANT: any \
                 fresh take_ax_snapshot invalidates all prior uids — snapshot immediately \
                 before each ax_click / ax_set_value / ax_select call. ax_set_value is \
                 value assignment, not keystrokes: no IME, no undo-stack entry. If a call \
                 fails with not_dispatchable and returns a fallback {x, y}, retry via \
                 click(x, y) (plus type_text(text) for ax_set_value).\n\n",
            );
        }

        instructions.push_str(
            "App debug protocol (app_* tools): For element-level precision in apps with an embedded \
             debug server. Use app_connect with a WebSocket URL first, then app_click, app_type, etc.\n\n\
             == ANDROID ==\n\n\
             All Android tools require connecting to a device first:\n\
             1. android_list_devices — find available devices and their serial numbers\n\
             2. android_connect(serial='...') — connect (this unlocks all other android_* tools)\n\
             To switch devices, call android_disconnect first, then android_connect to the new device.\n\n\
             CLICKING BY TEXT (PREFERRED): Use android_find_text to search the accessibility tree, \
             then android_click at the returned coordinates.\n\
             Example: android_find_text(text='Settings') → android_click(x=..., y=...).\n\n\
             CLICKING BY VISUAL POSITION: Use android_screenshot to see the screen, \
             then android_click at the desired coordinates.\n\
             Note: android_screenshot has no OCR — always prefer android_find_text for text elements.\n\n\
             Android coordinates are absolute screen pixels — no scale conversion needed.\n\
             Use android_press_key with Android keycodes (e.g., 'KEYCODE_BACK', 'KEYCODE_HOME').",
        );

        let capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        ServerConfig::new(capabilities)
            .with_protocol_version(SERVER_PROTOCOL_VERSION)
            .with_server_info(Implementation::new(
                "native-devtools-mcp",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(instructions)
    }

    /// Advertise only the protocol version this server was built against.
    ///
    /// rmcp negotiates `initialize` against this list: a client asking for a
    /// listed version gets it back, any other client gets the fallback from
    /// `get_info` (the same version). So every client, old or new, is answered
    /// with `2024-11-05`, which matches what older rmcp releases did with the
    /// pinned version.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(ProtocolVersion::known_up_to(&SERVER_PROTOCOL_VERSION))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let connected = self.is_connected().await;
        #[cfg(feature = "cdp")]
        let cdp_connected = self.is_cdp_connected().await;
        #[cfg(not(feature = "cdp"))]
        let cdp_connected = false;
        Ok(ListToolsResult {
            tools: Self::get_tools(
                connected,
                self.is_android_connected().await,
                cdp_connected,
                self.is_hover_tracking().await,
                self.is_recording().await,
            ),
            next_cursor: None,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.dispatch_tool_call(request, context)
            .await
            .map(CallToolResponse::from)
    }
}

impl MacOSDevToolsServer {
    /// Run one `tools/call` request. Every tool completes in a single round
    /// trip, so the result is always a plain `CallToolResult`.
    async fn dispatch_tool_call(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let args = request
            .arguments
            .map(Value::Object)
            .unwrap_or(Value::Object(Default::default()));

        match request.name.as_ref() {
            "take_screenshot" => {
                let params: screenshot::TakeScreenshotParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(screenshot::take_screenshot(params, Some(self.screenshot_cache.clone())).await)
            }
            "list_windows" => {
                let params: navigation::ListWindowsParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(navigation::list_windows(params))
            }
            "list_apps" => {
                let params: navigation::ListAppsParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(navigation::list_apps(params))
            }
            "focus_window" => {
                let params: navigation::FocusWindowParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(navigation::focus_window(params))
            }
            "launch_app" => {
                let params: navigation::LaunchAppParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(navigation::launch_app(params))
            }
            "quit_app" => {
                let params: navigation::QuitAppParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(navigation::quit_app(params))
            }
            // App Debug Protocol tools
            "app_connect" => {
                let params: app_tools::AppConnectParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                let result =
                    app_tools::app_connect(params, self.app_client.clone(), context.peer).await;
                Ok(result)
            }
            "app_disconnect" => {
                let result = app_tools::app_disconnect(self.app_client.clone(), context.peer).await;
                Ok(result)
            }
            "app_get_info" => Ok(app_tools::app_get_info(self.app_client.clone()).await),
            "app_get_tree" => {
                let params: app_tools::AppGetTreeParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_get_tree(params, self.app_client.clone()).await)
            }
            "app_query" => {
                let params: app_tools::AppQueryParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_query(params, self.app_client.clone()).await)
            }
            "app_get_element" => {
                let params: app_tools::AppGetElementParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_get_element(params, self.app_client.clone()).await)
            }
            "app_click" => {
                let params: app_tools::AppClickParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_click(params, self.app_client.clone()).await)
            }
            "app_type" => {
                let params: app_tools::AppTypeParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_type(params, self.app_client.clone()).await)
            }
            "app_press_key" => {
                let params: app_tools::AppPressKeyParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_press_key(params, self.app_client.clone()).await)
            }
            "app_focus" => {
                let params: app_tools::AppFocusParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_focus(params, self.app_client.clone()).await)
            }
            "app_screenshot" => {
                let params: app_tools::AppScreenshotParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_screenshot(params, self.app_client.clone()).await)
            }
            "app_list_windows" => Ok(app_tools::app_list_windows(self.app_client.clone()).await),
            "app_focus_window" => {
                let params: app_tools::AppFocusWindowParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(app_tools::app_focus_window(params, self.app_client.clone()).await)
            }
            // System-level input tools
            "click" => {
                let params: input_tools::ClickParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::click(params).await)
            }
            "move_mouse" => {
                let params: input_tools::MoveMouseParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::move_mouse(params).await)
            }
            "drag" => {
                let params: input_tools::DragParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::drag(params).await)
            }
            "scroll" => {
                let params: input_tools::ScrollParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::scroll(params).await)
            }
            "type_text" => {
                let params: input_tools::TypeTextParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::type_text(params).await)
            }
            "press_key" => {
                let params: input_tools::PressKeyParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::press_key(params).await)
            }
            "get_displays" => {
                let params: input_tools::GetDisplaysParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::get_displays(params))
            }
            "find_text" => {
                let params: input_tools::FindTextParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::find_text(params))
            }
            "element_at_point" => {
                let params: input_tools::ElementAtPointParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(input_tools::element_at_point(params))
            }
            "find_image" => {
                let params: find_image::FindImageParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(find_image::find_image(
                    params,
                    self.screenshot_cache.clone(),
                    self.image_cache.clone(),
                )
                .await)
            }
            "load_image" => {
                let params: load_image::LoadImageParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(load_image::load_image(params, self.image_cache.clone()).await)
            }
            "take_ax_snapshot" => {
                let params: crate::tools::ax_snapshot::TakeAxSnapshotParams =
                    serde_json::from_value(args)
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                #[cfg(target_os = "macos")]
                {
                    // Native macOS path: walk the AX tree, capture retained
                    // AXRef handles, swap them into the session (generation
                    // bump), format with the assigned generation so uids are
                    // stamped `a<N>g<gen>`.
                    let (nodes, refs) =
                        match crate::macos::ax::collect_ax_tree_indexed(params.app_name.as_deref())
                        {
                            Ok(v) => v,
                            Err(e) => {
                                return Ok(CallToolResult::error(vec![ContentBlock::text(e)]))
                            }
                        };
                    let generation = self.ax_session.create_snapshot(refs).await;
                    let snapshot =
                        crate::tools::ax_snapshot::format_snapshot(&nodes, Some(generation));
                    Ok(CallToolResult::success(vec![ContentBlock::text(snapshot)]))
                }
                #[cfg(not(target_os = "macos"))]
                {
                    // Windows UIA path: unchanged — no session, uids stay bare `a<N>`.
                    match crate::tools::ax_snapshot::take_ax_snapshot(params) {
                        Ok(snapshot) => {
                            Ok(CallToolResult::success(vec![ContentBlock::text(snapshot)]))
                        }
                        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
                    }
                }
            }
            #[cfg(target_os = "macos")]
            "ax_click" => {
                let params: crate::tools::ax_click::AxClickParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(crate::tools::ax_click::ax_click(params, self.ax_session.clone()).await)
            }
            #[cfg(target_os = "macos")]
            "ax_set_value" => {
                let params: crate::tools::ax_set_value::AxSetValueParams =
                    serde_json::from_value(args)
                        .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(crate::tools::ax_set_value::ax_set_value(params, self.ax_session.clone()).await)
            }
            #[cfg(target_os = "macos")]
            "ax_select" => {
                let params: crate::tools::ax_select::AxSelectParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(crate::tools::ax_select::ax_select(params, self.ax_session.clone()).await)
            }
            "probe_app" => {
                let params: crate::tools::probe_app::ProbeAppParams = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(crate::tools::probe_app::probe_app(params))
            }
            // Android tools
            "android_list_devices" => match crate::android::device::list_devices() {
                Ok(devices) => Ok(CallToolResult::success(vec![ContentBlock::text(
                    to_json_pretty(&devices),
                )])),
                Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
            },
            "android_connect" => {
                let serial = parse_string_field(&args, "serial")?;
                match AndroidDevice::connect(&serial) {
                    Ok(device) => {
                        let msg = format!(
                            "Connected to Android device '{}'. Android tools (android_*) are now available.",
                            device.serial
                        );
                        *self.android_device.write().await = Some(device);
                        let _ = context.peer.notify_tool_list_changed().await;
                        Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
                    }
                    Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
                }
            }
            "android_disconnect" => {
                if self.android_device.write().await.take().is_some() {
                    let _ = context.peer.notify_tool_list_changed().await;
                    Ok(CallToolResult::success(vec![ContentBlock::text(
                        "Disconnected from Android device. Android tools (android_*) are no longer available.",
                    )]))
                } else {
                    Ok(CallToolResult::error(vec![ContentBlock::text(
                        "No Android device connected.",
                    )]))
                }
            }
            "android_screenshot" => {
                let file_path = args
                    .get("file_path")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                Ok(self
                    .with_android_device(|device| {
                        let shot = match crate::android::screenshot::capture(device) {
                            Ok(s) => s,
                            Err(e) => return CallToolResult::error(vec![ContentBlock::text(e)]),
                        };
                        if let Some(ref path) = file_path {
                            return match std::fs::write(path, &shot.png_data) {
                                Ok(()) => {
                                    CallToolResult::success(vec![ContentBlock::text(format!(
                                        "Screenshot saved to {} ({}x{})",
                                        path, shot.width, shot.height
                                    ))])
                                }
                                Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
                                    "Failed to save screenshot: {}",
                                    e
                                ))]),
                            };
                        }
                        let (image_data, mime_type) = match screenshot::png_to_jpeg(&shot.png_data)
                        {
                            Ok(jpeg_data) => (jpeg_data, "image/jpeg"),
                            Err(e) => {
                                tracing::warn!("JPEG conversion failed, using PNG: {}", e);
                                (shot.png_data, "image/png")
                            }
                        };
                        let base64_data =
                            base64::engine::general_purpose::STANDARD.encode(&image_data);
                        let mut contents = vec![ContentBlock::image(base64_data, mime_type)];
                        contents.push(ContentBlock::text(to_json_pretty(&serde_json::json!({
                            "width": shot.width,
                            "height": shot.height,
                            "scale": 1.0,
                            "device": device.serial,
                        }))));
                        CallToolResult::success(contents)
                    })
                    .await)
            }
            "android_click" => {
                let (x, y) = parse_xy(&args)?;
                Ok(self
                    .with_android_device(|device| {
                        match crate::android::input::click(device, x, y) {
                            Ok(()) => CallToolResult::success(vec![ContentBlock::text(format!(
                                "Tapped at ({:.0}, {:.0})",
                                x, y
                            ))]),
                            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                        }
                    })
                    .await)
            }
            "android_swipe" => {
                #[derive(serde::Deserialize)]
                struct Params {
                    start_x: f64,
                    start_y: f64,
                    end_x: f64,
                    end_y: f64,
                    duration_ms: Option<u32>,
                }
                let p: Params = serde_json::from_value(args)
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
                Ok(self
                    .with_android_device(|device| {
                        match crate::android::input::swipe(
                            device,
                            p.start_x,
                            p.start_y,
                            p.end_x,
                            p.end_y,
                            p.duration_ms,
                        ) {
                            Ok(()) => CallToolResult::success(vec![ContentBlock::text(format!(
                                "Swiped from ({:.0}, {:.0}) to ({:.0}, {:.0})",
                                p.start_x, p.start_y, p.end_x, p.end_y
                            ))]),
                            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                        }
                    })
                    .await)
            }
            "android_type_text" => {
                let text = parse_string_field(&args, "text")?;
                let len = text.len();
                Ok(self
                    .with_android_device(|device| {
                        match crate::android::input::type_text(device, &text) {
                            Ok(()) => CallToolResult::success(vec![ContentBlock::text(format!(
                                "Typed {} characters",
                                len
                            ))]),
                            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                        }
                    })
                    .await)
            }
            "android_press_key" => {
                let key = parse_string_field(&args, "key")?;
                Ok(self
                    .with_android_device(|device| {
                        match crate::android::input::press_key(device, &key) {
                            Ok(()) => CallToolResult::success(vec![ContentBlock::text(format!(
                                "Pressed key: {}",
                                key
                            ))]),
                            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                        }
                    })
                    .await)
            }
            "android_find_text" => {
                let text = parse_string_field(&args, "text")?;
                Ok(self
                    .with_android_device(|device| {
                        match crate::android::ui_automator::find_text(device, &text) {
                            Ok(result) => {
                                let mut content =
                                    vec![ContentBlock::text(to_json_pretty(&result.matches))];
                                if result.matches.is_empty() {
                                    content.push(ContentBlock::text(
                                        input_tools::build_no_matches_hint(
                                            &text,
                                            &result.available_elements,
                                        ),
                                    ));
                                }
                                CallToolResult::success(content)
                            }
                            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                        }
                    })
                    .await)
            }
            "android_list_apps" => {
                let user_apps_only = args
                    .get("user_apps_only")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(self
                    .with_android_device(|device| {
                        match crate::android::navigation::list_apps(device, user_apps_only) {
                            Ok(apps) => CallToolResult::success(vec![ContentBlock::text(
                                to_json_pretty(&apps),
                            )]),
                            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                        }
                    })
                    .await)
            }
            "android_launch_app" => {
                let package_name = parse_string_field(&args, "package_name")?;
                Ok(self
                    .with_android_device(|device| {
                        match crate::android::navigation::launch_app(device, &package_name) {
                            Ok(()) => CallToolResult::success(vec![ContentBlock::text(format!(
                                "Launched {}",
                                package_name
                            ))]),
                            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                        }
                    })
                    .await)
            }
            "android_get_display_info" => Ok(self
                .with_android_device(|device| {
                    match crate::android::navigation::get_display_info(device) {
                        Ok(info) => {
                            CallToolResult::success(vec![ContentBlock::text(to_json_pretty(&info))])
                        }
                        Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                    }
                })
                .await),
            "android_get_current_activity" => Ok(self
                .with_android_device(
                    |device| match crate::android::navigation::get_current_activity(device) {
                        Ok(activity) => CallToolResult::success(vec![ContentBlock::text(
                            to_json_pretty(&activity),
                        )]),
                        Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
                    },
                )
                .await),
            "start_hover_tracking" => {
                // Auto-clean finished tracker (e.g. from max duration timeout)
                let already_active = {
                    let guard = self.hover_tracker.read().await;
                    match guard.as_ref() {
                        Some(t) if t.is_finished() => false, // will clean up below
                        Some(_) => true,
                        None => false,
                    }
                };
                if already_active {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(
                        "Hover tracking is already active. Use stop_hover_tracking to end the current session first.",
                    )]));
                }
                // Clean up any finished tracker before starting a new one
                if self.hover_tracker.read().await.is_some() {
                    self.hover_tracker.write().await.take();
                }

                let app_name = args
                    .get("app_name")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let poll_interval_ms = args
                    .get("poll_interval_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(100)
                    .clamp(10, 10_000) as u32;
                let max_duration_ms = args
                    .get("max_duration_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60000)
                    .clamp(100, u32::MAX as u64) as u32;
                let min_dwell_ms = args
                    .get("min_dwell_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(300)
                    .clamp(0, 10_000) as u32;

                let events = Arc::new(std::sync::Mutex::new(Vec::new()));
                let cancel = tokio_util::sync::CancellationToken::new();

                let task_handle = crate::tools::hover_tracker::start_polling(
                    events.clone(),
                    cancel.clone(),
                    app_name.clone(),
                    poll_interval_ms,
                    max_duration_ms,
                    min_dwell_ms,
                );

                let tracker =
                    crate::tools::hover_tracker::HoverTracker::new(events, task_handle, cancel);
                *self.hover_tracker.write().await = Some(tracker);
                let _ = context.peer.notify_tool_list_changed().await;

                let msg = format!(
                    "Hover tracking started (poll: {}ms, max: {}ms, dwell: {}ms{}). Use get_hover_events to read transitions, stop_hover_tracking to end.",
                    poll_interval_ms,
                    max_duration_ms,
                    min_dwell_ms,
                    app_name.map_or(String::new(), |a| format!(", app: {}", a)),
                );
                Ok(CallToolResult::success(vec![ContentBlock::text(msg)]))
            }
            "get_hover_events" => {
                // Single lock: check auto-stop and drain events together
                let result = {
                    let guard = self.hover_tracker.read().await;
                    guard.as_ref().map(|t| {
                        let auto_stopped = t.is_finished();
                        let events = t.drain_events();
                        (auto_stopped, events)
                    })
                };

                match result {
                    Some((auto_stopped, events)) => {
                        let json = to_json_pretty(&events);

                        if auto_stopped {
                            self.hover_tracker.write().await.take();
                            let _ = context.peer.notify_tool_list_changed().await;
                        }

                        // Always return the JSON array for consistent parsing.
                        // The timeout sentinel event (with timeout: true) signals
                        // auto-stop within the event stream itself.
                        Ok(CallToolResult::success(vec![ContentBlock::text(json)]))
                    }
                    None => Ok(CallToolResult::error(vec![ContentBlock::text(
                        "No hover tracking session is active. Use start_hover_tracking first.",
                    )])),
                }
            }
            "stop_hover_tracking" => {
                let tracker = self.hover_tracker.write().await.take();
                match tracker {
                    Some(tracker) => {
                        let events = tracker.cancel_and_drain().await;
                        let _ = context.peer.notify_tool_list_changed().await;
                        // Return raw JSON array for consistent parsing with get_hover_events
                        Ok(CallToolResult::success(vec![ContentBlock::text(
                            to_json_pretty(&events),
                        )]))
                    }
                    None => Ok(CallToolResult::error(vec![ContentBlock::text(
                        "No hover tracking session is active.",
                    )])),
                }
            }
            "start_recording" => {
                // Check if a previous recording auto-stopped (max_duration elapsed).
                // If so, drain remaining frames (log count) and clear stale state.
                {
                    let guard = self.screen_recorder.read().await;
                    if let Some(recorder) = guard.as_ref() {
                        if recorder.is_finished() {
                            let stale_count = recorder.drain_frames().len();
                            if stale_count > 0 {
                                tracing::warn!(
                                    "Discarding {stale_count} frames from auto-stopped recording \
                                     (stop_recording was not called)"
                                );
                            }
                            drop(guard);
                            self.screen_recorder.write().await.take();
                            let _ = context.peer.notify_tool_list_changed().await;
                        } else {
                            return Ok(CallToolResult::error(vec![ContentBlock::text(
                                "Recording is already active. Use stop_recording to end the current session first.",
                            )]));
                        }
                    }
                }

                let output_dir = parse_string_field(&args, "output_dir")?;
                let fps = args.get("fps").and_then(|v| v.as_u64()).unwrap_or(5) as u32;
                let max_duration_ms = args
                    .get("max_duration_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60_000)
                    .clamp(1_000, u32::MAX as u64) as u32;

                let output_path = std::path::PathBuf::from(&output_dir);
                if let Err(e) = std::fs::create_dir_all(&output_path) {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Failed to create output directory: {e}"
                    ))]));
                }
                // Probe-write to fail fast if the directory is not writable.
                match tempfile::tempfile_in(&output_path) {
                    Ok(_) => {} // drops and deletes automatically
                    Err(e) => {
                        return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                            "Output directory is not writable: {e}"
                        ))]));
                    }
                }

                let frames = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
                let cancel = tokio_util::sync::CancellationToken::new();

                let task_handle = crate::tools::screen_recorder::start_recording(
                    frames.clone(),
                    cancel.clone(),
                    output_path,
                    fps,
                    max_duration_ms,
                );

                let recorder =
                    crate::tools::screen_recorder::ScreenRecorder::new(frames, task_handle, cancel);
                *self.screen_recorder.write().await = Some(recorder);
                let _ = context.peer.notify_tool_list_changed().await;

                Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Recording started ({fps}fps, max: {max_duration_ms}ms, dir: {output_dir}). Use stop_recording to end.",
                ))]))
            }
            "stop_recording" => {
                let recorder = self.screen_recorder.write().await.take();
                match recorder {
                    Some(recorder) => {
                        let frames = recorder.cancel_and_drain().await;
                        let _ = context.peer.notify_tool_list_changed().await;
                        Ok(CallToolResult::success(vec![ContentBlock::text(
                            to_json_pretty(&frames),
                        )]))
                    }
                    None => Ok(CallToolResult::error(vec![ContentBlock::text(
                        "No recording session is active.",
                    )])),
                }
            }
            #[cfg(feature = "cdp")]
            "cdp_connect" => {
                let port_num = args.get("port").and_then(|v| v.as_u64()).ok_or_else(|| {
                    McpError::invalid_params("missing required param: port", None)
                })?;
                if port_num > 65535 {
                    return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                        "Invalid port: {}. Port must be 0-65535.",
                        port_num
                    ))]));
                }
                let port = port_num as u16;
                match crate::cdp::CdpClient::connect(port).await {
                    Ok(client) => {
                        let page_info = if let Some(page) = client.selected_page.as_ref() {
                            let url = crate::cdp::page_url(page).await;
                            format!("Selected page: {}", url)
                        } else {
                            "No pages found".to_string()
                        };
                        *self.cdp_client.write().await = Some(client);
                        // Tool list does not change on CDP connect/disconnect — CDP
                        // tools are always listed so prompt caches remain stable.
                        Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                            "Connected to Chrome/Electron on port {}. CDP tool calls will now succeed.\n{}",
                            port, page_info
                        ))]))
                    }
                    Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(e)])),
                }
            }
            #[cfg(feature = "cdp")]
            "cdp_disconnect" => {
                if let Some(client) = self.cdp_client.write().await.take() {
                    client.disconnect();
                    // Tool list is unchanged on disconnect — CDP tools remain
                    // listed and will return "not connected" errors until
                    // cdp_connect succeeds again.
                    Ok(CallToolResult::success(vec![ContentBlock::text(
                        "Disconnected from Chrome/Electron. CDP tool calls will return a 'not connected' error until cdp_connect is called again.",
                    )]))
                } else {
                    // Use the canonical "not connected" message shared by every
                    // CDP tool handler so clients see one stable error shape.
                    Ok(CallToolResult::error(vec![ContentBlock::text(
                        "No CDP connection. Use cdp_connect first.",
                    )]))
                }
            }
            #[cfg(feature = "cdp")]
            "cdp_take_dom_snapshot" => {
                let max_nodes = args
                    .get("max_nodes")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                Ok(
                    crate::cdp::tools::cdp_take_dom_snapshot(max_nodes, self.cdp_client.clone())
                        .await,
                )
            }
            #[cfg(feature = "cdp")]
            "cdp_summarize_page" => {
                Ok(crate::cdp::tools::cdp_summarize_page(self.cdp_client.clone()).await)
            }
            #[cfg(feature = "cdp")]
            "cdp_find_elements" => {
                let query = parse_string_field(&args, "query")?;
                let role = args.get("role").and_then(|v| v.as_str()).map(String::from);
                let max_results = args
                    .get("max_results")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                Ok(crate::cdp::tools::cdp_find_elements(
                    query,
                    role,
                    max_results,
                    self.cdp_client.clone(),
                )
                .await)
            }
            #[cfg(feature = "cdp")]
            "cdp_get_element_context" => {
                let uid = parse_string_field(&args, "uid")?;
                let ancestor_depth = args
                    .get("ancestor_depth")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let sibling_limit = args
                    .get("sibling_limit")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let child_limit = args
                    .get("child_limit")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                let max_chars = args
                    .get("max_chars")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32);
                Ok(crate::cdp::tools::cdp_get_element_context(
                    uid,
                    ancestor_depth,
                    sibling_limit,
                    child_limit,
                    max_chars,
                    self.cdp_client.clone(),
                )
                .await)
            }
            #[cfg(feature = "cdp")]
            "cdp_evaluate_script" => {
                let function = parse_string_field(&args, "function")?;
                let script_args = args.get("args").and_then(|v| v.as_array()).cloned();
                Ok(crate::cdp::tools::cdp_evaluate_script(
                    function,
                    script_args,
                    self.cdp_client.clone(),
                )
                .await)
            }
            #[cfg(feature = "cdp")]
            "cdp_click" => {
                let uid = parse_string_field(&args, "uid")?;
                let dbl_click = args
                    .get("dbl_click")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let include_snapshot = args
                    .get("include_snapshot")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(crate::cdp::tools::cdp_click(
                    uid,
                    dbl_click,
                    include_snapshot,
                    self.cdp_client.clone(),
                )
                .await)
            }
            #[cfg(feature = "cdp")]
            "cdp_list_pages" => {
                Ok(crate::cdp::tools::cdp_list_pages(self.cdp_client.clone()).await)
            }
            #[cfg(feature = "cdp")]
            "cdp_select_page" => {
                let page_idx = args
                    .get("page_idx")
                    .and_then(|v| v.as_u64())
                    .map(|p| p as usize)
                    .ok_or_else(|| {
                        McpError::invalid_params("missing required param: page_idx", None)
                    })?;
                Ok(crate::cdp::tools::cdp_select_page(page_idx, self.cdp_client.clone()).await)
            }
            #[cfg(feature = "cdp")]
            "cdp_hover" => {
                let uid = parse_string_field(&args, "uid")?;
                let include_snapshot = args
                    .get("include_snapshot")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(
                    crate::cdp::tools::cdp_hover(uid, include_snapshot, self.cdp_client.clone())
                        .await,
                )
            }
            #[cfg(feature = "cdp")]
            "cdp_fill" => {
                let uid = parse_string_field(&args, "uid")?;
                let value = parse_string_field(&args, "value")?;
                let include_snapshot = args
                    .get("include_snapshot")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(crate::cdp::tools::cdp_fill(
                    uid,
                    value,
                    include_snapshot,
                    self.cdp_client.clone(),
                )
                .await)
            }
            #[cfg(feature = "cdp")]
            "cdp_press_key" => {
                let key = parse_string_field(&args, "key")?;
                let include_snapshot = args
                    .get("include_snapshot")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(
                    crate::cdp::tools::cdp_press_key(
                        key,
                        include_snapshot,
                        self.cdp_client.clone(),
                    )
                    .await,
                )
            }
            #[cfg(feature = "cdp")]
            "cdp_handle_dialog" => {
                let action = parse_string_field(&args, "action")?;
                let prompt_text = args
                    .get("prompt_text")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                Ok(crate::cdp::tools::cdp_handle_dialog(
                    action,
                    prompt_text,
                    self.cdp_client.clone(),
                )
                .await)
            }
            #[cfg(feature = "cdp")]
            "cdp_navigate" => {
                let url = args.get("url").and_then(|v| v.as_str()).map(String::from);
                let nav_type = args.get("type").and_then(|v| v.as_str()).map(String::from);
                let timeout = args.get("timeout").and_then(|v| v.as_u64());
                Ok(
                    crate::cdp::tools::cdp_navigate(
                        url,
                        nav_type,
                        timeout,
                        self.cdp_client.clone(),
                    )
                    .await,
                )
            }
            #[cfg(feature = "cdp")]
            "cdp_new_page" => {
                let url = parse_string_field(&args, "url")?;
                Ok(crate::cdp::tools::cdp_new_page(url, self.cdp_client.clone()).await)
            }
            #[cfg(feature = "cdp")]
            "cdp_close_page" => {
                let page_idx = args
                    .get("page_idx")
                    .and_then(|v| v.as_u64())
                    .map(|p| p as usize)
                    .ok_or_else(|| {
                        McpError::invalid_params("missing required param: page_idx", None)
                    })?;
                Ok(crate::cdp::tools::cdp_close_page(page_idx, self.cdp_client.clone()).await)
            }
            #[cfg(feature = "cdp")]
            "cdp_wait_for" => {
                let texts: Vec<String> = args
                    .get("text")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                if texts.is_empty() {
                    return Err(McpError::invalid_params(
                        "missing required param: text (array of strings)",
                        None,
                    ));
                }
                let timeout = args.get("timeout").and_then(|v| v.as_u64());
                let include_snapshot = args
                    .get("include_snapshot")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(crate::cdp::tools::cdp_wait_for(
                    texts,
                    timeout,
                    include_snapshot,
                    self.cdp_client.clone(),
                )
                .await)
            }
            #[cfg(feature = "cdp")]
            "cdp_wait_for_page_change" => {
                let scope_uid = args
                    .get("scope_uid")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let condition = args
                    .get("condition")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let goal = args.get("goal").and_then(|v| v.as_str()).map(String::from);
                let timeout = args.get("timeout").and_then(|v| v.as_u64());
                let poll_interval_ms = args.get("poll_interval_ms").and_then(|v| v.as_u64());
                let stable_ms = args.get("stable_ms").and_then(|v| v.as_u64());
                let include_snapshot = args
                    .get("include_snapshot")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                Ok(crate::cdp::tools::cdp_wait_for_page_change(
                    scope_uid,
                    condition,
                    goal,
                    timeout,
                    poll_interval_ms,
                    stable_ms,
                    include_snapshot,
                    self.cdp_client.clone(),
                )
                .await)
            }
            #[cfg(feature = "cdp")]
            "cdp_type_text" => {
                let text = parse_string_field(&args, "text")?;
                let submit_key = args
                    .get("submit_key")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                Ok(
                    crate::cdp::tools::cdp_type_text(text, submit_key, self.cdp_client.clone())
                        .await,
                )
            }
            #[cfg(feature = "cdp")]
            "cdp_element_at_point" => {
                let (x, y) = parse_xy(&args)?;
                Ok(crate::cdp::tools::cdp_element_at_point(x, y, self.cdp_client.clone()).await)
            }
            _ => Err(McpError::invalid_params(
                format!("Unknown tool: {}", request.name),
                None,
            )),
        }
    }
}
