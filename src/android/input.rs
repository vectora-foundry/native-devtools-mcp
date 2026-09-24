use super::device::AndroidDevice;

pub fn click(device: &mut AndroidDevice, x: f64, y: f64) -> Result<(), String> {
    device.shell_args(&["input", "tap", &x.to_string(), &y.to_string()])?;
    Ok(())
}

pub fn swipe(
    device: &mut AndroidDevice,
    start_x: f64,
    start_y: f64,
    end_x: f64,
    end_y: f64,
    duration_ms: Option<u32>,
) -> Result<(), String> {
    let sx = start_x.to_string();
    let sy = start_y.to_string();
    let ex = end_x.to_string();
    let ey = end_y.to_string();
    let mut cmd = vec!["input", "swipe", &sx, &sy, &ex, &ey];
    let ms_str;
    if let Some(ms) = duration_ms {
        ms_str = ms.to_string();
        cmd.push(&ms_str);
    }
    device.shell_args(&cmd)?;
    Ok(())
}

pub fn type_text(device: &mut AndroidDevice, text: &str) -> Result<(), String> {
    let escaped = escape_for_input(text);
    device.shell_args(&["input", "text", &escaped])?;
    Ok(())
}

pub fn press_key(device: &mut AndroidDevice, key: &str) -> Result<(), String> {
    device.shell_args(&["input", "keyevent", key])?;
    Ok(())
}

/// Encode text for `adb shell input text`, which reads `%s` as a space.
/// Shell quoting is done separately by `AndroidDevice::shell_args`.
///
/// `input text` has no escape for a literal `%s`, so that sequence is always
/// typed as a space.
fn escape_for_input(text: &str) -> String {
    text.replace(' ', "%s")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_spaces() {
        assert_eq!(escape_for_input("hello world"), "hello%sworld");
    }

    #[test]
    fn test_escape_leaves_shell_metacharacters_for_quoting() {
        // Backslash escapes here would be typed literally, because
        // shell_args single-quotes the whole argument.
        assert_eq!(escape_for_input("a&b"), "a&b");
        assert_eq!(escape_for_input("it's"), "it's");
        assert_eq!(escape_for_input("$HOME"), "$HOME");
    }

    #[test]
    fn test_escape_no_special() {
        assert_eq!(escape_for_input("hello123"), "hello123");
    }
}
