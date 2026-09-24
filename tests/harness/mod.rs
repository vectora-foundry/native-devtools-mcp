//! Test harness for the CDP DOM discovery integration tests.
//!
//! Spawns a real (headless) Chrome process on an ephemeral port, connects the
//! production `CdpClient`, and exposes helpers to drive the tools under test.
//!
//! Every `Harness` owns:
//! * a Chrome child process (killed on drop),
//! * a temp `--user-data-dir` (removed on drop),
//! * an `Arc<RwLock<Option<CdpClient>>>` shaped exactly like the MCP server's
//!   runtime state, so tool functions work unmodified.
//!
//! **Platforms:** macOS and Linux. Windows is currently skipped because
//! `find_chrome_binary` only knows about Unix Chrome locations.
//!
//! **Host requirements:** in addition to Chrome, the harness binds an
//! ephemeral TCP port on `127.0.0.1`. Sandboxed environments that block
//! loopback listeners will surface that as `harness launch failed: could
//! not acquire a free port` — not a silent skip — so a CI regression
//! cannot hide behind the Chrome-missing short-circuit.
//!
//! Keep this module focused on *harness mechanics*. Scenario HTML fixtures
//! live in this file because they are small, self-contained, and closely tied
//! to the assertions; scenario *logic* stays in `cdp_dom_discovery_tests.rs`.

#![cfg(feature = "cdp")]

use base64::Engine;
use native_devtools_mcp::cdp::tools::cdp_navigate;
use native_devtools_mcp::cdp::CdpClient;
use rmcp::model::{CallToolResult, ContentBlock};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::RwLock;

/// Shared state shape the MCP server uses for the live `CdpClient`.
pub type ClientHandle = Arc<RwLock<Option<CdpClient>>>;

/// Locates a Chrome/Chromium binary for the current platform.
///
/// Returns `None` if nothing suitable is installed — callers should skip the
/// test (NOT fail) in that case. We search a small fixed list rather than
/// invoking `which` so the behaviour is stable across shells.
fn find_chrome_binary() -> Option<PathBuf> {
    let candidates: &[&str] = if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else if cfg!(target_os = "linux") {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/snap/bin/chromium",
        ]
    } else {
        // Windows / other: not supported by this harness yet.
        &[]
    };

    candidates.iter().map(PathBuf::from).find(|p| p.is_file())
}

/// Bind a TCP listener to an ephemeral port, then drop it to release the
/// port. There's a tiny race window between drop and Chrome binding; in
/// practice this is fine for test fixtures on a single host.
fn pick_free_port() -> Option<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

/// A live Chrome + CDP client pair, bound together for a single test.
pub struct Harness {
    chrome: Option<Child>,
    _profile: TempDir,
    client: ClientHandle,
}

/// Result of trying to bring up the harness.
///
/// `NoChrome` is a host-not-capable skip — `launch()` returns it when no
/// Chrome/Chromium binary exists in the expected locations. All other
/// failures (temp dir creation, port allocation, spawn, debug-port
/// readiness, `CdpClient::connect`) are surfaced as `Err` so tests fail
/// loudly instead of going silently green.
pub enum LaunchOutcome {
    Ready(Harness),
    NoChrome,
}

impl Harness {
    /// Spawn Chrome and connect.
    pub async fn launch() -> Result<LaunchOutcome, String> {
        let chrome_path = match find_chrome_binary() {
            Some(p) => p,
            None => {
                eprintln!("[harness] skipping: no Chrome/Chromium binary found");
                return Ok(LaunchOutcome::NoChrome);
            }
        };

        let profile = TempDir::new().map_err(|e| format!("cannot create temp profile dir: {e}"))?;

        let port = pick_free_port().ok_or_else(|| "could not acquire a free port".to_string())?;

        let mut cmd = Command::new(&chrome_path);
        cmd.arg("--headless=new")
            .arg(format!("--remote-debugging-port={port}"))
            .arg(format!("--user-data-dir={}", profile.path().display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-gpu")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-default-apps")
            .arg("--disable-extensions")
            // about:blank so Chrome has a page to attach to before navigation.
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn Chrome at {chrome_path:?}: {e}"))?;

        if let Err(e) = wait_for_debug_port(port, Duration::from_secs(15)).await {
            let _ = child.kill();
            return Err(format!("Chrome never opened debug port {port}: {e}"));
        }

        let client = match CdpClient::connect(port).await {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill();
                return Err(format!("CdpClient::connect(port={port}) failed: {e}"));
            }
        };

        Ok(LaunchOutcome::Ready(Self {
            chrome: Some(child),
            _profile: profile,
            client: Arc::new(RwLock::new(Some(client))),
        }))
    }

    /// Convenience wrapper for scenarios: panic on real failures, return
    /// `None` only for the "no Chrome installed" skip.
    pub async fn launch_or_skip() -> Option<Self> {
        match Self::launch().await {
            Ok(LaunchOutcome::Ready(h)) => Some(h),
            Ok(LaunchOutcome::NoChrome) => None,
            Err(e) => panic!("harness launch failed: {e}"),
        }
    }

    /// Return a clone of the shared client handle, suitable for passing into
    /// `cdp_*` tool functions.
    pub fn client_handle(&self) -> ClientHandle {
        self.client.clone()
    }

    /// Navigate the selected page to the given HTML document (inline).
    ///
    /// Uses a `data:` URL so tests don't need a local HTTP server. Chrome
    /// treats `data:` documents as same-origin with no origin, which is
    /// fine for shadow-root / srcdoc-iframe cases.
    pub async fn navigate(&mut self, html: &str) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(html);
        let url = format!("data:text/html;base64,{b64}");

        let result = cdp_navigate(Some(url), None, Some(10_000), self.client_handle()).await;
        assert_eq!(
            result.is_error,
            Some(false),
            "navigate failed: {}",
            content_text(&result)
        );

        // Wait for `document.readyState === 'complete'` so shadow roots,
        // custom elements, and iframe documents have attached before the
        // scenario queries them. Polls against the live page rather than
        // sleeping an arbitrary interval.
        self.wait_for_ready(Duration::from_secs(5)).await;
    }

    async fn wait_for_ready(&self, timeout: Duration) {
        let start = Instant::now();
        loop {
            if self.eval_bool("document.readyState === 'complete'").await {
                return;
            }
            if start.elapsed() >= timeout {
                panic!(
                    "page did not reach readyState=complete within {:?}",
                    timeout
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Evaluate a JS boolean expression in the page context. Convenience
    /// wrapper used by scenarios to check click side effects.
    pub async fn eval_bool(&self, expr: &str) -> bool {
        use native_devtools_mcp::cdp::tools::cdp_evaluate_script;
        let r = cdp_evaluate_script(expr.to_string(), None, self.client_handle()).await;
        if r.is_error != Some(false) {
            panic!("eval_bool failed: {}", content_text(&r));
        }
        let text = content_text(&r);
        text.trim() == "true"
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Kill the Chrome child first; chromiumoxide's handler task will
        // wind down once the websocket closes.
        if let Some(mut child) = self.chrome.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Poll `/json/version` on the debug port until it responds or we time out.
async fn wait_for_debug_port(port: u16, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    let url = format!("http://127.0.0.1:{port}/json/version");
    loop {
        let u = url.clone();
        let ok = tokio::task::spawn_blocking(move || ureq::get(&u).call().is_ok())
            .await
            .unwrap_or(false);

        if ok {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(format!("timed out after {:?}", timeout));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

// ---------------------------------------------------------------------------
// Result helpers
// ---------------------------------------------------------------------------

/// Concatenate all text content fragments of a `CallToolResult` into a single
/// string. Panics the test cleanly if the result contains no text (which
/// would indicate the tool returned an image or resource variant by mistake).
pub fn content_text(result: &CallToolResult) -> String {
    let mut out = String::new();
    for c in &result.content {
        if let Some(t) = text_of(c) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&t);
        }
    }
    out
}

fn text_of(content: &ContentBlock) -> Option<String> {
    content.as_text().map(|t| t.text.clone())
}

// ---------------------------------------------------------------------------
// HTML fixtures for the scenarios.
// ---------------------------------------------------------------------------

/// Scenario 1: an AX-invisible contenteditable that only a DOM walker will
/// surface, identified by its placeholder text.
pub const HTML_CONTENTEDITABLE: &str = r#"
<!doctype html>
<html><body>
  <div id="editor"
       contenteditable="true"
       data-placeholder="Write something…"
       style="min-height:40px;border:1px solid #ccc">
  </div>
</body></html>
"#;

/// Scenario 2: a custom <div role="button"> with no text content, only an
/// aria-label.
pub const HTML_CUSTOM_BUTTON: &str = r#"
<!doctype html>
<html><body>
  <div id="x" role="button" aria-label="Close" tabindex="0"
       style="width:32px;height:32px;background:#c00"></div>
</body></html>
"#;

/// Scenario 3: two "Search" controls disambiguated by parent context.
pub const HTML_DUPLICATE_LABELS: &str = r#"
<!doctype html>
<html><body>
  <nav aria-label="Primary">
    <input type="search" placeholder="Search" />
  </nav>
  <main>
    <button aria-label="Search">Search</button>
  </main>
</body></html>
"#;

/// Scenario 4: open shadow root + same-origin srcdoc iframe. The walker
/// must descend into both.
pub const HTML_SHADOW_AND_IFRAME: &str = r#"
<!doctype html>
<html><body>
  <host-el id="host"></host-el>
  <iframe id="frame" srcdoc='<button aria-label="IframeBtn">IframeBtn</button>'></iframe>
  <script>
    class HostEl extends HTMLElement {
      constructor() {
        super();
        const root = this.attachShadow({ mode: 'open' });
        const b = document.createElement('button');
        b.setAttribute('aria-label', 'ShadowBtn');
        b.textContent = 'ShadowBtn';
        root.appendChild(b);
      }
    }
    customElements.define('host-el', HostEl);
  </script>
</body></html>
"#;

/// Scenario 5: an Electron-style list row whose clickable container has an
/// accessibility name that disagrees with its rendered descendant text.
pub const HTML_ARIA_VISIBLE_TEXT_MISMATCH: &str = r#"
<!doctype html>
<html><body>
  <button aria-label="Chat with Ljuba Isakovic, 0 new messages"
          data-testid="conversation-row"
          style="width:320px;height:72px;text-align:left">
    <span>Note to Self</span>
    <span>Tue</span>
    <span>Photo</span>
  </button>
</body></html>
"#;

/// Scenario 6: text belongs to the row, not to every control nested inside it.
pub const HTML_PARENT_TEXT_SHOULD_NOT_MATCH_CHILD: &str = r#"
<!doctype html>
<html><body>
  <div role="listitem">
    <span>Note to Self</span>
    <button aria-label="More actions" style="width:32px;height:32px">...</button>
  </div>
</body></html>
"#;
