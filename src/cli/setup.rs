use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::install_source::{self, InstallSource};
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

    let install = InstallContext::detect();
    let detected = detect_clients(&install);

    if detected.is_empty() {
        println!("  No MCP clients detected.");
        println!();
        print_manual_config(&install);
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

        let server_config = match &client.server_config {
            Ok(server_config) => server_config,
            Err(reason) => {
                println!("  {YELLOW}!{RESET} {reason}");
                println!();
                continue;
            }
        };

        println!();
        println!("  Add this to your MCP configuration:");
        println!();
        for line in config_snippet(server_config).lines() {
            println!("    {DIM}{line}{RESET}");
        }
        println!();

        print!("  Write config automatically? [y/N] ");
        let _ = io::stdout().flush();
        let mut answer = String::new();
        let _ = io::stdin().read_line(&mut answer);

        if answer.trim().eq_ignore_ascii_case("y") {
            match write_client_config(&client.config_path, server_config) {
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
    /// Server entry to write, or why setup cannot write a working one.
    server_config: Result<serde_json::Value, &'static str>,
    already_configured: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientKind {
    ClaudeDesktop,
    ClaudeCode,
    Cursor,
}

/// How an MCP client should launch the server.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerLaunch {
    /// `npx -y native-devtools-mcp` — resolves the npm platform package.
    Npx,
    /// Run this binary directly.
    Executable(PathBuf),
}

impl ServerLaunch {
    fn to_server_config(&self) -> serde_json::Value {
        match self {
            ServerLaunch::Npx => serde_json::json!({
                "command": "npx",
                "args": ["-y", "native-devtools-mcp"]
            }),
            ServerLaunch::Executable(path) => serde_json::json!({
                "command": path.to_string_lossy()
            }),
        }
    }
}

/// Where the DMG installs the signed app bundle's binary.
const MAC_APP_BINARY: &str = "/Applications/NativeDevtools.app/Contents/MacOS/native-devtools-mcp";

const CLAUDE_DESKTOP_NEEDS_APP_BUNDLE: &str =
    "Claude Desktop on macOS needs the signed app bundle (Gatekeeper blocks npx).\n    \
     Download NativeDevtools-X.X.X.dmg from GitHub Releases, drag it to /Applications,\n    \
     then run setup again.";

/// What setup knows about the running binary and the machine.
struct InstallContext {
    exe_path: PathBuf,
    source: InstallSource,
    /// The DMG app binary, if it is installed.
    installed_app_binary: Option<PathBuf>,
    is_macos: bool,
}

impl InstallContext {
    fn detect() -> Self {
        let (exe_path, source) = install_source::detect_current().unwrap_or_else(|_| {
            // Unreachable in practice; fall back to a PATH lookup.
            (PathBuf::from("native-devtools-mcp"), InstallSource::Unknown)
        });
        let is_macos = cfg!(target_os = "macos");
        let app_binary = PathBuf::from(MAC_APP_BINARY);
        let installed_app_binary = (is_macos && app_binary.is_file()).then_some(app_binary);
        Self {
            exe_path,
            source,
            installed_app_binary,
            is_macos,
        }
    }

    fn server_config_for(&self, client: ClientKind) -> Result<serde_json::Value, &'static str> {
        resolve_server_launch(
            client,
            self.source,
            &self.exe_path,
            self.installed_app_binary.as_deref(),
            self.is_macos,
        )
        .map(|launch| launch.to_server_config())
    }
}

/// Picks the launch command for `client`, based on how the user installed
/// the server.
///
/// - Claude Desktop on macOS runs the signed app bundle: Gatekeeper blocks
///   the npx binary there, and the bundle keeps a stable identity for the
///   Accessibility / Screen Recording (TCC) grants. The bundle is used when
///   setup runs from it or when it is installed in `/Applications`. A
///   locally built binary (cargo/source/archive) runs by absolute path. An
///   npm-only install has no working option, so this returns an error.
/// - Other clients use npx for npm installs (matches the npm workflow and
///   picks up updates), and the running binary's absolute path otherwise,
///   so cargo and source users do not need Node.js or a published package.
fn resolve_server_launch(
    client: ClientKind,
    source: InstallSource,
    exe_path: &Path,
    installed_app_binary: Option<&Path>,
    is_macos: bool,
) -> Result<ServerLaunch, &'static str> {
    let executable = || ServerLaunch::Executable(exe_path.to_path_buf());

    if client == ClientKind::ClaudeDesktop && is_macos {
        return match (source, installed_app_binary) {
            (InstallSource::MacAppBundle, _) => Ok(executable()),
            (_, Some(app_binary)) => Ok(ServerLaunch::Executable(app_binary.to_path_buf())),
            (InstallSource::NpmPackage, None) => Err(CLAUDE_DESKTOP_NEEDS_APP_BUNDLE),
            (_, None) => Ok(executable()),
        };
    }

    match source {
        InstallSource::NpmPackage => Ok(ServerLaunch::Npx),
        _ => Ok(executable()),
    }
}

/// Renders the `"native-devtools": { ... }` snippet shown to the user.
fn config_snippet(server_config: &serde_json::Value) -> String {
    let body = serde_json::to_string_pretty(server_config).unwrap_or_default();
    format!("\"{SERVER_NAME}\": {body}")
}

fn detect_clients(install: &InstallContext) -> Vec<ClientInfo> {
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

    let candidates = [
        (
            "Claude Desktop",
            ClientKind::ClaudeDesktop,
            claude_desktop_path,
        ),
        (
            "Claude Code",
            ClientKind::ClaudeCode,
            home.join(".claude.json"),
        ),
        ("Cursor", ClientKind::Cursor, home.join(".cursor/mcp.json")),
    ];

    for (name, kind, config_path) in candidates {
        if !config_path.exists() {
            continue;
        }
        clients.push(ClientInfo {
            name,
            already_configured: config_has_native_devtools(&config_path),
            server_config: install.server_config_for(kind),
            config_path,
        });
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

fn write_client_config(
    config_path: &Path,
    server_config: &serde_json::Value,
) -> Result<(), String> {
    let content = std::fs::read_to_string(config_path).map_err(|e| format!("read error: {e}"))?;

    let mut json: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| format!("JSON parse error: {e}"))?;

    // Create backup
    let backup_path = config_path.with_extension("json.backup");
    std::fs::copy(config_path, &backup_path).map_err(|e| format!("backup failed: {e}"))?;
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
        .insert(SERVER_NAME.to_string(), server_config.clone());

    // Write back
    let formatted =
        serde_json::to_string_pretty(&json).map_err(|e| format!("JSON serialize error: {e}"))?;
    std::fs::write(config_path, formatted).map_err(|e| format!("write error: {e}"))?;

    Ok(())
}

fn print_manual_config(install: &InstallContext) {
    println!("  To configure manually, add this to your MCP client config:");
    println!();

    let print_snippet = |server_config: &serde_json::Value| {
        for line in config_snippet(server_config).lines() {
            println!("    {DIM}{line}{RESET}");
        }
        println!();
    };

    if install.is_macos {
        println!("  For Claude Desktop (macOS):");
        match install.server_config_for(ClientKind::ClaudeDesktop) {
            Ok(server_config) => print_snippet(&server_config),
            Err(reason) => {
                println!("    {reason}");
                println!();
            }
        }
    }

    println!("  For Claude Code / Cursor / other MCP clients:");
    if let Ok(server_config) = install.server_config_for(ClientKind::ClaudeCode) {
        print_snippet(&server_config);
    }
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

    const APP_BINARY: &str = "/Applications/NativeDevtools.app/Contents/MacOS/native-devtools-mcp";
    const NPX_EXE: &str = "/Users/example/.npm/_npx/0a1b2c/node_modules/@sh3ll3x3c/native-devtools-mcp-darwin-arm64/bin/native-devtools-mcp";
    const CARGO_EXE: &str = "/Users/example/.cargo/bin/native-devtools-mcp";

    fn exe(path: &str) -> ServerLaunch {
        ServerLaunch::Executable(PathBuf::from(path))
    }

    #[test]
    fn claude_desktop_mac_npx_run_with_app_installed_uses_app_bundle() {
        let launch = resolve_server_launch(
            ClientKind::ClaudeDesktop,
            InstallSource::NpmPackage,
            Path::new(NPX_EXE),
            Some(Path::new(APP_BINARY)),
            true,
        );
        assert_eq!(launch, Ok(exe(APP_BINARY)));
    }

    #[test]
    fn claude_desktop_mac_npx_run_without_app_refuses_to_write() {
        let launch = resolve_server_launch(
            ClientKind::ClaudeDesktop,
            InstallSource::NpmPackage,
            Path::new(NPX_EXE),
            None,
            true,
        );
        assert_eq!(launch, Err(CLAUDE_DESKTOP_NEEDS_APP_BUNDLE));
    }

    #[test]
    fn claude_desktop_mac_cargo_install_without_app_uses_cargo_binary() {
        let launch = resolve_server_launch(
            ClientKind::ClaudeDesktop,
            InstallSource::CargoInstall,
            Path::new(CARGO_EXE),
            None,
            true,
        );
        assert_eq!(launch, Ok(exe(CARGO_EXE)));
    }

    #[test]
    fn claude_desktop_mac_run_from_app_bundle_uses_that_bundle() {
        let bundle_in_downloads =
            "/Users/example/Downloads/NativeDevtools.app/Contents/MacOS/native-devtools-mcp";
        let launch = resolve_server_launch(
            ClientKind::ClaudeDesktop,
            InstallSource::MacAppBundle,
            Path::new(bundle_in_downloads),
            None,
            true,
        );
        assert_eq!(launch, Ok(exe(bundle_in_downloads)));
    }

    #[test]
    fn claude_desktop_windows_npm_install_uses_npx() {
        let launch = resolve_server_launch(
            ClientKind::ClaudeDesktop,
            InstallSource::NpmPackage,
            Path::new("C:/Users/example/AppData/Roaming/npm/node_modules/@sh3ll3x3c/native-devtools-mcp-win32-x64/bin/native-devtools-mcp.exe"),
            None,
            false,
        );
        assert_eq!(launch, Ok(ServerLaunch::Npx));
    }

    #[test]
    fn claude_code_npm_install_uses_npx_even_with_app_installed() {
        let launch = resolve_server_launch(
            ClientKind::ClaudeCode,
            InstallSource::NpmPackage,
            Path::new(NPX_EXE),
            Some(Path::new(APP_BINARY)),
            true,
        );
        assert_eq!(launch, Ok(ServerLaunch::Npx));
    }

    #[test]
    fn cursor_source_build_uses_built_binary() {
        let built = "/Users/example/src/native-devtools-mcp/target/release/native-devtools-mcp";
        let launch = resolve_server_launch(
            ClientKind::Cursor,
            InstallSource::SourceBuild,
            Path::new(built),
            None,
            true,
        );
        assert_eq!(launch, Ok(exe(built)));
    }

    #[test]
    fn executable_launch_writes_command_without_args() {
        let config = exe(CARGO_EXE).to_server_config();
        assert_eq!(config, json!({ "command": CARGO_EXE }));
    }

    #[test]
    fn npx_launch_writes_npx_with_package_args() {
        let config = ServerLaunch::Npx.to_server_config();
        assert_eq!(
            config,
            json!({ "command": "npx", "args": ["-y", "native-devtools-mcp"] })
        );
    }
}
