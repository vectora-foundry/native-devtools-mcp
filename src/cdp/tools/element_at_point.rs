use std::sync::Arc;
use tokio::sync::RwLock;

use chromiumoxide::cdp::browser_protocol::dom::{DescribeNodeParams, GetNodeForLocationParams};
use rmcp::model::{CallToolResult, ContentBlock};

use crate::cdp::{CdpClient, SnapshotMap};

/// Resolve screen coordinates to a CDP snapshot UID from the DOM snapshot.
pub async fn cdp_element_at_point(
    x: f64,
    y: f64,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    let client_guard = cdp_client.read().await;
    let client = match client_guard.as_ref() {
        Some(c) => c,
        None => {
            return CallToolResult::error(vec![ContentBlock::text(
                "No CDP connection. Use cdp_connect first.",
            )])
        }
    };

    let page = match client.require_page() {
        Ok(p) => p,
        Err(e) => return e,
    };

    // Step 1: Query window geometry and scroll offsets.
    let geo = match query_window_geometry(&page).await {
        Ok(g) => g,
        Err(e) => return CallToolResult::error(vec![ContentBlock::text(e)]),
    };

    // Step 2: Convert screen coords to viewport and page coords.
    let chrome_height = geo.outer_height - geo.inner_height;
    let viewport_x = x - geo.screen_x;
    let viewport_y = y - geo.screen_y - chrome_height;

    if viewport_x < 0.0
        || viewport_y < 0.0
        || viewport_x >= geo.inner_width
        || viewport_y >= geo.inner_height
    {
        return CallToolResult::error(vec![ContentBlock::text(format!(
            "Screen point ({}, {}) maps to viewport ({:.0}, {:.0}) which is outside \
             content area ({}x{}). The point may be in the title bar or outside the window.",
            x, y, viewport_x, viewport_y, geo.inner_width, geo.inner_height,
        ))]);
    }

    let page_x = viewport_x + geo.scroll_x;
    let page_y = viewport_y + geo.scroll_y;

    // Step 3: Hit-test via DOM.getNodeForLocation (page coords).
    let backend_node_id = match get_node_for_location(&page, page_x, page_y).await {
        Ok(id) => id,
        Err(_) => {
            // Step 4: Fallback via document.elementFromPoint (viewport coords).
            match element_from_point_fallback(&page, viewport_x, viewport_y).await {
                Ok(id) => id,
                Err(e) => {
                    return CallToolResult::error(vec![ContentBlock::text(format!(
                        "No element found at screen ({}, {}) / viewport ({:.0}, {:.0}): {}",
                        x, y, viewport_x, viewport_y, e,
                    ))]);
                }
            }
        }
    };

    // Step 5: Read-only lookup in the DOM snapshot map.
    let current_url = crate::cdp::page_url(&page).await;
    let note = match lookup_uid(client, backend_node_id, &current_url) {
        LookupResult::Found { uid, role, name } => {
            let json = serde_json::json!({
                "uid": uid,
                "role": role,
                "name": name,
                "backend_node_id": backend_node_id,
            });
            return CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&json).unwrap_or_default(),
            )]);
        }
        LookupResult::Stale => {
            "Snapshot is stale — page has navigated. Call cdp_take_dom_snapshot again."
        }
        LookupResult::NotInSnapshot => {
            "Element not in the DOM snapshot. Call cdp_take_dom_snapshot or \
             cdp_find_elements to get a UID."
        }
    };

    // Not found — return raw backendNodeId without minting a UID.
    let json = serde_json::json!({
        "uid": null,
        "backend_node_id": backend_node_id,
        "note": note,
    });
    CallToolResult::success(vec![ContentBlock::text(
        serde_json::to_string_pretty(&json).unwrap_or_default(),
    )])
}

enum LookupResult {
    Found {
        uid: String,
        role: String,
        name: String,
    },
    /// A DOM snapshot exists but its generation doesn't match
    /// `client.generation`. The page has navigated since the snapshot
    /// was taken — the caller should re-snapshot before resolving.
    Stale,
    /// The backendNodeId isn't present in the fresh DOM snapshot, or no
    /// snapshot has been taken yet.
    NotInSnapshot,
}

/// Read-only lookup in the DOM snapshot map. Distinguishes "stale"
/// (a snapshot exists but its generation or URL no longer match the live
/// page) from "not in snapshot" (no matching entry in a fresh snapshot).
fn lookup_uid(client: &CdpClient, backend_node_id: i64, current_url: &str) -> LookupResult {
    let Some(dom) = client.last_dom_snapshot.as_ref() else {
        return LookupResult::NotInSnapshot;
    };

    if dom.generation != client.generation || dom.page_url != current_url {
        return LookupResult::Stale;
    }

    match lookup_in_snapshot(dom, backend_node_id) {
        Some((uid, role, name)) => LookupResult::Found { uid, role, name },
        None => LookupResult::NotInSnapshot,
    }
}

struct WindowGeometry {
    screen_x: f64,
    screen_y: f64,
    outer_height: f64,
    inner_width: f64,
    inner_height: f64,
    scroll_x: f64,
    scroll_y: f64,
}

async fn query_window_geometry(page: &chromiumoxide::Page) -> Result<WindowGeometry, String> {
    use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;

    let js = "JSON.stringify([window.screenX, window.screenY, window.outerHeight, \
              window.innerWidth, window.innerHeight, window.scrollX, window.scrollY])";

    let mut params = EvaluateParams::new(js);
    params.return_by_value = Some(true);
    let result = page
        .execute(params)
        .await
        .map_err(|e| format!("Failed to query window geometry: {}", e))?;

    let raw = result
        .result
        .result
        .value
        .as_ref()
        .and_then(|v| v.as_str())
        .ok_or("Empty geometry response")?;

    let vals: Vec<f64> =
        serde_json::from_str(raw).map_err(|e| format!("Failed to parse geometry: {}", e))?;

    if vals.len() < 7 {
        return Err(format!("Expected 7 geometry values, got {}", vals.len()));
    }

    Ok(WindowGeometry {
        screen_x: vals[0],
        screen_y: vals[1],
        outer_height: vals[2],
        inner_width: vals[3],
        inner_height: vals[4],
        scroll_x: vals[5],
        scroll_y: vals[6],
    })
}

async fn get_node_for_location(
    page: &chromiumoxide::Page,
    page_x: f64,
    page_y: f64,
) -> Result<i64, String> {
    let params = GetNodeForLocationParams::new(page_x as i64, page_y as i64);
    let result = page
        .execute(params)
        .await
        .map_err(|e| format!("DOM.getNodeForLocation failed: {}", e))?;

    Ok(*result.result.backend_node_id.inner())
}

async fn element_from_point_fallback(
    page: &chromiumoxide::Page,
    viewport_x: f64,
    viewport_y: f64,
) -> Result<i64, String> {
    use chromiumoxide::cdp::js_protocol::runtime::EvaluateParams;

    let js = format!(
        "document.elementFromPoint({}, {})",
        viewport_x as i64, viewport_y as i64
    );
    let params = EvaluateParams::new(js);
    let eval_result = page
        .execute(params)
        .await
        .map_err(|e| format!("elementFromPoint failed: {}", e))?;

    let object_id = eval_result
        .result
        .result
        .object_id
        .ok_or("elementFromPoint returned null (no element at coordinates)")?;

    let describe_params = DescribeNodeParams::builder().object_id(object_id).build();

    let describe_result = page
        .execute(describe_params)
        .await
        .map_err(|e| format!("DOM.describeNode failed: {}", e))?;

    Ok(*describe_result.result.node.backend_node_id.inner())
}

fn lookup_in_snapshot(
    snapshot: &SnapshotMap,
    backend_node_id: i64,
) -> Option<(String, String, String)> {
    let uids = snapshot.backend_to_uids.get(&backend_node_id)?;
    if uids.len() == 1 {
        let uid = &uids[0];
        let node = snapshot.uid_to_node.get(uid)?;
        return Some((uid.clone(), node.role.clone(), node.name.clone()));
    }
    // Multiple UIDs for same backendNodeId: pick the one with a non-empty name.
    for uid in uids {
        if let Some(node) = snapshot.uid_to_node.get(uid) {
            if !node.name.is_empty() {
                return Some((uid.clone(), node.role.clone(), node.name.clone()));
            }
        }
    }
    // Fall back to first UID.
    let uid = &uids[0];
    let node = snapshot.uid_to_node.get(uid)?;
    Some((uid.clone(), node.role.clone(), node.name.clone()))
}
