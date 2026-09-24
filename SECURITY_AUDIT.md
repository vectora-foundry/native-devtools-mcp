# Security Audit Guide

native-devtools-mcp requires sensitive system permissions to function. This document explains what permissions are used, why, and how to verify you're running trusted code.

## Permissions Used

### macOS

| Permission | Used For | Tools That Need It |
|------------|----------|-------------------|
| **Accessibility** | Simulating mouse clicks, keyboard input, scrolling, dragging | `click`, `type_text`, `press_key`, `scroll`, `drag`, `move_mouse` |
| **Screen Recording** | Taking screenshots to see what's on screen | `take_screenshot` |

### Windows

No special permissions are required. Input injection may fail when targeting elevated (administrator) windows from a non-elevated process.

## How Permissions Are Used in Code

- **Accessibility check**: `src/macos/input.rs` — `check_accessibility_permission()` calls `AXIsProcessTrustedWithOptions`
- **Screen capture**: `src/macos/screenshot.rs` — uses `/usr/sbin/screencapture` (system binary)
- **Input simulation**: `src/macos/input.rs` — uses Core Graphics `CGEvent` API
- **Permission guard**: `src/tools/input.rs` — `check_permission()` runs before every input operation

The server does NOT:
- Scan or index your files (the only file read is `load_image`, which reads a path explicitly provided by the MCP client)
- Make unsolicited network requests (`app_connect` opens a WebSocket to a local debug server when explicitly invoked by the MCP client; `verify` subcommand fetches checksums from GitHub)
- Store or transmit screenshots (they're returned to the MCP client via stdout)
- Run in the background after the MCP client disconnects

## Verifying Binary Integrity

### Option 1: Verify a pre-built binary

```bash
native-devtools-mcp verify
```

This computes the SHA-256 hash of the running binary and compares it against the official checksums published on the [GitHub Releases](https://github.com/vectora-foundry/native-devtools-mcp/releases) page.

### Option 2: Build from source

```bash
curl -fsSL https://raw.githubusercontent.com/vectora-foundry/native-devtools-mcp/master/scripts/build-from-source.sh | bash
```

Or clone and build manually:

```bash
git clone https://github.com/vectora-foundry/native-devtools-mcp.git
cd native-devtools-mcp
cargo build --release
./target/release/native-devtools-mcp setup
```

## AI-Assisted Security Audit

You can use any LLM to audit this codebase. Here's a prompt you can use:

> I want you to perform a thorough security audit of the native-devtools-mcp codebase (https://github.com/vectora-foundry/native-devtools-mcp). This is an MCP server that requires macOS Accessibility and Screen Recording permissions.
>
> Please analyze:
> 1. All system permission usage — are Accessibility and Screen Recording used only for their stated purposes?
> 2. Network activity — does the binary make any outbound connections during normal MCP server operation?
> 3. File system access — does it read/write files outside of temporary screenshot storage?
> 4. Input handling — could a malicious MCP client cause the server to perform unintended actions?
> 5. Dependencies — are there any suspicious or unnecessary dependencies?
> 6. Build process — does the CI/CD pipeline (`.github/workflows/release.yml`) produce deterministic, verifiable builds?
>
> Focus on the `src/` directory, particularly `src/macos/input.rs`, `src/macos/screenshot.rs`, `src/tools/input.rs`, and `src/server.rs`.
