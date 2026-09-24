//! Chrome DevTools Protocol (CDP) client for browser automation.
//!
//! Connects to Chrome/Electron apps via their remote debugging port
//! using the chromiumoxide crate.

pub mod dom_discovery;
pub mod tools;

use chromiumoxide::browser::Browser;
use chromiumoxide::handler::HandlerConfig;
use chromiumoxide::page::Page;
use futures_util::StreamExt;
use rmcp::model::{CallToolResult, Content};
use std::collections::HashMap;
use tokio::task::JoinHandle;

pub const DOM_UID_PREFIX: &str = "d";

/// How long the chromiumoxide handler keeps a CDP request pending before it
/// fails it with `CdpError::Timeout`.
///
/// chromiumoxide defaults to 30 s. That is shorter than the longest wait a
/// tool may ask the page to run (`cdp_wait_for_page_change` accepts up to
/// 55 s), so the transport would cut such calls off before the tool's own
/// timeout fires. 65 s keeps a 10 s margin above the longest tool wait for
/// the JS timer to fire and the result to travel back.
///
/// Only calls sent through `Page::evaluate_expression` /
/// `Page::evaluate_function` use this limit. `Page::execute` arms its own
/// fixed 30 s timer that ignores `HandlerConfig::request_timeout`, so any
/// call that can await a long page promise must use the evaluate APIs (see
/// `tools::script`).
pub const CDP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(65);

/// CDP client state, owned by the MCP server.
pub struct CdpClient {
    pub browser: Browser,
    pub selected_page: Option<Page>,
    pub handler_handle: JoinHandle<()>,
    pub last_dom_snapshot: Option<SnapshotMap>,
    pub last_page_list: Vec<Page>,
    /// Monotonic counter bumped on every page-lifecycle event that could
    /// invalidate the `backendNodeId` space (navigate, reload, select/new/close
    /// page). Stamped onto each [`SnapshotMap`] at creation time so lookups
    /// can detect stale snapshots even when the page URL hasn't changed
    /// (same-URL reload, SPA pushState/replaceState, switching to another tab
    /// with an identical URL).
    pub generation: u64,
}

impl CdpClient {
    /// Connect to a Chrome/Electron instance via its remote debugging port.
    ///
    /// Resolves the WebSocket URL from `http://127.0.0.1:{port}`, spawns the
    /// chromiumoxide handler loop, and auto-selects the first non-extension page.
    pub async fn connect(port: u16) -> Result<Self, String> {
        let url = format!("http://127.0.0.1:{}", port);
        let handler_config = HandlerConfig {
            request_timeout: CDP_REQUEST_TIMEOUT,
            ..HandlerConfig::default()
        };
        let (mut browser, mut handler) = Browser::connect_with_config(&url, handler_config)
            .await
            .map_err(|e| format!("Cannot connect to port {}. Is the app running with --remote-debugging-port? Error: {}", port, e))?;

        let handler_handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

        // Discover pre-existing targets (pages opened before we connected).
        // Chrome 136+ with chromiumoxide: fetch_targets() queues discovery but
        // Page objects are NOT guaranteed to be ready when it returns. We must
        // poll until at least one real page appears or we time out.
        let selected_page = poll_for_page(&mut browser, std::time::Duration::from_secs(10)).await?;

        Ok(Self {
            browser,
            selected_page,
            handler_handle,
            last_dom_snapshot: None,
            last_page_list: Vec::new(),
            generation: 0,
        })
    }

    /// Disconnect from the browser by aborting the handler task.
    pub fn disconnect(self) {
        self.handler_handle.abort();
    }

    /// Mark the current `backendNodeId` space as invalidated.
    ///
    /// Bumps [`Self::generation`] and clears the DOM snapshot cache. Call
    /// after any navigation, reload, or page switch that invalidates
    /// element UIDs.
    pub fn invalidate_snapshots(&mut self) {
        self.last_dom_snapshot = None;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Get the selected page, or return a tool error.
    pub fn require_page(&self) -> Result<Page, CallToolResult> {
        self.selected_page.clone().ok_or_else(|| {
            cdp_error("No page selected. Use cdp_list_pages and cdp_select_page first.")
        })
    }
}

/// Convenience helper to get the URL of a page, returning an empty string on failure.
pub async fn page_url(page: &Page) -> String {
    page.url().await.ok().flatten().unwrap_or_default()
}

/// Return true if the URL belongs to a Chrome extension.
pub(crate) fn is_extension_url(url: &str) -> bool {
    url.starts_with("chrome-extension://")
}

/// Find the first non-extension page from a list of pages.
async fn first_non_extension_page(pages: &[Page]) -> Option<Page> {
    for page in pages {
        let url = page_url(page).await;
        if !is_extension_url(&url) {
            return Some(page.clone());
        }
    }
    None
}

/// Discover pre-existing targets and wait for at least one page to appear.
///
/// `fetch_targets()` sends `Target.getTargets` and triggers `AttachToTarget`
/// for each discovered target. The attach is asynchronous — the handler must
/// process the responses before `pages()` can see them. We call `fetch_targets`
/// once, then poll `pages()` until a non-extension page appears or we time out.
async fn poll_for_page(
    browser: &mut Browser,
    timeout: std::time::Duration,
) -> Result<Option<Page>, String> {
    // Kick off target discovery once. This triggers AttachToTarget for each
    // existing target, which the handler processes asynchronously.
    let _ = browser.fetch_targets().await;

    let interval = std::time::Duration::from_millis(100);
    let start = std::time::Instant::now();

    loop {
        let pages = browser
            .pages()
            .await
            .map_err(|e| format!("Failed to list pages: {}", e))?;

        if let Some(page) = first_non_extension_page(&pages).await {
            return Ok(Some(page));
        }

        if start.elapsed() >= timeout {
            return Ok(None);
        }

        tokio::time::sleep(interval).await;
    }
}

/// Shorthand for building a CDP tool error result.
pub fn cdp_error(msg: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg.into())])
}

/// Maps snapshot UIDs to CDP node identifiers for click/eval resolution.
///
/// Stale-snapshot detection uses two signals:
/// - `generation`, bumped on every page-lifecycle event the client drives
///   (navigate, reload, page switch) — catches same-URL reloads and SPA
///   navigations that don't change the URL.
/// - `page_url`, compared against the live page URL at lookup time —
///   catches out-of-band navigations (user clicks a link, JS `location.href`)
///   that happen between our tool calls.
///
/// Either signal mismatching is enough to reject the snapshot as stale.
pub struct SnapshotMap {
    pub uid_to_node: HashMap<String, SnapshotNode>,
    /// Rich candidate metadata keyed by d-prefixed UID. This is used by
    /// bounded expansion tools without requiring a fresh page-wide dump.
    pub uid_to_candidate: HashMap<String, dom_discovery::DomCandidate>,
    /// Reverse map: backendNodeId → list of snapshot UIDs.
    /// Skips entries where backendNodeId is 0 (no DOM backing).
    pub backend_to_uids: HashMap<i64, Vec<String>>,
    /// Snapshot order, matching the order UIDs were assigned in the response.
    pub ordered_uids: Vec<String>,
    /// URL of the page at the moment this snapshot was taken.
    pub page_url: String,
    /// Value of [`CdpClient::generation`] at the moment this snapshot was taken.
    pub generation: u64,
}

pub struct SnapshotNode {
    pub backend_node_id: i64,
    pub role: String,
    pub name: String,
}

/// Resolve a `d<N>`-prefixed UID to its SnapshotNode from the DOM map.
///
/// Errors when the prefix isn't `d`, the snapshot is missing or stale
/// (generation bumped or the live page URL changed out-of-band), or the
/// UID isn't present.
pub fn resolve_uid_from_maps<'a>(
    uid: &str,
    dom_snapshot: Option<&'a SnapshotMap>,
    current_generation: u64,
    current_url: &str,
) -> Result<&'a SnapshotNode, String> {
    if !uid.starts_with(DOM_UID_PREFIX) {
        return Err(format!(
            "Unknown UID prefix in '{}'. Expected 'd<N>' (DOM).",
            uid
        ));
    }

    let snapshot = dom_snapshot.ok_or(
        "No DOM snapshot available. Call cdp_take_dom_snapshot or cdp_find_elements first.",
    )?;

    if current_generation != snapshot.generation || current_url != snapshot.page_url {
        return Err(
            "Snapshot is stale — page has navigated since last snapshot. \
             Call cdp_take_dom_snapshot or cdp_find_elements again."
                .to_string(),
        );
    }

    snapshot.uid_to_node.get(uid).ok_or_else(|| {
        format!(
            "uid={} not found in DOM snapshot. Take a fresh snapshot.",
            uid
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://example.com/";

    fn make_dom_map(generation: u64, uid: &str, backend_node_id: i64) -> SnapshotMap {
        let mut map = SnapshotMap {
            uid_to_node: HashMap::new(),
            uid_to_candidate: HashMap::new(),
            backend_to_uids: HashMap::new(),
            ordered_uids: vec![uid.to_string()],
            page_url: URL.to_string(),
            generation,
        };
        map.uid_to_node.insert(
            uid.to_string(),
            SnapshotNode {
                backend_node_id,
                role: "button".to_string(),
                name: "Submit".to_string(),
            },
        );
        map
    }

    #[test]
    fn resolve_uid_dom_prefix() {
        let dom_map = make_dom_map(3, "d5", 99);

        let result = resolve_uid_from_maps("d5", Some(&dom_map), 3, URL);
        assert!(result.is_ok());
        let node = result.unwrap();
        assert_eq!(node.backend_node_id, 99);
    }

    #[test]
    fn resolve_uid_unknown_prefix_fails() {
        for uid in ["x1", "a1"] {
            match resolve_uid_from_maps(uid, None, 0, URL) {
                Err(msg) => assert!(
                    msg.contains("Unknown UID prefix"),
                    "uid={} got: {}",
                    uid,
                    msg
                ),
                Ok(_) => panic!("expected unknown-prefix error for uid={}", uid),
            }
        }
    }

    fn expect_stale(result: Result<&SnapshotNode, String>) {
        match result {
            Err(msg) => assert!(msg.contains("stale"), "expected stale error, got: {}", msg),
            Ok(_) => panic!("expected stale-snapshot error, got Ok"),
        }
    }

    #[test]
    fn resolve_uid_stale_generation_fails() {
        let dom_map = make_dom_map(1, "d1", 1);

        expect_stale(resolve_uid_from_maps("d1", Some(&dom_map), 2, URL));
    }

    /// Same-URL reload bumps the generation, so a snapshot taken before
    /// the reload must be rejected even though `page.url()` hasn't changed.
    #[test]
    fn same_url_reload_invalidates_snapshot() {
        let dom_map = make_dom_map(0, "d1", 42);

        expect_stale(resolve_uid_from_maps("d1", Some(&dom_map), 1, URL));
    }

    /// An out-of-band navigation (user clicks a link, `location.href = ...`)
    /// changes the live URL without bumping our generation. The snapshot
    /// must still be rejected.
    #[test]
    fn out_of_band_url_change_invalidates_snapshot() {
        let dom_map = make_dom_map(0, "d1", 42);

        expect_stale(resolve_uid_from_maps(
            "d1",
            Some(&dom_map),
            0,
            "https://example.com/different",
        ));
    }

    /// A snapshot looked up at its stamped generation and URL succeeds;
    /// bumping the generation causes the same snapshot to be rejected.
    #[test]
    fn snapshot_taken_before_navigation_is_stale_after_bump() {
        let dom_map = make_dom_map(0, "d1", 42);

        assert!(resolve_uid_from_maps("d1", Some(&dom_map), 0, URL).is_ok());

        expect_stale(resolve_uid_from_maps("d1", Some(&dom_map), 1, URL));
    }
}
