//! macOS Accessibility API (AXUIElement) helpers.
//!
//! Uses the macOS Accessibility tree for:
//! - Text search: find UI elements by name (faster than OCR for standard controls)
//! - Window raising: bring windows to front via AXRaise (works for bundle-less apps)
//! - Single-window raising: map a `CGWindowID` to its AX window and raise only that one

use super::ocr::{TextBounds, TextMatch};
use crate::tools::ax_snapshot::{map_ax_role, AXSnapshotNode};
use core_foundation::array::{kCFTypeArrayCallBacks, CFArray, CFArrayCreate};
use core_foundation::base::{kCFAllocatorDefault, CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::string::CFString;
use core_graphics::geometry::{CGPoint, CGSize};
use objc::runtime::{Class, Object};
use objc::{msg_send, sel, sel_impl};
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

// AXUIElement opaque type
type AXUIElementRef = *mut c_void;
// AXValue opaque type
type AXValueRef = *mut c_void;

// AXValueType constants
const K_AX_VALUE_TYPE_CGPOINT: u32 = 1;
const K_AX_VALUE_TYPE_CGSIZE: u32 = 2;

// AX error codes. Values from Apple's
// `<ApplicationServices/HIServices/AXError.h>`.
const K_AX_ERROR_SUCCESS: i32 = 0;
const K_AX_ERROR_ILLEGAL_ARGUMENT: i32 = -25204;
const K_AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25205;
const K_AX_ERROR_ACTION_UNSUPPORTED: i32 = -25206;

const MAX_DEPTH: u32 = 50;
const MAX_ELEMENTS: usize = 10_000;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: core_foundation::string::CFStringRef,
        value: *mut core_foundation::base::CFTypeRef,
    ) -> i32;
    fn AXValueGetValue(value: AXValueRef, value_type: u32, value_ptr: *mut c_void) -> bool;
    fn AXUIElementPerformAction(
        element: AXUIElementRef,
        action: core_foundation::string::CFStringRef,
    ) -> i32;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: core_foundation::string::CFStringRef,
        value: core_foundation::base::CFTypeRef,
    ) -> i32;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyElementAtPosition(
        application: AXUIElementRef,
        x: f32,
        y: f32,
        element: *mut AXUIElementRef,
    ) -> i32;
    fn AXUIElementGetPid(element: AXUIElementRef, pid: *mut i32) -> i32;
    /// Private but long-stable HIServices SPI that returns the `CGWindowID`
    /// backing an AX window element. There is no public API for this
    /// mapping; window managers and automation tools rely on it widely.
    fn _AXUIElementGetWindow(element: AXUIElementRef, window_id: *mut u32) -> i32;
}

/// Retained, thread-safe handle to an `AXUIElement`.
///
/// The raw `AXUIElementRef` (`*mut c_void`) is not `Send`/`Sync` by default,
/// but Apple documents the Accessibility API as safe to invoke from any
/// thread, and `CFRetain` / `CFRelease` are atomic. Cross-thread sharing is
/// real: `AXRef`s live inside `AxSession.current:
/// RwLock<Option<AxSnapshot>>`, held on `MacOSDevToolsServer` as
/// `ax_session: Arc<AxSession>`, and `ServerHandler::call_tool` runs on the
/// tokio multi-threaded runtime with `&self`. Any tool call can read the
/// session concurrently; `take_ax_snapshot` writes it under the write half
/// of the lock. The outer `Arc` makes clones free (no `CFRetain` per
/// handoff); the inner `Drop` preserves single-`CFRelease` per retained
/// element.
#[derive(Clone)]
pub struct AXRef(Arc<AXRefInner>);

struct AXRefInner(AXUIElementRef);

// SAFETY: see docs on `AXRef`. Apple's AX API is documented thread-safe;
// CFRetain/CFRelease are atomic.
unsafe impl Send for AXRefInner {}
unsafe impl Sync for AXRefInner {}

impl Drop for AXRefInner {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                core_foundation::base::CFRelease(self.0 as core_foundation::base::CFTypeRef);
            }
        }
    }
}

impl std::fmt::Debug for AXRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AXRef({:p})", self.0 .0)
    }
}

impl AXRef {
    /// Wrap a raw `AXUIElementRef` under the **create rule**: caller already
    /// holds a +1 refcount and transfers ownership to the `AXRef`. Drop will
    /// balance with a single `CFRelease`. Do not call `CFRelease` on the raw
    /// pointer after calling this.
    pub(crate) unsafe fn from_create(raw: AXUIElementRef) -> Self {
        AXRef(Arc::new(AXRefInner(raw)))
    }

    /// Wrap a raw `AXUIElementRef` under the **get rule**: the caller holds a
    /// borrowed reference. This function calls `CFRetain` to take ownership,
    /// and Drop will balance with `CFRelease`.
    pub(crate) unsafe fn from_get(raw: AXUIElementRef) -> Self {
        if !raw.is_null() {
            core_foundation::base::CFRetain(raw as core_foundation::base::CFTypeRef);
        }
        AXRef(Arc::new(AXRefInner(raw)))
    }

    /// Access the raw `AXUIElementRef` for FFI. The `AXRef` must outlive the
    /// borrow — the lifetime is bound to `&self`.
    pub(crate) fn as_raw(&self) -> AXUIElementRef {
        self.0 .0
    }
}

/// Find text in UI elements of an app's accessibility tree.
///
/// Searches the accessibility tree for elements whose AXTitle, AXValue, or
/// AXDescription contains the search string (case-insensitive).
/// Returns matching elements with screen coordinates for clicking.
pub fn find_text(search: &str, window_id: Option<u32>) -> Result<Vec<TextMatch>, String> {
    let debug = std::env::var("NATIVE_DEVTOOLS_DEBUG").is_ok();

    let pid = match window_id {
        Some(wid) => pid_for_window(wid)?,
        None => frontmost_pid()?,
    };

    if debug {
        eprintln!(
            "[DEBUG ax::find_text] search='{}', window_id={:?}, pid={}",
            search, window_id, pid
        );
    }

    let app_element = unsafe { AXUIElementCreateApplication(pid) };
    if app_element.is_null() {
        return Err(format!("Failed to create AXUIElement for pid {}", pid));
    }

    let search_lower = search.to_lowercase();
    let mut matches = Vec::new();
    let mut element_count: usize = 0;

    unsafe {
        walk_ax_tree(app_element, &mut element_count, 0, &mut |element| {
            let matched_text = ["AXTitle", "AXValue", "AXDescription"]
                .iter()
                .filter_map(|attr| get_string_attribute(element, attr))
                .find(|s| !s.is_empty() && s.to_lowercase().contains(search_lower.as_str()));

            if let Some(text) = matched_text {
                if let Some((position, size)) = get_position_and_size(element) {
                    if size.width > 0.0 && size.height > 0.0 {
                        let role = get_string_attribute(element, "AXRole");
                        let bounds = TextBounds {
                            x: position.x,
                            y: position.y,
                            width: size.width,
                            height: size.height,
                        };
                        matches.push(TextMatch {
                            text,
                            x: bounds.x + bounds.width / 2.0,
                            y: bounds.y + bounds.height / 2.0,
                            confidence: 1.0,
                            bounds,
                            role,
                        });
                    }
                }
            }
        });
        core_foundation::base::CFRelease(app_element as core_foundation::base::CFTypeRef);
    }

    if debug {
        eprintln!(
            "[DEBUG ax::find_text] found {} matches out of {} elements",
            matches.len(),
            element_count
        );
    }

    Ok(matches)
}

/// Get the children of an AX element as a CFArray.
/// Returns `None` if the element has no children or the attribute is unavailable.
unsafe fn get_ax_children(element: AXUIElementRef) -> Option<CFArray<*const c_void>> {
    let attr = CFString::new("AXChildren");
    let mut children_ref: core_foundation::base::CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut children_ref);

    if err != K_AX_ERROR_SUCCESS || children_ref.is_null() {
        return None;
    }

    Some(CFArray::wrap_under_create_rule(children_ref as _))
}

/// Recursively walk the AX element tree and call `visitor` on each element.
///
/// `depth` limits recursion to prevent runaway traversal of deep trees.
unsafe fn walk_ax_tree(
    element: AXUIElementRef,
    element_count: &mut usize,
    depth: u32,
    visitor: &mut dyn FnMut(AXUIElementRef),
) {
    // Guard against excessively deep or large trees
    if depth > MAX_DEPTH || *element_count >= MAX_ELEMENTS {
        return;
    }

    *element_count += 1;
    visitor(element);

    if let Some(children) = get_ax_children(element) {
        for i in 0..children.len() {
            let child = *children.get_unchecked(i) as AXUIElementRef;
            // Retain the child for the duration of our walk since CFArray only gives a get-rule ref
            core_foundation::base::CFRetain(child as core_foundation::base::CFTypeRef);
            walk_ax_tree(child, element_count, depth + 1, visitor);
            core_foundation::base::CFRelease(child as core_foundation::base::CFTypeRef);
        }
    }
}

/// Get a string attribute from an AX element. Returns None if the attribute
/// doesn't exist or isn't a string.
unsafe fn get_string_attribute(element: AXUIElementRef, attr_name: &str) -> Option<String> {
    let attr = CFString::new(attr_name);
    let mut value_ref: core_foundation::base::CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value_ref);

    if err != K_AX_ERROR_SUCCESS || value_ref.is_null() {
        return None;
    }

    // value_ref is owned (create rule) — wrap it so it gets released.
    // downcast_into consumes cf_value, avoiding an extra retain/release cycle.
    let cf_string = CFType::wrap_under_create_rule(value_ref).downcast_into::<CFString>()?;
    Some(cf_string.to_string())
}

/// Get a boolean attribute from an AX element. Returns None if the attribute
/// doesn't exist or isn't a CFBoolean.
unsafe fn get_bool_attribute(element: AXUIElementRef, attr_name: &str) -> Option<bool> {
    let attr = CFString::new(attr_name);
    let mut value_ref: core_foundation::base::CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value_ref);

    if err != K_AX_ERROR_SUCCESS || value_ref.is_null() {
        return None;
    }

    // value_ref is owned (create rule) — wrap it so it gets released.
    let cf_bool = CFType::wrap_under_create_rule(value_ref).downcast_into::<CFBoolean>()?;
    Some(bool::from(cf_bool))
}

/// Get position (CGPoint) and size (CGSize) of an AX element.
/// Returns None if either attribute is missing.
unsafe fn get_position_and_size(element: AXUIElementRef) -> Option<(CGPoint, CGSize)> {
    let position: CGPoint = get_ax_value(element, "AXPosition", K_AX_VALUE_TYPE_CGPOINT)?;
    let size: CGSize = get_ax_value(element, "AXSize", K_AX_VALUE_TYPE_CGSIZE)?;
    Some((position, size))
}

/// Read position + size from an AX element and return a `Rect`. Returns
/// `None` when either attribute is unreadable. The raw pointer must refer to
/// a live, retained `AXUIElement`.
pub(crate) unsafe fn element_bbox(
    element: AXUIElementRef,
) -> Option<crate::tools::ax_snapshot::Rect> {
    let (pos, size) = get_position_and_size(element)?;
    Some(crate::tools::ax_snapshot::Rect {
        x: pos.x,
        y: pos.y,
        w: size.width,
        h: size.height,
    })
}

/// Outcome of an `AXUIElementPerformAction` / `AXUIElementSetAttributeValue`
/// call, classified into the MCP error taxonomy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AXDispatchError {
    /// Action or attribute not supported by this element (element is not a
    /// valid dispatch target — e.g. decorative label, read-only field).
    NotDispatchable,
    /// Any other AX error code. Carries the raw integer for diagnostics.
    AxError(i32),
}

impl AXDispatchError {
    /// Classify the return value of `AXUIElementPerformAction(kAXPressAction)`.
    /// Returns `None` for success.
    pub fn from_press_code(code: i32) -> Option<Self> {
        match code {
            K_AX_ERROR_SUCCESS => None,
            K_AX_ERROR_ACTION_UNSUPPORTED => Some(AXDispatchError::NotDispatchable),
            other => Some(AXDispatchError::AxError(other)),
        }
    }

    /// Classify the return value of
    /// `AXUIElementSetAttributeValue(kAXValueAttribute, ...)`. Returns
    /// `None` for success.
    pub fn from_set_value_code(code: i32) -> Option<Self> {
        match code {
            K_AX_ERROR_SUCCESS => None,
            K_AX_ERROR_ATTRIBUTE_UNSUPPORTED | K_AX_ERROR_ILLEGAL_ARGUMENT => {
                Some(AXDispatchError::NotDispatchable)
            }
            other => Some(AXDispatchError::AxError(other)),
        }
    }
}

/// Perform `kAXPressAction` on an element. Returns `Ok(())` on success.
///
/// Narrowed to `pub(crate)` so external Rust consumers cannot combine it
/// with `AxSession::lookup` to recreate the lookup-then-dispatch race that
/// `AxSession::dispatch` exists to close. The blessed entry point for
/// dispatch is the session-pinned `dispatch` method.
pub(crate) fn press_element(element: &AXRef) -> Result<(), AXDispatchError> {
    let action = CFString::new("AXPress");
    let code = unsafe { AXUIElementPerformAction(element.as_raw(), action.as_concrete_TypeRef()) };
    match AXDispatchError::from_press_code(code) {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// Write `text` into an element's `kAXValueAttribute`. Returns `Ok(())` on
/// success.
///
/// This is value assignment, not key-event typing: the target app does not
/// observe keydown/keyup, does not see IME composition events, and does not
/// record the change on its undo stack. Elements whose role does not expose
/// a writable `kAXValueAttribute` return `NotDispatchable`.
///
/// Narrowed to `pub(crate)` so external Rust consumers cannot combine it
/// with `AxSession::lookup` to recreate the lookup-then-dispatch race that
/// `AxSession::dispatch` exists to close.
pub(crate) fn set_value_attribute(element: &AXRef, text: &str) -> Result<(), AXDispatchError> {
    let attr = CFString::new("AXValue");
    let value = CFString::new(text);
    let code = unsafe {
        AXUIElementSetAttributeValue(
            element.as_raw(),
            attr.as_concrete_TypeRef(),
            value.as_concrete_TypeRef() as core_foundation::base::CFTypeRef,
        )
    };
    match AXDispatchError::from_set_value_code(code) {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// Return the AX parent of `element` if the attribute is readable.
///
/// Private — walking ancestors outside `AxSession::dispatch`'s read lock
/// reopens the lookup-then-dispatch race `dispatch` exists to close.
fn ax_parent(element: &AXRef) -> Option<AXRef> {
    let attr = CFString::new("AXParent");
    let mut value_ref: core_foundation::base::CFTypeRef = ptr::null();
    let err = unsafe {
        AXUIElementCopyAttributeValue(element.as_raw(), attr.as_concrete_TypeRef(), &mut value_ref)
    };
    if err != K_AX_ERROR_SUCCESS || value_ref.is_null() {
        return None;
    }
    // `AXUIElementCopyAttributeValue` returns under the create rule, so
    // `from_create` takes that +1 directly — no CFRetain/CFRelease pair.
    Some(unsafe { AXRef::from_create(value_ref as AXUIElementRef) })
}

/// Number of parent links to traverse when hunting for an enclosing role.
/// Deep-enough to absorb the intermediate cells/groups native sidebars wrap
/// their rows in, shallow enough to bail cleanly on a pathological cycle.
const AX_ANCESTOR_WALK_LIMIT: u32 = 32;

/// Walk up the `AXParent` chain starting from `start` (inclusive) and
/// return a leaf-to-root vector of `(AXRef, Option<role>)` pairs.
///
/// The walk stops when `AXParent` becomes unreadable or when
/// `AX_ANCESTOR_WALK_LIMIT` is reached — either way a bounded vector is
/// returned rather than an error. Downstream logic (`ax_select`) inspects
/// the chain to pick out the enclosing `AXRow` and `AXOutline`/`AXTable`;
/// splitting the walk from the decision lets the decision logic be unit
/// tested without live `AXUIElement` handles.
pub(crate) fn ancestor_role_chain(start: &AXRef) -> Vec<(AXRef, Option<String>)> {
    let mut chain: Vec<(AXRef, Option<String>)> = Vec::new();
    let mut current = start.clone();
    for _ in 0..AX_ANCESTOR_WALK_LIMIT {
        let role = unsafe { get_string_attribute(current.as_raw(), "AXRole") };
        chain.push((current.clone(), role));
        match ax_parent(&current) {
            Some(p) => current = p,
            None => break,
        }
    }
    chain
}

/// Write `rows` into the `AXSelectedRows` attribute of an outline/table.
/// Returns `Ok(())` on success.
///
/// Callers must have already walked up to the enclosing `AXOutline` /
/// `AXTable` and verified the row is a direct descendant. The MVP selects
/// exactly one row per call — the list shape leaves room for multi-row
/// selection later without re-plumbing the FFI boundary.
///
/// Narrowed to `pub(crate)` so external Rust consumers cannot combine it
/// with `AxSession::lookup` to recreate the lookup-then-dispatch race that
/// `AxSession::dispatch` exists to close.
pub(crate) fn select_rows_attribute(
    container: &AXRef,
    rows: &[&AXRef],
) -> Result<(), AXDispatchError> {
    let attr = CFString::new("AXSelectedRows");
    // Build a CFArray<AXUIElementRef>. `kCFTypeArrayCallBacks` tells CFArray
    // to CFRetain each pointer on insert and CFRelease on destruction, which
    // is what we want for `AXUIElementRef` — a CFType under the hood. Using
    // `CFArray::from_copyable` would pass a null callback set, producing a
    // bag of unmanaged raw pointers that AX probably still accepts but that
    // leaks refcount semantics across the FFI boundary.
    let raw_ptrs: Vec<*const c_void> = rows.iter().map(|r| r.as_raw() as *const c_void).collect();
    let cf_array_ref = unsafe {
        CFArrayCreate(
            kCFAllocatorDefault,
            raw_ptrs.as_ptr(),
            raw_ptrs.len() as core_foundation::base::CFIndex,
            &kCFTypeArrayCallBacks,
        )
    };
    let cf_array: CFArray<*const c_void> = unsafe { CFArray::wrap_under_create_rule(cf_array_ref) };
    let code = unsafe {
        AXUIElementSetAttributeValue(
            container.as_raw(),
            attr.as_concrete_TypeRef(),
            cf_array.as_concrete_TypeRef() as core_foundation::base::CFTypeRef,
        )
    };
    match AXDispatchError::from_set_value_code(code) {
        None => Ok(()),
        Some(e) => Err(e),
    }
}

/// Extract a typed value (CGPoint or CGSize) from an AXValue attribute.
unsafe fn get_ax_value<T: Default>(
    element: AXUIElementRef,
    attr_name: &str,
    ax_value_type: u32,
) -> Option<T> {
    let attr = CFString::new(attr_name);
    let mut value_ref: core_foundation::base::CFTypeRef = ptr::null();
    let err = AXUIElementCopyAttributeValue(element, attr.as_concrete_TypeRef(), &mut value_ref);

    if err != K_AX_ERROR_SUCCESS || value_ref.is_null() {
        return None;
    }

    let mut result = T::default();
    let ok = AXValueGetValue(
        value_ref as AXValueRef,
        ax_value_type,
        &mut result as *mut T as *mut c_void,
    );

    core_foundation::base::CFRelease(value_ref);

    if ok {
        Some(result)
    } else {
        None
    }
}

/// Get the PID that owns a given window ID, using CGWindowListCopyWindowInfo.
fn pid_for_window(window_id: u32) -> Result<i32, String> {
    let window = super::window::find_window_by_id(window_id)?
        .ok_or_else(|| format!("Window {} not found", window_id))?;
    i32::try_from(window.owner_pid)
        .map_err(|_| format!("PID {} exceeds i32 range", window.owner_pid))
}

/// Get the application name for a PID via NSRunningApplication.
fn app_name_for_pid(pid: i32) -> Option<String> {
    unsafe {
        let app: *mut Object = msg_send![
            Class::get("NSRunningApplication")?,
            runningApplicationWithProcessIdentifier: pid
        ];
        if app.is_null() {
            return None;
        }
        let name_ns: *mut Object = msg_send![app, localizedName];
        if name_ns.is_null() {
            return None;
        }
        let utf8_ptr: *const std::ffi::c_char = msg_send![name_ns, UTF8String];
        if utf8_ptr.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr(utf8_ptr)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Get the PID of the frontmost application via NSWorkspace.
pub(crate) fn frontmost_pid() -> Result<i32, String> {
    unsafe {
        let cls = Class::get("NSWorkspace").ok_or("NSWorkspace class not available")?;
        let workspace: *mut Object = msg_send![cls, sharedWorkspace];
        if workspace.is_null() {
            return Err("NSWorkspace.sharedWorkspace returned nil".to_string());
        }
        let app: *mut Object = msg_send![workspace, frontmostApplication];
        if app.is_null() {
            return Err("No frontmost application found".to_string());
        }
        let pid: i32 = msg_send![app, processIdentifier];
        Ok(pid)
    }
}

/// Resolve app_name to PID by finding the first matching window.
fn pid_for_app_name(app_name: &str) -> Result<i32, String> {
    let windows = super::window::find_windows_by_app(app_name)
        .map_err(|e| format!("Failed to find windows: {}", e))?;
    let win = windows.first().ok_or_else(|| {
        format!(
            "No app found matching '{}'. Use list_apps to find the correct app name.",
            app_name
        )
    })?;
    i32::try_from(win.owner_pid).map_err(|_| format!("PID {} exceeds i32 range", win.owner_pid))
}

/// Get the PID of the process that owns an AX element.
unsafe fn get_pid_for_element(element: AXUIElementRef) -> Option<i32> {
    let mut pid: i32 = 0;
    if AXUIElementGetPid(element, &mut pid) == K_AX_ERROR_SUCCESS {
        Some(pid)
    } else {
        None
    }
}

/// Container roles where `AXUIElementCopyElementAtPosition` may stop too
/// early (e.g. Electron/Chromium web views). When the hit element has one of
/// these roles, we drill deeper into AX children to find the most specific
/// element at the coordinates.
fn is_container_role(role: &str) -> bool {
    matches!(
        role,
        "AXScrollArea"
            | "AXWebArea"
            | "AXGroup"
            | "AXSplitGroup"
            | "AXLayoutArea"
            | "AXList"
            | "AXOutline"
            | "AXTable"
            | "AXBrowser"
    )
}

/// Full tree-walk hit-test: find the smallest-area AX element whose bounds
/// contain (x, y). Walks the entire tree (no spatial pruning) because
/// Electron/Chromium apps may have intermediate containers with inaccurate
/// bounds that don't encompass their children.
unsafe fn hit_test_tree(root: AXUIElementRef, x: f64, y: f64) -> Option<HitResult> {
    let mut best: Option<HitResult> = None;
    let mut element_count: usize = 0;

    walk_ax_tree(root, &mut element_count, 0, &mut |element| {
        if let Some((pos, size)) = get_position_and_size(element) {
            if size.width > 0.0
                && size.height > 0.0
                && x >= pos.x
                && x <= pos.x + size.width
                && y >= pos.y
                && y <= pos.y + size.height
            {
                let area = size.width * size.height;
                let is_better = match &best {
                    Some(current) => area < current.area,
                    None => true,
                };
                if is_better {
                    best = Some(HitResult {
                        name: get_string_attribute(element, "AXTitle"),
                        role: get_string_attribute(element, "AXRole"),
                        subrole: get_string_attribute(element, "AXSubrole"),
                        label: get_string_attribute(element, "AXDescription"),
                        value: get_string_attribute(element, "AXValue"),
                        position: pos,
                        size,
                        area,
                    });
                }
            }
        }
    });

    best
}

/// Result of a hit-test tree walk — captures all attributes at visit time
/// since AX element references from the walk are borrowed, not owned.
struct HitResult {
    name: Option<String>,
    role: Option<String>,
    subrole: Option<String>,
    label: Option<String>,
    value: Option<String>,
    position: CGPoint,
    size: CGSize,
    area: f64,
}

/// Get the accessibility element at the given screen coordinates.
///
/// Uses `AXUIElementCopyElementAtPosition` to find the element at (x, y).
/// If the result is a container (e.g. AXScrollArea in Electron apps), drills
/// deeper into AX children to find the most specific element.
/// If `app_name` is provided, scopes the lookup to that app; otherwise uses
/// a system-wide lookup.
pub fn element_at_point(
    x: f64,
    y: f64,
    app_name: Option<&str>,
) -> Result<serde_json::Value, String> {
    let root = if let Some(name) = app_name {
        let pid = pid_for_app_name(name)?;
        let el = unsafe { AXUIElementCreateApplication(pid) };
        if el.is_null() {
            return Err(format!("Failed to create AXUIElement for app '{}'", name));
        }
        el
    } else {
        let el = unsafe { AXUIElementCreateSystemWide() };
        if el.is_null() {
            return Err("Failed to create system-wide AXUIElement".to_string());
        }
        el
    };

    let mut element: AXUIElementRef = ptr::null_mut();
    let err = unsafe { AXUIElementCopyElementAtPosition(root, x as f32, y as f32, &mut element) };

    unsafe {
        core_foundation::base::CFRelease(root as core_foundation::base::CFTypeRef);
    }

    if err != K_AX_ERROR_SUCCESS || element.is_null() {
        return Err(format!("No accessibility element found at ({}, {})", x, y));
    }

    // Read PID early — needed for both paths and must happen before release.
    let pid = unsafe { get_pid_for_element(element) };

    // If the hit element is a container, try a full tree-walk hit-test
    // from the app root. Needed for Electron/Chromium apps where
    // AXUIElementCopyElementAtPosition returns a shallow container whose
    // element reference exposes no children.
    let role_str = unsafe { get_string_attribute(element, "AXRole") };
    let is_container = role_str.as_deref().is_some_and(is_container_role);

    let (name, role, subrole, label, value, bounds) = if is_container {
        unsafe {
            core_foundation::base::CFRelease(element as core_foundation::base::CFTypeRef);
        }
        let hit = pid.and_then(|p| unsafe {
            let app = AXUIElementCreateApplication(p);
            if app.is_null() {
                return None;
            }
            let result = hit_test_tree(app, x, y);
            core_foundation::base::CFRelease(app as core_foundation::base::CFTypeRef);
            result
        });
        match hit {
            Some(h) => (
                h.name,
                h.role,
                h.subrole,
                h.label,
                h.value,
                Some((h.position, h.size)),
            ),
            None => (None, role_str, None, None, None, None),
        }
    } else {
        let name = unsafe { get_string_attribute(element, "AXTitle") };
        let subrole = unsafe { get_string_attribute(element, "AXSubrole") };
        let label = unsafe { get_string_attribute(element, "AXDescription") };
        let value = unsafe { get_string_attribute(element, "AXValue") };
        let bounds = unsafe { get_position_and_size(element) };
        unsafe {
            core_foundation::base::CFRelease(element as core_foundation::base::CFTypeRef);
        }
        (name, role_str, subrole, label, value, bounds)
    };

    // Resolve app name from PID
    let resolved_app_name = pid.and_then(app_name_for_pid);

    // Build response, omitting null fields
    let mut result = serde_json::Map::new();

    if let Some(r) = role {
        result.insert("role".to_string(), serde_json::Value::String(r));
    }
    if let Some(sr) = subrole {
        result.insert("subrole".to_string(), serde_json::Value::String(sr));
    }
    if let Some(n) = name {
        result.insert("name".to_string(), serde_json::Value::String(n));
    }
    if let Some(l) = label {
        result.insert("label".to_string(), serde_json::Value::String(l));
    }
    if let Some(v) = value {
        result.insert("value".to_string(), serde_json::Value::String(v));
    }
    if let Some((pos, size)) = bounds {
        result.insert(
            "bounds".to_string(),
            serde_json::json!({
                "x": pos.x,
                "y": pos.y,
                "width": size.width,
                "height": size.height,
            }),
        );
    }
    if let Some(p) = pid {
        result.insert("pid".to_string(), serde_json::Value::Number(p.into()));
    }
    if let Some(a) = resolved_app_name {
        result.insert("app_name".to_string(), serde_json::Value::String(a));
    }

    Ok(serde_json::Value::Object(result))
}

/// Collect all unique non-empty element names from the accessibility tree.
/// Used to provide a list of available elements when a search returns no matches.
pub fn list_element_names(window_id: Option<u32>) -> Result<Vec<String>, String> {
    let pid = match window_id {
        Some(wid) => pid_for_window(wid)?,
        None => frontmost_pid()?,
    };

    let app_element = unsafe { AXUIElementCreateApplication(pid) };
    if app_element.is_null() {
        return Err(format!("Failed to create AXUIElement for pid {}", pid));
    }

    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut element_count: usize = 0;

    unsafe {
        walk_ax_tree(app_element, &mut element_count, 0, &mut |element| {
            for attr in &["AXTitle", "AXValue", "AXDescription"] {
                if let Some(text) = get_string_attribute(element, attr) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() && seen.insert(trimmed.to_string()) {
                        names.push(trimmed.to_string());
                    }
                }
            }
        });
        core_foundation::base::CFRelease(app_element as core_foundation::base::CFTypeRef);
    }

    Ok(names)
}

/// Raise all windows of an application to the front using the Accessibility API.
///
/// Two-step approach:
/// 1. Set AXFrontmost on the app element (equivalent to System Events `set frontmost`)
/// 2. AXRaise on each window (physically brings windows to front)
///
/// Step 1 is critical for apps without a proper macOS app bundle (e.g. Tauri dev builds)
/// where NSRunningApplication.activate reports success but doesn't bring windows to front.
pub fn raise_windows(pid: i32) -> bool {
    let debug = std::env::var("NATIVE_DEVTOOLS_DEBUG").is_ok();

    unsafe {
        let app_element = AXUIElementCreateApplication(pid);
        if app_element.is_null() {
            if debug {
                eprintln!(
                    "[DEBUG ax::raise_windows] Failed to create AXUIElement for pid {}",
                    pid
                );
            }
            return false;
        }

        // Step 1: Set AXFrontmost on the app element to make it the frontmost process.
        // This is the programmatic equivalent of AppleScript:
        //   tell application "System Events" to set frontmost of process "X" to true
        let frontmost_attr = CFString::new("AXFrontmost");
        let frontmost_err = AXUIElementSetAttributeValue(
            app_element,
            frontmost_attr.as_concrete_TypeRef(),
            core_foundation::boolean::CFBoolean::true_value().as_CFTypeRef(),
        );
        if debug {
            eprintln!(
                "[DEBUG ax::raise_windows] AXFrontmost set for pid {} (err={})",
                pid, frontmost_err
            );
        }

        // Step 2: AXRaise each window to bring them to front in the window order.
        let windows_attr = CFString::new("AXWindows");
        let mut windows_ref: core_foundation::base::CFTypeRef = ptr::null();
        let err = AXUIElementCopyAttributeValue(
            app_element,
            windows_attr.as_concrete_TypeRef(),
            &mut windows_ref,
        );

        let mut raised = frontmost_err == K_AX_ERROR_SUCCESS;

        if err == K_AX_ERROR_SUCCESS && !windows_ref.is_null() {
            let windows: CFArray<*const c_void> = CFArray::wrap_under_create_rule(windows_ref as _);
            let raise_action = CFString::new("AXRaise");

            for i in 0..windows.len() {
                let window = *windows.get_unchecked(i) as AXUIElementRef;
                let result = AXUIElementPerformAction(window, raise_action.as_concrete_TypeRef());
                if result == K_AX_ERROR_SUCCESS {
                    raised = true;
                } else if debug {
                    eprintln!(
                        "[DEBUG ax::raise_windows] AXRaise failed for window {} (err={})",
                        i, result
                    );
                }
            }

            if debug {
                eprintln!(
                    "[DEBUG ax::raise_windows] pid={}, windows={}, raised={}",
                    pid,
                    windows.len(),
                    raised
                );
            }
        } else if debug {
            eprintln!(
                "[DEBUG ax::raise_windows] No AXWindows for pid {} (err={})",
                pid, err
            );
        }

        core_foundation::base::CFRelease(app_element as core_foundation::base::CFTypeRef);
        raised
    }
}

/// Max per-edge difference (points) for a CG frame and an AX frame to count
/// as the same window. Both use the same global, top-left-origin space, but
/// values can differ by rounding.
const WINDOW_FRAME_TOLERANCE: f64 = 1.0;

/// What we know about one entry of an app's `AXWindows` list.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AxWindowDescriptor {
    /// `CGWindowID` from `_AXUIElementGetWindow`, when that call succeeded.
    pub cg_window_id: Option<u32>,
    /// `AXTitle` of the window.
    pub title: Option<String>,
    /// `(x, y, width, height)` from `AXPosition` + `AXSize`.
    pub frame: Option<(f64, f64, f64, f64)>,
}

/// The window the caller asked for, as seen by the window server.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TargetWindow<'a> {
    pub cg_window_id: u32,
    /// `kCGWindowName`. Often `None` without Screen Recording permission.
    pub title: Option<&'a str>,
    pub frame: (f64, f64, f64, f64),
}

fn frames_match(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    (a.0 - b.0).abs() <= WINDOW_FRAME_TOLERANCE
        && (a.1 - b.1).abs() <= WINDOW_FRAME_TOLERANCE
        && (a.2 - b.2).abs() <= WINDOW_FRAME_TOLERANCE
        && (a.3 - b.3).abs() <= WINDOW_FRAME_TOLERANCE
}

/// Pick the AX window that corresponds to `target`.
///
/// 1. An exact `CGWindowID` match wins.
/// 2. Otherwise, only candidates whose ID is unknown are considered (a known,
///    different ID is a different window). A candidate matches when its
///    frame equals the target frame (within tolerance) and, when both titles
///    are known, the titles are equal.
/// 3. The fallback must be unambiguous: more than one match returns `None`.
pub(crate) fn select_ax_window(
    target: &TargetWindow<'_>,
    candidates: &[AxWindowDescriptor],
) -> Option<usize> {
    if let Some(index) = candidates
        .iter()
        .position(|c| c.cg_window_id == Some(target.cg_window_id))
    {
        return Some(index);
    }

    let mut matches = candidates.iter().enumerate().filter(|(_, c)| {
        c.cg_window_id.is_none()
            && c.frame.is_some_and(|f| frames_match(f, target.frame))
            && match (target.title, c.title.as_deref()) {
                (Some(want), Some(have)) => want == have,
                _ => true,
            }
    });

    let first = matches.next().map(|(index, _)| index);
    if matches.next().is_some() {
        None
    } else {
        first
    }
}

/// Raise exactly one window, identified by its `CGWindowID`, to the front.
///
/// Sets `AXFrontmost` on the owning app, finds the AX window that backs
/// `window.id` (see [`select_ax_window`]), then performs `AXRaise` on it and
/// makes it the app's main and focused window.
///
/// Returns `true` only when the requested window was found and `AXRaise`
/// succeeded. Returns `false` when the window has no matching AX element;
/// the caller decides how to fall back.
pub fn raise_window(window: &super::window::WindowInfo) -> bool {
    let debug = std::env::var("NATIVE_DEVTOOLS_DEBUG").is_ok();
    let pid = window.owner_pid as i32;

    unsafe {
        let app_element = AXUIElementCreateApplication(pid);
        if app_element.is_null() {
            if debug {
                eprintln!(
                    "[DEBUG ax::raise_window] Failed to create AXUIElement for pid {}",
                    pid
                );
            }
            return false;
        }
        // Own the app element so every return path releases it.
        let app_element = AXRef::from_create(app_element);
        let app_raw = app_element.as_raw();

        let frontmost_attr = CFString::new("AXFrontmost");
        let frontmost_err = AXUIElementSetAttributeValue(
            app_raw,
            frontmost_attr.as_concrete_TypeRef(),
            CFBoolean::true_value().as_CFTypeRef(),
        );

        let windows_attr = CFString::new("AXWindows");
        let mut windows_ref: core_foundation::base::CFTypeRef = ptr::null();
        let err = AXUIElementCopyAttributeValue(
            app_raw,
            windows_attr.as_concrete_TypeRef(),
            &mut windows_ref,
        );
        if err != K_AX_ERROR_SUCCESS || windows_ref.is_null() {
            if debug {
                eprintln!(
                    "[DEBUG ax::raise_window] No AXWindows for pid {} (err={})",
                    pid, err
                );
            }
            return false;
        }
        // AXUIElementCopyAttributeValue follows the create rule.
        let windows: CFArray<*const c_void> = CFArray::wrap_under_create_rule(windows_ref as _);

        let descriptors: Vec<AxWindowDescriptor> = windows
            .iter()
            .map(|item| {
                let element = *item as AXUIElementRef;
                let mut cg_window_id: u32 = 0;
                let cg_window_id = (_AXUIElementGetWindow(element, &mut cg_window_id)
                    == K_AX_ERROR_SUCCESS
                    && cg_window_id != 0)
                    .then_some(cg_window_id);
                AxWindowDescriptor {
                    cg_window_id,
                    title: get_string_attribute(element, "AXTitle"),
                    frame: get_position_and_size(element)
                        .map(|(p, s)| (p.x, p.y, s.width, s.height)),
                }
            })
            .collect();

        let bounds = &window.bounds;
        let target = TargetWindow {
            cg_window_id: window.id,
            title: window.name.as_deref(),
            frame: (bounds.x, bounds.y, bounds.width, bounds.height),
        };

        let Some(index) = select_ax_window(&target, &descriptors) else {
            if debug {
                eprintln!(
                    "[DEBUG ax::raise_window] No AX window matches window {} (pid={}, candidates={:?})",
                    window.id, pid, descriptors
                );
            }
            return false;
        };

        // Elements inside the array are borrowed (get rule); `windows` keeps
        // them alive until the end of this scope.
        let target_element = *windows.get_unchecked(index as isize) as AXUIElementRef;

        let raise_action = CFString::new("AXRaise");
        let raise_err =
            AXUIElementPerformAction(target_element, raise_action.as_concrete_TypeRef());

        let main_attr = CFString::new("AXMain");
        let main_err = AXUIElementSetAttributeValue(
            target_element,
            main_attr.as_concrete_TypeRef(),
            CFBoolean::true_value().as_CFTypeRef(),
        );

        let focused_attr = CFString::new("AXFocusedWindow");
        let focused_err = AXUIElementSetAttributeValue(
            app_raw,
            focused_attr.as_concrete_TypeRef(),
            target_element as core_foundation::base::CFTypeRef,
        );

        if debug {
            eprintln!(
                "[DEBUG ax::raise_window] window={} pid={} index={} frontmost_err={} raise_err={} main_err={} focused_err={}",
                window.id, pid, index, frontmost_err, raise_err, main_err, focused_err
            );
        }

        raise_err == K_AX_ERROR_SUCCESS
    }
}

/// Recursively walk the AX element tree and collect [`AXSnapshotNode`] entries
/// plus a `HashMap<uid, AXRef>` of retained handles.
///
/// UIDs are assigned sequentially via `next_uid` (starts at 1).
/// Traversal order is depth-first, matching `walk_ax_tree`.
unsafe fn collect_ax_tree_recursive(
    element: AXUIElementRef,
    element_count: &mut usize,
    depth: u32,
    next_uid: &mut u32,
    nodes: &mut Vec<AXSnapshotNode>,
    refs: &mut std::collections::HashMap<u32, AXRef>,
) {
    if depth > MAX_DEPTH || *element_count >= MAX_ELEMENTS {
        return;
    }

    *element_count += 1;

    let uid = *next_uid;
    *next_uid += 1;

    let role = get_string_attribute(element, "AXRole")
        .as_deref()
        .map(map_ax_role)
        .unwrap_or_else(|| "unknown".to_string());

    let name = get_string_attribute(element, "AXTitle");
    let value = get_string_attribute(element, "AXValue");
    let focused = get_bool_attribute(element, "AXFocused").unwrap_or(false);
    let disabled = get_bool_attribute(element, "AXEnabled")
        .map(|enabled| !enabled)
        .unwrap_or(false);
    let expanded = get_bool_attribute(element, "AXExpanded");
    let selected = get_bool_attribute(element, "AXSelected");
    let bbox = element_bbox(element);

    nodes.push(AXSnapshotNode {
        uid,
        role,
        name,
        value,
        focused,
        disabled,
        expanded,
        selected,
        depth,
        bbox,
    });

    // Retain under get-rule: the current `element` was obtained either from
    // `AXUIElementCreateApplication` (owned, passed to us) or from the
    // CFArray iteration below (which retains around recursion). Either way,
    // `from_get` is the correct rule here since the caller still holds the
    // owning reference for the duration of this visitor.
    refs.insert(uid, AXRef::from_get(element));

    if let Some(children) = get_ax_children(element) {
        for i in 0..children.len() {
            let child = *children.get_unchecked(i) as AXUIElementRef;
            core_foundation::base::CFRetain(child as core_foundation::base::CFTypeRef);
            collect_ax_tree_recursive(child, element_count, depth + 1, next_uid, nodes, refs);
            core_foundation::base::CFRelease(child as core_foundation::base::CFTypeRef);
        }
    }
}

/// Walk an application's AX tree and return flat nodes + a `HashMap<uid, AXRef>`.
///
/// The `AXRef` map is the source of truth for `AxSession` — each entry is a
/// retained handle that stays live until the session swaps in a new snapshot.
pub fn collect_ax_tree_indexed(
    app_name: Option<&str>,
) -> Result<(Vec<AXSnapshotNode>, std::collections::HashMap<u32, AXRef>), String> {
    let pid = match app_name {
        Some(name) => pid_for_app_name(name)?,
        None => frontmost_pid()?,
    };

    let app_element = unsafe { AXUIElementCreateApplication(pid) };
    if app_element.is_null() {
        return Err(format!("Failed to create AXUIElement for pid {}", pid));
    }

    let mut nodes = Vec::new();
    let mut element_count: usize = 0;
    let mut next_uid: u32 = 1;
    let mut refs = std::collections::HashMap::new();

    unsafe {
        collect_ax_tree_recursive(
            app_element,
            &mut element_count,
            0,
            &mut next_uid,
            &mut nodes,
            &mut refs,
        );
        core_foundation::base::CFRelease(app_element as core_foundation::base::CFTypeRef);
    }

    Ok((nodes, refs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Search for "9" in the frontmost app (Calculator).
    /// Requires Calculator to be running and in the foreground.
    /// Run with: cargo test test_ax_find_text_calculator -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_ax_find_text_calculator() {
        let results = find_text("9", None).expect("find_text should succeed");
        println!("AX find_text results for '9' (no window_id):");
        for m in &results {
            println!(
                "  '{}' at ({:.1}, {:.1}) bounds=({:.1}, {:.1}, {:.1}x{:.1})",
                m.text, m.x, m.y, m.bounds.x, m.bounds.y, m.bounds.width, m.bounds.height
            );
        }
        assert!(
            !results.is_empty(),
            "Should find at least one match for '9'"
        );
        for m in &results {
            assert!(m.x > 0.0, "x coordinate should be positive");
            assert!(m.y > 0.0, "y coordinate should be positive");
        }
    }

    /// Search for "9" using Calculator's window_id.
    /// Requires Calculator to be running.
    /// Run with: cargo test test_ax_find_text_with_window_id -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_ax_find_text_with_window_id() {
        let windows = crate::macos::window::find_windows_by_app("Calculator")
            .expect("find_windows_by_app should succeed");
        let calc_window = windows
            .first()
            .expect("Calculator must be running for this test");

        println!("Calculator window id: {}", calc_window.id);

        let results =
            find_text("9", Some(calc_window.id)).expect("find_text with window_id should succeed");
        println!(
            "AX find_text results for '9' (window_id={}):",
            calc_window.id
        );
        for m in &results {
            println!(
                "  '{}' at ({:.1}, {:.1}) bounds=({:.1}, {:.1}, {:.1}x{:.1})",
                m.text, m.x, m.y, m.bounds.x, m.bounds.y, m.bounds.width, m.bounds.height
            );
        }
        assert!(
            !results.is_empty(),
            "Should find at least one match for '9' with window_id"
        );
    }

    /// Test element_at_point with Calculator.
    /// Requires Calculator to be running and visible.
    /// Run with: cargo test test_ax_element_at_point_calculator -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_ax_element_at_point_calculator() {
        // First find Calculator's "5" button to get known coordinates
        let matches = find_text("5", None).expect("find_text should succeed");
        let button = matches
            .iter()
            .find(|m| m.text == "5")
            .expect("Should find the '5' button");

        // Now query element_at_point at those coordinates
        let result = element_at_point(button.x, button.y, Some("Calculator"))
            .expect("element_at_point should succeed");

        println!(
            "element_at_point result: {}",
            serde_json::to_string_pretty(&result).unwrap()
        );

        // Verify we got a meaningful result
        assert!(result.get("role").is_some(), "Should have a role");
        assert!(result.get("bounds").is_some(), "Should have bounds");
        assert!(result.get("pid").is_some(), "Should have a pid");
        assert!(result.get("app_name").is_some(), "Should have an app_name");
    }

    /// Test element_at_point with system-wide lookup (no app_name).
    /// Requires Calculator to be running and in the foreground.
    /// Run with: cargo test test_ax_element_at_point_system_wide -- --ignored --nocapture
    #[test]
    #[ignore]
    fn test_ax_element_at_point_system_wide() {
        let matches = find_text("5", None).expect("find_text should succeed");
        let button = matches
            .iter()
            .find(|m| m.text == "5")
            .expect("Should find the '5' button");

        let result =
            element_at_point(button.x, button.y, None).expect("element_at_point should succeed");

        println!(
            "element_at_point (system-wide): {}",
            serde_json::to_string_pretty(&result).unwrap()
        );

        assert!(result.get("role").is_some(), "Should have a role");
    }

    #[test]
    fn ax_dispatch_error_from_press_code() {
        // kAXErrorSuccess
        assert!(AXDispatchError::from_press_code(0).is_none());
        // kAXErrorActionUnsupported = -25206
        assert!(matches!(
            AXDispatchError::from_press_code(-25206),
            Some(AXDispatchError::NotDispatchable)
        ));
        // Any other non-zero code is a generic AXError.
        match AXDispatchError::from_press_code(-25204) {
            Some(AXDispatchError::AxError(-25204)) => (),
            other => panic!("expected AxError(-25204), got {:?}", other),
        }
    }

    #[test]
    fn ax_dispatch_error_from_set_value_code() {
        // Success
        assert!(AXDispatchError::from_set_value_code(0).is_none());
        // kAXErrorAttributeUnsupported = -25205
        assert!(matches!(
            AXDispatchError::from_set_value_code(-25205),
            Some(AXDispatchError::NotDispatchable)
        ));
        // kAXErrorIllegalArgument = -25204
        assert!(matches!(
            AXDispatchError::from_set_value_code(-25204),
            Some(AXDispatchError::NotDispatchable)
        ));
        // Anything else is generic.
        match AXDispatchError::from_set_value_code(-25212) {
            Some(AXDispatchError::AxError(-25212)) => (),
            other => panic!("expected AxError(-25212), got {:?}", other),
        }
    }

    /// Calculator has well-known bbox attributes on its buttons. Smoke test that
    /// `element_bbox` returns `Some` for the "5" button's AXRef and that the
    /// dimensions look plausible (positive width/height).
    #[test]
    #[ignore]
    fn test_element_bbox_calculator_five_button() {
        // This test runs under `cargo test -- --ignored` and requires
        // Calculator to be running.
        let (nodes, refs) = collect_ax_tree_indexed(Some("Calculator"))
            .expect("collect_ax_tree_indexed should succeed");

        // Find the "5" button node.
        let five = nodes
            .iter()
            .find(|n| n.name.as_deref() == Some("5"))
            .expect("Calculator should expose a '5' button");

        let r = refs
            .get(&five.uid)
            .expect("refs map must contain every uid");
        let bbox =
            unsafe { element_bbox(r.as_raw()) }.expect("5 button should expose position + size");
        assert!(bbox.w > 0.0);
        assert!(bbox.h > 0.0);

        // Node-level bbox should also be populated and match.
        let node_bbox = five
            .bbox
            .expect("node bbox should be populated during walk");
        assert!((node_bbox.x - bbox.x).abs() < 0.5);
        assert!((node_bbox.y - bbox.y).abs() < 0.5);
    }

    /// `AXRef::from_create` must NOT CFRetain (caller already owns a +1); Drop
    /// must CFRelease exactly once. `AXRef::from_get` must CFRetain (caller
    /// holds a borrowed reference); Drop must CFRelease exactly once. Cloning
    /// does not touch CFRetain/CFRelease (the inner Arc does the work).
    ///
    /// We verify by wrapping a `CFData` (heap-allocated — short `CFString`s
    /// can be optimized as tagged pointers with `retain_count = i64::MAX`,
    /// which makes the observable arithmetic meaningless). Retain/release
    /// are defined at the CFType layer so any heap-allocated CF type works.
    #[test]
    fn test_ax_ref_retain_release_balance_against_cfdata() {
        use core_foundation::base::{CFGetRetainCount, CFRetain, CFTypeRef, TCFType};
        use core_foundation::data::CFData;

        let d = CFData::from_buffer(&[1u8, 2, 3, 4, 5, 6, 7, 8]);
        let raw: CFTypeRef = d.as_concrete_TypeRef() as CFTypeRef;
        // Bump to +2 so we can observe `from_create` NOT adding another retain
        // and the final Drop balancing it cleanly.
        unsafe {
            CFRetain(raw);
        }
        let before = unsafe { CFGetRetainCount(raw) };
        assert!(
            (2..isize::MAX).contains(&before),
            "expected a finite retain count >= 2, got {before} — CFData should be heap-allocated"
        );

        // `from_create` transfers the +1 we just added — no extra retain.
        let aref = unsafe { super::AXRef::from_create(raw as *mut _) };
        let during = unsafe { CFGetRetainCount(raw) };
        assert_eq!(
            during, before,
            "from_create must not CFRetain — transfer ownership of existing +1"
        );

        // Clone is Arc-level — no extra CFRetain.
        let clone = aref.clone();
        let during2 = unsafe { CFGetRetainCount(raw) };
        assert_eq!(during2, before, "Arc clone must not touch CFRetain");

        drop(clone);
        let after_clone_drop = unsafe { CFGetRetainCount(raw) };
        assert_eq!(
            after_clone_drop, before,
            "dropping a clone while other Arcs live must not CFRelease"
        );

        drop(aref);
        let after_last_drop = unsafe { CFGetRetainCount(raw) };
        assert_eq!(
            after_last_drop,
            before - 1,
            "final Drop must CFRelease exactly once (count {before} -> {after_last_drop})"
        );

        // And `from_get` must CFRetain on construction.
        let before_get = unsafe { CFGetRetainCount(raw) };
        let aref2 = unsafe { super::AXRef::from_get(raw as *mut _) };
        let during_get = unsafe { CFGetRetainCount(raw) };
        assert_eq!(during_get, before_get + 1, "from_get must CFRetain once");
        drop(aref2);
        let after_get_drop = unsafe { CFGetRetainCount(raw) };
        assert_eq!(
            after_get_drop, before_get,
            "from_get + drop must be net-zero"
        );
    }
}

#[cfg(test)]
mod select_ax_window_tests {
    use super::{select_ax_window, AxWindowDescriptor, TargetWindow};

    fn candidate(
        cg_window_id: Option<u32>,
        title: Option<&str>,
        frame: Option<(f64, f64, f64, f64)>,
    ) -> AxWindowDescriptor {
        AxWindowDescriptor {
            cg_window_id,
            title: title.map(str::to_string),
            frame,
        }
    }

    fn target(title: Option<&str>) -> TargetWindow<'_> {
        TargetWindow {
            cg_window_id: 42,
            title,
            frame: (100.0, 200.0, 800.0, 600.0),
        }
    }

    #[test]
    fn picks_the_window_whose_cg_id_matches_even_when_it_is_not_last() {
        let candidates = [
            candidate(Some(41), Some("A"), Some((0.0, 0.0, 10.0, 10.0))),
            candidate(Some(42), Some("B"), Some((0.0, 0.0, 10.0, 10.0))),
            candidate(Some(43), Some("C"), Some((0.0, 0.0, 10.0, 10.0))),
        ];
        assert_eq!(select_ax_window(&target(None), &candidates), Some(1));
    }

    #[test]
    fn id_match_wins_over_a_frame_and_title_match() {
        let candidates = [
            candidate(None, Some("Doc"), Some((100.0, 200.0, 800.0, 600.0))),
            candidate(Some(42), Some("Other"), Some((5.0, 5.0, 5.0, 5.0))),
        ];
        assert_eq!(select_ax_window(&target(Some("Doc")), &candidates), Some(1));
    }

    #[test]
    fn returns_none_when_every_candidate_has_a_different_known_id() {
        let candidates = [
            candidate(Some(7), Some("Doc"), Some((100.0, 200.0, 800.0, 600.0))),
            candidate(Some(8), None, Some((100.0, 200.0, 800.0, 600.0))),
        ];
        assert_eq!(select_ax_window(&target(Some("Doc")), &candidates), None);
    }

    #[test]
    fn falls_back_to_frame_within_tolerance_when_ids_are_unknown() {
        let candidates = [
            candidate(None, Some("Doc"), Some((0.0, 0.0, 800.0, 600.0))),
            candidate(None, Some("Doc"), Some((100.5, 199.5, 800.0, 600.9))),
        ];
        assert_eq!(select_ax_window(&target(None), &candidates), Some(1));
    }

    #[test]
    fn frame_fallback_rejects_a_frame_outside_tolerance() {
        let candidates = [candidate(None, None, Some((102.0, 200.0, 800.0, 600.0)))];
        assert_eq!(select_ax_window(&target(None), &candidates), None);
    }

    #[test]
    fn frame_fallback_uses_title_to_break_a_tie() {
        let candidates = [
            candidate(None, Some("First"), Some((100.0, 200.0, 800.0, 600.0))),
            candidate(None, Some("Second"), Some((100.0, 200.0, 800.0, 600.0))),
        ];
        assert_eq!(
            select_ax_window(&target(Some("Second")), &candidates),
            Some(1)
        );
    }

    #[test]
    fn frame_fallback_returns_none_when_ambiguous() {
        let candidates = [
            candidate(None, Some("First"), Some((100.0, 200.0, 800.0, 600.0))),
            candidate(None, Some("Second"), Some((100.0, 200.0, 800.0, 600.0))),
        ];
        assert_eq!(select_ax_window(&target(None), &candidates), None);
    }

    #[test]
    fn frame_fallback_skips_candidates_without_a_frame() {
        let candidates = [candidate(None, Some("Doc"), None)];
        assert_eq!(select_ax_window(&target(Some("Doc")), &candidates), None);
    }
}
