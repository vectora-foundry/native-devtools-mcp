//! Integration tests for the CDP DOM discovery pipeline.
//!
//! # What this covers
//!
//! Unit tests already exercise the pure Rust logic (UID assignment, prefix
//! parsing, snapshot conversion). These tests cover the *live* pipeline:
//!
//!   Runtime.evaluate (JS walker) → DOM.describeNode (backendNodeId) →
//!   SnapshotMap (`d<N>`) → action-tool UID resolution → click/eval.
//!
//! Failures in that chain (element removed between calls, shadow root not
//! descended, prefix parsing bug, stale-map lookup) would silently mis-target
//! elements — nothing in the unit-test surface catches that.
//!
//! # Approach (A): real headless Chrome
//!
//! A mock CDP server would have to re-implement shadow DOM traversal, iframe
//! traversal, `DOM.describeNode`, and the chromiumoxide protocol just to make
//! the scenarios meaningful — that's essentially reimplementing a browser.
//! A real headless Chrome gives us authentic coverage of the pipeline the
//! production tools walk.
//!
//! # Gating
//!
//! All scenarios are `#[ignore]`d. They require:
//!
//! - A Google Chrome / Chromium binary on the host (macOS or Linux;
//!   Windows is currently unsupported by the harness).
//! - Permission to bind an ephemeral loopback TCP port. Sandboxed
//!   environments that disable local listeners will see every scenario
//!   panic with "could not acquire a free port" rather than skip.
//!
//! CI jobs that meet both should run them with:
//!
//! ```bash
//! cargo test --test cdp_dom_discovery_tests -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` keeps Chrome instances from fighting over a shared
//! `user-data-dir`. Each test still gets its own temp profile; the flag is
//! belt-and-braces insurance against stray global state (dialog handlers,
//! etc).
//!
//! If Chrome is not installed, every test short-circuits with a stderr
//! skip note. Any other launch failure (temp dir, port, spawn, debug-port
//! wait, connect) panics so a harness regression fails loud.

#![cfg(feature = "cdp")]

mod harness;

use harness::{
    content_text, Harness, HTML_ARIA_VISIBLE_TEXT_MISMATCH, HTML_CONTENTEDITABLE,
    HTML_CUSTOM_BUTTON, HTML_DUPLICATE_LABELS, HTML_PARENT_TEXT_SHOULD_NOT_MATCH_CHILD,
    HTML_SHADOW_AND_IFRAME,
};
use native_devtools_mcp::cdp::tools::{
    cdp_click, cdp_evaluate_script, cdp_fill, cdp_find_elements, cdp_get_element_context,
    cdp_summarize_page, cdp_wait_for_page_change,
};

const HTML_RICH_EDITOR_SEND_STATE: &str = r##"
<!doctype html>
<html>
<body>
  <div id="composer" data-placeholder="Message" contenteditable="true"></div>
  <button id="send" hidden>Send</button>
  <script>
    window.inputEvents = 0;
    const composer = document.querySelector("#composer");
    const send = document.querySelector("#send");
    composer.addEventListener("input", () => {
      window.inputEvents += 1;
      send.hidden = composer.textContent.trim().length === 0;
    });
  </script>
</body>
</html>
"##;

const HTML_SCOPED_WAIT_FOR_MESSAGE: &str = r##"
<!doctype html>
<html>
<body>
  <section id="messages" role="log" aria-label="Messages" tabindex="0">
    <p>Earlier message</p>
  </section>
  <div id="noise" hidden></div>
  <script>
    window.startMessageWaitScenario = () => {
      setTimeout(() => {
        const noise = document.querySelector("#noise");
        noise.className = "blink";
        noise.textContent = "hidden churn";
      }, 100);
      setTimeout(() => {
        const msg = document.createElement("p");
        msg.textContent = "what is 12 * 8?";
        document.querySelector("#messages").appendChild(msg);
      }, 400);
      return true;
    };
  </script>
</body>
</html>
"##;

/// Contenteditable editor found by placeholder only.
///
/// An AX-invisible input (no <input>, no role="textbox", just a <div
/// contenteditable data-placeholder="Write something…">) must be surfaced by
/// the DOM walker — the AX tree won't expose a meaningful label.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn contenteditable_found_by_placeholder() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_CONTENTEDITABLE).await;

    let result =
        cdp_find_elements("Write something".into(), None, Some(10), h.client_handle()).await;

    assert_eq!(
        result.is_error,
        Some(false),
        "find_elements failed: {:?}",
        result
    );
    let text = content_text(&result);
    assert!(
        text.contains("\"uid\": \"d1\""),
        "expected d1 match, got:\n{text}"
    );
    assert!(
        text.contains("Write something"),
        "expected placeholder label in response:\n{text}"
    );
    assert!(
        text.contains("\"role\": \"textbox\""),
        "contenteditable should surface as textbox role:\n{text}"
    );
}

/// Filling a contenteditable composer must route text through CDP input,
/// not direct DOM mutation, so app-level input handlers can enable submit UI.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn cdp_fill_contenteditable_uses_keyboard_insertion() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_RICH_EDITOR_SEND_STATE).await;

    let found = cdp_find_elements("Message".into(), None, Some(10), h.client_handle()).await;
    assert_eq!(
        found.is_error,
        Some(false),
        "find_elements failed: {:?}",
        found
    );
    let body = content_text(&found);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("find_elements returns JSON");
    let uid = parsed["matches"]
        .as_array()
        .and_then(|matches| matches.first())
        .and_then(|row| row["uid"].as_str())
        .expect("contenteditable uid")
        .to_string();

    let filled = cdp_fill(uid, "hello".into(), false, h.client_handle()).await;
    assert_eq!(filled.is_error, Some(false), "fill failed: {:?}", filled);
    let fill_text = content_text(&filled);
    assert!(
        fill_text.contains("strategy=rich_editor_keyboard"),
        "contenteditable fill should report keyboard strategy:\n{fill_text}"
    );
    assert!(
        h.eval_bool(
            "document.querySelector('#composer').textContent === 'hello' && !document.querySelector('#send').hidden && window.inputEvents > 0"
        )
        .await,
        "contenteditable fill should update text and enable send"
    );
}

/// The wait primitive should block inside one MCP call, watch the selected
/// semantic scope, and return a compact delta for the next LLM turn to judge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn cdp_wait_for_page_change_detects_scoped_semantic_delta() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_SCOPED_WAIT_FOR_MESSAGE).await;

    let found = cdp_find_elements(
        "Messages".into(),
        Some("log".into()),
        Some(10),
        h.client_handle(),
    )
    .await;
    assert_eq!(
        found.is_error,
        Some(false),
        "find_elements failed: {:?}",
        found
    );
    let found_body = content_text(&found);
    let found_json: serde_json::Value =
        serde_json::from_str(&found_body).expect("find_elements returns JSON");
    let uid = found_json["matches"]
        .as_array()
        .and_then(|matches| matches.first())
        .and_then(|row| row["uid"].as_str())
        .unwrap_or_else(|| panic!("expected messages scope uid:\n{found_body}"))
        .to_string();

    let started = cdp_evaluate_script(
        "() => window.startMessageWaitScenario()".to_string(),
        None,
        h.client_handle(),
    )
    .await;
    assert_eq!(
        started.is_error,
        Some(false),
        "scenario start failed: {:?}",
        started
    );

    let waited = cdp_wait_for_page_change(
        Some(uid.clone()),
        Some("new_visible_text".into()),
        Some("new incoming message in the Messages log".into()),
        Some(3_000),
        Some(100),
        Some(100),
        false,
        h.client_handle(),
    )
    .await;
    assert_eq!(waited.is_error, Some(false), "wait failed: {:?}", waited);
    let body = content_text(&waited);
    let json: serde_json::Value = serde_json::from_str(&body).expect("wait returns JSON");
    assert_eq!(json["source"].as_str(), Some("dom_semantic_wait"));
    assert_eq!(json["changed"].as_bool(), Some(true));
    assert_eq!(json["scope"]["uid"].as_str(), Some(uid.as_str()));
    assert!(
        body.contains("what is 12 * 8?"),
        "semantic delta should include new message text:\n{body}"
    );
}

/// Custom <div role="button" aria-label="Close"> with no visible text.
///
/// The DOM walker must pick up the aria-label as the element's semantic
/// name even though `textContent` is empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn custom_button_uses_aria_label() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_CUSTOM_BUTTON).await;

    let result = cdp_find_elements(
        "Close".into(),
        Some("button".into()),
        Some(10),
        h.client_handle(),
    )
    .await;
    assert_eq!(
        result.is_error,
        Some(false),
        "find_elements failed: {:?}",
        result
    );
    let text = content_text(&result);
    assert!(
        text.contains("\"uid\": \"d1\""),
        "expected d1 for aria-labelled button, got:\n{text}"
    );
    assert!(
        text.contains("\"label\": \"Close\""),
        "expected aria-label surfaced as label:\n{text}"
    );
    assert!(
        text.contains("\"role\": \"button\""),
        "role must be button:\n{text}"
    );
}

/// Two "Search" controls — one in a <nav> sidebar, one in <main>.
///
/// The DOM walker's `parentRole` / `parentName` context lets downstream
/// consumers disambiguate; here we verify both matches come back and that
/// their parent_role differs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn duplicate_labels_disambiguated_by_parent() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_DUPLICATE_LABELS).await;

    let result = cdp_find_elements("Search".into(), None, Some(10), h.client_handle()).await;
    assert_eq!(
        result.is_error,
        Some(false),
        "find_elements failed: {:?}",
        result
    );
    let text = content_text(&result);

    let json: serde_json::Value = serde_json::from_str(&text).expect("find_elements returns JSON");
    let matches = json["matches"].as_array().expect("matches array");

    // We expect at least 2 "Search" elements (sidebar input + main button).
    // There may be extra matches from aria-labelled containers; filter to
    // just the two interactive controls.
    let parents: Vec<String> = matches
        .iter()
        .map(|m| m["parent_role"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(
        parents.iter().any(|p| p == "nav"),
        "expected a match parented by <nav>, got parents={parents:?}"
    );
    assert!(
        parents.iter().any(|p| p == "main"),
        "expected a match parented by <main>, got parents={parents:?}"
    );
}

/// Open shadow root + same-origin iframe traversal.
///
/// The JS walker recurses into every element with a `shadowRoot` and into
/// every same-origin iframe's `contentDocument`. A regression that drops
/// shadow descent or iframe descent shows up here as a missing match.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn shadow_root_and_iframe_traversed() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_SHADOW_AND_IFRAME).await;

    // `ShadowBtn` lives inside a custom element's open shadow root.
    let shadow = cdp_find_elements(
        "ShadowBtn".into(),
        Some("button".into()),
        Some(10),
        h.client_handle(),
    )
    .await;
    assert_eq!(
        shadow.is_error,
        Some(false),
        "find_elements (shadow) failed: {:?}",
        shadow
    );
    assert_matches_label(&shadow, "ShadowBtn");

    // `IframeBtn` lives inside a same-origin iframe (srcdoc).
    let iframe = cdp_find_elements(
        "IframeBtn".into(),
        Some("button".into()),
        Some(10),
        h.client_handle(),
    )
    .await;
    assert_eq!(
        iframe.is_error,
        Some(false),
        "find_elements (iframe) failed: {:?}",
        iframe
    );
    assert_matches_label(&iframe, "IframeBtn");
}

/// Query matching must not rely only on the chosen accessibility label.
///
/// Signal and other Electron apps can expose a clickable row whose
/// `aria-label` disagrees with the visible descendant text. The discovery
/// result should still match on visible text and report the mismatch so
/// downstream agents do not fall back to arbitrary page scripts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn visible_text_match_survives_bad_aria_label() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_ARIA_VISIBLE_TEXT_MISMATCH).await;

    let result = cdp_find_elements("Note to Self".into(), None, Some(10), h.client_handle()).await;
    assert_eq!(
        result.is_error,
        Some(false),
        "find_elements failed: {:?}",
        result
    );

    let body = content_text(&result);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("find_elements returns JSON");
    let matches = parsed["matches"].as_array().expect("matches array");
    let row = matches
        .iter()
        .find(|m| {
            m["visible_text"]
                .as_str()
                .is_some_and(|text| text.contains("Note to Self"))
        })
        .unwrap_or_else(|| panic!("expected visible-text match, got:\n{body}"));

    assert_eq!(
        row["label"].as_str(),
        Some("Chat with Ljuba Isakovic, 0 new messages")
    );
    assert!(
        row["matched_on"].as_array().is_some_and(|fields| fields
            .iter()
            .any(|field| field.as_str() == Some("visible_text"))),
        "expected matched_on to include visible_text; body:\n{body}"
    );
    assert!(
        row["warnings"].as_array().is_some_and(|warnings| warnings
            .iter()
            .any(|warning| warning.as_str() == Some("accessible_name_visible_text_mismatch"))),
        "expected mismatch warning; body:\n{body}"
    );
    assert_eq!(row["in_viewport"].as_bool(), Some(true));
}

/// Parent context is returned for disambiguation, not for broad query hits.
///
/// A child button inside a row named "Note to Self" must not match the query
/// unless the button's own label/visible/value fields match. Otherwise the
/// first viewport-sorted hit can be a row menu rather than the target row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn parent_text_does_not_make_child_button_match() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_PARENT_TEXT_SHOULD_NOT_MATCH_CHILD).await;

    let result = cdp_find_elements("Note to Self".into(), None, Some(10), h.client_handle()).await;
    assert_eq!(
        result.is_error,
        Some(false),
        "find_elements failed: {:?}",
        result
    );

    let body = content_text(&result);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("find_elements returns JSON");
    let matches = parsed["matches"].as_array().expect("matches array");
    assert!(
        matches.is_empty(),
        "parent row text must not match unrelated child controls:\n{body}"
    );
}

/// The new query/expand contract:
/// 1. `cdp_summarize_page` gives compact inventory only.
/// 2. `cdp_find_elements` creates the targetable d<N> snapshot.
/// 3. Another summary call must not clobber that snapshot.
/// 4. `cdp_get_element_context` can expand the prior UID with bounded local context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn summarize_then_query_then_expand_preserves_target_uid() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_ARIA_VISIBLE_TEXT_MISMATCH).await;

    let summary = cdp_summarize_page(h.client_handle()).await;
    assert_eq!(
        summary.is_error,
        Some(false),
        "summarize failed: {:?}",
        summary
    );
    let summary_body = content_text(&summary);
    let summary_json: serde_json::Value =
        serde_json::from_str(&summary_body).expect("summary returns JSON");
    assert_eq!(summary_json["source"].as_str(), Some("dom_summary"));
    assert!(
        summary_json.get("matches").is_none(),
        "summary must not expose targetable matches:\n{summary_body}"
    );
    assert!(
        summary_json["inventory"]
            .as_array()
            .is_some_and(|items| !items.is_empty()),
        "summary should include compact role inventory:\n{summary_body}"
    );

    let found = cdp_find_elements("Note to Self".into(), None, Some(10), h.client_handle()).await;
    assert_eq!(
        found.is_error,
        Some(false),
        "find_elements failed: {:?}",
        found
    );
    let found_body = content_text(&found);
    let found_json: serde_json::Value =
        serde_json::from_str(&found_body).expect("find_elements returns JSON");
    let uid = found_json["matches"]
        .as_array()
        .and_then(|matches| {
            matches.iter().find_map(|m| {
                m["visible_text"]
                    .as_str()
                    .is_some_and(|text| text.contains("Note to Self"))
                    .then(|| m["uid"].as_str().unwrap_or_default().to_string())
            })
        })
        .unwrap_or_else(|| panic!("expected Note to Self match:\n{found_body}"));
    assert!(
        uid.starts_with('d'),
        "find_elements should return d-prefixed uid, got {uid:?}"
    );

    let summary_after_find = cdp_summarize_page(h.client_handle()).await;
    assert_eq!(
        summary_after_find.is_error,
        Some(false),
        "second summarize failed: {:?}",
        summary_after_find
    );

    let context = cdp_get_element_context(
        uid.clone(),
        Some(3),
        Some(2),
        Some(8),
        Some(240),
        h.client_handle(),
    )
    .await;
    assert_eq!(
        context.is_error,
        Some(false),
        "get_element_context failed for {uid}: {:?}",
        context
    );
    let context_body = content_text(&context);
    let context_json: serde_json::Value =
        serde_json::from_str(&context_body).expect("context returns JSON");
    assert_eq!(context_json["source"].as_str(), Some("dom_context"));
    assert_eq!(context_json["uid"].as_str(), Some(uid.as_str()));
    assert!(
        context_json["element"]["visible_text"]
            .as_str()
            .is_some_and(|text| text.contains("Note to Self")),
        "expanded context should carry stored match evidence:\n{context_body}"
    );
    assert!(
        context_json["live_context"]["element"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Note to Self")),
        "expanded context should include live local DOM text:\n{context_body}"
    );
}

/// CDP action tools invalidate prior query UIDs. After a click, an old d<N>
/// reference must not remain expandable; the agent has to query again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn cdp_action_invalidates_prior_query_uid() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(
        r#"
        <!doctype html>
        <html>
          <body>
            <button onclick="document.body.innerHTML='<button>New Target</button>'">Change</button>
          </body>
        </html>
        "#,
    )
    .await;

    let found = cdp_find_elements("Change".into(), None, Some(10), h.client_handle()).await;
    assert_eq!(
        found.is_error,
        Some(false),
        "find_elements failed: {:?}",
        found
    );
    let body = content_text(&found);
    let json: serde_json::Value = serde_json::from_str(&body).expect("find_elements returns JSON");
    let uid = json["matches"][0]["uid"]
        .as_str()
        .expect("match uid")
        .to_string();

    let clicked = cdp_click(uid.clone(), false, false, h.client_handle()).await;
    assert_eq!(clicked.is_error, Some(false), "click failed: {:?}", clicked);

    let stale_context =
        cdp_get_element_context(uid.clone(), None, None, None, None, h.client_handle()).await;
    assert_eq!(
        stale_context.is_error,
        Some(true),
        "old uid should be invalidated after click, got: {:?}",
        stale_context
    );
    let stale_body = content_text(&stale_context);
    assert!(
        stale_body.contains("No DOM snapshot available") || stale_body.contains("stale"),
        "expected stale/no-snapshot error after action, got:\n{stale_body}"
    );
}

/// Parse a `cdp_find_elements` response and assert that its `matches`
/// array contains at least one entry whose label equals `expected`.
/// The plain `inventory` field is ignored on purpose — it's populated
/// before query/visibility filtering, so a regression that empties
/// `matches` but leaves `inventory` would otherwise silently pass.
fn assert_matches_label(result: &rmcp::model::CallToolResult, expected: &str) {
    let body = content_text(result);
    let parsed: serde_json::Value =
        serde_json::from_str(&body).expect("find_elements returns JSON");
    let matches = parsed
        .get("matches")
        .and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("response missing `matches` array:\n{body}"));
    let found = matches.iter().any(|m| {
        m.get("label")
            .and_then(|l| l.as_str())
            .is_some_and(|l| l == expected)
    });
    assert!(
        found,
        "no entry in `matches` with label={expected:?}; body:\n{body}"
    );
}

// ---------------------------------------------------------------------------
// Calls that stay in flight longer than chromiumoxide's default 30 s request
// timeout. `cdp_wait_for_page_change` accepts waits up to 55 s and
// `cdp_evaluate_script` awaits page promises; both must return their own
// result instead of a generic transport timeout once 30 s pass.
// These take 35-40 s each.
// ---------------------------------------------------------------------------

const HTML_QUIET_MESSAGE_LOG: &str = r##"
<!doctype html>
<html>
<body>
  <section role="log" aria-label="Messages" tabindex="0"><p>nothing new here</p></section>
</body>
</html>
"##;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome and takes ~35 s — run with `cargo test -- --ignored`"]
async fn cdp_evaluate_script_awaits_promise_that_settles_after_30_seconds() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_QUIET_MESSAGE_LOG).await;

    let result = cdp_evaluate_script(
        "() => new Promise(resolve => setTimeout(() => resolve('settled late'), 35000))"
            .to_string(),
        None,
        h.client_handle(),
    )
    .await;

    assert_eq!(
        result.is_error,
        Some(false),
        "evaluate failed: {:?}",
        result
    );
    assert_eq!(content_text(&result).trim(), "\"settled late\"");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome and takes ~40 s — run with `cargo test -- --ignored`"]
async fn cdp_wait_for_page_change_reports_own_timeout_after_40_seconds() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_QUIET_MESSAGE_LOG).await;

    let waited = cdp_wait_for_page_change(
        None,
        None,
        None,
        Some(40_000),
        Some(500),
        Some(100),
        false,
        h.client_handle(),
    )
    .await;

    assert_eq!(waited.is_error, Some(false), "wait failed: {:?}", waited);
    let body = content_text(&waited);
    let json: serde_json::Value = serde_json::from_str(&body).expect("wait returns JSON");
    assert_eq!(json["changed"].as_bool(), Some(false));
    assert_eq!(json["timed_out"].as_bool(), Some(true));
    assert_eq!(json["timeout_ms"].as_u64(), Some(40_000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome and takes ~40 s — run with `cargo test -- --ignored`"]
async fn cdp_wait_for_page_change_scoped_reports_own_timeout_after_40_seconds() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_QUIET_MESSAGE_LOG).await;

    let uid = find_messages_log_uid(&h).await;

    let waited = cdp_wait_for_page_change(
        Some(uid.clone()),
        None,
        None,
        Some(40_000),
        Some(500),
        Some(100),
        false,
        h.client_handle(),
    )
    .await;

    assert_eq!(waited.is_error, Some(false), "wait failed: {:?}", waited);
    let body = content_text(&waited);
    let json: serde_json::Value = serde_json::from_str(&body).expect("wait returns JSON");
    assert_eq!(json["timed_out"].as_bool(), Some(true));
    assert_eq!(json["timeout_ms"].as_u64(), Some(40_000));
    assert_eq!(json["scope"]["uid"].as_str(), Some(uid.as_str()));
    assert_eq!(json["after"]["root"]["role"].as_str(), Some("log"));
}

/// Element arguments are passed in the page's main-world context. The
/// function must still see the first element as `this` and receive every
/// element as an argument.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn cdp_evaluate_script_with_element_args_binds_this_to_first_element() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_QUIET_MESSAGE_LOG).await;
    let uid = find_messages_log_uid(&h).await;

    let result = cdp_evaluate_script(
        "function(el) { return [this === el, arguments.length, el.getAttribute('aria-label')]; }"
            .to_string(),
        Some(vec![serde_json::json!({ "uid": uid })]),
        h.client_handle(),
    )
    .await;

    assert_eq!(
        result.is_error,
        Some(false),
        "evaluate failed: {:?}",
        result
    );
    let value: serde_json::Value =
        serde_json::from_str(&content_text(&result)).expect("evaluate returns JSON");
    assert_eq!(value, serde_json::json!([true, 1, "Messages"]));
}

async fn find_messages_log_uid(h: &Harness) -> String {
    let found = cdp_find_elements(
        "Messages".into(),
        Some("log".into()),
        Some(10),
        h.client_handle(),
    )
    .await;
    let found_json: serde_json::Value =
        serde_json::from_str(&content_text(&found)).expect("find_elements returns JSON");
    found_json["matches"][0]["uid"]
        .as_str()
        .expect("Messages log uid")
        .to_string()
}

/// Element arguments are resolved in the top page's main-world context.
/// An element inside a same-origin iframe must still resolve and be usable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn cdp_evaluate_script_accepts_same_origin_iframe_element_arg() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_SHADOW_AND_IFRAME).await;
    let found = cdp_find_elements(
        "IframeBtn".into(),
        Some("button".into()),
        Some(10),
        h.client_handle(),
    )
    .await;
    let found_json: serde_json::Value =
        serde_json::from_str(&content_text(&found)).expect("find_elements returns JSON");
    let uid = found_json["matches"][0]["uid"]
        .as_str()
        .expect("IframeBtn uid")
        .to_string();

    let result = cdp_evaluate_script(
        "(el) => [el.textContent, el.ownerDocument !== document]".to_string(),
        Some(vec![serde_json::json!({ "uid": uid })]),
        h.client_handle(),
    )
    .await;

    assert_eq!(
        result.is_error,
        Some(false),
        "evaluate failed: {:?}",
        result
    );
    let value: serde_json::Value =
        serde_json::from_str(&content_text(&result)).expect("evaluate returns JSON");
    assert_eq!(value, serde_json::json!(["IframeBtn", true]));
}

/// An expression that only contains an arrow callback must be evaluated
/// as is, not wrapped and called as a function.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Chrome — run with `cargo test -- --ignored`"]
async fn cdp_evaluate_script_evaluates_expression_with_arrow_callback() {
    let Some(mut h) = Harness::launch_or_skip().await else {
        return;
    };
    h.navigate(HTML_QUIET_MESSAGE_LOG).await;

    let result = cdp_evaluate_script(
        "[1, 2].map(x => x * 2)".to_string(),
        None,
        h.client_handle(),
    )
    .await;

    assert_eq!(
        result.is_error,
        Some(false),
        "evaluate failed: {:?}",
        result
    );
    let value: serde_json::Value =
        serde_json::from_str(&content_text(&result)).expect("evaluate returns JSON");
    assert_eq!(value, serde_json::json!([2, 4]));
}
