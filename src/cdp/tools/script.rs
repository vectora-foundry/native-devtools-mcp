//! CDP script and snapshot tools: evaluate_script, find_elements,
//! take_dom_snapshot, wait_for.

use super::js_function::is_function_expression;
use crate::cdp::{cdp_error, page_url, CdpClient, CDP_REQUEST_TIMEOUT};
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, ResolveNodeParams,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    CallArgument, CallFunctionOnParams, EvaluateParams, ExecutionContextId, ReleaseObjectParams,
    RemoteObjectId,
};
use chromiumoxide::error::CdpError;
use chromiumoxide::page::Page;
use rmcp::model::{CallToolResult, Content};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Why a JS call that may await a page promise failed.
#[derive(Debug, PartialEq)]
enum JsCallError {
    /// The script threw, or the promise it returned rejected.
    Exception(String),
    /// No JS result came back (transport error, request timeout, ...).
    Transport(String),
}

impl JsCallError {
    fn into_tool_error(self, exception_prefix: &str, transport_prefix: &str) -> CallToolResult {
        match self {
            Self::Exception(text) => cdp_error(format!("{}: {}", exception_prefix, text)),
            Self::Transport(text) => cdp_error(format!("{}: {}", transport_prefix, text)),
        }
    }
}

impl From<CdpError> for JsCallError {
    fn from(err: CdpError) -> Self {
        match err {
            CdpError::JavascriptException(details) => Self::Exception(details.text),
            CdpError::Timeout => Self::Transport(format!(
                "no response within the {} s CDP request timeout",
                CDP_REQUEST_TIMEOUT.as_secs()
            )),
            other => Self::Transport(other.to_string()),
        }
    }
}

/// Run `Runtime.evaluate` and return the by-value result.
///
/// Uses `Page::evaluate_expression`, not `Page::execute`: `execute` arms a
/// fixed 30 s timer that ignores the configured [`CDP_REQUEST_TIMEOUT`], so
/// awaiting a slower promise would fail with a generic timeout.
async fn evaluate_awaiting(page: &Page, params: EvaluateParams) -> Result<Value, JsCallError> {
    let result = page.evaluate_expression(params).await?;
    Ok(result.value().cloned().unwrap_or(Value::Null))
}

/// Run `Runtime.callFunctionOn` and return the by-value result.
///
/// Same reason as [`evaluate_awaiting`] for using `Page::evaluate_function`.
/// That API always sends an `executionContextId`, and Chrome rejects
/// `executionContextId` together with `objectId`. So `params` must not set
/// `object_id`; pass element handles as arguments resolved in the same
/// context (see [`main_world_context`]).
async fn call_function_awaiting(
    page: &Page,
    params: CallFunctionOnParams,
) -> Result<Value, JsCallError> {
    let result = page.evaluate_function(params).await?;
    Ok(result.value().cloned().unwrap_or(Value::Null))
}

/// Main-world execution context of the page's main frame.
///
/// Element handles passed to [`call_function_awaiting`] must be resolved in
/// this context, or Chrome rejects them as belonging to another world.
async fn main_world_context(page: &Page) -> Result<ExecutionContextId, String> {
    match page.execution_context().await {
        Ok(Some(id)) => Ok(id),
        Ok(None) => Err("the page has no JavaScript execution context yet".to_string()),
        Err(e) => Err(format!("cannot get the page execution context: {}", e)),
    }
}

/// Wrap a function declaration so it runs with `this` bound to its first
/// argument and still receives every argument, matching `callFunctionOn`
/// with that argument's `objectId` and the full argument list.
fn bind_this_to_first_argument(function_declaration: &str) -> String {
    format!(
        "function(...args) {{ return ({}\n).apply(args[0], args); }}",
        function_declaration
    )
}

/// Wrap a function declaration so its first argument becomes `this` and is
/// dropped from the arguments, matching `callFunctionOn` with that
/// argument's `objectId` and the remaining arguments.
fn use_first_argument_as_this(function_declaration: &str) -> String {
    format!(
        "function(thisArg, ...args) {{ return ({}\n).apply(thisArg, args); }}",
        function_declaration
    )
}

pub async fn cdp_evaluate_script(
    function: String,
    args: Option<Vec<serde_json::Value>>,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    let guard = cdp_client.read().await;
    let client = match guard.as_ref() {
        Some(c) => c,
        None => return cdp_error("No CDP connection. Use cdp_connect first."),
    };

    let page = match client.require_page() {
        Ok(p) => p,
        Err(e) => return e,
    };

    // Determine if we need to resolve element args.
    let has_uid_args = args
        .as_ref()
        .is_some_and(|a| a.iter().any(|v| v.get("uid").is_some()));

    if !has_uid_args {
        // Simple case: evaluate the expression directly.
        // If the whole source is a function expression, call it as an IIFE so
        // `() => document.title` returns the title, not the function object.
        // Expressions that merely contain an arrow, like `[1, 2].map(x => x * 2)`,
        // are evaluated as is.
        let expression = if is_function_expression(&function) {
            format!("({})()", function)
        } else {
            function
        };

        let mut eval_params = EvaluateParams::new(expression);
        eval_params.return_by_value = Some(true);
        eval_params.await_promise = Some(true);

        return match evaluate_awaiting(&page, eval_params).await {
            Ok(value) => pretty_json_result(&value),
            Err(e) => e.into_tool_error("JavaScript exception", "Failed to evaluate script"),
        };
    }

    // Args case: resolve UIDs to remote objects and call function with them.
    let arg_list = match args.as_ref() {
        Some(a) => a,
        None => return cdp_error("args required when passing element references"),
    };
    let mut call_arguments: Vec<CallArgument> = Vec::with_capacity(arg_list.len());

    // Collect (uid, backend_node_id) pairs first to avoid borrow issues.
    let mut uid_backend_pairs: Vec<(String, i64)> = Vec::with_capacity(arg_list.len());
    let current_url = page_url(&page).await;
    for arg in arg_list {
        if let Some(uid) = arg.get("uid").and_then(|v| v.as_str()) {
            let node = match crate::cdp::resolve_uid_from_maps(
                uid,
                client.last_dom_snapshot.as_ref(),
                client.generation,
                &current_url,
            ) {
                Ok(n) => n,
                Err(msg) => return cdp_error(msg),
            };
            uid_backend_pairs.push((uid.to_string(), node.backend_node_id));
        }
    }

    if uid_backend_pairs.is_empty() {
        return cdp_error("No element arguments could be resolved.");
    }

    // Resolve every element in the page's main-world context and call the
    // function in that same context (see `call_function_awaiting`).
    let context_id = match main_world_context(&page).await {
        Ok(id) => id,
        Err(msg) => return cdp_error(format!("Failed to call function: {}", msg)),
    };
    for (uid, backend_node_id) in &uid_backend_pairs {
        let resolve_params = ResolveNodeParams::builder()
            .backend_node_id(BackendNodeId::new(*backend_node_id))
            .execution_context_id(context_id)
            .build();

        let remote_object = match page.execute(resolve_params).await {
            Ok(resp) => resp.result.object,
            Err(_) => {
                return cdp_error(format!(
                    "Element uid={} could not be resolved to a DOM node.",
                    uid
                ));
            }
        };

        let object_id = match remote_object.object_id {
            Some(id) => id,
            None => {
                return cdp_error(format!(
                    "Element uid={} could not be resolved to a DOM node.",
                    uid
                ));
            }
        };

        call_arguments.push(CallArgument::builder().object_id(object_id).build());
    }

    // `this` stays bound to the first element, as with callFunctionOn on
    // that element's objectId.
    let call_params = match CallFunctionOnParams::builder()
        .function_declaration(bind_this_to_first_argument(&function))
        .execution_context_id(context_id)
        .arguments(call_arguments)
        .return_by_value(true)
        .await_promise(true)
        .build()
    {
        Ok(p) => p,
        Err(e) => return cdp_error(format!("Failed to build call params: {}", e)),
    };

    match call_function_awaiting(&page, call_params).await {
        Ok(value) => pretty_json_result(&value),
        Err(e) => e.into_tool_error("JavaScript exception", "Failed to call function"),
    }
}

fn pretty_json_result(value: &Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "null".to_string()),
    )])
}

const MAX_WAIT_TIMEOUT_MS: u64 = 60_000;
const MAX_PAGE_CHANGE_WAIT_TIMEOUT_MS: u64 = 55_000;
const DEFAULT_PAGE_CHANGE_WAIT_TIMEOUT_MS: u64 = 55_000;
const DEFAULT_PAGE_CHANGE_POLL_MS: u64 = 500;
const MIN_PAGE_CHANGE_POLL_MS: u64 = 100;
const MAX_PAGE_CHANGE_POLL_MS: u64 = 5_000;
const DEFAULT_PAGE_CHANGE_STABLE_MS: u64 = 500;
const MIN_PAGE_CHANGE_STABLE_MS: u64 = 100;
const MAX_PAGE_CHANGE_STABLE_MS: u64 = 2_000;

pub async fn cdp_wait_for(
    texts: Vec<String>,
    timeout_ms: Option<u64>,
    include_snapshot: bool,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    let raw_timeout = timeout_ms.unwrap_or(10_000).min(MAX_WAIT_TIMEOUT_MS);
    let timeout = std::time::Duration::from_millis(raw_timeout);
    let poll_interval = std::time::Duration::from_millis(500);
    let start = std::time::Instant::now();

    // Build JS check: resolves true when any of the texts appear in the page body.
    // serde_json::to_string on Vec<String> is infallible.
    let texts_json = serde_json::to_string(&texts).unwrap();
    let check_js = format!(
        "document.body && {}.some(t => document.body.innerText.includes(t))",
        texts_json
    );

    loop {
        let found = {
            let guard = cdp_client.read().await;
            let client = match guard.as_ref() {
                Some(c) => c,
                None => return cdp_error("No CDP connection. Use cdp_connect first."),
            };
            let page = match client.require_page() {
                Ok(p) => p,
                Err(e) => return e,
            };

            let mut eval_params = EvaluateParams::new(&check_js);
            eval_params.return_by_value = Some(true);

            match page.execute(eval_params).await {
                Ok(resp) => resp
                    .result
                    .result
                    .value
                    .as_ref()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                Err(_) => false,
            }
        };

        if found {
            let elapsed_ms = start.elapsed().as_millis();
            let header = format!("Text appeared after {}ms: {}", elapsed_ms, texts_json);
            if !include_snapshot {
                return CallToolResult::success(vec![Content::text(header)]);
            }
            // Smaller cap than the user-facing default: cdp_wait_for is
            // typically followed by a targeted cdp_find_elements, so a
            // lightweight snapshot is enough to show what appeared.
            let mut result = cdp_take_dom_snapshot(Some(100), cdp_client.clone()).await;
            result.content.insert(0, Content::text(header));
            return result;
        }

        if start.elapsed() >= timeout {
            return cdp_error(format!(
                "Timed out after {}ms waiting for text: {}",
                timeout.as_millis(),
                texts_json
            ));
        }

        tokio::time::sleep(poll_interval).await;
    }
}

const PAGE_CHANGE_WAIT_JS: &str = r#"
async function(timeoutMs, stableMs, pollIntervalMs) {
  const root = (this && this.nodeType === Node.ELEMENT_NODE) ? this : document.body;
  const startedAt = Date.now();
  const safeRoot = root || document.body;

  const normalizeLines = (value) => String(value || '')
    .replace(/\u200e|\u200f|\u202a|\u202b|\u202c|\u202d|\u202e|\u2066|\u2067|\u2068|\u2069/g, '')
    .split(/\n+/)
    .map((line) => line.replace(/\s+/g, ' ').trim())
    .filter(Boolean)
    .join('\n')
    .trim();
  const stripDynamic = (value) => normalizeLines(value)
    .replace(/\b(?:now|today|yesterday)\b/gi, '<relative-time>')
    .replace(/\b\d+\s*(?:s|sec|secs|second|seconds|m|min|mins|minute|minutes|h|hr|hrs|hour|hours|d|day|days|w|week|weeks|mo|month|months|y|yr|yrs|year|years)\b/gi, '<relative-time>');
  const hash = (value) => {
    let h = 2166136261;
    for (let i = 0; i < value.length; i++) {
      h ^= value.charCodeAt(i);
      h = Math.imul(h, 16777619);
    }
    return `${(h >>> 0).toString(16).padStart(8, '0')}:${value.length}`;
  };
  const roleFor = (el) => {
    if (!el || !el.getAttribute) return '';
    const role = el.getAttribute('role');
    if (role) return role;
    const tag = (el.tagName || '').toLowerCase();
    if (tag === 'button') return 'button';
    if (tag === 'a' && el.hasAttribute('href')) return 'link';
    if (tag === 'input' || tag === 'textarea' || el.isContentEditable) return 'textbox';
    if (tag === 'select') return 'combobox';
    return tag;
  };
  const isVisible = (el) => {
    if (!el || el.nodeType !== Node.ELEMENT_NODE) return false;
    if (el === document.body || el === safeRoot) return true;
    const style = window.getComputedStyle(el);
    if (style.display === 'none' || style.visibility === 'hidden') return false;
    const rect = el.getBoundingClientRect();
    return rect.width > 0 || rect.height > 0;
  };
  const fieldSelector = 'input, textarea, select, [contenteditable="true"], [contenteditable="plaintext-only"], [role="textbox"]';
  const fieldValue = (el) => {
    if (!el) return '';
    if ('value' in el && el.value != null && String(el.value).trim()) return el.value;
    return el.innerText || el.textContent || el.getAttribute('aria-label') || el.getAttribute('placeholder') || '';
  };
  const summarizeElement = (el) => {
    if (!el) return null;
    return {
      tag: (el.tagName || '').toLowerCase(),
      role: roleFor(el),
      aria_label: normalizeLines(el.getAttribute ? el.getAttribute('aria-label') : '').slice(0, 240),
      placeholder: normalizeLines(el.getAttribute ? (el.getAttribute('placeholder') || el.getAttribute('data-placeholder')) : '').slice(0, 240),
      text: normalizeLines(('value' in el && el.value) ? el.value : (el.innerText || el.textContent || '')).slice(-500)
    };
  };
  const capture = () => {
    const textSource = safeRoot && 'innerText' in safeRoot ? safeRoot.innerText : (safeRoot ? safeRoot.textContent : '');
    const visibleText = normalizeLines(textSource);
    const fields = [];
    if (safeRoot && safeRoot.matches && safeRoot.matches(fieldSelector)) fields.push(safeRoot);
    if (safeRoot && safeRoot.querySelectorAll) fields.push(...safeRoot.querySelectorAll(fieldSelector));
    const fieldText = fields
      .filter((el, index) => fields.indexOf(el) === index)
      .filter(isVisible)
      .map(fieldValue)
      .map(normalizeLines)
      .filter(Boolean)
      .join('\n');
    const semanticText = stripDynamic([visibleText, fieldText].filter(Boolean).join('\n'));
    const semanticUnits = semanticText.split(/\n+/).map((v) => v.trim()).filter(Boolean);
    return {
      signature: hash(`${location.href}\n${document.title || ''}\n${semanticText}`),
      url: location.href,
      title: document.title || '',
      text_length: visibleText.length,
      semantic_text_length: semanticText.length,
      visible_text_tail: visibleText.slice(-2500),
      semantic_text_tail: semanticText.slice(-2500),
      semantic_units: semanticUnits.slice(-50),
      root: summarizeElement(safeRoot),
      active_element: summarizeElement(document.activeElement)
    };
  };
  const addedUnits = (beforeUnits, afterUnits) => {
    const counts = new Map();
    for (const unit of beforeUnits || []) counts.set(unit, (counts.get(unit) || 0) + 1);
    const added = [];
    for (const unit of afterUnits || []) {
      const count = counts.get(unit) || 0;
      if (count > 0) {
        counts.set(unit, count - 1);
      } else {
        added.push(unit);
      }
    }
    return added.slice(-20);
  };
  const suffixDelta = (beforeText, afterText) => {
    let i = 0;
    const limit = Math.min(beforeText.length, afterText.length);
    while (i < limit && beforeText.charCodeAt(i) === afterText.charCodeAt(i)) i++;
    return afterText.slice(i).trim().slice(-2500);
  };
  const buildResult = (changed, timedOut, before, after, trigger) => ({
    source: 'dom_semantic_wait',
    page_url: after.url,
    title: after.title,
    changed,
    timed_out: timedOut,
    elapsed_ms: Date.now() - startedAt,
    timeout_ms: timeoutMs,
    stable_ms: stableMs,
    poll_interval_ms: pollIntervalMs,
    trigger,
    before: {
      signature: before.signature,
      text_length: before.text_length,
      semantic_text_length: before.semantic_text_length,
      visible_text_tail: before.visible_text_tail,
      semantic_text_tail: before.semantic_text_tail,
      root: before.root,
      active_element: before.active_element
    },
    after: {
      signature: after.signature,
      text_length: after.text_length,
      semantic_text_length: after.semantic_text_length,
      visible_text_tail: after.visible_text_tail,
      semantic_text_tail: after.semantic_text_tail,
      root: after.root,
      active_element: after.active_element
    },
    deltas: changed ? [{
      kind: 'semantic_text_delta',
      text: suffixDelta(before.semantic_text_tail, after.semantic_text_tail),
      added_text: addedUnits(before.semantic_units, after.semantic_units)
    }] : []
  });
  const meaningfulMutation = (mutation) => {
    if (mutation.type === 'childList' || mutation.type === 'characterData') return true;
    if (mutation.type !== 'attributes') return false;
    return [
      'value',
      'aria-label',
      'aria-selected',
      'aria-checked',
      'aria-expanded',
      'aria-pressed',
      'role',
      'placeholder',
      'title',
      'disabled'
    ].includes(mutation.attributeName);
  };
  const waitForWake = (remainingMs) => new Promise((resolve) => {
    let settled = false;
    let stableTimer = null;
    let observer = null;
    const cleanup = () => {
      if (stableTimer) clearTimeout(stableTimer);
      clearTimeout(timeoutTimer);
      clearInterval(pollTimer);
      if (observer) observer.disconnect();
    };
    const finish = (reason) => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(reason);
    };
    const schedule = (reason) => {
      if (stableTimer) clearTimeout(stableTimer);
      stableTimer = setTimeout(() => finish(reason), stableMs);
    };
    try {
      observer = new MutationObserver((mutations) => {
        if (mutations.some(meaningfulMutation)) schedule('mutation');
      });
      observer.observe(safeRoot, {
        subtree: true,
        childList: true,
        characterData: true,
        attributes: true,
        attributeFilter: [
          'value',
          'aria-label',
          'aria-selected',
          'aria-checked',
          'aria-expanded',
          'aria-pressed',
          'role',
          'placeholder',
          'title',
          'disabled'
        ]
      });
    } catch (_) {
      schedule('observer_unavailable');
    }
    const pollTimer = setInterval(() => schedule('poll'), pollIntervalMs);
    const timeoutTimer = setTimeout(() => finish('timeout'), Math.max(0, remainingMs));
  });

  const before = capture();
  let latest = before;
  while (Date.now() - startedAt < timeoutMs) {
    const remainingMs = timeoutMs - (Date.now() - startedAt);
    const trigger = await waitForWake(remainingMs);
    latest = capture();
    if (latest.signature !== before.signature) {
      return buildResult(true, false, before, latest, trigger);
    }
    if (trigger === 'timeout') break;
  }
  return buildResult(false, true, before, latest, 'timeout');
}
"#;

fn string_argument(value: impl Into<Value>) -> CallArgument {
    CallArgument::builder().value(value.into()).build()
}

async fn resolve_scope_backend_node_id(
    scope_uid: &str,
    page: &Page,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> Result<i64, CallToolResult> {
    let current_url = page_url(page).await;
    let guard = cdp_client.read().await;
    let client = guard
        .as_ref()
        .ok_or_else(|| cdp_error("No CDP connection. Use cdp_connect first."))?;
    let node = crate::cdp::resolve_uid_from_maps(
        scope_uid,
        client.last_dom_snapshot.as_ref(),
        client.generation,
        &current_url,
    )
    .map_err(cdp_error)?;

    Ok(node.backend_node_id)
}

async fn resolve_scope_object_id(
    scope_uid: &str,
    backend_node_id: i64,
    context_id: ExecutionContextId,
    page: &Page,
) -> Result<RemoteObjectId, CallToolResult> {
    let resolve_params = ResolveNodeParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .execution_context_id(context_id)
        .build();
    let remote_object = page.execute(resolve_params).await.map_err(|e| {
        cdp_error(format!(
            "Scope uid={} could not be resolved to a DOM node: {}",
            scope_uid, e
        ))
    })?;
    remote_object.result.object.object_id.ok_or_else(|| {
        cdp_error(format!(
            "Scope uid={} could not be resolved to a DOM node.",
            scope_uid
        ))
    })
}

const PAGE_CHANGE_WAIT_EXCEPTION_PREFIX: &str =
    "JavaScript exception while waiting for page change";
const PAGE_CHANGE_WAIT_TRANSPORT_PREFIX: &str = "Failed to wait for page change";

async fn wait_for_page_semantic_change(
    page: &Page,
    timeout_ms: u64,
    stable_ms: u64,
    poll_interval_ms: u64,
) -> Result<Value, CallToolResult> {
    let expression = format!(
        "({}).call(document.body, {}, {}, {})",
        PAGE_CHANGE_WAIT_JS, timeout_ms, stable_ms, poll_interval_ms
    );
    let mut eval_params = EvaluateParams::new(expression);
    eval_params.return_by_value = Some(true);
    eval_params.await_promise = Some(true);
    evaluate_awaiting(page, eval_params).await.map_err(|e| {
        e.into_tool_error(
            PAGE_CHANGE_WAIT_EXCEPTION_PREFIX,
            PAGE_CHANGE_WAIT_TRANSPORT_PREFIX,
        )
    })
}

async fn wait_for_scoped_semantic_change(
    page: &Page,
    context_id: ExecutionContextId,
    object_id: RemoteObjectId,
    timeout_ms: u64,
    stable_ms: u64,
    poll_interval_ms: u64,
) -> Result<Value, CallToolResult> {
    let call_params = CallFunctionOnParams::builder()
        .function_declaration(use_first_argument_as_this(PAGE_CHANGE_WAIT_JS))
        .execution_context_id(context_id)
        .arguments(vec![
            CallArgument::builder().object_id(object_id.clone()).build(),
            string_argument(timeout_ms),
            string_argument(stable_ms),
            string_argument(poll_interval_ms),
        ])
        .return_by_value(true)
        .await_promise(true)
        .build()
        .map_err(|e| cdp_error(format!("Failed to build wait call params: {}", e)))?;
    let call_result = call_function_awaiting(page, call_params).await;
    let _ = page.execute(ReleaseObjectParams::new(object_id)).await;
    call_result.map_err(|e| {
        e.into_tool_error(
            PAGE_CHANGE_WAIT_EXCEPTION_PREFIX,
            PAGE_CHANGE_WAIT_TRANSPORT_PREFIX,
        )
    })
}

fn decorate_semantic_wait_result(
    mut value: Value,
    scope_uid: Option<&str>,
    condition: &str,
    goal: Option<&str>,
) -> Value {
    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "scope".to_string(),
            serde_json::json!({
                "kind": if scope_uid.is_some() { "element" } else { "page" },
                "uid": scope_uid,
            }),
        );
        obj.insert(
            "condition".to_string(),
            Value::String(condition.to_string()),
        );
        if let Some(goal) = goal {
            obj.insert("goal".to_string(), Value::String(goal.to_string()));
        }
        obj.insert(
            "hint".to_string(),
            Value::String(
                "The wait tool consumed one agent step. Judge whether `deltas` satisfies the goal; if it does, act on it, otherwise call this wait tool again with the same scope rather than polling."
                    .to_string(),
            ),
        );
    }
    value
}

pub async fn cdp_wait_for_page_change(
    scope_uid: Option<String>,
    condition: Option<String>,
    goal: Option<String>,
    timeout_ms: Option<u64>,
    poll_interval_ms: Option<u64>,
    stable_ms: Option<u64>,
    include_snapshot: bool,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    let raw_timeout = timeout_ms
        .unwrap_or(DEFAULT_PAGE_CHANGE_WAIT_TIMEOUT_MS)
        .min(MAX_PAGE_CHANGE_WAIT_TIMEOUT_MS);
    let poll_interval = poll_interval_ms
        .unwrap_or(DEFAULT_PAGE_CHANGE_POLL_MS)
        .clamp(MIN_PAGE_CHANGE_POLL_MS, MAX_PAGE_CHANGE_POLL_MS);
    let stable = stable_ms
        .unwrap_or(DEFAULT_PAGE_CHANGE_STABLE_MS)
        .clamp(MIN_PAGE_CHANGE_STABLE_MS, MAX_PAGE_CHANGE_STABLE_MS);
    let condition = condition.unwrap_or_else(|| "semantic_delta".to_string());
    let scope_uid = scope_uid.filter(|uid| !uid.trim().is_empty());

    let page = {
        let guard = cdp_client.read().await;
        let client = match guard.as_ref() {
            Some(c) => c,
            None => return cdp_error("No CDP connection. Use cdp_connect first."),
        };
        match client.require_page() {
            Ok(page) => page,
            Err(e) => return e,
        }
    };

    let value = match scope_uid.as_deref() {
        Some(uid) => {
            let backend_node_id =
                match resolve_scope_backend_node_id(uid, &page, cdp_client.clone()).await {
                    Ok(backend_node_id) => backend_node_id,
                    Err(e) => return e,
                };
            let context_id = match main_world_context(&page).await {
                Ok(id) => id,
                Err(msg) => {
                    return cdp_error(format!("{}: {}", PAGE_CHANGE_WAIT_TRANSPORT_PREFIX, msg))
                }
            };
            let object_id =
                match resolve_scope_object_id(uid, backend_node_id, context_id, &page).await {
                    Ok(object_id) => object_id,
                    Err(e) => return e,
                };
            match wait_for_scoped_semantic_change(
                &page,
                context_id,
                object_id,
                raw_timeout,
                stable,
                poll_interval,
            )
            .await
            {
                Ok(value) => value,
                Err(e) => return e,
            }
        }
        None => {
            match wait_for_page_semantic_change(&page, raw_timeout, stable, poll_interval).await {
                Ok(value) => value,
                Err(e) => return e,
            }
        }
    };

    let result =
        decorate_semantic_wait_result(value, scope_uid.as_deref(), &condition, goal.as_deref());
    let result_text = serde_json::to_string_pretty(&result).unwrap_or_default();
    if !include_snapshot {
        return CallToolResult::success(vec![Content::text(result_text)]);
    }

    let mut snapshot = cdp_take_dom_snapshot(Some(100), cdp_client.clone()).await;
    snapshot.content.insert(0, Content::text(result_text));
    snapshot
}

/// Shared DOM walker + single-pass resolution logic used by both
/// `cdp_find_elements` and `cdp_take_dom_snapshot`.
///
/// Runs the JS walker with `return_by_value=false` to get element references,
/// then iterates to extract metadata and resolve `backendNodeId` atomically
/// via `DOM.describeNode`. Drops candidates where resolution fails or returns
/// `backendNodeId=0`.
async fn resolve_dom_candidates(
    page: &Page,
    walker_js: &str,
) -> Result<
    (
        Vec<crate::cdp::dom_discovery::DomCandidate>,
        serde_json::Value,
    ),
    CallToolResult,
> {
    // Step 1: Evaluate walker with return_by_value=false to get element references
    let mut eval_params = EvaluateParams::new(walker_js);
    eval_params.return_by_value = Some(false);

    let walker_result = match page.execute(eval_params).await {
        Ok(resp) => resp,
        Err(e) => return Err(cdp_error(format!("DOM walker failed: {}", e))),
    };

    let result_object_id = match walker_result.result.result.object_id {
        Some(id) => id,
        None => return Err(cdp_error("DOM walker returned no object reference")),
    };

    // Step 2: Extract inventory (by-value) from the result
    let inventory_js = "function() { return JSON.stringify(this.inventory); }";
    let inv_params = CallFunctionOnParams::builder()
        .function_declaration(inventory_js)
        .object_id(result_object_id.clone())
        .return_by_value(true)
        .build();
    let inventory: serde_json::Value = match page.execute(inv_params.unwrap()).await {
        Ok(resp) => resp
            .result
            .result
            .value
            .and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
            .unwrap_or(serde_json::json!([])),
        Err(_) => serde_json::json!([]),
    };

    // Step 3: Extract all metadata in one bulk call (avoids per-element round-trips)
    let meta_js = "function() { return JSON.stringify(this.metadata); }";
    let meta_params = CallFunctionOnParams::builder()
        .function_declaration(meta_js)
        .object_id(result_object_id.clone())
        .return_by_value(true)
        .build();
    let all_metadata: Vec<crate::cdp::dom_discovery::DomCandidate> =
        match page.execute(meta_params.unwrap()).await {
            Ok(resp) => resp
                .result
                .result
                .value
                .and_then(|v| v.as_str().and_then(|s| serde_json::from_str(s).ok()))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        };

    // Step 4: Resolve backendNodeIds in parallel. Each element requires three
    // RPCs (get ref, DOM.describeNode, releaseObject) — running them in
    // parallel pipelines them over the single CDP WebSocket instead of
    // paying round-trip latency per element.
    let describe_futures = all_metadata.into_iter().enumerate().map(|(i, candidate)| {
        let result_object_id = result_object_id.clone();
        async move { resolve_candidate(page, &result_object_id, i, candidate).await }
    });
    let candidates: Vec<crate::cdp::dom_discovery::DomCandidate> =
        futures_util::future::join_all(describe_futures)
            .await
            .into_iter()
            .flatten()
            .collect();

    // Release the wrapper remote object to avoid memory leaks
    let _ = page
        .execute(ReleaseObjectParams::new(result_object_id))
        .await;

    Ok((candidates, inventory))
}

/// Resolve one DOM candidate's backendNodeId via `DOM.describeNode`.
///
/// Returns `None` when any of the three round trips fails or the
/// describe returns `backendNodeId=0` (no real DOM backing).
async fn resolve_candidate(
    page: &Page,
    result_object_id: &chromiumoxide::cdp::js_protocol::runtime::RemoteObjectId,
    index: usize,
    mut candidate: crate::cdp::dom_discovery::DomCandidate,
) -> Option<crate::cdp::dom_discovery::DomCandidate> {
    let get_el_js = format!("function() {{ return this.elements[{}]; }}", index);
    let el_params = CallFunctionOnParams::builder()
        .function_declaration(&get_el_js)
        .object_id(result_object_id.clone())
        .return_by_value(false)
        .build()
        .ok()?;
    let el_object_id = page
        .execute(el_params)
        .await
        .ok()?
        .result
        .result
        .object_id?;

    let el_oid_for_release = el_object_id.clone();
    let describe = DescribeNodeParams::builder()
        .object_id(el_object_id)
        .build();
    let describe_result = page.execute(describe).await;
    let _ = page
        .execute(ReleaseObjectParams::new(el_oid_for_release))
        .await;

    let id = *describe_result.ok()?.result.node.backend_node_id.inner();
    if id == 0 {
        return None;
    }
    candidate.backend_node_id = id;
    Some(candidate)
}

fn dom_candidate_json(uid: &str, n: &crate::cdp::dom_discovery::DomCandidate) -> serde_json::Value {
    let viewport_rect = n.viewport_rect.as_ref().map(|r| {
        serde_json::json!({
            "x": r.x,
            "y": r.y,
            "width": r.width,
            "height": r.height,
        })
    });

    serde_json::json!({
        "uid": uid,
        "role": n.role,
        "label": n.label,
        "tag": n.tag,
        "disabled": n.disabled,
        "parent_role": n.parent_role,
        "parent_name": n.parent_name,
        "accessible_name": n.accessible_name,
        "visible_text": n.visible_text,
        "value": n.value,
        "placeholder": n.placeholder,
        "title": n.title,
        "alt_text": n.alt_text,
        "test_id": n.test_id,
        "matched_on": n.matched_on,
        "warnings": n.warnings,
        "viewport_rect": viewport_rect,
        "in_viewport": n.in_viewport,
    })
}

fn snapshot_node_json(uid: &str, node: &crate::cdp::SnapshotNode) -> serde_json::Value {
    serde_json::json!({
        "uid": uid,
        "role": node.role,
        "label": node.name,
    })
}

fn nearby_snapshot_candidates(
    snapshot: &crate::cdp::SnapshotMap,
    uid: &str,
    radius: usize,
) -> Vec<serde_json::Value> {
    if radius == 0 {
        return Vec::new();
    }
    let Some(index) = snapshot
        .ordered_uids
        .iter()
        .position(|candidate_uid| candidate_uid == uid)
    else {
        return Vec::new();
    };

    let start = index.saturating_sub(radius);
    let end = (index + radius + 1).min(snapshot.ordered_uids.len());
    snapshot.ordered_uids[start..end]
        .iter()
        .filter(|candidate_uid| candidate_uid.as_str() != uid)
        .filter_map(|candidate_uid| {
            snapshot
                .uid_to_candidate
                .get(candidate_uid)
                .map(|candidate| dom_candidate_json(candidate_uid, candidate))
                .or_else(|| {
                    snapshot
                        .uid_to_node
                        .get(candidate_uid)
                        .map(|node| snapshot_node_json(candidate_uid, node))
                })
        })
        .collect()
}

async fn page_title(page: &Page) -> String {
    let mut eval_params = EvaluateParams::new("document.title || \"\"");
    eval_params.return_by_value = Some(true);
    page.execute(eval_params)
        .await
        .ok()
        .and_then(|resp| resp.result.result.value)
        .and_then(|value| value.as_str().map(ToString::to_string))
        .unwrap_or_default()
}

async fn live_element_context(
    page: &Page,
    uid: &str,
    backend_node_id: i64,
    ancestor_depth: u32,
    sibling_limit: u32,
    child_limit: u32,
    max_chars: u32,
) -> Result<serde_json::Value, CallToolResult> {
    let resolve_params = ResolveNodeParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .build();
    let remote_object = page.execute(resolve_params).await.map_err(|e| {
        cdp_error(format!(
            "Element uid={} could not be resolved to a DOM node: {}",
            uid, e
        ))
    })?;
    let object_id = remote_object.result.object.object_id.ok_or_else(|| {
        cdp_error(format!(
            "Element uid={} could not be resolved to a DOM node.",
            uid
        ))
    })?;

    let context_fn = r#"function(ancestorDepth, siblingLimit, childLimit, maxChars) {
        const normalize = value => (value || "").replace(/\s+/g, " ").trim();
        const truncate = value => {
            const text = normalize(value);
            return text.length > maxChars ? text.substring(0, maxChars) : text;
        };
        const rectFor = el => {
            const rect = el.getBoundingClientRect();
            return {
                x: Math.round(rect.x * 10) / 10,
                y: Math.round(rect.y * 10) / 10,
                width: Math.round(rect.width * 10) / 10,
                height: Math.round(rect.height * 10) / 10,
            };
        };
        const roleFor = el => {
            const aria = el.getAttribute("role");
            if (aria) return aria;
            const tag = el.tagName;
            if (tag === "BUTTON" || (tag === "INPUT" && ["submit", "button", "reset"].includes(el.type))) return "button";
            if (tag === "A" && el.hasAttribute("href")) return "link";
            if (tag === "INPUT") {
                const type = el.type || "text";
                if (type === "checkbox") return "checkbox";
                if (type === "radio") return "radio";
                if (type === "search") return "searchbox";
                return "textbox";
            }
            if (tag === "TEXTAREA") return "textbox";
            if (tag === "SELECT") return "combobox";
            if (el.isContentEditable) return "textbox";
            return tag.toLowerCase();
        };
        const summarize = el => ({
            tag: el.tagName.toLowerCase(),
            role: roleFor(el),
            text: truncate(el.innerText || el.textContent || ""),
            aria_label: truncate(el.getAttribute("aria-label") || ""),
            title: truncate(el.getAttribute("title") || ""),
            placeholder: truncate(el.getAttribute("placeholder") || el.getAttribute("data-placeholder") || ""),
            value: truncate((el.tagName === "INPUT" || el.tagName === "TEXTAREA" || el.tagName === "SELECT") ? el.value : ""),
            test_id: truncate(el.getAttribute("data-testid") || el.getAttribute("data-test") || el.getAttribute("data-cy") || ""),
            disabled: el.disabled === true || el.getAttribute("aria-disabled") === "true",
            rect: rectFor(el),
        });

        const ancestors = [];
        let parent = this.parentElement;
        while (parent && ancestors.length < ancestorDepth) {
            ancestors.push(summarize(parent));
            parent = parent.parentElement;
        }

        const siblings = [];
        if (this.parentElement) {
            const children = Array.from(this.parentElement.children);
            const index = children.indexOf(this);
            const start = Math.max(0, index - siblingLimit);
            const end = Math.min(children.length, index + siblingLimit + 1);
            for (let i = start; i < end; i++) {
                if (children[i] !== this) siblings.push(summarize(children[i]));
            }
        }

        const children = Array.from(this.children).slice(0, childLimit).map(summarize);
        return {
            element: summarize(this),
            ancestors,
            siblings,
            children,
        };
    }"#;

    let call_params = CallFunctionOnParams::builder()
        .function_declaration(context_fn)
        .object_id(object_id.clone())
        .arguments(vec![
            CallArgument::builder()
                .value(serde_json::Value::from(ancestor_depth))
                .build(),
            CallArgument::builder()
                .value(serde_json::Value::from(sibling_limit))
                .build(),
            CallArgument::builder()
                .value(serde_json::Value::from(child_limit))
                .build(),
            CallArgument::builder()
                .value(serde_json::Value::from(max_chars))
                .build(),
        ])
        .return_by_value(true)
        .await_promise(true)
        .build()
        .map_err(|e| cdp_error(format!("Failed to build context call params: {}", e)))?;

    let call_result = page.execute(call_params).await;
    let _ = page.execute(ReleaseObjectParams::new(object_id)).await;

    match call_result {
        Ok(resp) => {
            if let Some(exc) = &resp.result.exception_details {
                return Err(cdp_error(format!("JavaScript exception: {}", exc.text)));
            }
            Ok(resp
                .result
                .result
                .value
                .as_ref()
                .cloned()
                .unwrap_or(serde_json::Value::Null))
        }
        Err(e) => Err(cdp_error(format!(
            "Failed to expand element context for uid={}: {}",
            uid, e
        ))),
    }
}

pub async fn cdp_summarize_page(cdp_client: Arc<RwLock<Option<CdpClient>>>) -> CallToolResult {
    let guard = cdp_client.read().await;
    let client = match guard.as_ref() {
        Some(c) => c,
        None => return cdp_error("No CDP connection. Use cdp_connect first."),
    };

    let page = match client.require_page() {
        Ok(p) => p,
        Err(e) => return e,
    };

    let page_url = page_url(&page).await;
    let title = page_title(&page).await;
    let generation = client.generation;
    let walker_js = crate::cdp::dom_discovery::dom_walker_js("", None, 0);
    let (_candidates, inventory) = match resolve_dom_candidates(&page, &walker_js).await {
        Ok(result) => result,
        Err(e) => return e,
    };

    let result = serde_json::json!({
        "page_url": page_url,
        "title": title,
        "source": "dom_summary",
        "snapshot_generation": generation,
        "inventory": inventory,
    });

    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )])
}

pub async fn cdp_get_element_context(
    uid: String,
    ancestor_depth: Option<u32>,
    sibling_limit: Option<u32>,
    child_limit: Option<u32>,
    max_chars: Option<u32>,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    let guard = cdp_client.read().await;
    let client = match guard.as_ref() {
        Some(c) => c,
        None => return cdp_error("No CDP connection. Use cdp_connect first."),
    };

    let page = match client.require_page() {
        Ok(p) => p,
        Err(e) => return e,
    };

    let current_url = page_url(&page).await;
    let snapshot =
        match client.last_dom_snapshot.as_ref() {
            Some(snapshot) => snapshot,
            None => return cdp_error(
                "No DOM snapshot available. Call cdp_find_elements or cdp_take_dom_snapshot before cdp_get_element_context.",
            ),
        };
    let node = match crate::cdp::resolve_uid_from_maps(
        &uid,
        Some(snapshot),
        client.generation,
        &current_url,
    ) {
        Ok(node) => node,
        Err(msg) => return cdp_error(msg),
    };

    let generation = snapshot.generation;
    let stored_element = snapshot
        .uid_to_candidate
        .get(&uid)
        .map(|candidate| dom_candidate_json(&uid, candidate))
        .unwrap_or_else(|| snapshot_node_json(&uid, node));
    let nearby =
        nearby_snapshot_candidates(snapshot, &uid, sibling_limit.unwrap_or(2).min(10) as usize);
    let backend_node_id = node.backend_node_id;

    let live_context = match live_element_context(
        &page,
        &uid,
        backend_node_id,
        ancestor_depth.unwrap_or(3).min(8),
        sibling_limit.unwrap_or(2).min(10),
        child_limit.unwrap_or(8).min(50),
        max_chars.unwrap_or(240).clamp(40, 1000),
    )
    .await
    {
        Ok(context) => context,
        Err(e) => return e,
    };

    let result = serde_json::json!({
        "page_url": current_url,
        "source": "dom_context",
        "uid": uid,
        "snapshot_generation": generation,
        "element": stored_element,
        "nearby_snapshot_matches": nearby,
        "live_context": live_context,
    });

    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )])
}

pub async fn cdp_find_elements(
    query: String,
    role: Option<String>,
    max_results: Option<u32>,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    let mut guard = cdp_client.write().await;
    let client = match guard.as_mut() {
        Some(c) => c,
        None => return cdp_error("No CDP connection. Use cdp_connect first."),
    };

    let page = match client.require_page() {
        Ok(p) => p,
        Err(e) => return e,
    };

    let max = max_results.unwrap_or(10);
    let page_url = page_url(&page).await;
    let generation = client.generation;

    let walker_js = crate::cdp::dom_discovery::dom_walker_js(&query, role.as_deref(), max);

    let (candidates, inventory) = match resolve_dom_candidates(&page, &walker_js).await {
        Ok(result) => result,
        Err(e) => return e,
    };

    // Build snapshot map and format response
    let snapshot_map =
        crate::cdp::dom_discovery::build_dom_snapshot(&candidates, page_url.clone(), generation);

    let matches_json: Vec<serde_json::Value> = candidates
        .iter()
        .enumerate()
        .map(|(i, n)| dom_candidate_json(&format!("d{}", i + 1), n))
        .collect();

    client.last_dom_snapshot = Some(snapshot_map);

    let result = serde_json::json!({
        "page_url": page_url,
        "source": "dom",
        "matches": matches_json,
        "inventory": inventory,
    });

    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&result).unwrap_or_default(),
    )])
}

pub async fn cdp_take_dom_snapshot(
    max_nodes: Option<u32>,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    let mut guard = cdp_client.write().await;
    let client = match guard.as_mut() {
        Some(c) => c,
        None => return cdp_error("No CDP connection. Use cdp_connect first."),
    };

    let page = match client.require_page() {
        Ok(p) => p,
        Err(e) => return e,
    };

    let max = max_nodes.unwrap_or(500);
    let page_url = page_url(&page).await;
    let generation = client.generation;

    // Use empty query to match all interactive elements
    let walker_js = crate::cdp::dom_discovery::dom_walker_js("", None, max);

    let (candidates, _inventory) = match resolve_dom_candidates(&page, &walker_js).await {
        Ok(result) => result,
        Err(e) => return e,
    };

    let snapshot_map =
        crate::cdp::dom_discovery::build_dom_snapshot(&candidates, page_url, generation);

    let output = crate::cdp::dom_discovery::format_dom_snapshot(&candidates);
    client.last_dom_snapshot = Some(snapshot_map);

    CallToolResult::success(vec![Content::text(output)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chromiumoxide::cdp::js_protocol::runtime::ExceptionDetails;

    #[test]
    fn request_timeout_outlasts_longest_page_change_wait() {
        let longest_wait = std::time::Duration::from_millis(MAX_PAGE_CHANGE_WAIT_TIMEOUT_MS);
        assert!(
            CDP_REQUEST_TIMEOUT > longest_wait,
            "CDP request timeout {:?} must exceed the longest in-page wait {:?}",
            CDP_REQUEST_TIMEOUT,
            longest_wait
        );
    }

    #[test]
    fn javascript_exception_maps_to_exception_with_its_text() {
        let details = ExceptionDetails::new(1, "Uncaught TypeError: boom", 0, 0);
        let err = JsCallError::from(CdpError::JavascriptException(Box::new(details)));
        assert_eq!(
            err,
            JsCallError::Exception("Uncaught TypeError: boom".to_string())
        );
    }

    #[test]
    fn request_timeout_maps_to_transport_error_naming_the_limit() {
        let err = JsCallError::from(CdpError::Timeout);
        assert_eq!(
            err,
            JsCallError::Transport("no response within the 65 s CDP request timeout".to_string())
        );
    }
}
