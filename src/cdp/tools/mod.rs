//! CDP tool implementations, split by concern:
//! - `input`: click, hover, fill, press_key
//! - `pages`: list_pages, select_page, navigate, new_page, close_page, handle_dialog
//! - `script`: evaluate_script, take_dom_snapshot, find_elements, wait_for
//! - `element_at_point`: resolve screen coordinates to snapshot UIDs

mod element_at_point;
mod input;
mod js_function;
mod pages;
mod script;

pub use element_at_point::cdp_element_at_point;
pub use input::{cdp_click, cdp_fill, cdp_hover, cdp_press_key, cdp_type_text};
pub use pages::{
    cdp_close_page, cdp_handle_dialog, cdp_list_pages, cdp_navigate, cdp_new_page, cdp_select_page,
};
pub use script::{
    cdp_evaluate_script, cdp_find_elements, cdp_get_element_context, cdp_summarize_page,
    cdp_take_dom_snapshot, cdp_wait_for, cdp_wait_for_page_change,
};

// Shared helpers used by input tools.

use crate::cdp::{cdp_error, CdpClient};
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, GetBoxModelParams, ResolveNodeParams, ScrollIntoViewIfNeededParams,
};
use chromiumoxide::page::Page;
use rmcp::model::CallToolResult;

/// Resolve a UID to a backend node ID and element metadata from the snapshot.
///
/// Staleness is determined by both `client.generation` (catches same-URL
/// reloads and SPA navigations) and `current_url` vs. the snapshot's
/// stamped URL (catches out-of-band navigations between tool calls).
fn resolve_node(
    uid: &str,
    client: &CdpClient,
    current_url: &str,
) -> Result<(BackendNodeId, String, String), CallToolResult> {
    let node = crate::cdp::resolve_uid_from_maps(
        uid,
        client.last_dom_snapshot.as_ref(),
        client.generation,
        current_url,
    )
    .map_err(cdp_error)?;

    Ok((
        BackendNodeId::new(node.backend_node_id),
        node.role.clone(),
        node.name.clone(),
    ))
}

/// Resolve a UID to a remote object ID for use with `callFunctionOn`.
async fn resolve_to_object_id(
    uid: &str,
    backend_node_id: BackendNodeId,
    page: &Page,
) -> Result<chromiumoxide::cdp::js_protocol::runtime::RemoteObjectId, CallToolResult> {
    let resolve_params = ResolveNodeParams::builder()
        .backend_node_id(backend_node_id)
        .build();

    let remote_object = page.execute(resolve_params).await.map_err(|e| {
        cdp_error(format!(
            "Element uid={} could not be resolved to a DOM node: {}",
            uid, e
        ))
    })?;

    remote_object.result.object.object_id.ok_or_else(|| {
        cdp_error(format!(
            "Element uid={} could not be resolved to a DOM node.",
            uid
        ))
    })
}

/// Resolve a UID to element center coordinates (scrolls into view).
async fn resolve_element_center(
    uid: &str,
    client: &CdpClient,
    page: &Page,
) -> Result<(String, String, f64, f64), CallToolResult> {
    let current_url = crate::cdp::page_url(page).await;
    let (backend_node_id, node_role, node_name) = resolve_node(uid, client, &current_url)?;

    let scroll_params = ScrollIntoViewIfNeededParams::builder()
        .backend_node_id(backend_node_id)
        .build();
    if let Err(e) = page.execute(scroll_params).await {
        return Err(cdp_error(format!(
            "Failed to scroll element uid={} into view: {}",
            uid, e
        )));
    }

    let box_params = GetBoxModelParams::builder()
        .backend_node_id(backend_node_id)
        .build();
    let box_result = page.execute(box_params).await.map_err(|e| {
        cdp_error(format!(
            "Element uid={} is no longer in the DOM: {}",
            uid, e
        ))
    })?;

    let quad = box_result.result.model.content.inner();
    if quad.len() < 8 {
        return Err(cdp_error(format!(
            "Element uid={} returned an invalid box model (expected 8 quad values, got {}).",
            uid,
            quad.len()
        )));
    }
    let cx = (quad[0] + quad[2] + quad[4] + quad[6]) / 4.0;
    let cy = (quad[1] + quad[3] + quad[5] + quad[7]) / 4.0;

    Ok((node_role, node_name, cx, cy))
}
