//! End-to-end MCP protocol tests over stdio.
//!
//! These tests spawn the real server binary and talk newline-delimited
//! JSON-RPC to it, the same way an MCP client does. They cover the parts of
//! the protocol that the MCP SDK (rmcp) owns: the `initialize` handshake and
//! version negotiation, `tools/list` serialization, tool results vs protocol
//! errors in `tools/call`, and `notifications/tools/list_changed`.
//!
//! Every tool used here is side-effect free and needs no GUI permissions, no
//! Android device, and no browser, so the tests run on plain CI runners.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const NOTIFICATION_GRACE: Duration = Duration::from_secs(2);
const LIST_CHANGED: &str = "notifications/tools/list_changed";
/// The protocol version the server answers every `initialize` with.
const SERVER_PROTOCOL_VERSION: &str = "2024-11-05";

/// A minimal MCP client that drives the server binary over stdio.
struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    notifications: Vec<Value>,
    next_id: u64,
}

impl StdioClient {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_native-devtools-mcp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn server binary");
        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let (sender, messages) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("server wrote non-JSON line {line:?}: {e}"));
                if sender.send(message).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            messages,
            notifications: Vec::new(),
            next_id: 1,
        }
    }

    fn send(&mut self, message: &Value) {
        writeln!(self.stdin, "{message}").expect("write to server stdin");
        self.stdin.flush().expect("flush server stdin");
    }

    /// Send a request and wait for the response with the same id. Messages
    /// without an id (notifications) that arrive meanwhile are recorded.
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let message = match self.messages.recv_timeout(remaining) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout) => panic!("no response to {method} (id {id})"),
                Err(RecvTimeoutError::Disconnected) => {
                    panic!("server closed stdout before answering {method} (id {id})")
                }
            };
            if message.get("id") == Some(&json!(id)) {
                return message;
            }
            if message.get("id").is_none() {
                self.notifications.push(message);
            }
        }
    }

    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": arguments}))
    }

    fn initialize(&mut self, protocol_version: &str) -> Value {
        let response = self.request(
            "initialize",
            json!({
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": "stdio-protocol-test", "version": "0.0.0"},
            }),
        );
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        response
    }

    fn list_tool_names(&mut self) -> Vec<String> {
        let response = self.request("tools/list", json!({}));
        let mut names: Vec<String> = result(&response)["tools"]
            .as_array()
            .expect("tools/list result has a tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name").to_string())
            .collect();
        names.sort();
        names
    }

    /// Remove and count the `list_changed` notifications seen so far, waiting
    /// briefly for late ones if none has arrived yet.
    fn take_list_changed_count(&mut self) -> usize {
        let deadline = Instant::now() + NOTIFICATION_GRACE;
        while !self.notifications.iter().any(is_list_changed) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(message) if message.get("id").is_none() => self.notifications.push(message),
                Ok(other) => panic!("unexpected response without a request: {other}"),
                Err(_) => break,
            }
        }
        let count = self
            .notifications
            .iter()
            .filter(|n| is_list_changed(n))
            .count();
        self.notifications.retain(|n| !is_list_changed(n));
        count
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn is_list_changed(message: &Value) -> bool {
    message["method"] == LIST_CHANGED
}

fn result(response: &Value) -> &Value {
    assert!(
        response.get("error").is_none(),
        "expected a result, got error: {response}"
    );
    &response["result"]
}

fn first_text(tool_result: &Value) -> &str {
    tool_result["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("tool result has no text content: {tool_result}"))
}

fn initialized_client(protocol_version: &str) -> StdioClient {
    let mut client = StdioClient::spawn();
    let response = client.initialize(protocol_version);
    assert_eq!(
        result(&response)["protocolVersion"],
        SERVER_PROTOCOL_VERSION,
        "client requested {protocol_version}"
    );
    client
}

/// Run `check` once per client protocol version the server must accept.
fn for_each_client_version(check: impl Fn(&str)) {
    for version in ["2024-11-05", "2025-06-18", "2025-11-25"] {
        check(version);
    }
}

#[test]
fn initialize_advertises_server_identity_and_tool_list_changed() {
    for_each_client_version(|version| {
        let mut client = StdioClient::spawn();
        let response = client.initialize(version);
        let init = result(&response);

        assert_eq!(init["protocolVersion"], SERVER_PROTOCOL_VERSION);
        assert_eq!(init["serverInfo"]["name"], "native-devtools-mcp");
        assert_eq!(init["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(init["capabilities"]["tools"]["listChanged"], true);
        assert!(
            init["instructions"]
                .as_str()
                .is_some_and(|text| text.starts_with("Native DevTools MCP server")),
            "unexpected instructions: {}",
            init["instructions"]
        );
    });
}

#[test]
fn tools_list_gates_connected_only_tools_and_sends_schema_and_annotations() {
    for_each_client_version(|version| {
        let mut client = initialized_client(version);
        let response = client.request("tools/list", json!({}));
        let tools = result(&response)["tools"].as_array().expect("tools array");

        let wire_names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        // Always listed: core tools, the connect entry points, and every CDP tool.
        for always_listed in [
            "take_screenshot",
            "click",
            "scroll",
            "find_text",
            "app_connect",
            "android_list_devices",
            "android_connect",
            "cdp_connect",
            "cdp_evaluate_script",
            "cdp_wait_for_page_change",
        ] {
            assert!(
                wire_names.contains(&always_listed),
                "{always_listed} missing from tools/list"
            );
        }
        // Listed only after the matching connect call succeeds.
        for gated in ["android_click", "android_screenshot", "app_get_tree"] {
            assert!(
                !wire_names.contains(&gated),
                "{gated} listed before connecting"
            );
        }

        let take_screenshot = tools
            .iter()
            .find(|t| t["name"] == "take_screenshot")
            .expect("take_screenshot is listed");
        assert_eq!(take_screenshot["inputSchema"]["type"], "object");
        assert_eq!(take_screenshot["annotations"]["readOnlyHint"], true);
        assert_eq!(take_screenshot["annotations"]["destructiveHint"], false);

        // Tools gated on a live session must not be listed before it exists.
        for gated in ["android_screenshot", "get_hover_events", "stop_recording"] {
            assert!(
                !wire_names.contains(&gated),
                "{gated} listed without a session"
            );
        }
    });
}

#[cfg(feature = "cdp")]
#[test]
fn cdp_connect_to_closed_port_returns_tool_error() {
    for_each_client_version(|version| {
        let mut client = initialized_client(version);
        let response = client.request("tools/list", json!({}));
        let cdp_connect = result(&response)["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .find(|t| t["name"] == "cdp_connect")
            .expect("cdp_connect is listed")
            .clone();
        assert_eq!(cdp_connect["inputSchema"]["required"], json!(["port"]));
        assert_eq!(
            cdp_connect["inputSchema"]["properties"]["port"]["type"],
            "integer"
        );

        // Port 1 (tcpmux) is never a Chrome debug port on a dev or CI machine.
        let response = client.call_tool("cdp_connect", json!({"port": 1}));
        let tool_result = result(&response);
        assert_eq!(tool_result["isError"], true);
        assert!(
            first_text(tool_result).starts_with("Cannot connect to port 1."),
            "unexpected error text: {tool_result}"
        );
    });
}

#[test]
fn unknown_tool_is_rejected_with_invalid_params_error() {
    for_each_client_version(|version| {
        let mut client = initialized_client(version);
        let response = client.call_tool("no_such_tool", json!({}));

        assert!(response.get("result").is_none(), "got result: {response}");
        assert_eq!(response["error"]["code"], -32602);
        assert_eq!(response["error"]["message"], "Unknown tool: no_such_tool");
    });
}

#[test]
fn missing_required_argument_is_rejected_with_invalid_params_error() {
    for_each_client_version(|version| {
        let mut client = initialized_client(version);
        let response = client.call_tool("android_connect", json!({}));

        assert_eq!(response["error"]["code"], -32602);
        assert_eq!(
            response["error"]["message"],
            "missing required param: serial"
        );
    });
}

#[test]
fn android_tools_report_missing_device_without_notifying() {
    for_each_client_version(|version| {
        let mut client = initialized_client(version);
        let response = client.call_tool("android_disconnect", json!({}));
        let tool_result = result(&response);

        assert_eq!(tool_result["isError"], true);
        assert_eq!(first_text(tool_result), "No Android device connected.");
        assert_eq!(client.take_list_changed_count(), 0);
    });
}

#[test]
fn hover_tracking_session_toggles_tools_and_sends_list_changed() {
    for_each_client_version(|version| {
        let mut client = initialized_client(version);
        let before = client.list_tool_names();
        assert!(!before.contains(&"get_hover_events".to_string()));
        assert!(!before.contains(&"stop_hover_tracking".to_string()));

        let response = client.call_tool(
            "start_hover_tracking",
            json!({"poll_interval_ms": 10_000, "max_duration_ms": 60_000}),
        );
        assert_eq!(result(&response)["isError"], false);
        assert_eq!(client.take_list_changed_count(), 1);

        let during = client.list_tool_names();
        let added: Vec<&String> = during.iter().filter(|n| !before.contains(n)).collect();
        assert_eq!(added, ["get_hover_events", "stop_hover_tracking"]);

        let response = client.call_tool("stop_hover_tracking", json!({}));
        assert_eq!(result(&response)["isError"], false);
        assert_eq!(client.take_list_changed_count(), 1);
        assert_eq!(client.list_tool_names(), before);
    });
}
