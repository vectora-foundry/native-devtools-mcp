//! Shared response helpers for `ax_click` and `ax_set_value`.
//!
//! Both tools emit the same JSON envelope — only the `dispatched_via` string
//! and the handful of error messages differ. Keeping these builders in one
//! place means the wire shape cannot drift between the two handlers.

use crate::tools::ax_snapshot::Rect;
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::json;

/// Build a `CallToolResult::success` with body
/// `{ ok: true, dispatched_via, bbox? }`. `bbox` is omitted entirely when
/// `None` (rather than rendered as `null`) so consumers can treat a missing
/// field and a missing reading identically.
pub(crate) fn success(dispatched_via: &str, bbox: Option<Rect>) -> CallToolResult {
    let body = match bbox {
        Some(r) => json!({
            "ok": true,
            "dispatched_via": dispatched_via,
            "bbox": { "x": r.x, "y": r.y, "w": r.w, "h": r.h }
        }),
        None => json!({ "ok": true, "dispatched_via": dispatched_via }),
    };
    CallToolResult::success(vec![ContentBlock::text(body.to_string())])
}

/// Build a `CallToolResult::error` with body
/// `{ error: { code, message, fallback } }`. `fallback` is the bbox centre
/// when a bbox is readable, `null` otherwise.
pub(crate) fn error(code: &str, message: &str, fallback: Option<Rect>) -> CallToolResult {
    let fb = match fallback {
        Some(r) => json!({ "x": r.x + r.w / 2.0, "y": r.y + r.h / 2.0 }),
        None => serde_json::Value::Null,
    };
    let body = json!({ "error": { "code": code, "message": message, "fallback": fb } });
    CallToolResult::error(vec![ContentBlock::text(body.to_string())])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_body(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn success_with_bbox_serializes_expected_shape() {
        let r = success(
            "AXPress",
            Some(Rect {
                x: 412.0,
                y: 285.0,
                w: 64.0,
                h: 32.0,
            }),
        );
        assert_eq!(r.is_error, Some(false));
        let body: serde_json::Value = serde_json::from_str(&text_body(&r)).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["dispatched_via"], "AXPress");
        assert_eq!(body["bbox"]["x"], 412.0);
        assert_eq!(body["bbox"]["y"], 285.0);
        assert_eq!(body["bbox"]["w"], 64.0);
        assert_eq!(body["bbox"]["h"], 32.0);
    }

    #[test]
    fn success_without_bbox_omits_bbox_field() {
        let r = success("AXPress", None);
        assert_eq!(r.is_error, Some(false));
        let body: serde_json::Value = serde_json::from_str(&text_body(&r)).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["dispatched_via"], "AXPress");
        assert!(body.get("bbox").is_none());
    }

    #[test]
    fn success_accepts_any_dispatched_via_label() {
        let r = success("AXSetAttributeValue", None);
        let body: serde_json::Value = serde_json::from_str(&text_body(&r)).unwrap();
        assert_eq!(body["dispatched_via"], "AXSetAttributeValue");
    }

    #[test]
    fn error_with_fallback_centers_on_bbox() {
        let r = error(
            "not_dispatchable",
            "element does not support AXPress",
            Some(Rect {
                x: 100.0,
                y: 200.0,
                w: 50.0,
                h: 40.0,
            }),
        );
        assert_eq!(r.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&text_body(&r)).unwrap();
        assert_eq!(body["error"]["code"], "not_dispatchable");
        assert_eq!(body["error"]["message"], "element does not support AXPress");
        // Centre = (100 + 50/2, 200 + 40/2) = (125, 220).
        assert_eq!(body["error"]["fallback"]["x"], 125.0);
        assert_eq!(body["error"]["fallback"]["y"], 220.0);
    }

    #[test]
    fn error_without_fallback_renders_null() {
        let r = error("snapshot_expired", "stale uid", None);
        assert_eq!(r.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&text_body(&r)).unwrap();
        assert_eq!(body["error"]["code"], "snapshot_expired");
        assert!(body["error"]["fallback"].is_null());
    }
}
