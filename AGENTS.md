# Agent Context: native-devtools-mcp

**About:** This is the AGENTS.md for **native-devtools-mcp**, an MCP (Model Context Protocol) server that enables **computer use** / **desktop automation** on macOS, Windows, and Android: screenshots, OCR, mouse/keyboard input, window management, and Android device control via ADB.

**Search keywords:** MCP, Model Context Protocol, computer use, desktop automation, UI automation, RPA, screenshots, OCR, screen reading, mouse, keyboard, macOS, Windows, Android, ADB, mobile testing, native-devtools-mcp.

**Role:** You are an agent equipped with "Computer Use" capabilities. You can see the screen, type, move the mouse, and interact with native desktop and mobile applications.

**Constraint:** You are operating a real machine. Actions are permanent. Ensure you verify the state of the screen before and after actions.

## 🧠 Core Reasoning Loop

For robust automation, follow this "Visual Feedback Loop":

1.  **OBSERVE:** Call `take_screenshot(app_name="TargetApp")` to see the current state.
2.  **LOCATE:** Analyze the image or use the OCR summary text in the response to find coordinates.
3.  **ACT:** Call `click()`, `type_text()`, or `scroll()` using those coordinates.
4.  **VERIFY:** Call `take_screenshot` again to confirm the action had the intended effect.

**macOS-preferred branch (native apps):** substitute OBSERVE with `take_ax_snapshot(app_name='...')`; LOCATE reads uid + bbox from the emitted tree; ACT calls `ax_click(uid)` for pressable controls, `ax_set_value(uid, text)` for text fields, or `ax_select(uid)` for `NSOutlineView` / `NSTableView` row selection (sidebars, rule lists); VERIFY re-snapshots and reads the new state. This branch does not move the cursor or steal focus, so it composes with background work.

---

## 🗺️ Capabilities Matrix (Strategy Guide)

Use this table to choose the right tool sequence for the user's goal.

| User Goal | Tool Sequence | Why? |
|-----------|---------------|------|
| "Click the 'Submit' button" | `find_text(text="Submit")` → `click(x, y)` | Fastest. No visual analysis needed if text is known. |
| "Click the red icon" | `take_screenshot()` → (Analyze Image) → `click(screenshot_x=..., screenshot_y=..., screenshot_origin_x=..., screenshot_origin_y=..., screenshot_scale=...)` | Visual features require full screenshot analysis. |
| "What element is at (500, 300)?" | `element_at_point(x=500, y=300)` | Returns the accessibility element at those coordinates (name, role, bounds, etc.). |
| "Type into the search bar" | `find_text(text="Search")` → `click(x, y)` → `type_text("hello")` | Must click to focus before typing. |
| "Scroll down" | `scroll(x=500, y=500, delta_y=200)` | Positive `delta_y` scrolls down. |
| "Find an open window" | `list_windows()` → `focus_window(window_id=...)` | Don't guess window names; list them first. |
| "Track what I hover over" | `start_hover_tracking(min_dwell_ms=300)` → user moves mouse → `stop_hover_tracking()` | Records element transitions with dwell filtering. |
| "Record what the user does" | `start_recording(output_dir="/tmp/rec")` → user interacts → `stop_recording()` | Captures frontmost app at ~5fps as JPEG frames. |
| "Launch Safari with debug port" | `launch_app(app_name="Safari", args=["--remote-debugging-port=9222"])` | Pass CLI args on fresh launch. |
| "Quit an app" | `quit_app(app_name="Safari")` | Graceful by default; use `force=true` to kill immediately. |
| "Click a named button in a native app (macOS)" | `take_ax_snapshot` → `ax_click(uid)` | Focus-preserving. Generation-tagged uids; fresh snapshot invalidates prior uids. |
| "Enter text into a text field via value assignment (macOS)" | `take_ax_snapshot` → `ax_set_value(uid, text)` | No key events, no IME, no undo-stack entry. Fall back to `click` + `type_text` on `not_dispatchable`. |
| "Select a sidebar row in System Settings / a `NSOutlineView` row (macOS)" | `take_ax_snapshot` → `ax_select(uid)` | Writes `AXSelectedRows` on the enclosing outline/table. Use this instead of `ax_click` for row targets — rows typically refuse `AXPress`. |
| "AX snapshot invalidation rule (macOS)" | — | Every `take_ax_snapshot` call bumps the generation. All prior uids become stale; `ax_*` tools return `snapshot_expired`. |
| "Click a button in Chrome" | `cdp_connect(port=9222)` → `cdp_find_elements(query="Submit")` → `cdp_click(uid="d1")` | CDP is more reliable than coordinates for web content. |
| "Type in a web input" | `cdp_find_elements(query="Email")` → `cdp_fill(uid="d1", value="hello")` | Works for `<input>`, `<textarea>`, and `<select>` elements. |
| "Run JS in a browser page" | `cdp_evaluate_script(function="() => document.title")` | Evaluate any JS in the selected page. |
| "Navigate to a URL" | `cdp_navigate(url="https://example.com")` | Also supports back, forward, reload. |
| "Press Enter or shortcut" | `cdp_press_key(key="Enter")` or `cdp_press_key(key="Control+A")` | Supports modifier combos. |
| "Wait for page content" | `cdp_wait_for(text=["Success"])` | Polls page text until any value appears or timeout. Pass `include_snapshot=true` to also get a DOM snapshot. |
| "Switch browser tabs" | `cdp_list_pages()` → `cdp_select_page(page_idx=1)` | List tabs, then select by index. |
| "Get browser page structure" | `cdp_take_dom_snapshot()` | Full interactive-element DOM snapshot with UIDs, roles, and labels. |
| "Is this app native, Electron, or Chrome?" | `probe_app(app_name="Signal")` | Decides between native tools (`take_ax_snapshot`, `click`, `find_text`) and CDP tools. Works whether the app is running or not. |
| "Wait for a new message in a web/Electron app" | `cdp_find_elements(query="Messages")` → `cdp_wait_for_page_change(scope_uid="d3")` | Blocks until the scoped element's visible text changes; returns compact before/after deltas. |

---

## 🛠️ Tool Definitions & Schemas

### 1. Vision & Perception (The "Eyes")

#### `take_ax_snapshot` (cross-platform with macOS session semantics)

**Purpose:** Return a structured text snapshot of an app's accessibility tree with per-element uids, roles, names, state attributes, and (on macOS) bounding boxes.

**Schema:**
- `app_name: string` (optional) — target app; defaults to frontmost.

**macOS session state:** on macOS this tool is no longer a pure read — it writes session state. Every call bumps a monotonic generation and replaces the server's cached map of `AXUIElement` handles. Uids are emitted as `a<N>g<gen>` (e.g. `a42g3`). All uids from prior snapshots become stale for `ax_click` / `ax_set_value` / `ax_select` consumption (return `snapshot_expired`). Windows behavior is unchanged — no session, uids stay bare `a<N>`.

**Usage pattern (macOS):** snapshot immediately before each `ax_click` / `ax_set_value` / `ax_select` call to avoid `snapshot_expired`. Every branch or retry starts with a fresh snapshot.

#### `take_screenshot`
Captures pixel data and layout.
*   **Inputs:**
    *   `mode` (string, default `"window"`): `"screen"`, `"window"`, or `"region"`.
    *   `app_name` (string, optional): Capture this app's window (for mode `"window"`).
    *   `window_id` (number, optional): Window ID (for mode `"window"`).
    *   `x`, `y`, `width`, `height` (numbers): Region bounds (for mode `"region"`).
    *   `include_ocr` (boolean, default `true`): Include OCR summary text with coordinates.
*   **Returns (content list):**
    ```json
    [
      { "type": "image", "mime": "image/jpeg", "data": "..." },
      { "type": "text", "text": "{ \"screenshot_origin_x\": 0, \"screenshot_origin_y\": 0, \"screenshot_scale\": 2.0, \"screenshot_window_id\": 1234, \"screenshot_pixel_width\": 1920, \"screenshot_pixel_height\": 1080 }" },
      { "type": "text", "text": "## OCR Text Detected (click coordinates)\n- \"File\" at (10, 10) bounds: {x: 0, y: 0, w: 50, h: 20}" }
    ]
    ```
    *   **Metadata fields:**
        *   `screenshot_origin_x`, `screenshot_origin_y`: Screen-space origin of the screenshot (top-left corner), in points.
        *   `screenshot_scale`: Display scale factor (e.g., 2.0 for Retina).
        *   `screenshot_window_id`: Window ID (only for mode `"window"`). Present even when using `app_name`.
        *   `screenshot_pixel_width`, `screenshot_pixel_height`: Actual pixel dimensions of the captured image.

#### `find_text`
Fast-path to get coordinates without image analysis.
*   **Inputs:** `text` (string, case-insensitive substring match against accessibility element names, then OCR), `app_name` (string, optional), `window_id` (number, optional), `display_id` (number, optional).
*   **Returns (JSON array):**
    ```json
    [
      { "text": "Save", "x": 500, "y": 300, "confidence": 1.0, "bounds": { "x": 480, "y": 290, "width": 40, "height": 20 } }
    ]
    ```
*   **Platform behavior:**
    *   **Both platforms:** Uses the **platform accessibility API** as the primary mechanism — searches the accessibility tree for elements by name. This gives precise element-level coordinates (`confidence: 1.0`). Falls back to OCR automatically if accessibility finds no matches.
    *   **macOS:** Accessibility API (primary), Vision OCR (fallback). Matches against element title, value, and description. Note: accessibility results use semantic names (e.g., "All Clear" instead of "AC", "Subtract" instead of "−"), so search by meaning rather than displayed symbol.
    *   **Windows:** UI Automation (primary), WinRT OCR (fallback). Matches against element Name property only.

#### `element_at_point`
Inspect the accessibility element at given screen coordinates.
*   **Inputs:** `x` (number, required), `y` (number, required), `app_name` (string, optional — scope lookup to a specific app for faster, more precise results).
*   **Returns (JSON object, fields present only when available):**
    ```json
    {
      "role": "AXButton",
      "name": "Save",
      "label": "Save document",
      "value": "...",
      "bounds": { "x": 480, "y": 290, "width": 40, "height": 20 },
      "pid": 12345,
      "app_name": "TextEdit"
    }
    ```
*   **Platform behavior:**
    *   **macOS:** Uses `AXUIElementCopyElementAtPosition`. When the result is a container (e.g. AXScrollArea in Electron apps), automatically walks the full AX tree to find the most specific element at the coordinates. With `app_name`, scopes to that app's element tree (useful when windows overlap).
    *   **Windows:** Uses `IUIAutomation::ElementFromPoint`. `app_name` is not yet supported (ignored).
*   **Known limitation:** Some privacy-focused Electron apps (e.g. Signal) intentionally restrict their accessibility tree, exposing only top-level containers without individual UI elements. In these cases, `element_at_point` returns the outermost container (e.g. AXScrollArea or AXWindow). **Workaround:** If the returned element is a large container, use `take_screenshot(app_name=..., include_ocr=true)` and check the OCR results to identify the text/element at those coordinates.

#### `get_displays`
Returns all connected displays with bounds, scale factors, and resolution. No inputs.

### 2. Input & Interaction (The "Hands")

#### `click`
Simulates a mouse click.
*   **Inputs:**
    *   **Method A (Screen Absolute):** `x` (number), `y` (number). Use with `find_text` results.
    *   **Method B (Window Relative):** `window_x`, `window_y`, `window_id`.
    *   **Method C (Screenshot Relative):** `screenshot_x`, `screenshot_y`, `screenshot_origin_x`, `screenshot_origin_y`, `screenshot_scale`. Use with `take_screenshot` visual analysis.
    *   `button`: "left" (default), "right", "center".
    *   `click_count`: 1 (default), 2 (double-click).

#### `move_mouse`
Moves the cursor to screen coordinates without clicking.
*   **Inputs:** `x` (number), `y` (number).

#### `drag`
Drags from one point to another.
*   **Inputs:** `start_x`, `start_y`, `end_x`, `end_y` (numbers, screen coordinates), `button` (optional: `"left"` default, `"right"`, `"center"`).

#### `press_key`
Presses a key combination in the focused app.
*   **Inputs:** `key` (string, e.g. `"return"`, `"tab"`, `"escape"`, `"a"`, `"f1"`, `"left"`), `modifiers` (array, optional: `"shift"`, `"control"`, `"option"`, `"command"`).

#### `type_text`
Types text at the *current* cursor position.
*   **Inputs:** `text` (string).
*   **Warning:** Always `click()` the input field first to ensure focus!

#### `ax_click` (macOS only)

**Purpose:** Press a button identified by its AX uid, without stealing cursor focus.

**Schema:**
- `uid: string` — generation-tagged uid from the most recent `take_ax_snapshot`, e.g. `"a42g3"`.

**Response:**
- Success: `{ "ok": true, "dispatched_via": "AXPress", "bbox": { "x", "y", "w", "h" } }`.
- Error: `CallToolResult::error` with JSON body `{ "error": { "code", "message", "fallback": { "x", "y" } | null } }`. Codes: `snapshot_expired`, `uid_not_found`, `not_dispatchable`, `ax_error`.

**Gotchas:**
- Any fresh `take_ax_snapshot` invalidates every prior uid — snapshot immediately before calling.
- When `fallback` is populated on `not_dispatchable`, retry via `click(fallback.x, fallback.y)`.

#### `ax_set_value` (macOS only)

**Purpose:** Write to an element's `kAXValueAttribute`. Value assignment, **not** keystroke typing.

**Schema:**
- `uid: string` — generation-tagged uid.
- `text: string` — value to assign.

**Response:** same shape as `ax_click`, with `"dispatched_via": "AXSetAttributeValue"`.

**Limits:**
- Bypasses key events (no `keydown`/`keyup` observed by apps).
- Does not participate in IME / composition.
- Does not populate the app's undo stack.
- Only works on elements that expose a writable `kAXValueAttribute` (e.g. `AXTextField`, `AXTextArea`, `AXSearchField`).

**Fallback on `not_dispatchable`:** perform `click(fallback.x, fallback.y)` to focus, then `type_text(text)` for true key-event input.

#### `ax_select` (macOS only)

**Purpose:** Select a row inside an `NSOutlineView` / `NSTableView` by writing `AXSelectedRows` on the enclosing outline/table. Use for sidebars (System Settings, Mail, Xcode, Finder), rule lists, and any native browser-style row container where `AXPress` is not supported.

**Schema:**
- `uid: string` — generation-tagged uid. Points at the row itself, a cell inside the row, or any descendant; the tool walks up to the enclosing `AXRow`.

**Response:**
- Success: `{ "ok": true, "dispatched_via": "AXSelectedRows", "bbox": { "x", "y", "w", "h" } }`. The bbox describes the resolved row (not necessarily the uid-targeted descendant).
- Error: same envelope as `ax_click` with codes `snapshot_expired`, `uid_not_found`, `no_row_ancestor`, `no_outline_container`, `not_dispatchable`, `ax_error`.

**When to reach for `ax_select` vs `ax_click`:** rows in native sidebars typically refuse `AXPress` — `ax_click` returns `not_dispatchable` or AX error `-25205` against them. Use `ax_select` for row targets. A coordinate-based fallback (`click(x, y)`) works but steals focus — the whole reason `ax_*` exists.

**Gotchas:**
- `no_row_ancestor` means the uid is not inside a row at all — you probably targeted the wrong element. Re-snapshot and pick the `AXRow` or one of its descendants.
- `no_outline_container` means the row exists but is not nested in an `AXOutline` / `AXTable` — unusual; custom row containers may need `click(fallback.x, fallback.y)`.

#### `scroll`
Scrolls at a specific screen position.
*   **Inputs:** `x` (number), `y` (number), `delta_y` (integer), `delta_x` (integer, optional).
*   **Direction:** Positive `delta_y` scrolls down; negative scrolls up.

### 3. Window Management

*   `list_windows`: Returns array of `{ id, title, bounds, app_name }`.
*   `list_apps`: List running apps with names, bundle IDs, and PIDs. Optional `app_name` (substring filter) and `user_apps_only` (excludes agents, helpers, daemons).
*   `probe_app`: Classify an app as `Native`, `ElectronApp`, or `ChromeBrowser`. **Input:** `app_name` (string). Works whether the app is running or not. Use it to pick native tools or CDP tools.
*   `focus_window`: Accepts `{ window_id: 123 }`, `{ app_name: "Code" }`, or `{ pid: 999 }`.
*   `launch_app`: Launch an app by name. Optional `args` parameter for CLI arguments. If app is already running with no args, brings to front; with args, returns error (use `quit_app` first).
*   `quit_app`: Quit a running app. Accepts `app_name` (required) and `force` (boolean, default false).

### 4. Hover Tracking

Track cursor hover transitions across UI elements. Designed for observing user navigation patterns (tooltip triggers, dropdown reveals, panel expansions).

* `start_hover_tracking`: Begin polling session. Inputs: `app_name` (optional), `poll_interval_ms` (default 100), `max_duration_ms` (default 60000), `min_dwell_ms` (default 300 — cursor must stay on element this long before recording).
* `get_hover_events`: Drain buffered events since last call. Returns JSON array of dwell events: `{ timestamp_ms, cursor: {x, y}, element: {role, name, label, bounds, app_name, pid}, dwell_ms }`.
* `stop_hover_tracking`: End session, return remaining events.

Tools appear dynamically — `get_hover_events` and `stop_hover_tracking` only show while tracking is active. Use `element_at_point` with event cursor coordinates for full element details (value, etc.).

#### Hover Tracking Example
```
1. start_hover_tracking(min_dwell_ms=500, app_name="Safari")
2. (user moves mouse around)
3. get_hover_events  → [{timestamp_ms: 1200, cursor: {x: 500, y: 300}, element: {role: "AXLink", name: "Home", ...}, dwell_ms: 800}]
4. stop_hover_tracking → [remaining events]
```

### 5. Screen Recording

Record the frontmost app's window as timestamped JPEG frames. Automatically follows app switches — when the user moves to a different app, recording captures the new app's window.

* `start_recording`: Begin recording. Inputs: `output_dir` (required — directory for JPEG frames), `fps` (default 5), `max_duration_ms` (default 60000 = 1 min).
* `stop_recording`: End session, return all frame metadata as JSON array.

Each frame includes: `{ timestamp_ms, path, app_name, window_id, origin_x, origin_y, scale, pixel_width, pixel_height }`.

Tools appear dynamically — `stop_recording` only shows while recording is active.

#### Screen Recording Example
```
1. start_recording(output_dir="/tmp/recording", fps=5)
2. (user interacts with apps)
3. stop_recording → [{timestamp_ms: 1234, path: "/tmp/recording/frame_1234.jpg", app_name: "Safari", ...}, ...]
```

### 6. Browser Automation (CDP)

Connect to Chrome or Electron apps via Chrome DevTools Protocol for DOM-level element targeting. More reliable than screen coordinates for web content — handles offscreen elements, responsive layouts, and iframes.

**Setup:**
- **Chrome 136+:** Must launch with both `--remote-debugging-port=PORT` and `--user-data-dir=PATH` (non-default profile required — Chrome silently ignores the debug port with the default profile). The profile is persistent across launches.
- **Electron apps** (Signal, Discord, Slack, VS Code, etc.): Only need `--remote-debugging-port=PORT`. No `--user-data-dir` required — Electron respects the flag with its default profile.

#### Tools (always listed; calls fail with "No CDP connection" until `cdp_connect` succeeds)

*   `cdp_connect(port)`: Connect to a Chrome/Electron debug port. Auto-selects the first page.
*   `cdp_disconnect`: Disconnect the CDP client. The CDP tools stay listed; subsequent calls return a "not connected" error until `cdp_connect` is called again.
*   `cdp_take_dom_snapshot(max_nodes?)`: Full DOM snapshot of interactive elements — returns UIDs prefixed `d` (e.g., d1, d2) with roles, labels, and parent context. Use when you need the complete page structure; for targeted lookups prefer `cdp_find_elements`. **Always take a fresh snapshot after any navigation or DOM change before resolving UIDs.**
*   `cdp_summarize_page`: Compact page summary — URL, title, page generation, and interactive elements grouped by role with sample labels. Returns no UIDs and does not replace the current UID snapshot. Use for orientation before targeted `cdp_find_elements` queries.
*   `cdp_find_elements(query, role?, max_results?)`: **Preferred discovery tool.** Search the live DOM for interactive elements matching a text query. Returns matches with `d`-prefixed UIDs plus a page-level inventory grouped by role — focused results without flooding context.
*   `cdp_get_element_context(uid, ancestor_depth?, sibling_limit?, child_limit?, max_chars?)`: Expand a UID from the most recent `cdp_find_elements` / `cdp_take_dom_snapshot` call. Returns the stored match evidence, nearby matches, and bounded live DOM context (ancestors, siblings, children). Use when a search returns several plausible matches.
*   `cdp_click(uid, dbl_click?)`: Click an element by UID. Scrolls into view automatically.
*   `cdp_hover(uid)`: Hover over an element by UID.
*   `cdp_fill(uid, value)`: Type text into an input/textarea or select an option from a `<select>`.
*   `cdp_press_key(key)`: Press a key or combo (e.g., `"Enter"`, `"Control+A"`, `"Control+Shift+R"`). Modifiers: Control, Shift, Alt, Meta.
*   `cdp_type_text(text, submit_key?)`: Character-by-character keyboard input into a previously focused element. Use `cdp_fill` for form fields; use this for inputs that react to each keypress.
*   `cdp_handle_dialog(action, prompt_text?)`: Accept or dismiss a JS dialog (alert, confirm, prompt).
*   `cdp_navigate(url?, type?)`: Navigate to a URL, or go back/forward/reload. Type: `"url"` (default), `"back"`, `"forward"`, `"reload"`.
*   `cdp_new_page(url)`: Create a new tab and navigate to URL. Becomes the selected page.
*   `cdp_close_page(page_idx)`: Close a tab by index. Cannot close the last page.
*   `cdp_wait_for(text, timeout?, include_snapshot?)`: Wait for any value in `text` to appear on the page (polls `document.body.innerText`, default 10s timeout). Returns a one-line "text appeared after Xms" header by default; pass `include_snapshot=true` to also append a DOM snapshot.
*   `cdp_wait_for_page_change(scope_uid?, condition?, goal?, timeout?, poll_interval_ms?, stable_ms?, include_snapshot?)`: Block until the page, or the element at `scope_uid`, has a semantic visible-text / editor-value change. Returns compact before/after deltas. Use for unknown incoming content (messages, replies, notifications). Prefer a stable container as `scope_uid`. Default and max timeout: 55s.
*   `cdp_evaluate_script(function, args?)`: Evaluate JS in the page. No args: `() => document.title`. With element args: `(el) => el.innerText` + `args=[{uid: "d5"}]`.
*   `cdp_list_pages`: List open tabs/windows with indices. Selected page marked with `*`.
*   `cdp_select_page(page_idx)`: Switch to a tab/window by index.
*   `cdp_element_at_point(x, y)`: Given screen coordinates (in points), hit-test the DOM element at that position. Always returns `backend_node_id`. If a current DOM snapshot already contains the node, the response also carries the `d`-prefixed UID, role, and name; otherwise `uid` is `null` with a note to call `cdp_take_dom_snapshot` or `cdp_find_elements`. Requires an active CDP connection.

#### Key Patterns

**Finding elements in large snapshots:** Snapshots can have 500+ elements. Look for elements by their role and name — buttons, links, textboxes, and headings are the most useful. Elements like `StaticText`, `InlineTextBox`, `none`, and `generic` are usually structural and can be skipped when looking for interactive targets.

#### CDP Workflow Examples

**Chrome:**
```
1. launch_app(app_name="Google Chrome", args=["--remote-debugging-port=9222", "--user-data-dir=/tmp/chrome-profile"])
2. cdp_connect(port=9222)                        → "Connected. Selected page: chrome://new-tab-page/"
3. cdp_navigate(url="https://example.com")
4. cdp_find_elements(query="Search")              → matches=[{uid:"d1", role:"textbox", label:"Search", ...}]
5. cdp_fill(uid="d1", value="search query")
6. cdp_press_key(key="Enter")
7. cdp_wait_for(text=["Results"])
8. cdp_find_elements(query="Submit")              → matches=[{uid:"d1", role:"button", label:"Submit", ...}]
9. cdp_click(uid="d1")                             → "Clicked uid=d1 'Submit' (button) at (200, 300)"
```

**Electron app (e.g., Slack, Discord, Signal):**
```
1. quit_app(app_name="MyElectronApp")
2. launch_app(app_name="MyElectronApp", args=["--remote-debugging-port=9333"])
3. cdp_connect(port=9333)                  → "Connected. Selected page: file:///...app.html"
4. cdp_take_dom_snapshot()                  → uid=d1 button "New Chat" tag=button ... (full interactive surface)
5. cdp_click(uid="d5")                      → click a list item or button
6. cdp_take_dom_snapshot()                  → fresh snapshot of the new view
7. cdp_fill(uid="d12", value="hello")
```

### 7. Android Device Control

Android support is built into every release. No feature flag or separate build is required.

Android tools use the `android_` prefix. Device management tools are always available; all other tools appear after connecting to a device.

#### Device Management
*   `android_list_devices`: Lists connected ADB devices. Returns `[{ "serial": "abc123", "state": "device" }]`.
*   `android_connect`: Connect to a device. **Input:** `serial` (string). Unlocks all other `android_*` tools.
*   `android_disconnect`: Disconnect from the current device.

#### Vision
*   `android_screenshot`: Captures the device screen. Returns a JPEG image + metadata `{ "device": "abc123", "width": 1080, "height": 2400, "scale": 1.0 }`.
*   `android_find_text`: Find UI elements by text (case-insensitive substring). Uses `uiautomator dump` to search the accessibility tree. **Input:** `text` (string). Returns `[{ "text": "OK", "x": 540, "y": 1200, "bounds": { "x": 480, "y": 1170, "width": 120, "height": 60 } }]`.

#### Input
*   `android_click`: Tap at screen coordinates. **Inputs:** `x`, `y` (numbers).
*   `android_swipe`: Swipe between two points. **Inputs:** `start_x`, `start_y`, `end_x`, `end_y` (numbers), `duration_ms` (optional).
*   `android_type_text`: Type text on the device. **Input:** `text` (string). Handles shell escaping automatically.
*   `android_press_key`: Press a key. **Input:** `key` (string, e.g., `"KEYCODE_HOME"`, `"KEYCODE_BACK"`, `"KEYCODE_ENTER"`).

#### App & Display Info
*   `android_launch_app`: Launch an app. **Input:** `package` (string, e.g., `"com.android.settings"`).
*   `android_list_apps`: List installed packages.
*   `android_get_display_info`: Returns `{ "width": 1080, "height": 2400, "density": 440 }`.
*   `android_get_current_activity`: Returns the current foreground activity component.

#### Android Workflow Example
```
1. android_list_devices                    → [{"serial": "abc123", "state": "device"}]
2. android_connect(serial="abc123")        → Connected
3. android_screenshot                      → [image + metadata]
4. android_find_text(text="Settings")      → [{"text": "Settings", "x": 540, "y": 800, ...}]
5. android_click(x=540, y=800)             → Tapped
6. android_screenshot                      → Verify result
```

**Note:** Android coordinates are absolute screen pixels (no scale conversion needed). Use `x`/`y` from `android_find_text` directly with `android_click`.

### 8. AppDebugKit Apps

For apps that embed AppDebugKit (a debug server reachable over WebSocket). `app_connect` is always listed; all other `app_*` tools appear after it connects.

*   `app_connect(url, expected_bundle_id?, expected_app_name?)`: Connect to the app's debug server (e.g. `ws://127.0.0.1:9222`). Fails if the expected bundle ID or name does not match.
*   `app_disconnect`: Disconnect from the debug server.
*   `app_get_info`: Runtime info (name, bundle ID, version, etc.).
*   `app_get_tree(depth?, root_id?)`: View hierarchy (default depth 5; `-1` for unlimited).
*   `app_query(selector, all?)`: Find elements with a CSS-like selector (`#id`, `.ClassName`, `[prop=value]`).
*   `app_get_element(element_id)`: Detailed info for one element.
*   `app_click(element_id, click_count?)`: Click an element by ID.
*   `app_type(text, element_id?, clear_first?)`: Type into an element (focused element if `element_id` is omitted).
*   `app_press_key(key, modifiers?)`: Press a key or combination.
*   `app_focus(element_id)`: Make an element first responder.
*   `app_screenshot(element_id?)`: Screenshot an element, or the whole window if omitted.
*   `app_list_windows`: List the app's windows.
*   `app_focus_window(window_id)`: Make a window key and main.

---

## 📐 Coordinate Systems & Best Practices

**CRITICAL:** There are two ways to target clicks. Choose ONE based on your data source.

### Method A: Absolute Screen Coordinates (Recommended)
Use this when you have data from `find_text` OR `take_screenshot` (OCR results).
*   **Source:** `find_text` returns `{ "x": 500, "y": 300 }`.
*   **Action:** `click(x=500, y=300)`.
*   **Why:** These are already global screen coordinates.

### Method B: Relative Screenshot Coordinates
Use this when you (the model) look at the *image* from `take_screenshot` and estimate positions (e.g., "The icon is at 50% width").
*   **Source:** `take_screenshot` returns metadata `{ "screenshot_origin_x": 100, "screenshot_origin_y": 100, "screenshot_scale": 2.0, "screenshot_pixel_width": 1920, "screenshot_pixel_height": 1080 }`.
*   **Your Vision:** You see a button at pixel `(x=50, y=50)` inside the image.
*   **Action:** `click(screenshot_x=50, screenshot_y=50, screenshot_origin_x=100, screenshot_origin_y=100, screenshot_scale=2.0)`.
*   **Why:** The tool handles the math to convert image-pixels to screen-pixels.

**Manual conversion (for tools that only accept screen coordinates, e.g. `drag`):**
*   `screen_x = screenshot_origin_x + (screenshot_x / screenshot_scale)`
*   `screen_y = screenshot_origin_y + (screenshot_y / screenshot_scale)`

---

## ⚡ Intent Examples (Chain of Thought)

### "Click the 'Save' button in Notepad"
1.  **Thought:** I need to find the text "Save" in the app "Notepad".
2.  **Call:** `focus_window(app_name="Notepad")`
3.  **Call:** `find_text(text="Save")` -> Returns `[{"text":"Save","x":200,"y":400,...}]`
4.  **Call:** `click(x=200, y=400)`

### "Draw a circle in Paint"
1.  **Thought:** Text search won't work for a canvas. I need to see the screen.
2.  **Call:** `take_screenshot(app_name="Paint")`
3.  **Analysis:** I see the canvas center at pixel (500, 500) in the image.
4.  **Compute:** `start_x = screenshot_origin_x + 500 / screenshot_scale`, `start_y = screenshot_origin_y + 500 / screenshot_scale`
5.  **Call:** `drag(start_x=..., start_y=..., end_x=..., end_y=...)`

### "Copy text from this window"
1.  **Thought:** I can read text directly from the screenshot OCR data without using the clipboard.
2.  **Call:** `take_screenshot(include_ocr=true)`
3.  **Action:** Read the OCR summary text in the response (lines include clickable coordinates).

### "Open the Wi-Fi pane in System Settings (macOS)"
1.  **Thought:** This is a native macOS app. The AX-dispatch branch is preferred — no mouse movement, no focus steal, and sidebar rows are a perfect fit for `ax_select`.
2.  **Call:** `take_ax_snapshot(app_name="System Settings")` → tree with a row `AXRow "Wi-Fi"` at `uid="a18g3"`.
3.  **Thought:** Sidebar rows are backed by `NSOutlineView` — they refuse `AXPress`, so use `ax_select` rather than `ax_click`.
4.  **Call:** `ax_select(uid="a18g3")` → `{ "ok": true, "dispatched_via": "AXSelectedRows" }`.
5.  **Call:** `take_ax_snapshot(app_name="System Settings")` → verify the Wi-Fi detail pane is visible. The generation bumps; any earlier uids are now stale.

### "Fill the search field in Finder (macOS)"
1.  **Thought:** Native macOS app with a text field. `ax_set_value` writes `kAXValueAttribute` directly — but it doesn't fire keydown/keyup and doesn't feed IME, so if the app has key-handlers fall back to `click` + `type_text`.
2.  **Call:** `take_ax_snapshot(app_name="Finder")` → `AXSearchField` at `uid="a7g2"`.
3.  **Call:** `ax_set_value(uid="a7g2", text="invoice.pdf")` → `{ "ok": true, "dispatched_via": "AXSetAttributeValue" }`.
4.  **Fallback on `not_dispatchable`:** `click(fallback.x, fallback.y)` to focus, then `type_text("invoice.pdf")` for real key events.

---

## 🖼️ Template Matching (Advanced Vision)

For finding non-text UI elements like icons, shapes, or specific visual patterns, use the `find_image` tool with template matching.

### `load_image`
Load an image from a local file path and cache it for use with `find_image`.
*   **Inputs:**
    *   `path` (string, required): Local filesystem path to the image file.
    *   `id_prefix` (string, optional): Prefix for the generated ID (e.g., "template", "mask").
    *   `max_width`, `max_height` (integer, optional): Downscale constraints (maintains aspect ratio).
    *   `as_mask` (boolean, default `false`): Convert to single-channel grayscale mask.
    *   `return_base64` (boolean, default `false`): Include base64-encoded image data in response.
*   **Returns (JSON):**
    ```json
    {
      "image_id": "template-0",
      "width": 64,
      "height": 64,
      "channels": 4,
      "mime": "image/png",
      "sha256": "abc123..."
    }
    ```

### `find_image`
Find a template image within a screenshot using template matching. Returns precise click coordinates.
*   **Inputs:**
    *   `screenshot_id` (string, optional): Screenshot ID from `take_screenshot` (preferred).
    *   `screenshot_image_base64` (string, optional): Base64-encoded screenshot (if no screenshot_id).
    *   `template_id` (string, optional): Image ID from `load_image` (preferred).
    *   `template_image_base64` (string, optional): Base64-encoded template (if no template_id).
    *   `mask_id` (string, optional): Image ID for the mask (from `load_image`).
    *   `mask_image_base64` (string, optional): Base64-encoded mask (white=match, black=ignore).
    *   `mode` (string, default `"fast"`): `"fast"` or `"accurate"`. Fast uses downscaling/early-exit for speed; accurate uses full-res, wider scales, smaller stride.
    *   `threshold` (number, optional): Minimum match score 0.0-1.0.
    *   `max_results` (integer, optional): Maximum matches to return.
    *   `scales` (object, optional): Scale search range `{min, max, step}`.
    *   `rotations` (array, optional): Rotations to try in degrees (only 0, 90, 180, 270 supported).
*   **Returns (JSON):**
    ```json
    {
      "matches": [
        {
          "score": 0.95,
          "bbox": {"x": 100, "y": 200, "w": 64, "h": 64},
          "center": {"x": 132, "y": 232},
          "scale": 1.0,
          "rotation": 0,
          "screen_x": 166,
          "screen_y": 216
        }
      ]
    }
    ```

### Template Matching Example Flow
```
1. take_screenshot(app_name="MyApp")      → screenshot_id: "screenshot-0"
2. load_image(path="/path/to/icon.png")   → image_id: "image-0"
3. find_image(screenshot_id="screenshot-0", template_id="image-0")
   → matches: [{screen_x: 150, screen_y: 200, ...}]
4. click(x=150, y=200)
```
