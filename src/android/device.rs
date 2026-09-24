use adb_client::server::ADBServer;
use adb_client::server_device::ADBServerDevice;
use adb_client::ADBDeviceExt;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct AndroidDeviceInfo {
    pub serial: String,
    pub state: String,
}

pub struct AndroidDevice {
    pub serial: String,
    device: ADBServerDevice,
}

pub fn list_devices() -> Result<Vec<AndroidDeviceInfo>, String> {
    let mut server = ADBServer::default();
    let devices = server
        .devices()
        .map_err(|e| format!("Failed to list ADB devices: {}", e))?;

    Ok(devices
        .into_iter()
        .map(|d| AndroidDeviceInfo {
            serial: d.identifier,
            state: d.state.to_string(),
        })
        .collect())
}

impl AndroidDevice {
    pub fn connect(serial: &str) -> Result<Self, String> {
        let mut server = ADBServer::default();
        let device = server
            .get_device_by_name(serial)
            .map_err(|e| format!("Failed to connect to device '{}': {}", serial, e))?;

        Ok(Self {
            serial: serial.to_string(),
            device,
        })
    }

    pub fn shell(&mut self, command: &str) -> Result<String, String> {
        let mut output = Vec::new();
        self.device
            .shell_command(&command, Some(&mut output), None)
            .map_err(|e| format!("Shell command failed: {}", e))?;

        String::from_utf8(output).map_err(|e| format!("Shell output is not valid UTF-8: {}", e))
    }

    /// Run a shell command built from separate arguments. Each argument is
    /// quoted, so the device shell sees it as one literal word.
    pub fn shell_args(&mut self, args: &[&str]) -> Result<String, String> {
        self.shell(&join_shell_args(args))
    }

    /// Run a shell command and capture raw bytes output.
    pub fn shell_bytes(&mut self, args: &[&str], output: &mut Vec<u8>) -> Result<(), String> {
        let command = join_shell_args(args);
        self.device
            .shell_command(&command, Some(output), None)
            .map_err(|e| format!("Shell command failed: {}", e))?;
        Ok(())
    }

    pub fn framebuffer_png(&mut self) -> Result<Vec<u8>, String> {
        self.device
            .framebuffer_bytes()
            .map_err(|e| format!("Failed to capture framebuffer: {}", e))
    }
}

/// Quote one argument for the device's POSIX shell. Arguments made only of
/// safe characters are left as-is; anything else is wrapped in single quotes,
/// with embedded single quotes written as `'\''`.
fn quote_shell_arg(arg: &str) -> String {
    let is_safe = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-.,/:=@%+".contains(c));
    if is_safe {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Join arguments into one shell command line, quoting each one.
fn join_shell_args(args: &[&str]) -> String {
    args.iter()
        .map(|arg| quote_shell_arg(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_argument_is_not_quoted() {
        assert_eq!(quote_shell_arg("com.example.app"), "com.example.app");
        assert_eq!(quote_shell_arg("hello%sworld"), "hello%sworld");
    }

    #[test]
    fn argument_with_metacharacters_is_single_quoted() {
        assert_eq!(quote_shell_arg("a;reboot"), "'a;reboot'");
        assert_eq!(quote_shell_arg("$HOME"), "'$HOME'");
        assert_eq!(quote_shell_arg("*"), "'*'");
    }

    #[test]
    fn newline_stays_inside_quotes() {
        assert_eq!(quote_shell_arg("a\nreboot"), "'a\nreboot'");
    }

    #[test]
    fn embedded_single_quote_is_closed_escaped_and_reopened() {
        assert_eq!(quote_shell_arg("it's"), r"'it'\''s'");
    }

    #[test]
    fn empty_argument_becomes_empty_quotes() {
        assert_eq!(quote_shell_arg(""), "''");
    }

    #[cfg(unix)]
    #[test]
    fn posix_shell_receives_each_argument_verbatim() {
        let hostile = [
            "a\nreboot",
            "it's",
            "$(id)",
            "`id`",
            "a;b|c&d",
            "*",
            "#x",
            "\\",
        ];
        for original in hostile {
            let command = format!("printf %s {}", quote_shell_arg(original));
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .output()
                .expect("sh should run");
            assert_eq!(String::from_utf8_lossy(&output.stdout), original);
        }
    }

    #[test]
    fn join_quotes_each_argument_separately() {
        assert_eq!(
            join_shell_args(&["input", "text", "a b;c"]),
            "input text 'a b;c'"
        );
    }
}
