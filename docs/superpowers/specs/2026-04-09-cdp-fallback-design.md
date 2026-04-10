# CDP Fallback for find_ui_elements

## Problem

Electron/Chromium apps expose little to nothing via AT-SPI. `find_ui_elements` returns 0 matches for ~40% of desktop apps (Obsidian, VS Code, Discord, Slack, Spotify, etc.). The agent is blind.

## Solution

When AT-SPI returns 0 matches and a CDP session exists, `find_ui_elements` falls back to CDP DOM queries. Same output format, different source. No new tools.

## Changes

### 1. Dependency

Add `chromiumoxide` (async, tokio-native CDP client). Supports connecting to an existing browser via `Browser::connect()`.

### 2. Session struct

Add `cdp_browser: Option<chromiumoxide::Browser>` field. Set when `launch_app` is called with `chromium: true`. `None` otherwise.

### 3. launch_app

Add `chromium: bool` parameter (default false) to `LaunchAppParams`.

When `chromium` is true:
1. Bind `TcpListener` to `:0`, grab the port, drop the listener.
2. Append ` --remote-debugging-port=<port>` to the command before writing to bwrap stdin.
3. After the existing window-detection poll succeeds, poll `http://127.0.0.1:<port>/json/version` until it responds (same timeout).
4. `Browser::connect(format!("http://127.0.0.1:{port}"))`.
5. Spawn the handler task: `tokio::spawn(async move { while handler.next().await.is_some() {} })`.
6. Store `Browser` in `Session.cdp_browser`.

Network is shared (no `--unshare-net`), so the port is reachable from the host.

### 4. find_ui_elements

After the existing AT-SPI traversal, if `out.is_empty()`:
1. Check `Session.cdp_browser` — if `None`, return existing "no elements" response.
2. Get pages via `browser.pages()`, use first page.
3. Run `Runtime.evaluate` with JS that queries all interactive elements and returns their tag, text content, role attribute, and bounding rects:
   ```js
   [...document.querySelectorAll('button, a, input, select, textarea, [role], [onclick], [tabindex]')]
     .filter(el => el.offsetParent !== null)
     .map(el => {
       const r = el.getBoundingClientRect();
       return { tag: el.tagName, text: (el.textContent || '').trim().slice(0, 80),
                role: el.getAttribute('role') || el.tagName.toLowerCase(),
                x: r.x, y: r.y, w: r.width, h: r.height };
     })
   ```
4. Filter elements with `w > 1 && h > 1` (same as `is_useful()`).
5. Format each as `role\ttext\t(x, y, wxh)` — identical to AT-SPI output.
6. Coordinates are already window-relative (CDP bounding rects are relative to the viewport, which maps 1:1 to kwin-mcp's window-relative coordinate space).

### 5. session_stop

If `cdp_browser` is `Some`, drop it before killing bwrap. No explicit cleanup needed — dropping closes the WebSocket.

## Requirements Check

| Req | Pass | Notes |
|---|---|---|
| Zero shell | Yes | All Rust: TcpListener, chromiumoxide, tokio |
| Isolation | Yes | App inside bwrap, CDP port on shared localhost |
| 1:1 UX fidelity | Yes | `--remote-debugging-port` doesn't alter rendering |
| Agent write boundary | Yes | Same boundary as launch_app (overlay fs) |
| Unprivileged | Yes | WebSocket client, no privileges |
| HID isolation | Yes | No HID changes |
| No user setup | Yes | chromiumoxide compiled into binary |

## Non-goals

- No `cdp_eval` tool. The agent doesn't get arbitrary JS execution.
- No `cdp_click` tool. The agent uses coordinates from `find_ui_elements` with existing `mouse_click`.
- No auto-detection of Chromium binaries. The agent passes `chromium: true` explicitly.
