use crate::platform;
use crate::tools::screenshot_cache::{ScreenshotCache, ScreenshotMetadata as CacheMetadata};
use base64::Engine;
use image::ImageReader;
use rmcp::model::{CallToolResult, ContentBlock};
use serde::{Deserialize, Serialize};
use serde_json::to_string_pretty;
use std::io::Cursor;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Deserialize)]
pub struct TakeScreenshotParams {
    /// Capture mode: "screen", "window", or "region"
    #[serde(default = "default_mode")]
    pub mode: String,

    /// Window ID (for mode="window")
    pub window_id: Option<u32>,

    /// Application name to capture (for mode="window", alternative to window_id)
    pub app_name: Option<String>,

    /// Region coordinates (required for mode="region")
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub width: Option<f64>,
    pub height: Option<f64>,

    /// Include OCR text annotations with clickable coordinates (default: true)
    #[serde(default = "default_include_ocr")]
    pub include_ocr: bool,
}

fn default_mode() -> String {
    "window".to_string()
}

fn default_include_ocr() -> bool {
    true
}

use super::JPEG_QUALITY;

/// Convert PNG data to JPEG.
pub(crate) fn png_to_jpeg(png_data: &[u8]) -> Result<Vec<u8>, String> {
    let img = ImageReader::new(Cursor::new(png_data))
        .with_guessed_format()
        .map_err(|e| format!("Failed to read image: {}", e))?
        .decode()
        .map_err(|e| format!("Failed to decode PNG: {}", e))?;

    let mut jpeg_data = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_data, JPEG_QUALITY);
    img.write_with_encoder(encoder)
        .map_err(|e| format!("Failed to encode JPEG: {}", e))?;

    Ok(jpeg_data)
}

#[derive(Debug, Serialize)]
struct ScreenshotMetadata {
    /// Unique ID for referencing this screenshot in subsequent tool calls (e.g., find_image).
    #[serde(skip_serializing_if = "Option::is_none")]
    screenshot_id: Option<String>,
    screenshot_origin_x: f64,
    screenshot_origin_y: f64,
    screenshot_scale: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    screenshot_window_id: Option<u32>,
    /// Pixel width of the screenshot image.
    screenshot_pixel_width: u32,
    /// Pixel height of the screenshot image.
    screenshot_pixel_height: u32,
}

/// Take a screenshot, optionally caching it for later use with find_image.
///
/// If a cache is provided, the screenshot PNG is stored and a `screenshot_id`
/// is included in the response metadata.
pub async fn take_screenshot(
    params: TakeScreenshotParams,
    cache: Option<Arc<RwLock<ScreenshotCache>>>,
) -> CallToolResult {
    // Track resolved window_id for metadata (important when using app_name)
    let mut resolved_window_id: Option<u32> = None;

    let result = match params.mode.as_str() {
        "screen" => platform::capture_screen(),
        "window" => {
            let window_id = match (params.window_id, &params.app_name) {
                (Some(id), _) => id,
                (None, Some(app_name)) => match platform::find_windows_by_app(app_name) {
                    Ok(windows) if !windows.is_empty() => windows[0].id,
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
                (None, None) => {
                    return CallToolResult::error(vec![ContentBlock::text(
                        "window_id or app_name is required for mode='window'",
                    )]);
                }
            };
            // Store the resolved window_id for metadata
            resolved_window_id = Some(window_id);
            platform::capture_window(window_id)
        }
        "region" => {
            let (x, y, w, h) = match (params.x, params.y, params.width, params.height) {
                (Some(x), Some(y), Some(w), Some(h)) => (x, y, w, h),
                _ => {
                    return CallToolResult::error(vec![ContentBlock::text(
                        "x, y, width, and height are required for mode='region'",
                    )]);
                }
            };
            platform::capture_region(x, y, w, h)
        }
        _ => {
            return CallToolResult::error(vec![ContentBlock::text(format!(
                "Unknown mode '{}'. Use 'screen', 'window', or 'region'",
                params.mode
            ))]);
        }
    };

    match result {
        Ok(screenshot) => {
            // Store in cache if provided (store raw PNG for accuracy)
            let screenshot_id = if let Some(cache) = cache {
                let cache_metadata = CacheMetadata {
                    origin_x: screenshot.origin_x,
                    origin_y: screenshot.origin_y,
                    scale: screenshot.scale_factor,
                    window_id: resolved_window_id,
                    pixel_width: screenshot.pixel_width,
                    pixel_height: screenshot.pixel_height,
                };
                let id = cache
                    .write()
                    .await
                    .store(screenshot.png_data.clone(), cache_metadata);
                Some(id)
            } else {
                None
            };

            // Convert to JPEG for smaller payload size
            let (image_data, mime_type) = match png_to_jpeg(&screenshot.png_data) {
                Ok(jpeg_data) => (jpeg_data, "image/jpeg"),
                Err(e) => {
                    // Fall back to PNG if JPEG conversion fails
                    tracing::warn!("JPEG conversion failed, using PNG: {}", e);
                    (screenshot.png_data.clone(), "image/png")
                }
            };

            let base64_data = base64::engine::general_purpose::STANDARD.encode(&image_data);
            let mut contents = vec![ContentBlock::image(base64_data, mime_type)];
            let metadata = ScreenshotMetadata {
                screenshot_id,
                screenshot_origin_x: screenshot.origin_x,
                screenshot_origin_y: screenshot.origin_y,
                screenshot_scale: screenshot.scale_factor,
                screenshot_window_id: resolved_window_id,
                screenshot_pixel_width: screenshot.pixel_width,
                screenshot_pixel_height: screenshot.pixel_height,
            };
            if let Ok(json) = to_string_pretty(&metadata) {
                contents.push(ContentBlock::text(json));
            }

            // Run OCR if requested
            if params.include_ocr {
                #[cfg(target_os = "macos")]
                let ocr_result =
                    platform::ocr_image(&screenshot.png_data, Some(screenshot.scale_factor), false);
                #[cfg(target_os = "windows")]
                let ocr_result =
                    platform::ocr_image(&screenshot.png_data, Some(screenshot.scale_factor));
                match ocr_result {
                    Ok(mut matches) => {
                        apply_ocr_offset(&mut matches, screenshot.origin_x, screenshot.origin_y);
                        if !matches.is_empty() {
                            let ocr_text = format_ocr_results(&matches);
                            contents.push(ContentBlock::text(ocr_text));
                        }
                    }
                    Err(e) => {
                        contents.push(ContentBlock::text(format!("OCR failed: {}", e)));
                    }
                }
            }

            CallToolResult::success(contents)
        }
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!(
            "Screenshot failed: {}",
            e
        ))]),
    }
}

/// Convert OCR coordinates from image-relative to screen-absolute.
///
/// OCR detects text at pixel positions within the screenshot image. For window
/// or region screenshots, these positions are relative to the image origin (0,0).
/// To make the coordinates directly clickable, we add the screenshot's screen
/// origin so the LLM can use them with the click tool without further translation.
fn apply_ocr_offset(matches: &mut [platform::TextMatch], offset_x: f64, offset_y: f64) {
    if offset_x == 0.0 && offset_y == 0.0 {
        return;
    }

    for m in matches {
        m.x += offset_x;
        m.y += offset_y;
        m.bounds.x += offset_x;
        m.bounds.y += offset_y;
    }
}

/// Format OCR results as a text summary with clickable coordinates and bounds.
fn format_ocr_results(matches: &[platform::TextMatch]) -> String {
    let mut result = String::from("## OCR Text Detected\nCoordinates below are screen coordinates — use directly with the click tool. No conversion with screenshot_origin/screenshot_scale needed.\n\n");

    for m in matches.iter().filter(|m| m.confidence > 0.5) {
        result.push_str(&format!(
            "- \"{}\" at ({:.0}, {:.0}) bounds: {{x: {:.0}, y: {:.0}, w: {:.0}, h: {:.0}}}\n",
            m.text, m.x, m.y, m.bounds.x, m.bounds.y, m.bounds.width, m.bounds.height
        ));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::ocr::{TextBounds, TextMatch};

    fn make_text_match(text: &str, x: f64, y: f64, confidence: f64) -> TextMatch {
        TextMatch {
            text: text.to_string(),
            x,
            y,
            confidence,
            bounds: TextBounds {
                x,
                y,
                width: 50.0,
                height: 20.0,
            },
            role: None,
        }
    }

    // MARK: - apply_ocr_offset tests

    #[test]
    fn test_apply_ocr_offset_adds_offsets() {
        let mut matches = vec![
            make_text_match("Hello", 100.0, 200.0, 0.9),
            make_text_match("World", 300.0, 400.0, 0.8),
        ];

        apply_ocr_offset(&mut matches, 50.0, 75.0);

        assert_eq!(matches[0].x, 150.0);
        assert_eq!(matches[0].y, 275.0);
        assert_eq!(matches[0].bounds.x, 150.0);
        assert_eq!(matches[0].bounds.y, 275.0);

        assert_eq!(matches[1].x, 350.0);
        assert_eq!(matches[1].y, 475.0);
    }

    #[test]
    fn test_apply_ocr_offset_skips_zero_offset() {
        let mut matches = vec![make_text_match("Test", 100.0, 200.0, 0.9)];

        apply_ocr_offset(&mut matches, 0.0, 0.0);

        // Values should be unchanged
        assert_eq!(matches[0].x, 100.0);
        assert_eq!(matches[0].y, 200.0);
    }

    #[test]
    fn test_apply_ocr_offset_handles_negative_offsets() {
        // Multi-display setups can have negative coordinates
        let mut matches = vec![make_text_match("Negative", 100.0, 100.0, 0.9)];

        apply_ocr_offset(&mut matches, -200.0, -150.0);

        assert_eq!(matches[0].x, -100.0);
        assert_eq!(matches[0].y, -50.0);
    }

    #[test]
    fn test_apply_ocr_offset_empty_matches() {
        let mut matches: Vec<TextMatch> = vec![];
        apply_ocr_offset(&mut matches, 100.0, 100.0);
        assert!(matches.is_empty());
    }

    // MARK: - format_ocr_results tests

    #[test]
    fn test_format_ocr_results_filters_low_confidence() {
        let matches = vec![
            make_text_match("HighConf", 100.0, 200.0, 0.9),
            make_text_match("LowConf", 300.0, 400.0, 0.3),
            make_text_match("Borderline", 500.0, 600.0, 0.5), // Exactly 0.5 should be excluded
        ];

        let result = format_ocr_results(&matches);

        assert!(result.contains("HighConf"));
        assert!(!result.contains("LowConf"));
        assert!(!result.contains("Borderline"));
    }

    #[test]
    fn test_format_ocr_results_includes_header() {
        let matches = vec![make_text_match("Test", 100.0, 200.0, 0.9)];
        let result = format_ocr_results(&matches);

        assert!(
            result.starts_with("## OCR Text Detected\nCoordinates below are screen coordinates")
        );
    }

    #[test]
    fn test_format_ocr_results_formats_coordinates() {
        let matches = vec![make_text_match("Button", 123.7, 456.2, 0.95)];
        let result = format_ocr_results(&matches);

        // Coordinates should be rounded to integers in output
        assert!(result.contains("\"Button\" at (124, 456)"));
        assert!(result.contains("bounds: {x: 124, y: 456, w: 50, h: 20}"));
    }

    #[test]
    fn test_format_ocr_results_empty_matches() {
        let matches: Vec<TextMatch> = vec![];
        let result = format_ocr_results(&matches);

        // Should still have header but no items
        assert!(result.contains("## OCR Text Detected\nCoordinates below are screen coordinates"));
        assert!(!result.contains("- \""));
    }
}
