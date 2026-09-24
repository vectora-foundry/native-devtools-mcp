//! Input tools for system-level mouse and keyboard simulation.
//!
//! These tools wrap input operations in `spawn_blocking` to avoid
//! blocking the tokio runtime, since input operations use `thread::sleep`.

use crate::platform::{display, input, ocr};
use rmcp::model::{CallToolResult, ContentBlock};
use serde::Deserialize;

/// Check accessibility permission and return a standardized plain-text error
/// result if not granted. Returns `None` when permission is available.
///
/// Shared between the coord-based input tools (`click`, `type_text`, …) and
/// the macOS AX dispatch tools (`ax_click`, `ax_set_value`) so all user-facing
/// permission messages are identical.
pub(crate) fn check_permission() -> Option<CallToolResult> {
    if !input::check_accessibility_permission() {
        #[cfg(target_os = "macos")]
        let msg = "Accessibility permission required.\n\n\
             Grant permission to your MCP client (e.g., Claude Desktop, VS Code, Terminal) in:\n\
             System Settings → Privacy & Security → Accessibility\n\n\
             The permission must be granted to the app that runs this MCP server, \
             not to the server binary itself.";

        #[cfg(target_os = "windows")]
        let msg = "Input injection permission denied.\n\n\
             This typically occurs when targeting elevated (admin) windows \
             from a non-elevated process, or when targeting secure desktops.";

        return Some(CallToolResult::error(vec![ContentBlock::text(msg)]));
    }
    None
}

/// Run a blocking input operation and convert the result to CallToolResult.
async fn run_input<F>(op: F, success_msg: String, error_prefix: &str) -> CallToolResult
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    match tokio::task::spawn_blocking(op).await {
        Ok(Ok(())) => CallToolResult::success(vec![ContentBlock::text(success_msg)]),
        Ok(Err(e)) => {
            CallToolResult::error(vec![ContentBlock::text(format!("{}: {}", error_prefix, e))])
        }
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("Task failed: {}", e))]),
    }
}

// ============================================================================
// Click
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ClickParams {
    /// Screen X coordinate (required unless using window-relative)
    pub x: Option<f64>,
    /// Screen Y coordinate (required unless using window-relative)
    pub y: Option<f64>,

    /// Window-relative X coordinate
    pub window_x: Option<f64>,
    /// Window-relative Y coordinate
    pub window_y: Option<f64>,
    /// Window ID for window-relative coordinates
    pub window_id: Option<u32>,

    /// Screenshot pixel X coordinate
    pub screenshot_x: Option<f64>,
    /// Screenshot pixel Y coordinate
    pub screenshot_y: Option<f64>,
    /// Screenshot origin X coordinate in screen space
    pub screenshot_origin_x: Option<f64>,
    /// Screenshot origin Y coordinate in screen space
    pub screenshot_origin_y: Option<f64>,
    /// Backing scale factor used for the screenshot
    pub screenshot_scale: Option<f64>,
    /// Window ID that the screenshot was taken from (for scaling)
    pub screenshot_window_id: Option<u32>,

    /// Mouse button: "left" (default), "right", or "center"
    #[serde(default)]
    pub button: Option<String>,

    /// Number of clicks (1 for single, 2 for double)
    #[serde(default = "default_click_count")]
    pub click_count: u32,
}

fn default_click_count() -> u32 {
    1
}

/// Identifier for the one concrete coordinate variant resolved for a click.
///
/// The schema exposes all variant fields as optional top-level properties
/// (required because Anthropic's tool-use API rejects top-level `oneOf`).
/// [`select_click_variant`] enforces the "exactly one variant" contract at
/// runtime from the submitted params.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickVariant {
    /// Screen-space (x, y).
    Screen,
    /// Window-relative (window_x, window_y, window_id).
    WindowRelative,
    /// Screenshot image pixels + origin/scale metadata (preferred).
    ScreenshotPixels,
    /// Legacy screenshot pixels (screenshot_x, screenshot_y, screenshot_window_id).
    ScreenshotPixelsLegacy,
}

impl ClickVariant {
    fn title(self) -> &'static str {
        match self {
            ClickVariant::Screen => "screen",
            ClickVariant::WindowRelative => "window-relative",
            ClickVariant::ScreenshotPixels => "screenshot-pixels",
            ClickVariant::ScreenshotPixelsLegacy => "screenshot-pixels-legacy",
        }
    }
}

/// Select exactly one coordinate variant from the submitted params. Returns
/// the matched variant or a descriptive error message naming what went wrong.
///
/// A valid call has every required field of exactly one variant set and no
/// field from any other variant set. The schema cannot enforce this (top-level
/// `oneOf` is rejected by Anthropic's tool-use API), so this runtime check is
/// the single source of truth: a payload mixing `x`/`y` with screenshot fields
/// fails fast with a clear message instead of silently picking a branch.
///
/// Pure function — no I/O.
fn select_click_variant(params: &ClickParams) -> Result<ClickVariant, String> {
    // Fields of every variant, in their declared order.
    const SCREEN_FIELDS: &[&str] = &["x", "y"];
    const WINDOW_FIELDS: &[&str] = &["window_x", "window_y", "window_id"];
    const SCREENSHOT_PIXELS_FIELDS: &[&str] = &[
        "screenshot_x",
        "screenshot_y",
        "screenshot_origin_x",
        "screenshot_origin_y",
        "screenshot_scale",
    ];
    const LEGACY_FIELDS: &[&str] = &["screenshot_x", "screenshot_y", "screenshot_window_id"];

    let screen_present = [params.x.is_some(), params.y.is_some()];
    let window_present = [
        params.window_x.is_some(),
        params.window_y.is_some(),
        params.window_id.is_some(),
    ];
    let pixels_present = [
        params.screenshot_x.is_some(),
        params.screenshot_y.is_some(),
        params.screenshot_origin_x.is_some(),
        params.screenshot_origin_y.is_some(),
        params.screenshot_scale.is_some(),
    ];
    let legacy_present = [
        params.screenshot_x.is_some(),
        params.screenshot_y.is_some(),
        params.screenshot_window_id.is_some(),
    ];

    // A variant "has activity" if any of its unique fields are present; this
    // determines which variants the caller was trying to use so we can
    // reject mixes.
    let screen_active = params.x.is_some() || params.y.is_some();
    let window_active =
        params.window_x.is_some() || params.window_y.is_some() || params.window_id.is_some();
    // The two screenshot variants overlap on (screenshot_x, screenshot_y),
    // so disambiguate by their unique fields.
    let pixels_unique_active = params.screenshot_origin_x.is_some()
        || params.screenshot_origin_y.is_some()
        || params.screenshot_scale.is_some();
    let legacy_unique_active = params.screenshot_window_id.is_some();
    let any_screenshot_coord = params.screenshot_x.is_some() || params.screenshot_y.is_some();

    let screenshot_active = pixels_unique_active || legacy_unique_active || any_screenshot_coord;

    let active_count = [screen_active, window_active, screenshot_active]
        .iter()
        .filter(|b| **b)
        .count();

    if active_count == 0 {
        return Err(describe_click_coord_error(params));
    }

    // Multiple families active: a true mixed payload. Describe exactly which
    // families the caller is mixing so they know *what* to remove, not just
    // which variant they were closest to. `describe_click_coord_error`'s
    // "closest variant / missing: ..." shape is misleading here because it
    // can report `missing: (none)` when one of the mixed variants is
    // already complete.
    if active_count > 1 {
        let mut active: Vec<&str> = Vec::new();
        if screen_active {
            active.push("screen (x, y)");
        }
        if window_active {
            active.push("window-relative (window_x, window_y, window_id)");
        }
        if screenshot_active {
            active
                .push("screenshot (screenshot_x/y + origin/scale, or legacy screenshot_window_id)");
        }
        return Err(format!(
            "click received fields from multiple coordinate variants: {}. \
             Send exactly one variant. Prefer screenshot-pixels after take_screenshot: \
             screenshot_x, screenshot_y, screenshot_origin_x, screenshot_origin_y, \
             screenshot_scale.",
            active.join(" + ")
        ));
    }

    // Exactly one family is active — now require its fields to be complete.
    if screen_active {
        if screen_present.iter().all(|p| *p) {
            return Ok(ClickVariant::Screen);
        }
        return Err(format_missing_fields_error(
            ClickVariant::Screen,
            SCREEN_FIELDS,
            &screen_present,
        ));
    }
    if window_active {
        if window_present.iter().all(|p| *p) {
            return Ok(ClickVariant::WindowRelative);
        }
        return Err(format_missing_fields_error(
            ClickVariant::WindowRelative,
            WINDOW_FIELDS,
            &window_present,
        ));
    }
    // Screenshot family: pick the variant the caller clearly intended.
    if pixels_unique_active && legacy_unique_active {
        return Err(
            "click received screenshot_origin_*/screenshot_scale (screenshot-pixels variant) \
             together with screenshot_window_id (screenshot-pixels-legacy variant). \
             Send exactly one. Prefer screenshot-pixels: screenshot_x, screenshot_y, \
             screenshot_origin_x, screenshot_origin_y, screenshot_scale."
                .to_string(),
        );
    }
    if pixels_unique_active {
        if pixels_present.iter().all(|p| *p) {
            return Ok(ClickVariant::ScreenshotPixels);
        }
        return Err(format_missing_fields_error(
            ClickVariant::ScreenshotPixels,
            SCREENSHOT_PIXELS_FIELDS,
            &pixels_present,
        ));
    }
    if legacy_unique_active {
        if legacy_present.iter().all(|p| *p) {
            return Ok(ClickVariant::ScreenshotPixelsLegacy);
        }
        return Err(format_missing_fields_error(
            ClickVariant::ScreenshotPixelsLegacy,
            LEGACY_FIELDS,
            &legacy_present,
        ));
    }
    // Only screenshot_x and/or screenshot_y provided — ambiguous between the
    // two screenshot variants. Default to the preferred one for the error.
    Err(format_missing_fields_error(
        ClickVariant::ScreenshotPixels,
        SCREENSHOT_PIXELS_FIELDS,
        &pixels_present,
    ))
}

fn format_missing_fields_error(variant: ClickVariant, fields: &[&str], present: &[bool]) -> String {
    debug_assert_eq!(fields.len(), present.len());
    let missing: Vec<&str> = fields
        .iter()
        .zip(present.iter())
        .filter_map(|(n, p)| if *p { None } else { Some(*n) })
        .collect();
    format!(
        "click variant '{}' is missing required fields: {}. \
         Send every field of exactly one variant, and no fields from other variants.",
        variant.title(),
        missing.join(", ")
    )
}

/// Build a user-facing error message explaining why the provided click params
/// don't match any supported coordinate variant. Names the fields that were
/// provided and suggests the closest variant based on heuristic overlap.
///
/// Pure function — used by `click` when no variant validates successfully.
fn describe_click_coord_error(params: &ClickParams) -> String {
    // (variant_name, [(field_name, is_provided), ...])
    // Order matters: the first listed variant wins ties, which is why
    // "screenshot-pixels" is first — it's the preferred variant and a
    // natural target to steer callers toward.
    let variants: [(&str, &[(&str, bool)]); 4] = [
        (
            "screenshot-pixels",
            &[
                ("screenshot_x", params.screenshot_x.is_some()),
                ("screenshot_y", params.screenshot_y.is_some()),
                ("screenshot_origin_x", params.screenshot_origin_x.is_some()),
                ("screenshot_origin_y", params.screenshot_origin_y.is_some()),
                ("screenshot_scale", params.screenshot_scale.is_some()),
            ],
        ),
        (
            "screen",
            &[("x", params.x.is_some()), ("y", params.y.is_some())],
        ),
        (
            "window-relative",
            &[
                ("window_x", params.window_x.is_some()),
                ("window_y", params.window_y.is_some()),
                ("window_id", params.window_id.is_some()),
            ],
        ),
        (
            "screenshot-pixels-legacy",
            &[
                ("screenshot_x", params.screenshot_x.is_some()),
                ("screenshot_y", params.screenshot_y.is_some()),
                (
                    "screenshot_window_id",
                    params.screenshot_window_id.is_some(),
                ),
            ],
        ),
    ];

    // Every field in the union, preserving source order for a stable message.
    let all_fields: &[(&str, bool)] = &[
        ("x", params.x.is_some()),
        ("y", params.y.is_some()),
        ("window_x", params.window_x.is_some()),
        ("window_y", params.window_y.is_some()),
        ("window_id", params.window_id.is_some()),
        ("screenshot_x", params.screenshot_x.is_some()),
        ("screenshot_y", params.screenshot_y.is_some()),
        ("screenshot_origin_x", params.screenshot_origin_x.is_some()),
        ("screenshot_origin_y", params.screenshot_origin_y.is_some()),
        ("screenshot_scale", params.screenshot_scale.is_some()),
        (
            "screenshot_window_id",
            params.screenshot_window_id.is_some(),
        ),
    ];

    let provided: Vec<&str> = all_fields
        .iter()
        .filter_map(|(n, p)| if *p { Some(*n) } else { None })
        .collect();

    // Closest variant = highest count of provided fields.
    // Ties are broken in favor of earlier variants (so "screenshot-pixels"
    // wins ties as the preferred form). std's `max_by_key` returns the last
    // maximal element, so we invert with `min_by_key` on a (negative_score,
    // index) tuple to pick the earliest best.
    let (closest_variant, fields) = variants
        .iter()
        .enumerate()
        .min_by_key(|(idx, (_, fields))| {
            let score = fields.iter().filter(|(_, p)| *p).count() as isize;
            (-score, *idx)
        })
        .map(|(_, v)| *v)
        .expect("variants table is non-empty");

    let missing: Vec<&str> = fields
        .iter()
        .filter_map(|(n, p)| if *p { None } else { Some(*n) })
        .collect();

    let provided_str = if provided.is_empty() {
        "(no coordinate fields)".to_string()
    } else {
        provided.join(", ")
    };
    let missing_str = if missing.is_empty() {
        "(none)".to_string()
    } else {
        missing.join(", ")
    };

    format!(
        "click requires exactly one complete coordinate variant. \
         Provided fields: {provided_str}. \
         Closest variant: '{closest_variant}' — missing: {missing_str}.\n\
         Supported variants:\n\
         - screenshot-pixels (PREFERRED after take_screenshot): \
           screenshot_x, screenshot_y, screenshot_origin_x, screenshot_origin_y, screenshot_scale\n\
         - screen: x, y\n\
         - window-relative: window_x, window_y, window_id\n\
         - screenshot-pixels-legacy (deprecated): \
           screenshot_x, screenshot_y, screenshot_window_id"
    )
}

pub async fn click(params: ClickParams) -> CallToolResult {
    if let Some(err) = check_permission() {
        return err;
    }

    // Parse button
    let button = match params.button.as_deref() {
        Some("right") => input::MouseButton::Right,
        Some("center") | Some("middle") => input::MouseButton::Center,
        _ => input::MouseButton::Left,
    };

    // Enforce the oneOf contract: exactly one variant's fields may be
    // present. Clients that don't validate schemas used to silently pick
    // the first complete branch on a mixed payload — now we reject it.
    let variant = match select_click_variant(&params) {
        Ok(v) => v,
        Err(msg) => return CallToolResult::error(vec![ContentBlock::text(msg)]),
    };

    // Resolve coordinates based on the validated variant.
    let (x, y) = match variant {
        ClickVariant::Screen => (
            params
                .x
                .expect("select_click_variant guarantees x is Some for Screen"),
            params
                .y
                .expect("select_click_variant guarantees y is Some for Screen"),
        ),
        ClickVariant::WindowRelative => {
            let wx = params.window_x.expect("window_x guaranteed present");
            let wy = params.window_y.expect("window_y guaranteed present");
            let window_id = params.window_id.expect("window_id guaranteed present");
            let window = match crate::platform::find_window_by_id(window_id) {
                Ok(Some(w)) => w,
                Ok(None) => {
                    return CallToolResult::error(vec![ContentBlock::text(format!(
                        "Window {} not found",
                        window_id
                    ))])
                }
                Err(e) => return CallToolResult::error(vec![ContentBlock::text(e)]),
            };
            let bounds = display::WindowBounds {
                x: window.bounds.x,
                y: window.bounds.y,
            };
            display::window_to_screen(&bounds, wx, wy)
        }
        ClickVariant::ScreenshotPixels => {
            let px = params
                .screenshot_x
                .expect("screenshot_x guaranteed present");
            let py = params
                .screenshot_y
                .expect("screenshot_y guaranteed present");
            let origin_x = params
                .screenshot_origin_x
                .expect("screenshot_origin_x guaranteed present");
            let origin_y = params
                .screenshot_origin_y
                .expect("screenshot_origin_y guaranteed present");
            let scale = params
                .screenshot_scale
                .expect("screenshot_scale guaranteed present");
            let bounds = display::WindowBounds {
                x: origin_x,
                y: origin_y,
            };
            display::screenshot_to_screen(&bounds, scale, px, py)
        }
        ClickVariant::ScreenshotPixelsLegacy => {
            let px = params
                .screenshot_x
                .expect("screenshot_x guaranteed present");
            let py = params
                .screenshot_y
                .expect("screenshot_y guaranteed present");
            let window_id = params
                .screenshot_window_id
                .expect("screenshot_window_id guaranteed present");
            let window = match crate::platform::find_window_by_id(window_id) {
                Ok(Some(w)) => w,
                Ok(None) => {
                    return CallToolResult::error(vec![ContentBlock::text(format!(
                        "Window {} not found",
                        window_id
                    ))])
                }
                Err(e) => return CallToolResult::error(vec![ContentBlock::text(e)]),
            };
            let bounds = display::WindowBounds {
                x: window.bounds.x,
                y: window.bounds.y,
            };
            // macOS: screencapture captures in physical (Retina) pixels, need scale factor.
            // Windows: BitBlt captures in logical coordinates, scale is always 1.0.
            #[cfg(target_os = "macos")]
            let scale = display::backing_scale_for_point(window.bounds.x, window.bounds.y);
            #[cfg(target_os = "windows")]
            let scale = 1.0;
            display::screenshot_to_screen(&bounds, scale, px, py)
        }
    };

    let click_count = params.click_count;
    run_input(
        move || input::click(x, y, button, click_count),
        format!("Clicked at ({:.0}, {:.0})", x, y),
        "Click failed",
    )
    .await
}

// ============================================================================
// Move Mouse
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct MoveMouseParams {
    /// Screen X coordinate
    pub x: f64,
    /// Screen Y coordinate
    pub y: f64,
}

pub async fn move_mouse(params: MoveMouseParams) -> CallToolResult {
    if let Some(err) = check_permission() {
        return err;
    }

    let (x, y) = (params.x, params.y);
    run_input(
        move || input::move_mouse(x, y),
        format!("Moved mouse to ({:.0}, {:.0})", x, y),
        "Move failed",
    )
    .await
}

// ============================================================================
// Drag
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct DragParams {
    /// Start X coordinate
    pub start_x: f64,
    /// Start Y coordinate
    pub start_y: f64,
    /// End X coordinate
    pub end_x: f64,
    /// End Y coordinate
    pub end_y: f64,
    /// Mouse button: "left" (default), "right", or "center"
    #[serde(default)]
    pub button: Option<String>,
}

pub async fn drag(params: DragParams) -> CallToolResult {
    if let Some(err) = check_permission() {
        return err;
    }

    let button = match params.button.as_deref() {
        Some("right") => input::MouseButton::Right,
        Some("center") | Some("middle") => input::MouseButton::Center,
        _ => input::MouseButton::Left,
    };

    let (start_x, start_y, end_x, end_y) =
        (params.start_x, params.start_y, params.end_x, params.end_y);
    run_input(
        move || input::drag(start_x, start_y, end_x, end_y, button),
        format!(
            "Dragged from ({:.0}, {:.0}) to ({:.0}, {:.0})",
            start_x, start_y, end_x, end_y
        ),
        "Drag failed",
    )
    .await
}

// ============================================================================
// Scroll
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ScrollParams {
    /// X coordinate to scroll at
    pub x: f64,
    /// Y coordinate to scroll at
    pub y: f64,
    /// Horizontal scroll delta (positive = right)
    #[serde(default)]
    pub delta_x: i32,
    /// Vertical scroll delta (positive = down, negative = up)
    pub delta_y: i32,
}

pub async fn scroll(params: ScrollParams) -> CallToolResult {
    if let Some(err) = check_permission() {
        return err;
    }

    let (x, y, delta_x, delta_y) = (params.x, params.y, params.delta_x, params.delta_y);
    run_input(
        move || input::scroll(x, y, delta_x, delta_y),
        format!(
            "Scrolled at ({:.0}, {:.0}) by ({}, {})",
            x, y, delta_x, delta_y
        ),
        "Scroll failed",
    )
    .await
}

// ============================================================================
// Type Text
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct TypeTextParams {
    /// Text to type
    pub text: String,
}

pub async fn type_text(params: TypeTextParams) -> CallToolResult {
    if let Some(err) = check_permission() {
        return err;
    }

    let len = params.text.len();
    let text = params.text;
    run_input(
        move || input::type_text(&text),
        format!("Typed {} characters", len),
        "Type failed",
    )
    .await
}

// ============================================================================
// Press Key
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct PressKeyParams {
    /// Key to press (e.g., "return", "tab", "a", "f1")
    pub key: String,
    /// Modifier keys: "shift", "control", "option", "command"
    #[serde(default)]
    pub modifiers: Vec<String>,
}

pub async fn press_key(params: PressKeyParams) -> CallToolResult {
    if let Some(err) = check_permission() {
        return err;
    }

    let key_desc = if params.modifiers.is_empty() {
        params.key.clone()
    } else {
        format!("{}+{}", params.modifiers.join("+"), params.key)
    };

    let key = params.key;
    let modifiers = params.modifiers;
    run_input(
        move || input::press_key(&key, &modifiers),
        format!("Pressed {}", key_desc),
        "Key press failed",
    )
    .await
}

// ============================================================================
// Get Displays
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct GetDisplaysParams {}

pub fn get_displays(_params: GetDisplaysParams) -> CallToolResult {
    match display::get_displays() {
        Ok(displays) => match serde_json::to_string_pretty(&displays) {
            Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
                "Failed to serialize displays: {}",
                e
            ))]),
        },
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
            "Failed to get displays: {}",
            e
        ))]),
    }
}

// ============================================================================
// Find Text (Accessibility + OCR)
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct FindTextParams {
    pub text: String,
    /// Optional display ID to search on. If omitted, searches the main display.
    /// Ignored when window_id or app_name is provided.
    pub display_id: Option<u32>,
    /// Window ID to scope the search to a specific window.
    pub window_id: Option<u32>,
    /// Application name to scope the search to a specific app's window.
    pub app_name: Option<String>,
    /// Enable language correction (helps with word accuracy but hurts single-character
    /// detection). Defaults to false, which is better for UI automation.
    #[serde(default)]
    pub uses_language_correction: bool,
}

pub fn find_text(params: FindTextParams) -> CallToolResult {
    let debug = std::env::var("NATIVE_DEVTOOLS_DEBUG").is_ok();

    // Resolve window_id from app_name if provided
    let window_id = match (params.window_id, &params.app_name) {
        (Some(id), _) => Some(id),
        (None, Some(app_name)) => match crate::platform::find_windows_by_app(app_name) {
            Ok(windows) if !windows.is_empty() => Some(windows[0].id),
            Ok(_) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "No window found for app '{}'",
                    app_name
                ))]);
            }
            Err(e) => {
                return CallToolResult::error(vec![ContentBlock::text(format!(
                    "Failed to find window: {}",
                    e
                ))]);
            }
        },
        (None, None) => None,
    };

    // Primary: try accessibility tree search
    match find_text_accessibility(&params.text, window_id) {
        Ok(mut matches) if !matches.is_empty() => {
            rank_matches(&mut matches, &params.text);
            return serialize_matches(&matches);
        }
        Ok(_) if debug => {
            eprintln!(
                "[DEBUG find_text] no accessibility matches for '{}', trying OCR",
                params.text
            );
        }
        Err(e) if debug => {
            eprintln!(
                "[DEBUG find_text] accessibility failed for '{}': {}, trying OCR",
                params.text, e
            );
        }
        _ => {}
    }

    // Fallback: OCR
    let ocr_result = if let Some(wid) = window_id {
        find_text_in_window(&params.text, wid, params.uses_language_correction)
    } else {
        #[cfg(target_os = "macos")]
        {
            ocr::find_text(
                &params.text,
                params.display_id,
                params.uses_language_correction,
            )
        }
        #[cfg(target_os = "windows")]
        {
            ocr::find_text(&params.text, params.display_id)
        }
    };

    match ocr_result {
        Ok(ref matches) if !matches.is_empty() => serialize_matches(matches),
        Ok(_) => empty_result_with_available_elements(&params.text, window_id, debug),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
    }
}

// ============================================================================
// Element At Point
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct ElementAtPointParams {
    pub x: f64,
    pub y: f64,
    pub app_name: Option<String>,
}

pub fn element_at_point(params: ElementAtPointParams) -> CallToolResult {
    let result = element_at_point_platform(params.x, params.y, params.app_name.as_deref());
    match result {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
            Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
                "Failed to serialize: {}",
                e
            ))]),
        },
        Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
    }
}

fn element_at_point_platform(
    x: f64,
    y: f64,
    app_name: Option<&str>,
) -> Result<serde_json::Value, String> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::ax::element_at_point(x, y, app_name)
    }
    #[cfg(target_os = "windows")]
    {
        crate::windows::uia::element_at_point(x, y, app_name)
    }
}

/// Try to find text using the platform accessibility API.
fn find_text_accessibility(
    search: &str,
    window_id: Option<u32>,
) -> Result<Vec<ocr::TextMatch>, String> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::ax::find_text(search, window_id)
    }
    #[cfg(target_os = "windows")]
    {
        // TODO: support targeting specific window_id via ElementFromHandle(hwnd)
        let _ = window_id;
        crate::windows::uia::find_text(search)
    }
}

/// Collect all visible element names using the platform accessibility API.
fn list_element_names_accessibility(window_id: Option<u32>) -> Result<Vec<String>, String> {
    #[cfg(target_os = "macos")]
    {
        crate::macos::ax::list_element_names(window_id)
    }
    #[cfg(target_os = "windows")]
    {
        let _ = window_id;
        crate::windows::uia::list_element_names()
    }
}

const MAX_HINT_ELEMENTS: usize = 200;

/// Build a "no matches found" hint JSON string with available element names.
/// Shared between desktop `find_text` and `android_find_text`.
pub fn build_no_matches_hint(search: &str, available_elements: &[String]) -> String {
    let capped: Vec<&str> = available_elements
        .iter()
        .take(MAX_HINT_ELEMENTS)
        .map(|s| s.as_str())
        .collect();
    let hint = serde_json::json!({
        "message": format!("No matches found for \"{}\"", search),
        "available_elements": capped,
    });
    hint.to_string()
}

/// Build an empty result with a list of available UI elements as a hint.
fn empty_result_with_available_elements(
    search: &str,
    window_id: Option<u32>,
    debug: bool,
) -> CallToolResult {
    let mut content = vec![ContentBlock::text("[]")];

    match list_element_names_accessibility(window_id) {
        Ok(names) => {
            if debug && !names.is_empty() {
                eprintln!(
                    "[DEBUG find_text] listing {} available element names as hint",
                    names.len()
                );
            }
            content.push(ContentBlock::text(build_no_matches_hint(search, &names)));
        }
        Err(e) if debug => {
            eprintln!("[DEBUG find_text] failed to list element names: {}", e);
        }
        _ => {}
    }

    CallToolResult::success(content)
}

/// Serialize text matches to a JSON CallToolResult.
/// Rank find_text results so that exact matches and interactive elements appear first.
///
/// Ranking priority (lower score = higher rank):
///   0 — exact match + interactive element
///   1 — exact match + non-interactive element
///   2 — substring match + interactive element
///   3 — substring match + non-interactive element
///
/// Within the same rank, original tree-traversal order is preserved (stable sort).
fn rank_matches(matches: &mut [ocr::TextMatch], search: &str) {
    let search_lower = search.to_lowercase();
    matches.sort_by_key(|m| {
        let is_exact = m.text.to_lowercase() == search_lower;
        let is_interactive = m.role.as_deref().is_some_and(is_interactive_role);
        match (is_exact, is_interactive) {
            (true, true) => 0u8,
            (true, false) => 1,
            (false, true) => 2,
            (false, false) => 3,
        }
    });
}

/// Check whether an accessibility role represents an interactive element.
///
/// Covers both macOS AXRole names (e.g. "AXButton") and Windows UIA control
/// type names (e.g. "Button").
fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        // macOS AXRoles
        "AXButton"
        | "AXTextField"
        | "AXTextArea"
        | "AXLink"
        | "AXCheckBox"
        | "AXRadioButton"
        | "AXPopUpButton"
        | "AXMenuButton"
        | "AXSlider"
        | "AXIncrementor"
        | "AXComboBox"
        | "AXMenuItem"
        | "AXTabGroup"
        | "AXTab"
        // Windows UIA control types
        | "Button"
        | "Edit"
        | "Hyperlink"
        | "CheckBox"
        | "RadioButton"
        | "ComboBox"
        | "Slider"
        | "Spinner"
        | "MenuItem"
        | "TabItem"
        | "ListItem"
        | "TreeItem"
        | "DataItem"
        | "SplitButton"
    )
}

fn serialize_matches(matches: &[ocr::TextMatch]) -> CallToolResult {
    match serde_json::to_string_pretty(matches) {
        Ok(json) => CallToolResult::success(vec![ContentBlock::text(json)]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
            "Failed to serialize: {}",
            e
        ))]),
    }
}

/// Run OCR scoped to a single window and return matching text with screen coordinates.
fn find_text_in_window(
    search: &str,
    window_id: u32,
    uses_language_correction: bool,
) -> Result<Vec<ocr::TextMatch>, String> {
    let screenshot = crate::platform::capture_window(window_id)
        .map_err(|e| format!("Failed to capture window: {}", e))?;

    #[cfg(target_os = "macos")]
    let mut matches = ocr::ocr_image(
        &screenshot.png_data,
        Some(screenshot.scale_factor),
        uses_language_correction,
    )?;
    #[cfg(target_os = "windows")]
    let mut matches = {
        let _ = uses_language_correction; // Windows OCR doesn't support this param
        ocr::ocr_image(&screenshot.png_data, Some(screenshot.scale_factor))?
    };

    // Offset OCR coordinates from image-relative to screen-absolute
    for m in &mut matches {
        m.x += screenshot.origin_x;
        m.y += screenshot.origin_y;
        m.bounds.x += screenshot.origin_x;
        m.bounds.y += screenshot.origin_y;
    }

    // Filter by search term
    let search_lower = search.to_lowercase();
    matches.retain(|m| m.text.to_lowercase().contains(&search_lower));
    matches.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ocr::{TextBounds, TextMatch};

    fn make_match(text: &str, role: Option<&str>) -> TextMatch {
        TextMatch {
            text: text.to_string(),
            x: 0.0,
            y: 0.0,
            confidence: 1.0,
            bounds: TextBounds {
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 20.0,
            },
            role: role.map(|s| s.to_string()),
        }
    }

    #[test]
    fn test_rank_exact_match_before_substring() {
        let mut matches = vec![
            make_match("2×3", None), // substring match, no role
            make_match("2", None),   // exact match, no role
        ];
        rank_matches(&mut matches, "2");
        assert_eq!(matches[0].text, "2");
        assert_eq!(matches[1].text, "2×3");
    }

    #[test]
    fn test_rank_interactive_before_static() {
        let mut matches = vec![
            make_match("Submit", Some("AXStaticText")), // static
            make_match("Submit", Some("AXButton")),     // interactive
        ];
        rank_matches(&mut matches, "Submit");
        assert_eq!(matches[0].role.as_deref(), Some("AXButton"));
        assert_eq!(matches[1].role.as_deref(), Some("AXStaticText"));
    }

    #[test]
    fn test_rank_exact_interactive_is_top() {
        let mut matches = vec![
            make_match("2×3", Some("AXButton")),   // substring + interactive
            make_match("2", Some("AXStaticText")), // exact + static
            make_match("2×3", Some("AXStaticText")), // substring + static
            make_match("2", Some("AXButton")),     // exact + interactive
        ];
        rank_matches(&mut matches, "2");
        assert_eq!(matches[0].text, "2");
        assert_eq!(matches[0].role.as_deref(), Some("AXButton"));
        assert_eq!(matches[1].text, "2");
        assert_eq!(matches[1].role.as_deref(), Some("AXStaticText"));
        assert_eq!(matches[2].text, "2×3");
        assert_eq!(matches[2].role.as_deref(), Some("AXButton"));
        assert_eq!(matches[3].text, "2×3");
        assert_eq!(matches[3].role.as_deref(), Some("AXStaticText"));
    }

    #[test]
    fn test_rank_preserves_order_within_same_rank() {
        let mut matches = vec![
            make_match("Open", Some("AXButton")),
            make_match("Open", Some("AXMenuItem")),
        ];
        rank_matches(&mut matches, "Open");
        // Both are exact + interactive, original order preserved (stable sort)
        assert_eq!(matches[0].role.as_deref(), Some("AXButton"));
        assert_eq!(matches[1].role.as_deref(), Some("AXMenuItem"));
    }

    #[test]
    fn test_rank_case_insensitive_exact_match() {
        let mut matches = vec![
            make_match("SUBMIT button", Some("AXStaticText")),
            make_match("submit", Some("AXButton")),
        ];
        rank_matches(&mut matches, "Submit");
        assert_eq!(matches[0].text, "submit");
        assert_eq!(matches[1].text, "SUBMIT button");
    }

    #[test]
    fn test_rank_no_role_treated_as_non_interactive() {
        let mut matches = vec![
            make_match("OK", None),             // exact, no role (OCR)
            make_match("OK", Some("AXButton")), // exact, interactive
        ];
        rank_matches(&mut matches, "OK");
        assert_eq!(matches[0].role.as_deref(), Some("AXButton"));
        assert_eq!(matches[1].role, None);
    }

    #[test]
    fn test_is_interactive_role_macos() {
        assert!(is_interactive_role("AXButton"));
        assert!(is_interactive_role("AXTextField"));
        assert!(is_interactive_role("AXLink"));
        assert!(is_interactive_role("AXCheckBox"));
        assert!(is_interactive_role("AXMenuItem"));
        assert!(!is_interactive_role("AXStaticText"));
        assert!(!is_interactive_role("AXGroup"));
        assert!(!is_interactive_role("AXImage"));
        assert!(!is_interactive_role("AXScrollArea"));
    }

    #[test]
    fn test_is_interactive_role_windows() {
        assert!(is_interactive_role("Button"));
        assert!(is_interactive_role("Edit"));
        assert!(is_interactive_role("Hyperlink"));
        assert!(is_interactive_role("CheckBox"));
        assert!(is_interactive_role("MenuItem"));
        assert!(!is_interactive_role("Text"));
        assert!(!is_interactive_role("Group"));
        assert!(!is_interactive_role("Image"));
        assert!(!is_interactive_role("Pane"));
    }

    // MARK: - click coordinate variant parsing & validator error tests

    fn empty_click_params() -> ClickParams {
        ClickParams {
            x: None,
            y: None,
            window_x: None,
            window_y: None,
            window_id: None,
            screenshot_x: None,
            screenshot_y: None,
            screenshot_origin_x: None,
            screenshot_origin_y: None,
            screenshot_scale: None,
            screenshot_window_id: None,
            button: None,
            click_count: 1,
        }
    }

    #[test]
    fn test_click_params_accepts_valid_screenshot_pixels_variant() {
        let params: ClickParams = serde_json::from_value(serde_json::json!({
            "screenshot_x": 10.0,
            "screenshot_y": 20.0,
            "screenshot_origin_x": 100.0,
            "screenshot_origin_y": 200.0,
            "screenshot_scale": 2.0,
        }))
        .expect("valid screenshot-pixels payload should deserialize");
        assert_eq!(params.screenshot_x, Some(10.0));
        assert_eq!(params.screenshot_scale, Some(2.0));
    }

    #[test]
    fn test_click_params_accepts_valid_screen_variant() {
        let params: ClickParams = serde_json::from_value(serde_json::json!({
            "x": 500.0,
            "y": 400.0,
        }))
        .expect("valid screen payload should deserialize");
        assert_eq!(params.x, Some(500.0));
        assert_eq!(params.y, Some(400.0));
    }

    #[test]
    fn test_click_error_reports_provided_fields_for_partial_screenshot_pixels() {
        // Caller sent only screenshot_x/y — no origin/scale, no window_id.
        // Closest variant should be screenshot-pixels (highest overlap).
        let mut p = empty_click_params();
        p.screenshot_x = Some(10.0);
        p.screenshot_y = Some(20.0);

        let msg = describe_click_coord_error(&p);
        assert!(msg.contains("screenshot_x"), "msg: {msg}");
        assert!(msg.contains("screenshot_y"), "msg: {msg}");
        assert!(
            msg.contains("'screenshot-pixels'"),
            "closest variant should be screenshot-pixels: {msg}"
        );
        // Missing fields are named.
        assert!(msg.contains("screenshot_origin_x"), "msg: {msg}");
        assert!(msg.contains("screenshot_scale"), "msg: {msg}");
    }

    #[test]
    fn test_click_error_reports_closest_screen_variant_when_only_x_set() {
        let mut p = empty_click_params();
        p.x = Some(100.0);

        let msg = describe_click_coord_error(&p);
        assert!(msg.contains("'screen'"), "msg: {msg}");
        assert!(msg.contains("missing: y"), "msg: {msg}");
    }

    #[test]
    fn test_click_error_reports_closest_window_relative_variant() {
        let mut p = empty_click_params();
        p.window_x = Some(10.0);
        p.window_y = Some(20.0);

        let msg = describe_click_coord_error(&p);
        assert!(msg.contains("'window-relative'"), "msg: {msg}");
        assert!(msg.contains("window_id"), "msg: {msg}");
    }

    #[test]
    fn test_click_error_for_empty_params_names_no_provided_fields() {
        let p = empty_click_params();
        let msg = describe_click_coord_error(&p);
        assert!(msg.contains("(no coordinate fields)"), "msg: {msg}");
    }

    // MARK: - select_click_variant (oneOf runtime enforcement)

    #[test]
    fn test_select_variant_picks_screen_for_pure_x_y() {
        let mut p = empty_click_params();
        p.x = Some(100.0);
        p.y = Some(200.0);
        assert_eq!(select_click_variant(&p), Ok(ClickVariant::Screen));
    }

    #[test]
    fn test_select_variant_picks_screenshot_pixels_for_full_payload() {
        let mut p = empty_click_params();
        p.screenshot_x = Some(10.0);
        p.screenshot_y = Some(20.0);
        p.screenshot_origin_x = Some(100.0);
        p.screenshot_origin_y = Some(200.0);
        p.screenshot_scale = Some(2.0);
        assert_eq!(select_click_variant(&p), Ok(ClickVariant::ScreenshotPixels));
    }

    #[test]
    fn test_select_variant_picks_window_relative() {
        let mut p = empty_click_params();
        p.window_x = Some(10.0);
        p.window_y = Some(20.0);
        p.window_id = Some(42);
        assert_eq!(select_click_variant(&p), Ok(ClickVariant::WindowRelative));
    }

    #[test]
    fn test_select_variant_picks_legacy_screenshot() {
        let mut p = empty_click_params();
        p.screenshot_x = Some(10.0);
        p.screenshot_y = Some(20.0);
        p.screenshot_window_id = Some(42);
        assert_eq!(
            select_click_variant(&p),
            Ok(ClickVariant::ScreenshotPixelsLegacy)
        );
    }

    #[test]
    fn test_select_variant_rejects_mixed_screen_and_screenshot_pixels() {
        // This was the silent-pick bug: the old flat resolver would pick
        // `screen` as the first complete branch and click at (x,y) even
        // though the caller also supplied full screenshot-pixels fields.
        let mut p = empty_click_params();
        p.x = Some(100.0);
        p.y = Some(200.0);
        p.screenshot_x = Some(10.0);
        p.screenshot_y = Some(20.0);
        p.screenshot_origin_x = Some(100.0);
        p.screenshot_origin_y = Some(200.0);
        p.screenshot_scale = Some(2.0);

        let err = select_click_variant(&p).unwrap_err();
        // The message must explicitly flag the mix (naming both families),
        // not just report a "closest variant" with missing: (none).
        assert!(
            err.contains("multiple coordinate variants"),
            "err should flag the mix, got: {err}"
        );
        assert!(err.contains("screen"), "err: {err}");
        assert!(err.contains("screenshot"), "err: {err}");
        assert!(!err.contains("missing: (none)"), "err: {err}");
    }

    #[test]
    fn test_select_variant_rejects_mixed_screen_and_window_relative() {
        let mut p = empty_click_params();
        p.x = Some(100.0);
        p.y = Some(200.0);
        p.window_x = Some(10.0);
        p.window_y = Some(20.0);
        p.window_id = Some(42);

        let err = select_click_variant(&p).unwrap_err();
        assert!(
            err.contains("multiple coordinate variants"),
            "err should flag the mix, got: {err}"
        );
        assert!(err.contains("screen"), "err: {err}");
        assert!(err.contains("window-relative"), "err: {err}");
    }

    #[test]
    fn test_select_variant_rejects_mixed_screenshot_pixels_and_legacy() {
        // Supplying both the origin+scale set AND screenshot_window_id is
        // ambiguous and must be rejected.
        let mut p = empty_click_params();
        p.screenshot_x = Some(10.0);
        p.screenshot_y = Some(20.0);
        p.screenshot_origin_x = Some(100.0);
        p.screenshot_origin_y = Some(200.0);
        p.screenshot_scale = Some(2.0);
        p.screenshot_window_id = Some(7);

        let err = select_click_variant(&p).unwrap_err();
        assert!(err.contains("screenshot-pixels"), "err: {err}");
    }

    #[test]
    fn test_select_variant_rejects_partial_screen() {
        // `x` without `y` is invalid.
        let mut p = empty_click_params();
        p.x = Some(100.0);

        let err = select_click_variant(&p).unwrap_err();
        assert!(
            err.contains("'screen'") || err.contains("screen"),
            "err: {err}"
        );
        assert!(err.contains("y"), "err: {err}");
    }

    #[test]
    fn test_select_variant_rejects_empty_params() {
        let p = empty_click_params();
        assert!(select_click_variant(&p).is_err());
    }
}
