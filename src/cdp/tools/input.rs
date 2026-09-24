//! CDP input tools: click, hover, fill, press_key, type_text.

use super::{resolve_element_center, resolve_node, resolve_to_object_id};
use crate::cdp::{cdp_error, CdpClient};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::js_protocol::runtime::{CallArgument, CallFunctionOnParams};
use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Cap for snapshots auto-appended after an action. Smaller than the
/// user-facing `cdp_take_dom_snapshot` default (500) because the "quick
/// look after click/hover/fill" use case doesn't need the full page, and
/// every extra element costs three CDP round trips.
const AUTO_SNAPSHOT_MAX_NODES: u32 = 100;

/// Append a snapshot to an existing tool result if `include_snapshot` is true.
async fn maybe_append_snapshot(
    mut result: CallToolResult,
    include_snapshot: bool,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    if !include_snapshot {
        return result;
    }
    let snapshot =
        super::script::cdp_take_dom_snapshot(Some(AUTO_SNAPSHOT_MAX_NODES), cdp_client).await;
    result.content.extend(snapshot.content);
    result
}

async fn invalidate_snapshot_cache(cdp_client: Arc<RwLock<Option<CdpClient>>>) {
    if let Some(client) = cdp_client.write().await.as_mut() {
        client.invalidate_snapshots();
    }
}

async fn finish_after_action(
    result: CallToolResult,
    include_snapshot: bool,
    cdp_client: Arc<RwLock<Option<CdpClient>>>,
) -> CallToolResult {
    invalidate_snapshot_cache(cdp_client.clone()).await;
    maybe_append_snapshot(result, include_snapshot, cdp_client).await
}

fn observed_fill_status(strategy: &str, observed_text: &str, value: &str) -> &'static str {
    let matched = if strategy == "select_value" {
        observed_text
            .lines()
            .any(|part| part.trim() == value || part.contains(value))
    } else {
        observed_text.contains(value)
    };
    if matched {
        "observed_text=true"
    } else {
        "observed_text=false"
    }
}

pub async fn cdp_click(
    uid: String,
    dbl_click: bool,
    include_snapshot: bool,
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

    let (node_role, node_name, cx, cy) = match resolve_element_center(&uid, client, &page).await {
        Ok(v) => v,
        Err(e) => return e,
    };

    drop(guard);

    let click_count = if dbl_click { 2_i64 } else { 1_i64 };

    let move_event = DispatchMouseEventParams::new(DispatchMouseEventType::MouseMoved, cx, cy);

    let mut press_event =
        DispatchMouseEventParams::new(DispatchMouseEventType::MousePressed, cx, cy);
    press_event.button = Some(MouseButton::Left);
    press_event.buttons = Some(1);
    press_event.click_count = Some(click_count);

    let mut release_event =
        DispatchMouseEventParams::new(DispatchMouseEventType::MouseReleased, cx, cy);
    release_event.button = Some(MouseButton::Left);
    release_event.click_count = Some(click_count);

    for event in [move_event, press_event, release_event] {
        if let Err(e) = page.execute(event).await {
            return cdp_error(format!("Click failed on uid={}: {}", uid, e));
        }
    }

    let dbl_note = if dbl_click { " (double-click)" } else { "" };
    let result = CallToolResult::success(vec![ContentBlock::text(format!(
        "Clicked uid={} '{}' ({}) at ({:.1}, {:.1}){}",
        uid, node_name, node_role, cx, cy, dbl_note
    ))]);
    finish_after_action(result, include_snapshot, cdp_client).await
}

pub async fn cdp_hover(
    uid: String,
    include_snapshot: bool,
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

    let (node_role, node_name, cx, cy) = match resolve_element_center(&uid, client, &page).await {
        Ok(v) => v,
        Err(e) => return e,
    };

    drop(guard);

    let move_event = DispatchMouseEventParams::new(DispatchMouseEventType::MouseMoved, cx, cy);
    if let Err(e) = page.execute(move_event).await {
        return cdp_error(format!("Hover failed on uid={}: {}", uid, e));
    }

    let result = CallToolResult::success(vec![ContentBlock::text(format!(
        "Hovered uid={} '{}' ({}) at ({:.1}, {:.1})",
        uid, node_name, node_role, cx, cy
    ))]);
    finish_after_action(result, include_snapshot, cdp_client).await
}

pub async fn cdp_fill(
    uid: String,
    value: String,
    include_snapshot: bool,
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

    let current_url = crate::cdp::page_url(&page).await;
    let (backend_node_id, node_role, node_name) = match resolve_node(&uid, client, &current_url) {
        Ok(v) => v,
        Err(e) => return e,
    };

    drop(guard);

    let object_id = match resolve_to_object_id(&uid, backend_node_id, &page).await {
        Ok(id) => id,
        Err(e) => return e,
    };

    let fill_fn = r#"function(value) {
        function textOf(el) {
            if (!el) return "";
            if (el.tagName === "SELECT") {
                const selected = el.options && el.selectedIndex >= 0 ? el.options[el.selectedIndex] : null;
                const selectedValue = selected ? selected.value : (el.value || "");
                const selectedText = selected ? (selected.textContent || "").replace(/\s+/g, " ").trim() : "";
                return [selectedValue, selectedText].filter(Boolean).join("\n");
            }
            if ("value" in el) return el.value || "";
            return (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim();
        }

        function findRichEditor(el) {
            if (el && el.isContentEditable) return el;
            if (!el || !el.querySelector) return null;
            return el.querySelector([
                "[contenteditable='true']",
                "[contenteditable='plaintext-only']",
                ".ql-editor",
                ".ProseMirror",
                "[data-lexical-editor='true']",
                "[role='textbox'][contenteditable]"
            ].join(","));
        }

        function selectEditableContents(el) {
            el.focus({ preventScroll: true });
            const doc = el.ownerDocument || document;
            const selection = doc.getSelection && doc.getSelection();
            if (!selection) return;
            const range = doc.createRange();
            range.selectNodeContents(el);
            selection.removeAllRanges();
            selection.addRange(range);
        }

        function setNativeValue(el, nextValue) {
            const proto = el.tagName === "TEXTAREA"
                ? HTMLTextAreaElement.prototype
                : HTMLInputElement.prototype;
            const setter = Object.getOwnPropertyDescriptor(proto, "value")?.set;
            if (setter) setter.call(el, nextValue);
            else el.value = nextValue;
            el.dispatchEvent(new InputEvent("input", {
                bubbles: true,
                composed: true,
                inputType: "insertText",
                data: nextValue
            }));
            el.dispatchEvent(new Event("change", { bubbles: true }));
        }

        if (this.tagName === 'SELECT') {
            const option = Array.from(this.options).find(o => o.value === value || o.textContent.trim() === value);
            if (!option) throw new Error('Option not found: ' + value);
            this.value = option.value;
            this.dispatchEvent(new Event('input', { bubbles: true }));
            this.dispatchEvent(new Event('change', { bubbles: true }));
            return { strategy: "select_value", observedText: textOf(this) };
        }

        if (this.tagName === "INPUT" || this.tagName === "TEXTAREA") {
            this.focus({ preventScroll: true });
            if (this.select) this.select();
            setNativeValue(this, value);
            return { strategy: "native_value_setter", observedText: textOf(this) };
        }

        const richEditor = findRichEditor(this);
        if (richEditor) {
            selectEditableContents(richEditor);
            return {
                strategy: "rich_editor_keyboard",
                observedText: textOf(richEditor),
                targetTag: richEditor.tagName.toLowerCase(),
                targetClass: String(richEditor.className || "")
            };
        }

        this.focus();
        if (this.select) this.select();
        else document.execCommand('selectAll', false, null);
        document.execCommand('insertText', false, value);
        return { strategy: "exec_command", observedText: textOf(this) };
    }"#;

    let call_params = match CallFunctionOnParams::builder()
        .function_declaration(fill_fn)
        .object_id(object_id.clone())
        .arguments(vec![CallArgument::builder()
            .value(serde_json::Value::String(value.clone()))
            .build()])
        .await_promise(true)
        .return_by_value(true)
        .build()
    {
        Ok(p) => p,
        Err(e) => return cdp_error(format!("Failed to build call params: {}", e)),
    };

    let prep = match page.execute(call_params).await {
        Ok(resp) => {
            if let Some(exc) = &resp.result.exception_details {
                return cdp_error(format!("Fill failed: {}", exc.text));
            }
            resp.result.result.value.unwrap_or(Value::Null)
        }
        Err(e) => return cdp_error(format!("Fill failed on uid={}: {}", uid, e)),
    };

    let strategy = prep
        .get("strategy")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    if strategy == "rich_editor_keyboard" {
        if let Err(e) = page.execute(InsertTextParams::new(value.clone())).await {
            return cdp_error(format!(
                "Fill failed on uid={} with CDP text insertion: {}",
                uid, e
            ));
        }
    }

    let verify_fn = r#"function() {
        function textOf(el) {
            if (!el) return "";
            if (el.tagName === "SELECT") {
                const selected = el.options && el.selectedIndex >= 0 ? el.options[el.selectedIndex] : null;
                const selectedValue = selected ? selected.value : (el.value || "");
                const selectedText = selected ? (selected.textContent || "").replace(/\s+/g, " ").trim() : "";
                return [selectedValue, selectedText].filter(Boolean).join("\n");
            }
            if ("value" in el) return el.value || "";
            return (el.innerText || el.textContent || "").replace(/\s+/g, " ").trim();
        }
        function findRichEditor(el) {
            if (el && el.isContentEditable) return el;
            if (!el || !el.querySelector) return null;
            return el.querySelector([
                "[contenteditable='true']",
                "[contenteditable='plaintext-only']",
                ".ql-editor",
                ".ProseMirror",
                "[data-lexical-editor='true']",
                "[role='textbox'][contenteditable]"
            ].join(","));
        }
        const target = findRichEditor(this) || this;
        return { observedText: textOf(target), active: document.activeElement === target };
    }"#;
    let verify_params = match CallFunctionOnParams::builder()
        .function_declaration(verify_fn)
        .object_id(object_id)
        .return_by_value(true)
        .await_promise(true)
        .build()
    {
        Ok(p) => p,
        Err(e) => return cdp_error(format!("Failed to build verification params: {}", e)),
    };
    let observed_text = match page.execute(verify_params).await {
        Ok(resp) => {
            if let Some(exc) = &resp.result.exception_details {
                return cdp_error(format!("Fill verification failed: {}", exc.text));
            }
            resp.result
                .result
                .value
                .and_then(|v| {
                    v.get("observedText")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default()
        }
        Err(e) => return cdp_error(format!("Fill verification failed on uid={}: {}", uid, e)),
    };

    let observed = observed_fill_status(strategy, &observed_text, &value);
    let rich_hint = if strategy == "rich_editor_keyboard" {
        "; rich editor used CDP keyboard insertion. If this is a chat composer and the message is ready, use cdp_press_key({\"key\":\"Enter\"}) or find/click an enabled Send control to submit."
    } else {
        ""
    };

    let result = CallToolResult::success(vec![ContentBlock::text(format!(
        "Filled uid={} '{}' ({}) with '{}' (strategy={}, {}{})",
        uid, node_name, node_role, value, strategy, observed, rich_hint
    ))]);
    finish_after_action(result, include_snapshot, cdp_client).await
}

/// Map a key name to its CDP key identifier, code, and Windows virtual key code.
fn key_definition(key: &str) -> Option<(&'static str, &'static str, i64)> {
    Some(match key {
        "Enter" => ("Enter", "Enter", 13),
        "Tab" => ("Tab", "Tab", 9),
        "Escape" => ("Escape", "Escape", 27),
        "Backspace" => ("Backspace", "Backspace", 8),
        "Delete" => ("Delete", "Delete", 46),
        "ArrowUp" => ("ArrowUp", "ArrowUp", 38),
        "ArrowDown" => ("ArrowDown", "ArrowDown", 40),
        "ArrowLeft" => ("ArrowLeft", "ArrowLeft", 37),
        "ArrowRight" => ("ArrowRight", "ArrowRight", 39),
        "Home" => ("Home", "Home", 36),
        "End" => ("End", "End", 35),
        "PageUp" => ("PageUp", "PageUp", 33),
        "PageDown" => ("PageDown", "PageDown", 34),
        "Space" | " " => (" ", "Space", 32),
        "F1" => ("F1", "F1", 112),
        "F2" => ("F2", "F2", 113),
        "F3" => ("F3", "F3", 114),
        "F4" => ("F4", "F4", 115),
        "F5" => ("F5", "F5", 116),
        "F6" => ("F6", "F6", 117),
        "F7" => ("F7", "F7", 118),
        "F8" => ("F8", "F8", 119),
        "F9" => ("F9", "F9", 120),
        "F10" => ("F10", "F10", 121),
        "F11" => ("F11", "F11", 122),
        "F12" => ("F12", "F12", 123),
        _ if key.len() == 1 => return None,
        _ => return None,
    })
}

/// Map a single character to its DOM `code` and Windows virtual key code.
fn char_key_code(ch: char) -> (&'static str, i64) {
    match ch {
        'a'..='z' | 'A'..='Z' => {
            let upper = ch.to_ascii_uppercase();
            let code = match upper {
                'A' => "KeyA",
                'B' => "KeyB",
                'C' => "KeyC",
                'D' => "KeyD",
                'E' => "KeyE",
                'F' => "KeyF",
                'G' => "KeyG",
                'H' => "KeyH",
                'I' => "KeyI",
                'J' => "KeyJ",
                'K' => "KeyK",
                'L' => "KeyL",
                'M' => "KeyM",
                'N' => "KeyN",
                'O' => "KeyO",
                'P' => "KeyP",
                'Q' => "KeyQ",
                'R' => "KeyR",
                'S' => "KeyS",
                'T' => "KeyT",
                'U' => "KeyU",
                'V' => "KeyV",
                'W' => "KeyW",
                'X' => "KeyX",
                'Y' => "KeyY",
                'Z' => "KeyZ",
                _ => unreachable!(),
            };
            (code, upper as i64)
        }
        '0' => ("Digit0", 0x30),
        '1' => ("Digit1", 0x31),
        '2' => ("Digit2", 0x32),
        '3' => ("Digit3", 0x33),
        '4' => ("Digit4", 0x34),
        '5' => ("Digit5", 0x35),
        '6' => ("Digit6", 0x36),
        '7' => ("Digit7", 0x37),
        '8' => ("Digit8", 0x38),
        '9' => ("Digit9", 0x39),
        '-' => ("Minus", 0xBD),
        '=' | '+' => ("Equal", 0xBB),
        '[' => ("BracketLeft", 0xDB),
        ']' => ("BracketRight", 0xDD),
        '\\' => ("Backslash", 0xDC),
        ';' => ("Semicolon", 0xBA),
        '\'' => ("Quote", 0xDE),
        ',' => ("Comma", 0xBC),
        '.' => ("Period", 0xBE),
        '/' => ("Slash", 0xBF),
        '`' => ("Backquote", 0xC0),
        _ => ("Unidentified", 0),
    }
}

const MODIFIER_ALT: i64 = 1;
const MODIFIER_CONTROL: i64 = 2;
const MODIFIER_META: i64 = 4;
const MODIFIER_SHIFT: i64 = 8;

/// Map a modifier name to its CDP bitmask.
fn modifier_bit(name: &str) -> Option<i64> {
    match name {
        "Alt" => Some(MODIFIER_ALT),
        "Control" => Some(MODIFIER_CONTROL),
        "Meta" => Some(MODIFIER_META),
        "Shift" => Some(MODIFIER_SHIFT),
        _ => None,
    }
}

/// Parsed key combination: modifier bitmask + main key name.
#[derive(Debug)]
struct ParsedKeyCombo {
    modifiers: i64,
    modifier_names: Vec<String>,
    main_key: String,
}

/// Parse a key combo string like "Control+Shift+A" or "Control++".
/// Returns Err with the unknown modifier name on failure.
fn parse_key_combo(key: &str) -> Result<ParsedKeyCombo, String> {
    let parts: Vec<&str> = key.split('+').collect();

    let (modifier_parts, main_key) = if key.ends_with("++") {
        (&parts[..parts.len() - 2], "+")
    } else if parts.len() > 1 {
        (&parts[..parts.len() - 1], *parts.last().unwrap_or(&""))
    } else {
        (&[][..], parts[0])
    };

    let mut modifiers: i64 = 0;
    let mut modifier_names = Vec::new();
    for &m in modifier_parts {
        match modifier_bit(m) {
            Some(bit) => {
                modifiers |= bit;
                modifier_names.push(m.to_string());
            }
            None => return Err(m.to_string()),
        }
    }

    Ok(ParsedKeyCombo {
        modifiers,
        modifier_names,
        main_key: main_key.to_string(),
    })
}

/// Dispatch a named key press (RawKeyDown + KeyUp) with optional modifiers.
async fn dispatch_named_key(
    page: &chromiumoxide::page::Page,
    key_val: &str,
    code: &str,
    vk: i64,
    modifiers: i64,
) -> Result<(), String> {
    let mut down = DispatchKeyEventParams::new(DispatchKeyEventType::RawKeyDown);
    down.key = Some(key_val.to_string());
    down.code = Some(code.to_string());
    down.windows_virtual_key_code = Some(vk);
    down.modifiers = Some(modifiers);
    page.execute(down)
        .await
        .map_err(|e| format!("Failed to press key {}: {}", key_val, e))?;

    let mut up = DispatchKeyEventParams::new(DispatchKeyEventType::KeyUp);
    up.key = Some(key_val.to_string());
    up.code = Some(code.to_string());
    up.windows_virtual_key_code = Some(vk);
    up.modifiers = Some(modifiers);
    page.execute(up)
        .await
        .map_err(|e| format!("Failed to release key {}: {}", key_val, e))?;

    Ok(())
}

/// Dispatch a single character key press (RawKeyDown + Char + KeyUp).
async fn dispatch_char(
    page: &chromiumoxide::page::Page,
    ch: char,
    modifiers: i64,
) -> Result<(), String> {
    let (code, vk) = char_key_code(ch);

    let mut down = DispatchKeyEventParams::new(DispatchKeyEventType::RawKeyDown);
    down.key = Some(ch.to_string());
    down.code = Some(code.to_string());
    down.windows_virtual_key_code = Some(vk);
    down.modifiers = Some(modifiers);
    page.execute(down)
        .await
        .map_err(|e| format!("Failed to press key {}: {}", ch, e))?;

    // Only send Char event if no modifiers (otherwise it's a shortcut).
    if modifiers == 0 || modifiers == MODIFIER_SHIFT {
        let mut char_event = DispatchKeyEventParams::new(DispatchKeyEventType::Char);
        char_event.text = Some(ch.to_string());
        char_event.modifiers = Some(modifiers);
        let _ = page.execute(char_event).await;
    }

    let mut up = DispatchKeyEventParams::new(DispatchKeyEventType::KeyUp);
    up.key = Some(ch.to_string());
    up.code = Some(code.to_string());
    up.windows_virtual_key_code = Some(vk);
    up.modifiers = Some(modifiers);
    page.execute(up)
        .await
        .map_err(|e| format!("Failed to release key {}: {}", ch, e))?;

    Ok(())
}

pub async fn cdp_press_key(
    key: String,
    include_snapshot: bool,
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

    drop(guard);

    let combo = match parse_key_combo(&key) {
        Ok(c) => c,
        Err(unknown) => {
            return cdp_error(format!(
                "Unknown modifier '{}'. Use Control, Shift, Alt, or Meta.",
                unknown
            ))
        }
    };
    let modifiers = combo.modifiers;
    let main_key = &combo.main_key;

    // Dispatch modifier key-downs.
    for m in &combo.modifier_names {
        let mut params = DispatchKeyEventParams::new(DispatchKeyEventType::KeyDown);
        params.key = Some(m.clone());
        params.modifiers = Some(modifiers);
        if let Err(e) = page.execute(params).await {
            return cdp_error(format!("Failed to press modifier {}: {}", m, e));
        }
    }

    // Dispatch main key.
    if let Some((key_val, code, vk)) = key_definition(main_key) {
        if let Err(e) = dispatch_named_key(&page, key_val, code, vk, modifiers).await {
            return cdp_error(e);
        }
    } else if main_key.len() == 1 {
        let ch = main_key.chars().next().unwrap_or(' ');
        if let Err(e) = dispatch_char(&page, ch, modifiers).await {
            return cdp_error(e);
        }
    } else {
        return cdp_error(format!(
            "Unknown key '{}'. Use key names like Enter, Tab, ArrowUp, or single characters.",
            main_key
        ));
    }

    // Release modifier keys in reverse order.
    for m in combo.modifier_names.iter().rev() {
        let mut params = DispatchKeyEventParams::new(DispatchKeyEventType::KeyUp);
        params.key = Some(m.clone());
        let _ = page.execute(params).await;
    }

    let result = CallToolResult::success(vec![ContentBlock::text(format!("Pressed key: {}", key))]);
    finish_after_action(result, include_snapshot, cdp_client).await
}

pub async fn cdp_type_text(
    text: String,
    submit_key: Option<String>,
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

    // Validate submit_key before typing so we fail fast on bad input.
    let submit_def = if let Some(ref sk) = submit_key {
        match key_definition(sk) {
            Some(def) => Some(def),
            None => {
                return cdp_error(format!(
                    "Unknown submit key '{}'. Use key names like Enter, Tab, Escape.",
                    sk
                ))
            }
        }
    } else {
        None
    };

    drop(guard);

    for ch in text.chars() {
        if let Err(e) = dispatch_char(&page, ch, 0).await {
            return cdp_error(e);
        }
    }

    if let Some((key_val, code, vk)) = submit_def {
        if let Err(e) = dispatch_named_key(&page, key_val, code, vk, 0).await {
            return cdp_error(e);
        }
    }

    let suffix = submit_key
        .as_ref()
        .map(|k| format!(" + {}", k))
        .unwrap_or_default();
    invalidate_snapshot_cache(cdp_client).await;
    CallToolResult::success(vec![ContentBlock::text(format!(
        "Typed text \"{}{}\"",
        text, suffix
    ))])
}

#[cfg(test)]
mod tests {
    use super::*;

    // MARK: - key_definition tests

    #[test]
    fn observed_fill_status_accepts_select_value_or_visible_text() {
        let observed = "us\nUnited States";
        assert_eq!(
            observed_fill_status("select_value", observed, "us"),
            "observed_text=true"
        );
        assert_eq!(
            observed_fill_status("select_value", observed, "United States"),
            "observed_text=true"
        );
    }

    #[test]
    fn key_definition_returns_enter() {
        let (key, code, vk) = key_definition("Enter").unwrap();
        assert_eq!(key, "Enter");
        assert_eq!(code, "Enter");
        assert_eq!(vk, 13);
    }

    #[test]
    fn key_definition_returns_tab() {
        let (_, _, vk) = key_definition("Tab").unwrap();
        assert_eq!(vk, 9);
    }

    #[test]
    fn key_definition_returns_arrow_keys() {
        assert_eq!(key_definition("ArrowUp").unwrap().2, 38);
        assert_eq!(key_definition("ArrowDown").unwrap().2, 40);
        assert_eq!(key_definition("ArrowLeft").unwrap().2, 37);
        assert_eq!(key_definition("ArrowRight").unwrap().2, 39);
    }

    #[test]
    fn key_definition_returns_space() {
        let (key, code, vk) = key_definition("Space").unwrap();
        assert_eq!(key, " ");
        assert_eq!(code, "Space");
        assert_eq!(vk, 32);
        // Also works with literal space.
        assert!(key_definition(" ").is_some());
    }

    #[test]
    fn key_definition_returns_f_keys() {
        assert_eq!(key_definition("F1").unwrap().2, 112);
        assert_eq!(key_definition("F12").unwrap().2, 123);
    }

    #[test]
    fn key_definition_returns_none_for_single_char() {
        assert!(key_definition("a").is_none());
        assert!(key_definition("Z").is_none());
        assert!(key_definition("1").is_none());
    }

    #[test]
    fn key_definition_returns_none_for_unknown() {
        assert!(key_definition("FooBar").is_none());
        assert!(key_definition("").is_none());
    }

    // MARK: - modifier_bit tests

    #[test]
    fn modifier_bit_returns_correct_values() {
        assert_eq!(modifier_bit("Alt"), Some(1));
        assert_eq!(modifier_bit("Control"), Some(2));
        assert_eq!(modifier_bit("Meta"), Some(4));
        assert_eq!(modifier_bit("Shift"), Some(8));
    }

    #[test]
    fn modifier_bit_returns_none_for_unknown() {
        assert_eq!(modifier_bit("Ctrl"), None);
        assert_eq!(modifier_bit("alt"), None);
        assert_eq!(modifier_bit(""), None);
    }

    // MARK: - parse_key_combo tests

    #[test]
    fn parse_single_key() {
        let combo = parse_key_combo("Enter").unwrap();
        assert_eq!(combo.main_key, "Enter");
        assert_eq!(combo.modifiers, 0);
        assert!(combo.modifier_names.is_empty());
    }

    #[test]
    fn parse_single_character() {
        let combo = parse_key_combo("a").unwrap();
        assert_eq!(combo.main_key, "a");
        assert_eq!(combo.modifiers, 0);
    }

    #[test]
    fn parse_control_a() {
        let combo = parse_key_combo("Control+A").unwrap();
        assert_eq!(combo.main_key, "A");
        assert_eq!(combo.modifiers, MODIFIER_CONTROL);
        assert_eq!(combo.modifier_names, vec!["Control"]);
    }

    #[test]
    fn parse_control_shift_r() {
        let combo = parse_key_combo("Control+Shift+R").unwrap();
        assert_eq!(combo.main_key, "R");
        assert_eq!(combo.modifiers, MODIFIER_CONTROL | MODIFIER_SHIFT);
        assert_eq!(combo.modifier_names, vec!["Control", "Shift"]);
    }

    #[test]
    fn parse_control_plus_key() {
        // "Control++" means Control + the '+' key.
        let combo = parse_key_combo("Control++").unwrap();
        assert_eq!(combo.main_key, "+");
        assert_eq!(combo.modifiers, MODIFIER_CONTROL);
    }

    #[test]
    fn parse_all_modifiers() {
        let combo = parse_key_combo("Alt+Control+Meta+Shift+x").unwrap();
        assert_eq!(combo.main_key, "x");
        assert_eq!(
            combo.modifiers,
            MODIFIER_ALT | MODIFIER_CONTROL | MODIFIER_META | MODIFIER_SHIFT
        );
        assert_eq!(combo.modifier_names.len(), 4);
    }

    #[test]
    fn parse_unknown_modifier_returns_error() {
        let err = parse_key_combo("Ctrl+A").unwrap_err();
        assert_eq!(err, "Ctrl");
    }

    #[test]
    fn parse_meta_enter() {
        let combo = parse_key_combo("Meta+Enter").unwrap();
        assert_eq!(combo.main_key, "Enter");
        assert_eq!(combo.modifiers, MODIFIER_META);
    }

    // MARK: - char_key_code tests

    #[test]
    fn char_key_code_letters() {
        assert_eq!(char_key_code('a'), ("KeyA", 65));
        assert_eq!(char_key_code('Z'), ("KeyZ", 90));
    }

    #[test]
    fn char_key_code_digits() {
        assert_eq!(char_key_code('0'), ("Digit0", 0x30));
        assert_eq!(char_key_code('9'), ("Digit9", 0x39));
    }

    #[test]
    fn char_key_code_punctuation() {
        assert_eq!(char_key_code('+'), ("Equal", 0xBB));
        assert_eq!(char_key_code('-'), ("Minus", 0xBD));
        assert_eq!(char_key_code('/'), ("Slash", 0xBF));
        assert_eq!(char_key_code('.'), ("Period", 0xBE));
        assert_eq!(char_key_code(','), ("Comma", 0xBC));
        assert_eq!(char_key_code(';'), ("Semicolon", 0xBA));
    }

    #[test]
    fn char_key_code_unknown() {
        assert_eq!(char_key_code('€'), ("Unidentified", 0));
    }
}
