//! Tests for window capture alignment on macOS.
//!
//! These tests verify that:
//! 1. Window screenshots (screencapture -o) match kCGWindowBounds dimensions
//! 2. Screenshot pixel dimensions equal bounds × scale factor
//!
//! This ensures clicking at pixel coordinates derived from screenshots
//! will land on the intended UI elements.
//!
//! Run with: cargo test --test window_capture_alignment_tests -- --ignored --nocapture

#![cfg(target_os = "macos")]

use std::io::Cursor;
use std::process::Command;

/// Get PNG dimensions from raw data
fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    use image::ImageReader;
    let reader = ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

/// Get window bounds from CGWindowListCopyWindowInfo
fn get_window_bounds(window_id: u32) -> Option<(f64, f64, f64, f64)> {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        kCGWindowBounds, kCGWindowListOptionIncludingWindow, kCGWindowNumber,
        CGWindowListCopyWindowInfo,
    };
    use std::ffi::c_void;

    type CFDict = CFDictionary<*const c_void, *const c_void>;

    let options = kCGWindowListOptionIncludingWindow;
    let ptr = unsafe { CGWindowListCopyWindowInfo(options, window_id) };
    if ptr.is_null() {
        return None;
    }

    let list: CFArray<*const c_void> = unsafe { CFArray::wrap_under_create_rule(ptr) };

    for i in 0..list.len() {
        let dict: CFDict =
            unsafe { CFDictionary::wrap_under_get_rule(*list.get_unchecked(i) as *const _) };

        let get_value = |key: *const c_void| -> Option<CFType> {
            dict.find(key)
                .map(|v| unsafe { CFType::wrap_under_get_rule(*v as *const _) })
        };

        let wid = get_value(unsafe { kCGWindowNumber } as *const c_void)?
            .downcast::<CFNumber>()?
            .to_i64()? as u32;

        if wid != window_id {
            continue;
        }

        // Get bounds
        let bounds_ptr = dict.find(unsafe { kCGWindowBounds } as *const c_void)?;
        let bounds: CFDict = unsafe { CFDictionary::wrap_under_get_rule(*bounds_ptr as *const _) };

        let get_f64 = |k: &str| -> Option<f64> {
            let cf_key = CFString::new(k);
            bounds
                .find(cf_key.as_concrete_TypeRef() as *const c_void)
                .map(|v| unsafe { CFType::wrap_under_get_rule(*v as *const _) })?
                .downcast::<CFNumber>()?
                .to_f64()
        };

        return Some((
            get_f64("X")?,
            get_f64("Y")?,
            get_f64("Width")?,
            get_f64("Height")?,
        ));
    }

    None
}

/// Capture window using screencapture -o (no shadow)
fn capture_window_no_shadow(window_id: u32) -> Option<Vec<u8>> {
    let temp_dir = tempfile::tempdir().ok()?;
    let path = temp_dir.path().join("screenshot.png");
    let path_str = path.to_string_lossy().to_string();

    let output = Command::new("screencapture")
        .args([
            "-x",
            "-o",
            "-l",
            &window_id.to_string(),
            "-t",
            "png",
            &path_str,
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    std::fs::read(&path).ok()
}

/// Get display scale factor from main display
fn get_main_scale_factor() -> f64 {
    use core_graphics::display::CGDisplay;
    let main = CGDisplay::main();
    if let Some(mode) = main.display_mode() {
        let pixel_width = mode.pixel_width() as f64;
        let point_width = mode.width() as f64;
        pixel_width / point_width
    } else {
        2.0 // Default to Retina
    }
}

/// Find a standard window (layer 0) suitable for testing.
/// Returns a window that is large enough to be a proper application window.
fn find_testable_window() -> Option<u32> {
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use core_graphics::window::{
        kCGNullWindowID, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
        kCGWindowListOptionOnScreenOnly, kCGWindowNumber, kCGWindowOwnerName,
        CGWindowListCopyWindowInfo,
    };
    use std::ffi::c_void;

    type CFDict = CFDictionary<*const c_void, *const c_void>;

    let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let ptr = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
    if ptr.is_null() {
        return None;
    }

    let list: CFArray<*const c_void> = unsafe { CFArray::wrap_under_create_rule(ptr) };

    for i in 0..list.len() {
        let dict: CFDict =
            unsafe { CFDictionary::wrap_under_get_rule(*list.get_unchecked(i) as *const _) };

        let get_value = |key: *const c_void| -> Option<CFType> {
            dict.find(key)
                .map(|v| unsafe { CFType::wrap_under_get_rule(*v as *const _) })
        };

        let window_id = get_value(unsafe { kCGWindowNumber } as *const c_void)?
            .downcast::<CFNumber>()?
            .to_i64()? as u32;

        let owner = get_value(unsafe { kCGWindowOwnerName } as *const c_void)
            .and_then(|v| v.downcast::<CFString>())
            .map(|s| s.to_string())
            .unwrap_or_default();

        let layer = get_value(unsafe { kCGWindowLayer } as *const c_void)
            .and_then(|v| v.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            .unwrap_or(-1);

        // Only test standard windows (layer 0)
        // Skip system processes
        if layer != 0 {
            continue;
        }
        if owner == "Window Server"
            || owner == "Dock"
            || owner == "Control Center"
            || owner == "Notification Center"
            || owner.is_empty()
        {
            continue;
        }

        // Verify we can get bounds and the window is reasonably sized
        // (filter out tiny status bar items, tooltips, etc.)
        if let Some((_, _, w, h)) = get_window_bounds(window_id) {
            // Minimum 100x100 points to be considered a real window
            if w >= 100.0 && h >= 100.0 {
                return Some(window_id);
            }
        }
    }

    None
}

#[cfg(test)]
mod window_capture_alignment {
    use super::*;

    /// Verify that screencapture -o produces images matching kCGWindowBounds × scale.
    ///
    /// This is the core alignment test. If this fails, click coordinates derived
    /// from screenshots will be offset from intended targets.
    #[test]
    #[ignore = "requires an on-screen app window and Screen Recording permission — run with `cargo test --test window_capture_alignment_tests -- --ignored`"]
    fn test_screenshot_dimensions_match_bounds() {
        let Some(window_id) = find_testable_window() else {
            println!("No testable window found - skipping test");
            println!("Try opening a standard application window (Finder, Calculator, etc.)");
            return;
        };

        let Some((_, _, bounds_w, bounds_h)) = get_window_bounds(window_id) else {
            panic!("Failed to get bounds for window {}", window_id);
        };

        let Some(png_data) = capture_window_no_shadow(window_id) else {
            panic!("Failed to capture window {}", window_id);
        };

        let Some((pixel_w, pixel_h)) = png_dimensions(&png_data) else {
            panic!("Failed to read PNG dimensions");
        };

        let scale = get_main_scale_factor();
        let expected_w = (bounds_w * scale).round() as u32;
        let expected_h = (bounds_h * scale).round() as u32;

        println!(
            "Window {}: bounds={:.0}x{:.0} scale={} expected={}x{} actual={}x{}",
            window_id, bounds_w, bounds_h, scale, expected_w, expected_h, pixel_w, pixel_h
        );

        // Allow 1 pixel tolerance for rounding
        let w_diff = (pixel_w as i32 - expected_w as i32).abs();
        let h_diff = (pixel_h as i32 - expected_h as i32).abs();

        assert!(
            w_diff <= 1 && h_diff <= 1,
            "Screenshot dimensions don't match bounds × scale: \
             expected {}x{}, got {}x{}, diff=({}, {})",
            expected_w,
            expected_h,
            pixel_w,
            pixel_h,
            w_diff,
            h_diff
        );
    }

    /// Verify the relationship between kCGWindowBounds and screencapture with/without shadow.
    ///
    /// This diagnostic test prints detailed info about shadow offsets.
    /// It's marked as ignored by default since it's informational.
    #[test]
    #[ignore]
    fn diagnostic_bounds_vs_shadow() {
        use core_foundation::array::CFArray;
        use core_foundation::base::{CFType, TCFType};
        use core_foundation::dictionary::CFDictionary;
        use core_foundation::number::CFNumber;
        use core_foundation::string::CFString;
        use core_graphics::window::{
            kCGNullWindowID, kCGWindowLayer, kCGWindowListExcludeDesktopElements,
            kCGWindowListOptionOnScreenOnly, kCGWindowName, kCGWindowNumber, kCGWindowOwnerName,
            CGWindowListCopyWindowInfo,
        };
        use std::ffi::c_void;

        type CFDict = CFDictionary<*const c_void, *const c_void>;

        let options = kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
        let ptr = unsafe { CGWindowListCopyWindowInfo(options, kCGNullWindowID) };
        assert!(!ptr.is_null(), "Failed to get window list");

        let list: CFArray<*const c_void> = unsafe { CFArray::wrap_under_create_rule(ptr) };
        let scale_factor = get_main_scale_factor();

        println!("\n=== Window Bounds vs Screenshot Diagnostic ===");
        println!("Display scale factor: {}", scale_factor);
        println!();

        let mut tested_count = 0;

        for i in 0..list.len() {
            let dict: CFDict =
                unsafe { CFDictionary::wrap_under_get_rule(*list.get_unchecked(i) as *const _) };

            let get_value = |key: *const c_void| -> Option<CFType> {
                dict.find(key)
                    .map(|v| unsafe { CFType::wrap_under_get_rule(*v as *const _) })
            };

            let window_id = get_value(unsafe { kCGWindowNumber } as *const c_void)
                .and_then(|v| v.downcast::<CFNumber>())
                .and_then(|n| n.to_i64())
                .unwrap_or(0) as u32;

            let owner = get_value(unsafe { kCGWindowOwnerName } as *const c_void)
                .and_then(|v| v.downcast::<CFString>())
                .map(|s| s.to_string())
                .unwrap_or_default();

            let name = get_value(unsafe { kCGWindowName } as *const c_void)
                .and_then(|v| v.downcast::<CFString>())
                .map(|s| s.to_string());

            let layer = get_value(unsafe { kCGWindowLayer } as *const c_void)
                .and_then(|v| v.downcast::<CFNumber>())
                .and_then(|n| n.to_i64())
                .unwrap_or(0);

            if layer != 0 {
                continue;
            }
            if owner == "Window Server" || owner == "Dock" || owner.is_empty() {
                continue;
            }

            let Some((_, _, bounds_w, bounds_h)) = get_window_bounds(window_id) else {
                continue;
            };

            let Some(png_data_no_shadow) = capture_window_no_shadow(window_id) else {
                continue;
            };
            let Some((px_w, px_h)) = png_dimensions(&png_data_no_shadow) else {
                continue;
            };

            println!("Window: {} - {:?} (id={})", owner, name, window_id);
            println!(
                "  kCGWindowBounds: {:.1} x {:.1} points",
                bounds_w, bounds_h
            );
            println!(
                "  Expected pixels: {:.0} x {:.0}",
                bounds_w * scale_factor,
                bounds_h * scale_factor
            );
            println!("  Actual pixels:   {} x {}", px_w, px_h);

            let expected_w = (bounds_w * scale_factor) as u32;
            let expected_h = (bounds_h * scale_factor) as u32;
            let w_diff = px_w as i32 - expected_w as i32;
            let h_diff = px_h as i32 - expected_h as i32;

            if w_diff == 0 && h_diff == 0 {
                println!("  ✓ Matches expected");
            } else {
                println!("  ⚠ Mismatch: diff = ({}, {}) pixels", w_diff, h_diff);
            }

            println!();
            tested_count += 1;

            if tested_count >= 5 {
                break;
            }
        }

        if tested_count == 0 {
            println!("No suitable windows found for testing.");
            println!("Try opening a Finder window or another standard application.");
        }

        println!("=== Diagnostic Complete ===");
    }
}
