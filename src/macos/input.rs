//! CGEvent-based input simulation for macOS.
//!
//! This module provides system-level input simulation that works with any application,
//! regardless of the UI framework used (AppKit, SwiftUI, egui, etc.).

use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use std::thread;
use std::time::Duration;

/// Mouse button types for click operations.
#[derive(Debug, Clone, Copy, Default)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Center,
}

/// Check if the current process has accessibility permissions.
/// CGEvent-based input requires accessibility access to be granted.
pub fn check_accessibility_permission() -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    // Link to ApplicationServices framework for AXIsProcessTrustedWithOptions
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: core_foundation::base::CFTypeRef) -> bool;
    }

    // kAXTrustedCheckOptionPrompt key
    let key = CFString::new("AXTrustedCheckOptionPrompt");
    let value = core_foundation::boolean::CFBoolean::false_value();

    let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);

    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as _) }
}

/// Move the mouse cursor to the specified screen coordinates.
pub fn move_mouse(x: f64, y: f64) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| "Failed to create event source")?;

    let point = CGPoint::new(x, y);

    let event = CGEvent::new_mouse_event(
        source,
        CGEventType::MouseMoved,
        point,
        CGMouseButton::Left, // Button doesn't matter for move
    )
    .map_err(|_| "Failed to create mouse move event")?;

    event.post(CGEventTapLocation::HID);
    Ok(())
}

/// Click at the specified screen coordinates.
pub fn click(x: f64, y: f64, button: MouseButton, click_count: u32) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| "Failed to create event source")?;

    let point = CGPoint::new(x, y);

    let (down_type, up_type, cg_button) = match button {
        MouseButton::Left => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGMouseButton::Left,
        ),
        MouseButton::Right => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGMouseButton::Right,
        ),
        MouseButton::Center => (
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
        ),
    };

    // Perform the click(s)
    for i in 0..click_count {
        let down_event = CGEvent::new_mouse_event(source.clone(), down_type, point, cg_button)
            .map_err(|_| "Failed to create mouse down event")?;

        let up_event = CGEvent::new_mouse_event(source.clone(), up_type, point, cg_button)
            .map_err(|_| "Failed to create mouse up event")?;

        // Set click count (important for double/triple clicks)
        down_event.set_integer_value_field(
            core_graphics::event::EventField::MOUSE_EVENT_CLICK_STATE,
            (i + 1) as i64,
        );
        up_event.set_integer_value_field(
            core_graphics::event::EventField::MOUSE_EVENT_CLICK_STATE,
            (i + 1) as i64,
        );

        down_event.post(CGEventTapLocation::HID);
        // Small delay between down and up for realism
        thread::sleep(Duration::from_millis(10));
        up_event.post(CGEventTapLocation::HID);

        // Delay between clicks for multi-click
        if i < click_count - 1 {
            thread::sleep(Duration::from_millis(50));
        }
    }

    Ok(())
}

/// Drag from one point to another.
pub fn drag(
    start_x: f64,
    start_y: f64,
    end_x: f64,
    end_y: f64,
    button: MouseButton,
) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| "Failed to create event source")?;

    let start_point = CGPoint::new(start_x, start_y);
    let end_point = CGPoint::new(end_x, end_y);

    let (down_type, drag_type, up_type, cg_button) = match button {
        MouseButton::Left => (
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseDragged,
            CGEventType::LeftMouseUp,
            CGMouseButton::Left,
        ),
        MouseButton::Right => (
            CGEventType::RightMouseDown,
            CGEventType::RightMouseDragged,
            CGEventType::RightMouseUp,
            CGMouseButton::Right,
        ),
        MouseButton::Center => (
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseDragged,
            CGEventType::OtherMouseUp,
            CGMouseButton::Center,
        ),
    };

    // Mouse down at start
    let down_event = CGEvent::new_mouse_event(source.clone(), down_type, start_point, cg_button)
        .map_err(|_| "Failed to create mouse down event")?;
    down_event.post(CGEventTapLocation::HID);
    thread::sleep(Duration::from_millis(10));

    // Drag to end (interpolate for smoother movement)
    let steps = 10;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let current_x = start_x + (end_x - start_x) * t;
        let current_y = start_y + (end_y - start_y) * t;
        let current_point = CGPoint::new(current_x, current_y);

        let drag_event =
            CGEvent::new_mouse_event(source.clone(), drag_type, current_point, cg_button)
                .map_err(|_| "Failed to create drag event")?;
        drag_event.post(CGEventTapLocation::HID);
        thread::sleep(Duration::from_millis(10));
    }

    // Mouse up at end
    let up_event = CGEvent::new_mouse_event(source.clone(), up_type, end_point, cg_button)
        .map_err(|_| "Failed to create mouse up event")?;
    up_event.post(CGEventTapLocation::HID);

    Ok(())
}

/// Scroll at the specified position.
pub fn scroll(x: f64, y: f64, delta_x: i32, delta_y: i32) -> Result<(), String> {
    // First move mouse to position
    move_mouse(x, y)?;
    thread::sleep(Duration::from_millis(10));

    // Use CGEventCreateScrollWheelEvent2 via FFI since core-graphics crate doesn't expose it
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        // The non-variadic variant: `CGEventCreateScrollWheelEvent` takes its
        // extra wheels through `...`, which a fixed-arity binding cannot pass
        // correctly on arm64.
        fn CGEventCreateScrollWheelEvent2(
            source: *const std::ffi::c_void,
            units: u32,
            wheel_count: u32,
            wheel1: i32,
            wheel2: i32,
            wheel3: i32,
        ) -> *mut std::ffi::c_void;
        fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
        fn CFRelease(cf: *mut std::ffi::c_void);
    }

    let (wheel_vertical, wheel_horizontal) = scroll_wheel_deltas(delta_x, delta_y);

    unsafe {
        // units: 0 = pixel, 1 = line
        let event = CGEventCreateScrollWheelEvent2(
            std::ptr::null(),
            0, // kCGScrollEventUnitPixel
            2, // wheel_count
            wheel_vertical,
            wheel_horizontal,
            0,
        );

        if event.is_null() {
            return Err("Failed to create scroll event".to_string());
        }

        CGEventPost(0, event); // kCGHIDEventTap = 0
        CFRelease(event);
    }

    Ok(())
}

/// Convert tool deltas (positive = down / right) to CGEvent wheel values
/// `(wheel1, wheel2)`. For CGEvent, positive `wheel1` scrolls up and positive
/// `wheel2` scrolls left, so both axes are negated. Natural scrolling does not
/// change this for posted events.
fn scroll_wheel_deltas(delta_x: i32, delta_y: i32) -> (i32, i32) {
    (delta_y.saturating_neg(), delta_x.saturating_neg())
}

/// Map a key name to a CGKeyCode.
fn key_name_to_code(key: &str) -> Option<CGKeyCode> {
    // Virtual key codes from Events.h
    Some(match key.to_lowercase().as_str() {
        // Letters
        "a" => 0x00,
        "s" => 0x01,
        "d" => 0x02,
        "f" => 0x03,
        "h" => 0x04,
        "g" => 0x05,
        "z" => 0x06,
        "x" => 0x07,
        "c" => 0x08,
        "v" => 0x09,
        "b" => 0x0B,
        "q" => 0x0C,
        "w" => 0x0D,
        "e" => 0x0E,
        "r" => 0x0F,
        "y" => 0x10,
        "t" => 0x11,
        "1" | "!" => 0x12,
        "2" | "@" => 0x13,
        "3" | "#" => 0x14,
        "4" | "$" => 0x15,
        "6" | "^" => 0x16,
        "5" | "%" => 0x17,
        "=" | "+" => 0x18,
        "9" | "(" => 0x19,
        "7" | "&" => 0x1A,
        "-" | "_" => 0x1B,
        "8" | "*" => 0x1C,
        "0" | ")" => 0x1D,
        "]" | "}" => 0x1E,
        "o" => 0x1F,
        "u" => 0x20,
        "[" | "{" => 0x21,
        "i" => 0x22,
        "p" => 0x23,
        "l" => 0x25,
        "j" => 0x26,
        "'" | "\"" => 0x27,
        "k" => 0x28,
        ";" | ":" => 0x29,
        "\\" | "|" => 0x2A,
        "," | "<" => 0x2B,
        "/" | "?" => 0x2C,
        "n" => 0x2D,
        "m" => 0x2E,
        "." | ">" => 0x2F,
        "`" | "~" => 0x32,

        // Special keys
        "return" | "enter" => 0x24,
        "tab" => 0x30,
        "space" | " " => 0x31,
        "delete" | "backspace" => 0x33,
        "escape" | "esc" => 0x35,
        "command" | "cmd" => 0x37,
        "shift" => 0x38,
        "capslock" => 0x39,
        "option" | "alt" => 0x3A,
        "control" | "ctrl" => 0x3B,
        "rightshift" => 0x3C,
        "rightoption" | "rightalt" => 0x3D,
        "rightcontrol" | "rightctrl" => 0x3E,
        "fn" | "function" => 0x3F,

        // Function keys
        "f1" => 0x7A,
        "f2" => 0x78,
        "f3" => 0x63,
        "f4" => 0x76,
        "f5" => 0x60,
        "f6" => 0x61,
        "f7" => 0x62,
        "f8" => 0x64,
        "f9" => 0x65,
        "f10" => 0x6D,
        "f11" => 0x67,
        "f12" => 0x6F,
        "f13" => 0x69,
        "f14" => 0x6B,
        "f15" => 0x71,
        "f16" => 0x6A,
        "f17" => 0x40,
        "f18" => 0x4F,
        "f19" => 0x50,
        "f20" => 0x5A,

        // Navigation
        "home" => 0x73,
        "end" => 0x77,
        "pageup" => 0x74,
        "pagedown" => 0x79,
        "left" | "leftarrow" => 0x7B,
        "right" | "rightarrow" => 0x7C,
        "down" | "downarrow" => 0x7D,
        "up" | "uparrow" => 0x7E,
        "forwarddelete" => 0x75,
        "help" => 0x72,

        _ => return None,
    })
}

/// Press a key with optional modifiers.
pub fn press_key(key: &str, modifiers: &[String]) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| "Failed to create event source")?;

    let keycode = key_name_to_code(key).ok_or_else(|| format!("Unknown key: {}", key))?;

    // Build modifier flags
    let mut flags = CGEventFlags::empty();
    for modifier in modifiers {
        match modifier.to_lowercase().as_str() {
            "shift" => flags |= CGEventFlags::CGEventFlagShift,
            "control" | "ctrl" => flags |= CGEventFlags::CGEventFlagControl,
            "option" | "alt" => flags |= CGEventFlags::CGEventFlagAlternate,
            "command" | "cmd" => flags |= CGEventFlags::CGEventFlagCommand,
            _ => return Err(format!("Unknown modifier: {}", modifier)),
        }
    }

    // Key down
    let down_event = CGEvent::new_keyboard_event(source.clone(), keycode, true)
        .map_err(|_| "Failed to create key down event")?;
    down_event.set_flags(flags);
    down_event.post(CGEventTapLocation::HID);

    thread::sleep(Duration::from_millis(10));

    // Key up
    let up_event = CGEvent::new_keyboard_event(source, keycode, false)
        .map_err(|_| "Failed to create key up event")?;
    up_event.set_flags(flags);
    up_event.post(CGEventTapLocation::HID);

    Ok(())
}

/// Type a string of text by simulating key presses.
pub fn type_text(text: &str) -> Result<(), String> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| "Failed to create event source")?;

    for c in text.chars() {
        // Check if shift is needed
        let needs_shift = c.is_uppercase()
            || matches!(
                c,
                '!' | '@'
                    | '#'
                    | '$'
                    | '%'
                    | '^'
                    | '&'
                    | '*'
                    | '('
                    | ')'
                    | '_'
                    | '+'
                    | '{'
                    | '}'
                    | '|'
                    | ':'
                    | '"'
                    | '<'
                    | '>'
                    | '?'
                    | '~'
            );

        let key_char = c.to_lowercase().next().unwrap_or(c);
        let key_str = key_char.to_string();

        if let Some(keycode) = key_name_to_code(&key_str) {
            let mut flags = CGEventFlags::empty();
            if needs_shift {
                flags |= CGEventFlags::CGEventFlagShift;
            }

            // Key down
            let down_event = CGEvent::new_keyboard_event(source.clone(), keycode, true)
                .map_err(|_| "Failed to create key down event")?;
            down_event.set_flags(flags);
            down_event.post(CGEventTapLocation::HID);

            thread::sleep(Duration::from_millis(5));

            // Key up
            let up_event = CGEvent::new_keyboard_event(source.clone(), keycode, false)
                .map_err(|_| "Failed to create key up event")?;
            up_event.set_flags(flags);
            up_event.post(CGEventTapLocation::HID);

            thread::sleep(Duration::from_millis(5));
        } else {
            // For characters we don't have a direct mapping for,
            // use CGEvent's ability to set Unicode string
            let down_event = CGEvent::new_keyboard_event(source.clone(), 0, true)
                .map_err(|_| "Failed to create key event")?;

            // Set the Unicode string for this character
            down_event.set_string(&c.to_string());
            down_event.post(CGEventTapLocation::HID);

            thread::sleep(Duration::from_millis(5));

            let up_event = CGEvent::new_keyboard_event(source.clone(), 0, false)
                .map_err(|_| "Failed to create key up event")?;
            up_event.post(CGEventTapLocation::HID);

            thread::sleep(Duration::from_millis(5));
        }
    }

    Ok(())
}

/// Get the current cursor position in screen coordinates.
///
/// Creates a dummy CGEvent and reads its location, which reflects
/// the current mouse position.
pub fn get_cursor_position() -> Result<(f64, f64), String> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| "Failed to create event source")?;

    let event = CGEvent::new(source).map_err(|_| "Failed to create CGEvent")?;

    let point = event.location();
    Ok((point.x, point.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_mapping() {
        assert!(key_name_to_code("a").is_some());
        assert!(key_name_to_code("return").is_some());
        assert!(key_name_to_code("f1").is_some());
        assert!(key_name_to_code("nonexistent").is_none());
    }

    #[test]
    fn scroll_down_maps_to_negative_wheel1() {
        // Measured on macOS 27 with natural scrolling on: a posted event with
        // wheel1 = +50 moved the content up by 50 px (scrolled toward the top).
        assert_eq!(scroll_wheel_deltas(0, 3), (-3, 0));
    }

    #[test]
    fn scroll_right_maps_to_negative_wheel2() {
        // Same measurement: wheel2 = +50 scrolled toward the left edge.
        assert_eq!(scroll_wheel_deltas(4, 0), (0, -4));
    }

    #[test]
    fn scroll_delta_extreme_does_not_overflow() {
        assert_eq!(scroll_wheel_deltas(0, i32::MIN), (i32::MAX, 0));
    }

    #[test]
    fn test_get_cursor_position_returns_coordinates() {
        // Just verify it returns successfully; coordinates can be negative
        // on multi-monitor setups where a display is above/left of primary.
        let (_x, _y) = get_cursor_position().expect("should get cursor position");
    }
}
