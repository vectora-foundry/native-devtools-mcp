//! SendInput-based input simulation for Windows.
//!
//! This module provides system-level input simulation using the Win32 SendInput API.

use std::thread;
use std::time::Duration;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
    KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
    MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, VIRTUAL_KEY, VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END,
    VK_ESCAPE, VK_F1, VK_F10, VK_F11, VK_F12, VK_F13, VK_F14, VK_F15, VK_F16, VK_F17, VK_F18,
    VK_F19, VK_F2, VK_F20, VK_F3, VK_F4, VK_F5, VK_F6, VK_F7, VK_F8, VK_F9, VK_HOME, VK_INSERT,
    VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RCONTROL,
    VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

/// Mouse button types for click operations.
#[derive(Debug, Clone, Copy, Default)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Center,
}

/// Check if the current process has permissions for input injection.
/// On Windows, this generally works without special permissions unless
/// targeting elevated windows or secure desktops.
pub fn check_accessibility_permission() -> bool {
    // Windows doesn't have an equivalent accessibility permission check.
    // Input injection works for non-elevated windows. We return true
    // and let specific operations fail if they can't inject.
    true
}

/// Convert screen coordinates to absolute mouse input coordinates.
/// SendInput uses normalized coordinates (0-65535) across the virtual screen.
fn to_absolute_coords(x: f64, y: f64) -> (i32, i32) {
    unsafe {
        let vscreen_x = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vscreen_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vscreen_w = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vscreen_h = GetSystemMetrics(SM_CYVIRTUALSCREEN);

        // Normalize to 0-65535 range relative to virtual screen
        let norm_x = ((x - vscreen_x as f64) / vscreen_w as f64 * 65535.0) as i32;
        let norm_y = ((y - vscreen_y as f64) / vscreen_h as f64 * 65535.0) as i32;

        (norm_x, norm_y)
    }
}

/// Create a mouse input event.
fn make_mouse_input(dx: i32, dy: i32, flags: u32, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Move the mouse cursor to the specified screen coordinates.
pub fn move_mouse(x: f64, y: f64) -> Result<(), String> {
    let (abs_x, abs_y) = to_absolute_coords(x, y);

    let input = make_mouse_input(
        abs_x,
        abs_y,
        (MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK).0,
        0,
    );

    unsafe {
        let result = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        if result == 0 {
            return Err("SendInput failed for mouse move".to_string());
        }
    }

    Ok(())
}

/// Click at the specified screen coordinates.
pub fn click(x: f64, y: f64, button: MouseButton, click_count: u32) -> Result<(), String> {
    // First move the cursor to the click position - many applications require
    // the cursor to actually be at the click location to receive the event
    move_mouse(x, y)?;
    thread::sleep(Duration::from_millis(10));

    let (down_flag, up_flag) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Center => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };

    for _ in 0..click_count {
        let down = make_mouse_input(0, 0, down_flag.0, 0);
        let up = make_mouse_input(0, 0, up_flag.0, 0);

        unsafe {
            let result = SendInput(&[down, up], std::mem::size_of::<INPUT>() as i32);
            if result == 0 {
                return Err("SendInput failed for click".to_string());
            }
        }

        // Small delay between clicks for multi-click
        if click_count > 1 {
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
    let (down_flag, up_flag) = match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Center => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    };

    let base_flags = (MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK).0;
    let move_flags = (MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK).0;

    // Move cursor to start position first (like click does)
    move_mouse(start_x, start_y)?;
    thread::sleep(Duration::from_millis(10));

    // Press down at current cursor position
    let down_input = make_mouse_input(0, 0, down_flag.0, 0);

    unsafe {
        let result = SendInput(&[down_input], std::mem::size_of::<INPUT>() as i32);
        if result == 0 {
            return Err("SendInput failed for drag start".to_string());
        }
    }

    thread::sleep(Duration::from_millis(10));

    // Interpolate movement
    let steps = 10;
    for i in 1..=steps {
        let t = i as f64 / steps as f64;
        let current_x = start_x + (end_x - start_x) * t;
        let current_y = start_y + (end_y - start_y) * t;
        let (abs_x, abs_y) = to_absolute_coords(current_x, current_y);

        let move_input = make_mouse_input(abs_x, abs_y, move_flags, 0);

        unsafe {
            let _ = SendInput(&[move_input], std::mem::size_of::<INPUT>() as i32);
        }

        thread::sleep(Duration::from_millis(10));
    }

    // Release button at end
    let (end_abs_x, end_abs_y) = to_absolute_coords(end_x, end_y);
    let up_input = make_mouse_input(end_abs_x, end_abs_y, base_flags | up_flag.0, 0);

    unsafe {
        let result = SendInput(&[up_input], std::mem::size_of::<INPUT>() as i32);
        if result == 0 {
            return Err("SendInput failed for drag end".to_string());
        }
    }

    Ok(())
}

/// Scroll at the specified position.
pub fn scroll(x: f64, y: f64, delta_x: i32, delta_y: i32) -> Result<(), String> {
    // First move to position
    move_mouse(x, y)?;
    thread::sleep(Duration::from_millis(10));

    let (wheel_vertical, wheel_horizontal) = scroll_wheel_deltas(delta_x, delta_y);

    // Vertical scroll
    if delta_y != 0 {
        let input = make_mouse_input(0, 0, MOUSEEVENTF_WHEEL.0, wheel_vertical);

        unsafe {
            let result = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            if result == 0 {
                return Err("SendInput failed for vertical scroll".to_string());
            }
        }
    }

    // Horizontal scroll
    if delta_x != 0 {
        let input = make_mouse_input(0, 0, MOUSEEVENTF_HWHEEL.0, wheel_horizontal);

        unsafe {
            let result = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
            if result == 0 {
                return Err("SendInput failed for horizontal scroll".to_string());
            }
        }
    }

    Ok(())
}

/// Convert tool deltas (positive = down / right) to `mouseData` values
/// `(WHEEL, HWHEEL)` in `WHEEL_DELTA` (120) units. A positive `WHEEL` value
/// scrolls up, so the vertical axis is negated; a positive `HWHEEL` value
/// already scrolls right.
fn scroll_wheel_deltas(delta_x: i32, delta_y: i32) -> (i32, i32) {
    (delta_y.saturating_mul(-120), delta_x.saturating_mul(120))
}

/// Map a key name to a Windows virtual key code.
fn key_name_to_vk(key: &str) -> Option<VIRTUAL_KEY> {
    Some(match key.to_lowercase().as_str() {
        // Letters (VK codes are ASCII for A-Z)
        "a" => VIRTUAL_KEY(0x41),
        "b" => VIRTUAL_KEY(0x42),
        "c" => VIRTUAL_KEY(0x43),
        "d" => VIRTUAL_KEY(0x44),
        "e" => VIRTUAL_KEY(0x45),
        "f" => VIRTUAL_KEY(0x46),
        "g" => VIRTUAL_KEY(0x47),
        "h" => VIRTUAL_KEY(0x48),
        "i" => VIRTUAL_KEY(0x49),
        "j" => VIRTUAL_KEY(0x4A),
        "k" => VIRTUAL_KEY(0x4B),
        "l" => VIRTUAL_KEY(0x4C),
        "m" => VIRTUAL_KEY(0x4D),
        "n" => VIRTUAL_KEY(0x4E),
        "o" => VIRTUAL_KEY(0x4F),
        "p" => VIRTUAL_KEY(0x50),
        "q" => VIRTUAL_KEY(0x51),
        "r" => VIRTUAL_KEY(0x52),
        "s" => VIRTUAL_KEY(0x53),
        "t" => VIRTUAL_KEY(0x54),
        "u" => VIRTUAL_KEY(0x55),
        "v" => VIRTUAL_KEY(0x56),
        "w" => VIRTUAL_KEY(0x57),
        "x" => VIRTUAL_KEY(0x58),
        "y" => VIRTUAL_KEY(0x59),
        "z" => VIRTUAL_KEY(0x5A),

        // Numbers (VK codes are ASCII for 0-9)
        "0" | ")" => VIRTUAL_KEY(0x30),
        "1" | "!" => VIRTUAL_KEY(0x31),
        "2" | "@" => VIRTUAL_KEY(0x32),
        "3" | "#" => VIRTUAL_KEY(0x33),
        "4" | "$" => VIRTUAL_KEY(0x34),
        "5" | "%" => VIRTUAL_KEY(0x35),
        "6" | "^" => VIRTUAL_KEY(0x36),
        "7" | "&" => VIRTUAL_KEY(0x37),
        "8" | "*" => VIRTUAL_KEY(0x38),
        "9" | "(" => VIRTUAL_KEY(0x39),

        // Special keys
        "return" | "enter" => VK_RETURN,
        "tab" => VK_TAB,
        "space" | " " => VK_SPACE,
        "delete" | "backspace" => VK_BACK,
        "forwarddelete" => VK_DELETE,
        "escape" | "esc" => VK_ESCAPE,
        "shift" => VK_SHIFT,
        "control" | "ctrl" => VK_CONTROL,
        "option" | "alt" => VK_MENU,
        "command" | "cmd" | "win" | "windows" => VK_LWIN,
        "capslock" => VK_CAPITAL,

        // Left/Right modifiers
        "leftshift" | "lshift" => VK_LSHIFT,
        "rightshift" | "rshift" => VK_RSHIFT,
        "leftcontrol" | "lctrl" | "leftctrl" => VK_LCONTROL,
        "rightcontrol" | "rctrl" | "rightctrl" => VK_RCONTROL,
        "leftoption" | "leftalt" | "lalt" => VK_LMENU,
        "rightoption" | "rightalt" | "ralt" => VK_RMENU,
        "leftcommand" | "leftcmd" | "lwin" => VK_LWIN,
        "rightcommand" | "rightcmd" | "rwin" => VK_RWIN,

        // Function keys
        "f1" => VK_F1,
        "f2" => VK_F2,
        "f3" => VK_F3,
        "f4" => VK_F4,
        "f5" => VK_F5,
        "f6" => VK_F6,
        "f7" => VK_F7,
        "f8" => VK_F8,
        "f9" => VK_F9,
        "f10" => VK_F10,
        "f11" => VK_F11,
        "f12" => VK_F12,
        "f13" => VK_F13,
        "f14" => VK_F14,
        "f15" => VK_F15,
        "f16" => VK_F16,
        "f17" => VK_F17,
        "f18" => VK_F18,
        "f19" => VK_F19,
        "f20" => VK_F20,

        // Navigation
        "home" => VK_HOME,
        "end" => VK_END,
        "pageup" | "prior" => VK_PRIOR,
        "pagedown" | "next" => VK_NEXT,
        "left" | "leftarrow" => VK_LEFT,
        "right" | "rightarrow" => VK_RIGHT,
        "up" | "uparrow" => VK_UP,
        "down" | "downarrow" => VK_DOWN,
        "insert" => VK_INSERT,

        // Punctuation - use OEM codes
        "-" | "_" => VIRTUAL_KEY(0xBD),  // VK_OEM_MINUS
        "=" | "+" => VIRTUAL_KEY(0xBB),  // VK_OEM_PLUS
        "[" | "{" => VIRTUAL_KEY(0xDB),  // VK_OEM_4
        "]" | "}" => VIRTUAL_KEY(0xDD),  // VK_OEM_6
        "\\" | "|" => VIRTUAL_KEY(0xDC), // VK_OEM_5
        ";" | ":" => VIRTUAL_KEY(0xBA),  // VK_OEM_1
        "'" | "\"" => VIRTUAL_KEY(0xDE), // VK_OEM_7
        "," | "<" => VIRTUAL_KEY(0xBC),  // VK_OEM_COMMA
        "." | ">" => VIRTUAL_KEY(0xBE),  // VK_OEM_PERIOD
        "/" | "?" => VIRTUAL_KEY(0xBF),  // VK_OEM_2
        "`" | "~" => VIRTUAL_KEY(0xC0),  // VK_OEM_3

        _ => return None,
    })
}

/// Create a keyboard input event.
fn make_key_input(vk: VIRTUAL_KEY, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Create a Unicode keyboard input event.
fn make_unicode_input(c: u16, key_up: bool) -> INPUT {
    let flags = if key_up {
        (KEYEVENTF_UNICODE | KEYEVENTF_KEYUP).0
    } else {
        KEYEVENTF_UNICODE.0
    };

    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: c,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(flags),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Press a key with optional modifiers.
pub fn press_key(key: &str, modifiers: &[String]) -> Result<(), String> {
    let vk = key_name_to_vk(key).ok_or_else(|| format!("Unknown key: {}", key))?;

    // Build list of modifier VKs
    let mut mod_vks = Vec::new();
    for m in modifiers {
        let mod_vk = match m.to_lowercase().as_str() {
            "shift" => VK_SHIFT,
            "control" | "ctrl" => VK_CONTROL,
            "option" | "alt" => VK_MENU,
            "command" | "cmd" | "win" | "windows" => VK_LWIN,
            _ => return Err(format!("Unknown modifier: {}", m)),
        };
        mod_vks.push(mod_vk);
    }

    // Press modifiers down
    for &mod_vk in &mod_vks {
        let input = make_key_input(mod_vk, 0);
        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    // Press and release main key
    let down = make_key_input(vk, 0);
    let up = make_key_input(vk, KEYEVENTF_KEYUP.0);

    unsafe {
        SendInput(&[down], std::mem::size_of::<INPUT>() as i32);
        thread::sleep(Duration::from_millis(10));
        SendInput(&[up], std::mem::size_of::<INPUT>() as i32);
    }

    // Release modifiers
    for &mod_vk in mod_vks.iter().rev() {
        let input = make_key_input(mod_vk, KEYEVENTF_KEYUP.0);
        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }
    }

    Ok(())
}

/// Type a string of text using Unicode input.
/// This is layout-independent and works with any Unicode characters.
pub fn type_text(text: &str) -> Result<(), String> {
    for c in text.chars() {
        // Handle surrogate pairs for characters outside BMP
        let mut buf = [0u16; 2];
        let encoded = c.encode_utf16(&mut buf);

        for code_unit in encoded.iter().copied() {
            let down = make_unicode_input(code_unit, false);
            let up = make_unicode_input(code_unit, true);

            unsafe {
                let result = SendInput(&[down, up], std::mem::size_of::<INPUT>() as i32);
                if result == 0 {
                    return Err(format!("SendInput failed for character '{}'", c));
                }
            }
        }

        thread::sleep(Duration::from_millis(5));
    }

    Ok(())
}

/// Get the current cursor position in screen coordinates.
pub fn get_cursor_position() -> Result<(f64, f64), String> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut point = POINT::default();
    unsafe {
        GetCursorPos(&mut point).map_err(|e| format!("GetCursorPos failed: {}", e))?;
    }
    Ok((point.x as f64, point.y as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_mapping() {
        assert!(key_name_to_vk("a").is_some());
        assert!(key_name_to_vk("return").is_some());
        assert!(key_name_to_vk("f1").is_some());
        assert!(key_name_to_vk("nonexistent").is_none());
    }

    #[test]
    fn scroll_down_maps_to_negative_wheel_delta() {
        // MOUSEEVENTF_WHEEL: positive mouseData scrolls away from the user (up).
        assert_eq!(scroll_wheel_deltas(0, 2), (-240, 0));
    }

    #[test]
    fn scroll_right_maps_to_positive_hwheel_delta() {
        // MOUSEEVENTF_HWHEEL: positive mouseData tilts to the right.
        assert_eq!(scroll_wheel_deltas(1, 0), (0, 120));
    }

    #[test]
    fn test_absolute_coords() {
        // This just tests that the function doesn't panic
        let (x, y) = to_absolute_coords(100.0, 100.0);
        assert!(x >= 0);
        assert!(y >= 0);
    }

    #[test]
    fn test_get_cursor_position_returns_coordinates() {
        let (x, y) = get_cursor_position().expect("should get cursor position");
        assert!(x.is_finite());
        assert!(y.is_finite());
    }
}
