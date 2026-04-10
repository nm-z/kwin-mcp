# CDP Fallback for find_ui_elements — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When AT-SPI returns 0 matches in `find_ui_elements`, fall back to CDP DOM queries for Chromium-based apps, returning results in the same format.

**Architecture:** Add `chromiumoxide` dep. `launch_app` gains `chromium: bool` — when true, injects `--remote-debugging-port`, connects via WebSocket, stores `Browser` in Session. `find_ui_elements` checks CDP when AT-SPI is empty.

**Tech Stack:** `chromiumoxide` (async CDP client), `tokio` (existing)

---

### Task 1: Add chromiumoxide dependency

**Files:**
- Modify: `Cargo.toml:19` (add dep after `png`)

- [ ] **Step 1: Add dependency**

In `Cargo.toml`, add after the `png = "0.17"` line:

```toml
chromiumoxide = { version = "0.9", features = ["tokio-runtime"], default-features = false }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo check 2>&1 | head -5`
Expected: no errors (warnings OK — we haven't used it yet)

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "deps: add chromiumoxide for CDP fallback"
```

---

### Task 2: Add CDP browser to Session

**Files:**
- Modify: `src/main.rs:467-477` (Session struct)
- Modify: `src/main.rs:549-558` (teardown fn)

- [ ] **Step 1: Add field to Session**

In `src/main.rs`, add `cdp_browser` field to the `Session` struct (after `_uinput_keyboard`):

```rust
struct Session {
    kwin_conn: zbus::Connection,
    _proxy_conn: zbus::Connection,
    kwin_unique_name: String,
    eis: Eis,
    bwrap_child: std::process::Child,
    bwrap_stdin: std::process::ChildStdin,
    host_xdg_dir: std::path::PathBuf,
    _uinput_mouse: evdev::uinput::VirtualDevice,
    _uinput_keyboard: evdev::uinput::VirtualDevice,
    cdp_browser: Option<chromiumoxide::Browser>,
}
```

- [ ] **Step 2: Fix all Session construction sites**

Find every place `Session { ... }` is constructed (should be in `session_start`). Add `cdp_browser: None` to each. Search for the pattern:

```bash
grep -n 'Session {' src/main.rs
```

- [ ] **Step 3: Update teardown**

In `fn teardown`, drop `cdp_browser` before dropping `bwrap_stdin` (so the WebSocket closes before the process is killed):

```rust
fn teardown(mut sess: Session) {
    drop(sess.cdp_browser);
    drop(sess.bwrap_stdin);
    let pid = sess.bwrap_child.id();
    if let Ok(neg) = i32::try_from(pid).map(|p| -p) {
        let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(neg), nix::sys::signal::Signal::SIGTERM);
    }
    let _ = sess.bwrap_child.wait();
    if let Err(e) = std::fs::remove_dir_all(&sess.host_xdg_dir) { eprintln!("teardown cleanup: {e}"); }
}
```

- [ ] **Step 4: Verify it compiles**

Run: `cargo clippy 2>&1 | tail -10`
Expected: clean (no errors, no denied lints)

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: add cdp_browser field to Session"
```

---

### Task 3: Modify launch_app for CDP connection

**Files:**
- Modify: `src/main.rs:791-794` (LaunchAppParams)
- Modify: `src/main.rs:1583-1614` (launch_app fn)

- [ ] **Step 1: Add chromium param to LaunchAppParams**

```rust
#[derive(Deserialize, schemars::JsonSchema)]
struct LaunchAppParams {
    command: String,
    #[serde(default)]
    chromium: bool,
}
```

- [ ] **Step 2: Modify launch_app to inject debug port and connect**

Replace the `launch_app` method body. Key changes:
1. When `chromium` is true, bind a free port, append `--remote-debugging-port=<port>` to command
2. After window detection succeeds, poll the CDP HTTP endpoint
3. Connect via `chromiumoxide::Browser::connect`
4. Store in session

```rust
    async fn launch_app(
        &self,
        peer: rmcp::Peer<rmcp::RoleServer>,
        Parameters(params): Parameters<LaunchAppParams>,
    ) -> Result<CallToolResult, McpError> {
        use std::io::Write;

        // If chromium mode, find a free port
        let cdp_port = if params.chromium {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(KwinError::from)?;
            let port = listener.local_addr().map_err(KwinError::from)?.port();
            drop(listener);
            Some(port)
        } else {
            None
        };

        let cmd = if let Some(port) = cdp_port {
            format!("{} --remote-debugging-port={port}", params.command)
        } else {
            params.command.clone()
        };

        let (conn, kwin_unique, xdg) = {
            let mut guard = self.session.lock().await;
            let sess = guard.as_mut().ok_or_else(|| {
                McpError::internal_error("no session — call session_start first", None)
            })?;
            writeln!(sess.bwrap_stdin, "{cmd}").map_err(KwinError::from)?;
            sess.bwrap_stdin.flush().map_err(KwinError::from)?;
            (sess.kwin_conn.clone(), sess.kwin_unique_name.clone(), sess.host_xdg_dir.clone())
        };

        // Poll for window readiness (up to 15s)
        let mut window_info = None;
        for _ in 0..75_u32 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if let Ok((_, _, info)) = active_window_info(&conn, &kwin_unique, &xdg).await {
                window_info = Some(info);
                break;
            }
        }

        // If chromium mode, connect CDP after window is up
        if let Some(port) = cdp_port {
            let cdp_url = format!("http://127.0.0.1:{port}");

            // Poll for CDP endpoint readiness (up to 15s)
            let mut cdp_ready = false;
            for _ in 0..75_u32 {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                match reqwest::get(format!("{cdp_url}/json/version")).await {
                    Ok(r) if r.status().is_success() => { cdp_ready = true; break; }
                    _ => continue,
                }
            }

            if cdp_ready {
                match chromiumoxide::Browser::connect(&cdp_url).await {
                    Ok((browser, mut handler)) => {
                        tokio::spawn(async move { while handler.next().await.is_some() {} });
                        let mut guard = self.session.lock().await;
                        if let Some(sess) = guard.as_mut() {
                            sess.cdp_browser = Some(browser);
                        }
                    }
                    Err(e) => eprintln!("CDP connect failed: {e}"),
                }
            } else {
                eprintln!("CDP endpoint not ready after 15s");
            }
        }

        match window_info {
            Some(info) => Ok(structured_result(&peer, format!("launched: {} window: {info}", params.command), serde_json::json!({
                "action": "launch", "command": params.command, "window": info,
                "cdp": cdp_port.is_some(),
            })).await),
            None => Ok(structured_result(&peer, format!("launched: {} (no window after 15s)", params.command), serde_json::json!({
                "action": "launch", "command": params.command, "window": "timeout",
                "cdp": false,
            })).await),
        }
    }
```

**Note:** This adds `reqwest` as a dependency for the HTTP poll. Add to Cargo.toml:

```toml
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
```

Also add the error variant to `KwinError`:

```rust
#[error(transparent)] Reqwest(#[from] reqwest::Error),
```

And add to the use block at the top of the file:

```rust
use futures::StreamExt;
```

And add `futures` to Cargo.toml (needed for `handler.next()`):

```toml
futures = "0.3"
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo clippy 2>&1 | tail -20`
Expected: clean

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs
git commit -m "feat: launch_app chromium mode with CDP connection"
```

---

### Task 4: CDP fallback in find_ui_elements

**Files:**
- Modify: `src/main.rs:1326-1394` (find_ui_elements fn)

- [ ] **Step 1: Add CDP fallback after AT-SPI returns empty**

Replace the `if out.is_empty()` block at the end of `find_ui_elements` (lines 1388-1393). When AT-SPI finds nothing and a CDP browser is available, query the DOM instead:

```rust
        if out.is_empty() {
            // CDP fallback: try DOM queries if a chromium session exists
            let cdp_browser = self.session.lock().await
                .as_ref()
                .and_then(|s| s.cdp_browser.clone());

            if let Some(browser) = cdp_browser {
                if let Ok(pages) = browser.pages().await {
                    if let Some(page) = pages.into_iter().next() {
                        let js = r#"
                            JSON.stringify(
                                [...document.querySelectorAll('button, a, input, select, textarea, [role], [onclick], [tabindex]')]
                                    .filter(el => el.offsetParent !== null)
                                    .map(el => {
                                        const r = el.getBoundingClientRect();
                                        return {
                                            role: el.getAttribute('role') || el.tagName.toLowerCase(),
                                            text: (el.textContent || '').trim().slice(0, 80),
                                            x: Math.round(r.x),
                                            y: Math.round(r.y),
                                            w: Math.round(r.width),
                                            h: Math.round(r.height)
                                        };
                                    })
                            )
                        "#;
                        if let Ok(result) = page.evaluate(js).await {
                            if let Ok(json_str) = result.into_value::<String>() {
                                #[derive(Deserialize)]
                                struct CdpElement {
                                    role: String,
                                    text: String,
                                    x: i32,
                                    y: i32,
                                    w: i32,
                                    h: i32,
                                }
                                if let Ok(elements) = serde_json::from_str::<Vec<CdpElement>>(&json_str) {
                                    for el in &elements {
                                        if el.w > 1 && el.h > 1
                                            && (el.text.to_lowercase().contains(&query)
                                                || el.role.to_lowercase().contains(&query))
                                        {
                                            out.push(format!(
                                                "{}\t{}\t({}, {}, {}x{})",
                                                el.role, el.text, el.x, el.y, el.w, el.h
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if out.is_empty() {
            Ok(structured_result(&peer, format!("no elements matching '{}'", params.query), serde_json::json!({"matches": 0, "query": params.query})).await)
        } else {
            let results = out.join("\n");
            Ok(structured_result(&peer, results.clone(), serde_json::json!({"matches": out.len(), "query": params.query, "results": results})).await)
        }
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo clippy 2>&1 | tail -20`
Expected: clean

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: CDP DOM fallback in find_ui_elements"
```

---

### Task 5: Update tool description and build release

**Files:**
- Modify: `src/main.rs:1327-1329` (find_ui_elements tool annotation)
- Modify: `src/main.rs:1584-1585` (launch_app tool annotation)

- [ ] **Step 1: Update tool descriptions**

Update `find_ui_elements` description to mention CDP fallback:

```rust
    #[rmcp::tool(
        name = "find_ui_elements",
        description = "Search UI elements by name/role/description (case-insensitive). Falls back to CDP DOM queries for Chromium apps launched with chromium: true.",
        annotations(read_only_hint = true)
    )]
```

Update `launch_app` description to mention chromium flag:

```rust
    #[rmcp::tool(
        name = "launch_app",
        description = "Launch an application inside the container. Set chromium: true for Electron/Chromium apps to enable CDP-based element discovery in find_ui_elements."
    )]
```

- [ ] **Step 2: Full clippy check**

Run: `cargo clippy 2>&1`
Expected: zero warnings, zero errors

- [ ] **Step 3: Release build**

Run: `cargo build --release 2>&1 | tail -5`
Expected: compiles cleanly

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: update tool descriptions for CDP support"
```
