//! OCR functionality using Apple Vision for text detection on screen.

// This module still uses the deprecated `cocoa` crate (superseded by `objc2`)
// and `image::io::Reader`.
#![allow(deprecated)]

use super::display;
use cocoa::base::nil;
use cocoa::foundation::NSAutoreleasePool;
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::data::CFData;
use objc::runtime::{Class, Object};
use objc::{msg_send, sel, sel_impl};
use serde::{Deserialize, Serialize};
use std::process::Command;
use std::ptr;

#[link(name = "ImageIO", kind = "framework")]
extern "C" {
    fn CGImageSourceCreateWithData(data: CFTypeRef, options: CFTypeRef) -> *mut std::ffi::c_void;
    fn CGImageSourceCreateImageAtIndex(
        source: *mut std::ffi::c_void,
        index: usize,
        options: CFTypeRef,
    ) -> *mut std::ffi::c_void;
    fn CGImageGetWidth(image: *mut std::ffi::c_void) -> usize;
    fn CGImageGetHeight(image: *mut std::ffi::c_void) -> usize;
}

// Link Vision framework to ensure classes are loaded before runtime lookup
#[link(name = "Vision", kind = "framework")]
extern "C" {}

#[repr(C)]
struct CGRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// Bounding box in screen coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A text match found by OCR with screen coordinates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextMatch {
    pub text: String,
    pub x: f64,
    pub y: f64,
    pub confidence: f64,
    pub bounds: TextBounds,
    /// Accessibility role of the element (e.g. "AXButton", "AXStaticText").
    /// Present for accessibility-tree results, absent for OCR results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// Run OCR on PNG image data and return all detected text with screen coordinates.
///
/// When `uses_language_correction` is `false` (recommended for UI automation),
/// Vision skips word-level correction, improving detection of isolated characters
/// like calculator buttons, single-letter labels, and symbols.
pub fn ocr_image(
    png_data: &[u8],
    scale: Option<f64>,
    uses_language_correction: bool,
) -> Result<Vec<TextMatch>, String> {
    let scale = scale.unwrap_or_else(|| {
        display::get_main_display()
            .map(|d| d.backing_scale_factor)
            .unwrap_or(2.0)
    });

    unsafe { run_vision_ocr(png_data, scale, uses_language_correction) }
}

unsafe fn run_vision_ocr(
    png_data: &[u8],
    scale: f64,
    uses_language_correction: bool,
) -> Result<Vec<TextMatch>, String> {
    // Check Vision framework availability
    let handler_class = Class::get("VNImageRequestHandler")
        .ok_or("Vision framework not available (requires macOS 10.13+)")?;
    let request_class = Class::get("VNRecognizeTextRequest")
        .ok_or("VNRecognizeTextRequest not available (requires macOS 10.15+)")?;
    let dict_class = Class::get("NSDictionary").ok_or("NSDictionary class not available")?;
    let array_class = Class::get("NSArray").ok_or("NSArray class not available")?;

    // Create autorelease pool to prevent memory leaks from Objective-C objects
    let pool = NSAutoreleasePool::new(nil);

    // Load image
    let cf_data = CFData::from_buffer(png_data);
    let image_source = CGImageSourceCreateWithData(cf_data.as_CFTypeRef(), ptr::null());
    if image_source.is_null() {
        let _: () = msg_send![pool, drain];
        return Err("Failed to create CGImageSource".into());
    }

    let cg_image = CGImageSourceCreateImageAtIndex(image_source, 0, ptr::null());
    if cg_image.is_null() {
        CFRelease(image_source as CFTypeRef);
        let _: () = msg_send![pool, drain];
        return Err("Failed to create CGImage".into());
    }

    let img_w = CGImageGetWidth(cg_image) as f64;
    let img_h = CGImageGetHeight(cg_image) as f64;

    // Create Vision request handler
    let handler: *mut Object = msg_send![handler_class, alloc];
    let empty_dict: *mut Object = msg_send![dict_class, dictionary];
    let handler: *mut Object = msg_send![handler, initWithCGImage:cg_image options:empty_dict];

    if handler.is_null() {
        CFRelease(cg_image as CFTypeRef);
        CFRelease(image_source as CFTypeRef);
        let _: () = msg_send![pool, drain];
        return Err("Failed to create VNImageRequestHandler".into());
    }

    // Create and configure text recognition request
    let request: *mut Object = msg_send![request_class, alloc];
    let request: *mut Object = msg_send![request, init];

    // VNRequestTextRecognitionLevel: 0 = accurate, 1 = fast (NSInteger)
    let _: () = msg_send![request, setRecognitionLevel: 0isize];
    let _: () = msg_send![request, setUsesLanguageCorrection: uses_language_correction as i8];

    // Execute request
    let requests: *mut Object = msg_send![array_class, arrayWithObject: request];
    let mut error: *mut Object = ptr::null_mut();
    let success: bool = msg_send![handler, performRequests:requests error:&mut error];

    if !success {
        let desc = if !error.is_null() {
            nsstring_to_string(msg_send![error, localizedDescription])
        } else {
            "Unknown error".into()
        };
        let _: () = msg_send![request, release];
        let _: () = msg_send![handler, release];
        CFRelease(cg_image as CFTypeRef);
        CFRelease(image_source as CFTypeRef);
        let _: () = msg_send![pool, drain];
        return Err(format!("Vision OCR failed: {}", desc));
    }

    // Extract results
    let results: *mut Object = msg_send![request, results];
    let count: usize = if results.is_null() {
        0
    } else {
        msg_send![results, count]
    };

    let mut matches = Vec::with_capacity(count);

    for i in 0..count {
        let obs: *mut Object = msg_send![results, objectAtIndex: i];
        let candidates: *mut Object = msg_send![obs, topCandidates: 1usize];
        let candidate_count: usize = msg_send![candidates, count];
        if candidate_count == 0 {
            continue;
        }

        let candidate: *mut Object = msg_send![candidates, objectAtIndex: 0usize];
        let text = nsstring_to_string(msg_send![candidate, string]);
        // VNRecognizedText.confidence is Float (f32) in ObjC, read as f32 then cast
        let confidence: f32 = msg_send![candidate, confidence];
        let confidence = confidence as f64;
        let bbox: CGRect = msg_send![obs, boundingBox];

        let (center_x, center_y, bounds) =
            convert_vision_bbox(bbox.x, bbox.y, bbox.width, bbox.height, img_w, img_h, scale);

        matches.push(TextMatch {
            text,
            x: center_x,
            y: center_y,
            confidence,
            bounds,
            role: None,
        });
    }

    // Cleanup
    let _: () = msg_send![request, release];
    let _: () = msg_send![handler, release];
    CFRelease(cg_image as CFTypeRef);
    CFRelease(image_source as CFTypeRef);
    let _: () = msg_send![pool, drain];

    Ok(matches)
}

unsafe fn nsstring_to_string(nsstring: *mut Object) -> String {
    if nsstring.is_null() {
        return String::new();
    }
    let utf8: *const i8 = msg_send![nsstring, UTF8String];
    if utf8.is_null() {
        return String::new();
    }
    std::ffi::CStr::from_ptr(utf8)
        .to_string_lossy()
        .into_owned()
}

/// Find text on screen using OCR. Returns screen coordinates for each match.
pub fn find_text(
    search: &str,
    display_id: Option<u32>,
    uses_language_correction: bool,
) -> Result<Vec<TextMatch>, String> {
    let displays = display::get_displays().map_err(|e| format!("get_displays failed: {}", e))?;
    let (display_index, display) = displays
        .iter()
        .enumerate()
        .find(|(_, d)| display_id.map_or(d.is_main, |id| d.id == id))
        .map(|(i, d)| (i + 1, d.clone()))
        .ok_or("Display not found")?;

    // Capture screen using a temp directory path (not NamedTempFile which can be deleted)
    let temp_dir = std::env::temp_dir();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let temp_path = temp_dir.join(format!(
        "native_devtools_ocr_{}_{}.png",
        std::process::id(),
        timestamp
    ));
    let temp_path_str = temp_path
        .to_str()
        .ok_or("tempfile path is not valid UTF-8")?;

    let status = Command::new("/usr/sbin/screencapture")
        .args(["-x", "-D", &display_index.to_string(), temp_path_str])
        .status()
        .map_err(|e| format!("screencapture command failed: {}", e))?;

    if !status.success() {
        return Err(format!(
            "screencapture exited with status: {:?}",
            status.code()
        ));
    }

    let png_data =
        std::fs::read(&temp_path).map_err(|e| format!("failed to read screenshot file: {}", e))?;

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_path);
    let mut matches = ocr_image(
        &png_data,
        Some(display.backing_scale_factor),
        uses_language_correction,
    )?;

    // Offset for multi-display and filter by search term
    let search_lower = search.to_lowercase();
    for m in &mut matches {
        m.x += display.bounds.x;
        m.y += display.bounds.y;
        m.bounds.x += display.bounds.x;
        m.bounds.y += display.bounds.y;
    }

    matches.retain(|m| m.text.to_lowercase().contains(&search_lower));
    matches.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
    Ok(matches)
}

/// Convert Vision normalized bounding box to screen coordinates.
///
/// Vision returns normalized coordinates (0.0-1.0) with origin at bottom-left.
/// Screen coordinates have origin at top-left and use points (not pixels).
///
/// # Arguments
/// * `norm_x`, `norm_y` - Normalized bbox origin (0.0-1.0, bottom-left origin)
/// * `norm_w`, `norm_h` - Normalized bbox size (0.0-1.0)
/// * `img_w`, `img_h` - Image dimensions in pixels
/// * `scale` - Display backing scale factor (e.g., 2.0 for Retina)
///
/// # Returns
/// (center_x, center_y, bounds) in screen point coordinates
fn convert_vision_bbox(
    norm_x: f64,
    norm_y: f64,
    norm_w: f64,
    norm_h: f64,
    img_w: f64,
    img_h: f64,
    scale: f64,
) -> (f64, f64, TextBounds) {
    // Convert normalized coords to pixel coords
    let px = norm_x * img_w;
    let pw = norm_w * img_w;
    let ph = norm_h * img_h;
    // Y-flip: Vision origin is bottom-left, screen origin is top-left
    let py = (1.0 - norm_y - norm_h) * img_h;

    // Convert pixels to points and calculate center
    let center_x = (px + pw / 2.0) / scale;
    let center_y = (py + ph / 2.0) / scale;

    let bounds = TextBounds {
        x: px / scale,
        y: py / scale,
        width: pw / scale,
        height: ph / scale,
    };

    (center_x, center_y, bounds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ocr_on_calculator_screenshot() {
        // Load the Calculator screenshot from test fixtures
        let png_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/calculator.png");
        let png_data = std::fs::read(png_path).expect("Failed to read calculator.png fixture");

        let matches = ocr_image(&png_data, Some(2.0), false).expect("OCR should succeed");

        println!("Found {} text matches:", matches.len());
        for m in &matches {
            println!(
                "  '{}' at ({:.1}, {:.1}) conf={:.2}",
                m.text, m.x, m.y, m.confidence
            );
        }

        // Should find at least some digits from the calculator
        assert!(
            !matches.is_empty(),
            "OCR should detect text from calculator"
        );

        let texts: Vec<&str> = matches.iter().map(|m| m.text.as_str()).collect();
        println!("Detected texts: {:?}", texts);

        // Verify we detect expected calculator elements
        let has_digit = texts.iter().any(|t| t.chars().any(|c| c.is_ascii_digit()));
        assert!(has_digit, "Should detect at least one digit");

        // The calculator screenshot shows "9×9 = 81", verify we detect this
        let has_result = texts.iter().any(|t| t.contains("81") || t.contains("9×9"));
        assert!(
            has_result,
            "Should detect the calculation result (81 or 9×9)"
        );
    }

    #[test]
    fn test_convert_vision_bbox_basic() {
        // Vision bbox at bottom-left corner: (0, 0) with size 0.5x0.25
        // Image: 1000x800 pixels, scale 2.0
        let (cx, cy, bounds) = convert_vision_bbox(0.0, 0.0, 0.5, 0.25, 1000.0, 800.0, 2.0);

        // Pixel coords: x=0, w=500, h=200
        // Y-flip: py = (1.0 - 0.0 - 0.25) * 800 = 600
        // Points: bounds = (0, 300, 250, 100)
        // Center: (125, 350)
        assert_eq!(bounds.x, 0.0);
        assert_eq!(bounds.y, 300.0);
        assert_eq!(bounds.width, 250.0);
        assert_eq!(bounds.height, 100.0);
        assert_eq!(cx, 125.0);
        assert_eq!(cy, 350.0);
    }

    #[test]
    fn test_convert_vision_bbox_top_right() {
        // Vision bbox at top-right: (0.5, 0.75) with size 0.5x0.25
        // Image: 1000x800 pixels, scale 2.0
        let (cx, cy, bounds) = convert_vision_bbox(0.5, 0.75, 0.5, 0.25, 1000.0, 800.0, 2.0);

        // Pixel coords: x=500, w=500, h=200
        // Y-flip: py = (1.0 - 0.75 - 0.25) * 800 = 0
        // Points: bounds = (250, 0, 250, 100)
        // Center: (375, 50)
        assert_eq!(bounds.x, 250.0);
        assert_eq!(bounds.y, 0.0);
        assert_eq!(bounds.width, 250.0);
        assert_eq!(bounds.height, 100.0);
        assert_eq!(cx, 375.0);
        assert_eq!(cy, 50.0);
    }

    #[test]
    fn test_convert_vision_bbox_center() {
        // Vision bbox centered: (0.25, 0.375) with size 0.5x0.25
        // Image: 1000x800 pixels, scale 1.0 (non-Retina)
        let (cx, cy, bounds) = convert_vision_bbox(0.25, 0.375, 0.5, 0.25, 1000.0, 800.0, 1.0);

        // Pixel coords: x=250, w=500, h=200
        // Y-flip: py = (1.0 - 0.375 - 0.25) * 800 = 300
        // Points (scale=1): bounds = (250, 300, 500, 200)
        // Center: (500, 400)
        assert_eq!(bounds.x, 250.0);
        assert_eq!(bounds.y, 300.0);
        assert_eq!(bounds.width, 500.0);
        assert_eq!(bounds.height, 200.0);
        assert_eq!(cx, 500.0);
        assert_eq!(cy, 400.0);
    }

    /// Compare OCR results from window capture vs full-screen capture on Calculator.
    /// Requires Calculator to be running. Run with:
    ///   cargo test test_ocr_window_vs_screen -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_ocr_window_vs_screen() {
        use crate::macos::screenshot;
        use crate::macos::window;

        // Find Calculator window
        let windows = window::find_windows_by_app("Calculator").expect("Failed to list windows");
        let calc_window = windows
            .first()
            .expect("Calculator must be running for this test");
        let window_id = calc_window.id;
        println!("Calculator window id: {}", window_id);

        // --- Path 1: capture_window (what take_screenshot + find_text with app_name use) ---
        let win_screenshot =
            screenshot::capture_window(window_id).expect("Failed to capture Calculator window");
        println!(
            "Window capture: {}x{} pixels, scale={}",
            win_screenshot.pixel_width, win_screenshot.pixel_height, win_screenshot.scale_factor
        );
        let win_matches = ocr_image(
            &win_screenshot.png_data,
            Some(win_screenshot.scale_factor),
            false,
        )
        .expect("OCR on window capture failed");
        let mut win_texts: Vec<&str> = win_matches.iter().map(|m| m.text.as_str()).collect();
        win_texts.sort();

        // --- Path 2: screencapture full screen (what find_text without app_name uses) ---
        let displays = display::get_displays().expect("Failed to get displays");
        let (display_index, display_info) = displays
            .iter()
            .enumerate()
            .find(|(_, d)| d.is_main)
            .map(|(i, d)| (i + 1, d.clone()))
            .expect("No main display found");

        let temp_path = std::env::temp_dir().join("native_devtools_ocr_comparison_test.png");
        let temp_path_str = temp_path.to_str().unwrap();
        let status = std::process::Command::new("/usr/sbin/screencapture")
            .args(["-x", "-D", &display_index.to_string(), temp_path_str])
            .status()
            .expect("screencapture failed");
        assert!(status.success(), "screencapture exited with error");
        let screen_png = std::fs::read(&temp_path).expect("Failed to read screen capture");
        let _ = std::fs::remove_file(&temp_path);

        // Get screen image dimensions
        let screen_dims = image::io::Reader::new(std::io::Cursor::new(&screen_png))
            .with_guessed_format()
            .ok()
            .and_then(|r| r.into_dimensions().ok());
        if let Some((w, h)) = screen_dims {
            println!(
                "Screen capture: {}x{} pixels, scale={}",
                w, h, display_info.backing_scale_factor
            );
        }

        let screen_matches = ocr_image(&screen_png, Some(display_info.backing_scale_factor), false)
            .expect("OCR on screen capture failed");

        // Filter screen OCR to just Calculator-relevant items by position
        // (the screen has lots of other text)
        let calc_x = calc_window.bounds.x;
        let calc_y = calc_window.bounds.y;
        let calc_w = calc_window.bounds.width;
        let calc_h = calc_window.bounds.height;
        println!(
            "Calculator bounds: ({}, {}) {}x{}",
            calc_x, calc_y, calc_w, calc_h
        );

        let screen_calc_matches: Vec<&TextMatch> = screen_matches
            .iter()
            .filter(|m| {
                // Screen OCR coords are image-relative (no display offset yet),
                // but window bounds are in screen coords.
                // For main display at origin (0,0), these align.
                let sx = m.x + display_info.bounds.x;
                let sy = m.y + display_info.bounds.y;
                sx >= calc_x && sx <= calc_x + calc_w && sy >= calc_y && sy <= calc_y + calc_h
            })
            .collect();
        let mut screen_texts: Vec<&str> = screen_calc_matches
            .iter()
            .map(|m| m.text.as_str())
            .collect();
        screen_texts.sort();

        println!("\n=== Results ===");
        println!(
            "Window capture OCR: {} matches — {:?}",
            win_texts.len(),
            win_texts
        );
        println!(
            "Screen capture OCR (Calculator region): {} matches — {:?}",
            screen_texts.len(),
            screen_texts
        );

        // Show what each found that the other didn't
        let only_window: Vec<&&str> = win_texts
            .iter()
            .filter(|t| !screen_texts.contains(t))
            .collect();
        let only_screen: Vec<&&str> = screen_texts
            .iter()
            .filter(|t| !win_texts.contains(t))
            .collect();
        println!("\nOnly in window capture: {:?}", only_window);
        println!("Only in screen capture: {:?}", only_screen);
        println!(
            "Total screen OCR matches (all windows): {}",
            screen_matches.len()
        );
    }
}
