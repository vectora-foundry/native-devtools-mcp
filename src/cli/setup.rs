use std::io::{self, Write};
use std::path::PathBuf;

use super::{BOLD, DIM, GREEN, RED, RESET, YELLOW};

const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn run() {
    println!();
    println!("{BOLD}native-devtools-mcp v{VERSION} — Setup{RESET}");
    println!("{DIM}Guided setup for permissions and MCP client configuration{RESET}");
    println!();

    #[cfg(target_os = "macos")]
    run_macos();

    #[cfg(target_os = "windows")]
    run_windows();

    configure_mcp_clients();

    println!("{BOLD}Setup complete!{RESET}");
    println!();
}

// ── macOS permission checks ──────────────────────────────────────────

#[cfg(target_os = "macos")]
fn run_macos() {
    println!("{BOLD}Step 1: Permissions{RESET}");
    println!();

    check_macos_permission(
        "Accessibility",
        "This permission lets the AI click, type, scroll, and drag on your behalf.\n    \
         Grant it to the app that runs this server (e.g., Terminal, VS Code, Claude Desktop).",
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        || check_accessibility(true),
        || check_accessibility(false),
    );

    check_macos_permission(
        "Screen Recording",
        "This permission lets the AI take screenshots to see what's on screen.\n    \
         Grant it to the app that runs this server (e.g., Terminal, VS Code, Claude Desktop).",
        "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
        check_screen_recording,
        check_screen_recording,
    );
}

#[cfg(target_os = "macos")]
fn check_macos_permission(
    name: &str,
    explanation: &str,
    prefs_url: &str,
    initial_check: impl FnOnce() -> bool,
    recheck: impl FnOnce() -> bool,
) {
    if initial_check() {
        println!("  {GREEN}✓{RESET} {name}: granted");
        println!();
        return;
    }

    println!("  {RED}✗{RESET} {name}: not granted");
    println!();
    println!("    {explanation}");
    println!();

    let _ = std::process::Command::new("open").arg(prefs_url).status();
    println!("    → System Settings opened to {name}.");
    wait_for_enter("    Press Enter after granting permission...");
    println!();

    if recheck() {
        println!("  {GREEN}✓{RESET} {name}: granted");
    } else {
        println!(
            "  {YELLOW}!{RESET} {name}: still not granted — you may need to restart your terminal."
        );
    }
    println!();
}

#[cfg(target_os = "macos")]
fn check_accessibility(prompt: bool) -> bool {
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::string::CFString;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrustedWithOptions(options: core_foundation::base::CFTypeRef) -> bool;
    }

    let key = CFString::new("AXTrustedCheckOptionPrompt");
    let value = if prompt {
        CFBoolean::true_value()
    } else {
        CFBoolean::false_value()
    };
    let options = CFDictionary::from_CFType_pairs(&[(key.as_CFType(), value.as_CFType())]);

    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as _) }
}

#[cfg(target_os = "macos")]
fn check_screen_recording() -> bool {
    let temp_dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(_) => return false,
    };
    let path = temp_dir.path().join("test.png");

    let output = std::process::Command::new("/usr/sbin/screencapture")
        .args(["-x", "-C", "-t", "png"])
        .arg(&path)
        .output();

    match output {
        Ok(o) if o.status.success() => {
            // If Screen Recording is denied, the file will exist but be tiny/empty
            std::fs::metadata(&path)
                .map(|m| m.len() > 1024)
                .unwrap_or(false)
        }
        _ => false,
    }
}

// ── Windows permission checks ────────────────────────────────────────

#[cfg(target_os = "windows")]
fn run_windows() {
    println!("{BOLD}Step 1: Permissions{RESET}");
    println!();
    println!("  {GREEN}✓{RESET} No special permissions required on Windows.");
    println!("    (Input injection may fail when targeting elevated/admin windows)");
    println!();
}

// ── Shared utilities ─────────────────────────────────────────────────

fn wait_for_enter(prompt: &str) {
    print!("{prompt}");
    let _ = io::stdout().flush();
    let mut buf = String::new();
    let _ = io::stdin().read_line(&mut buf);
}

// ── MCP client configuration ────────────────────────────────────────

fn configure_mcp_clients() {
    println!("{BOLD}Step 2: MCP Client Configuration{RESET}");
    println!();

    let detected = detect_clients();

    if detected.is_empty() {
        println!("  No MCP clients detected.");
        println!();
        print_manual_config();
        return;
    }

    for client in &detected {
        println!("  Found: {BOLD}{}{RESET}", client.name);
        println!("  Config: {DIM}{}{RESET}", client.config_path.display());

        if client.already_configured {
            println!("  {GREEN}✓{RESET} Already configured with native-devtools");
            println!();
            continue;
        }

        println!();
        println!("  Add this to your MCP configuration:");
        println!();
        for line in client.config_snippet.lines() {
            println!("    {DIM}{line}{RESET}");
        }
        println!();

        print!("  Write config automatically? [y/N] ");
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let _ = io::stdin().read_line(&mut answer);

        if answer.trim().eq_ignore_ascii_case("y") {
            match write_client_config(client) {
                Ok(()) => println!("  {GREEN}✓{RESET} Config written successfully."),
                Err(e) => println!("  {RED}✗{RESET} Failed to write config: {e}"),
            }
        } else {
            println!("  Skipped. You can add the config manually later.");
        }
        println!();
    }
}

struct ClientInfo {
    name: &'static str,
    config_path: PathBuf,
    config_snippet: &'static str,
    server_config: serde_json::Value,
    already_configured: bool,
}

/// npx-based server config used by most MCP clients.
fn npx_server_config() -> serde_json::Value {
    serde_json::json!({
        "command": "npx",
        "args": ["-y", "native-devtools-mcp"]
    })
}

const NPX_SNIPPET: &str = r#""native-devtools": {
  "command": "npx",
  "args": ["-y", "native-devtools-mcp"]
}"#;

#[cfg(target_os = "macos")]
const CLAUDE_DESKTOP_SNIPPET: &str = r#""native-devtools": {
  "command": "/Applications/NativeDevtools.app/Contents/MacOS/native-devtools-mcp"
}"#;

#[cfg(target_os = "windows")]
const CLAUDE_DESKTOP_SNIPPET: &str = NPX_SNIPPET;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const CLAUDE_DESKTOP_SNIPPET: &str = NPX_SNIPPET;

fn detect_clients() -> Vec<ClientInfo> {
    let mut clients = Vec::new();
    let home = match home_dir() {
        Some(h) => h,
        None => return clients,
    };

    // Claude Desktop
    #[cfg(target_os = "macos")]
    let claude_desktop_path =
        home.join("Library/Application Support/Claude/claude_desktop_config.json");
    #[cfg(target_os = "windows")]
    let claude_desktop_path = home.join("AppData/Roaming/Claude/claude_desktop_config.json");
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let claude_desktop_path = home.join(".config/Claude/claude_desktop_config.json");

    if claude_desktop_path.exists() {
        #[cfg(target_os = "macos")]
        let server_config = serde_json::json!({
            "command": "/Applications/NativeDevtools.app/Contents/MacOS/native-devtools-mcp"
        });
        #[cfg(not(target_os = "macos"))]
        let server_config = npx_server_config();

        clients.push(ClientInfo {
            name: "Claude Desktop",
            config_path: claude_desktop_path,
            config_snippet: CLAUDE_DESKTOP_SNIPPET,
            server_config,
            already_configured: false, // set below
        });
    }

    // Claude Code
    let claude_code_path = home.join(".claude.json");
    if claude_code_path.exists() {
        clients.push(ClientInfo {
            name: "Claude Code",
            config_path: claude_code_path,
            config_snippet: NPX_SNIPPET,
            server_config: npx_server_config(),
            already_configured: false,
        });
    }

    // Cursor
    let cursor_path = home.join(".cursor/mcp.json");
    if cursor_path.exists() {
        clients.push(ClientInfo {
            name: "Cursor",
            config_path: cursor_path,
            config_snippet: NPX_SNIPPET,
            server_config: npx_server_config(),
            already_configured: false,
        });
    }

    // Check which are already configured
    for client in &mut clients {
        client.already_configured = config_has_native_devtools(&client.config_path);
    }

    clients
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("USERPROFILE").ok().map(PathBuf::from)
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var("HOME").ok().map(PathBuf::from)
    }
}

/// Server name used for the entry that `setup` writes into `mcpServers`.
const SERVER_NAME: &str = "native-devtools";

fn config_has_native_devtools(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
        .is_some_and(|json| has_native_devtools_server(&json))
}

/// Returns `true` if the client config already declares a native-devtools MCP
/// server in its top-level `mcpServers` map — the location `setup` writes to.
///
/// Claude Desktop, Cursor, and Claude Code (user scope) all read top-level
/// `mcpServers`. Claude Code also keeps per-project servers under
/// `projects.<path>.mcpServers`, but those only apply inside that one project,
/// so they do not count as configured. Other keys in `~/.claude.json`
/// (project paths, history) are ignored even if they mention native-devtools.
fn has_native_devtools_server(config: &serde_json::Value) -> bool {
    config
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|servers| {
            servers
                .iter()
                .any(|(name, entry)| name == SERVER_NAME || entry_launches_native_devtools(entry))
        })
}

/// Detects an entry registered under a different name that still launches
/// this server, either by binary path or via an npm package argument.
fn entry_launches_native_devtools(entry: &serde_json::Value) -> bool {
    let is_our_binary = |value: &str| {
        let file_name = value.rsplit(['/', '\\']).next().unwrap_or(value);
        let file_name = file_name.strip_suffix(".exe").unwrap_or(file_name);
        file_name == "native-devtools-mcp"
    };
    let is_our_package =
        |value: &str| value == "native-devtools-mcp" || value.starts_with("native-devtools-mcp@");

    let command_matches = entry
        .get("command")
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_our_binary);
    let args_match = entry
        .get("args")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|args| {
            args.iter()
                .filter_map(serde_json::Value::as_str)
                .any(|arg| is_our_package(arg) || is_our_binary(arg))
        });

    command_matches || args_match
}

fn write_client_config(client: &ClientInfo) -> Result<(), String> {
    let content =
        std::fs::read_to_string(&client.config_path).map_err(|e| format!("read error: {e}"))?;

    let mut json: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("JSON parse error: {e}"))?;

    // Create backup
    let backup_path = client.config_path.with_extension("json.backup");
    std::fs::copy(&client.config_path, &backup_path).map_err(|e| format!("backup failed: {e}"))?;
    println!("  {DIM}Backed up to: {}{RESET}", backup_path.display());

    // Add or merge mcpServers
    let mcp_servers = json
        .as_object_mut()
        .ok_or("config is not a JSON object")?
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));

    mcp_servers
        .as_object_mut()
        .ok_or("mcpServers is not a JSON object")?
        .insert(SERVER_NAME.to_string(), client.server_config.clone());

    // Write back
    let formatted =
        serde_json::to_string_pretty(&json).map_err(|e| format!("JSON serialize error: {e}"))?;
    std::fs::write(&client.config_path, formatted).map_err(|e| format!("write error: {e}"))?;

    Ok(())
}

fn print_manual_config() {
    println!("  To configure manually, add this to your MCP client config:");
    println!();

    #[cfg(target_os = "macos")]
    {
        println!("  For Claude Desktop (macOS, recommended):");
        println!("    {DIM}\"native-devtools\": {{");
        println!("      \"command\": \"/Applications/NativeDevtools.app/Contents/MacOS/native-devtools-mcp\"");
        println!("    }}{RESET}");
        println!();
    }

    println!("  For Claude Code / Cursor / other MCP clients:");
    println!("    {DIM}\"native-devtools\": {{");
    println!("      \"command\": \"npx\",");
    println!("      \"args\": [\"-y\", \"native-devtools-mcp\"]");
    println!("    }}{RESET}");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn claude_code_project_history_mentioning_checkout_is_not_configured() {
        // ~/.claude.json with a project entry for a native-devtools-mcp
        // checkout, but no MCP server registered anywhere.
        let config = json!({
            "numStartups": 12,
            "projects": {
                "/Users/example/src/native-devtools-mcp": {
                    "allowedTools": [],
                    "history": [{ "display": "fix native-devtools setup" }],
                    "mcpServers": {}
                }
            },
            "mcpServers": {
                "other-server": { "command": "npx", "args": ["-y", "other-mcp"] }
            }
        });

        assert!(!has_native_devtools_server(&config));
    }

    #[test]
    fn claude_code_project_scoped_server_is_not_user_configured() {
        let config = json!({
            "projects": {
                "/Users/example/app": {
                    "mcpServers": {
                        "native-devtools": { "command": "npx", "args": ["-y", "native-devtools-mcp"] }
                    }
                }
            }
        });

        assert!(!has_native_devtools_server(&config));
    }

    #[test]
    fn top_level_entry_named_native_devtools_is_configured() {
        let config = json!({
            "mcpServers": {
                "native-devtools": {
                    "command": "/Applications/NativeDevtools.app/Contents/MacOS/native-devtools-mcp"
                }
            }
        });

        assert!(has_native_devtools_server(&config));
    }

    #[test]
    fn entry_under_other_name_running_npx_package_is_configured() {
        let config = json!({
            "mcpServers": {
                "desktop": { "command": "npx", "args": ["-y", "native-devtools-mcp@0.9.0"] }
            }
        });

        assert!(has_native_devtools_server(&config));
    }

    #[test]
    fn entry_under_other_name_running_windows_binary_is_configured() {
        let config = json!({
            "mcpServers": {
                "desktop": { "command": "C:\\Tools\\native-devtools-mcp.exe" }
            }
        });

        assert!(has_native_devtools_server(&config));
    }

    #[test]
    fn similarly_named_package_is_not_configured() {
        let config = json!({
            "mcpServers": {
                "devtools": { "command": "npx", "args": ["-y", "native-devtools-mcp-proxy"] }
            }
        });

        assert!(!has_native_devtools_server(&config));
    }

    #[test]
    fn config_without_mcp_servers_is_not_configured() {
        assert!(!has_native_devtools_server(&json!({ "theme": "dark" })));
        assert!(!has_native_devtools_server(&json!([])));
    }
}
