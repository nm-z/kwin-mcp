# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

MCP server for KDE Plasma 6 Wayland GUI automation. Single-binary Rust project using `rmcp` for MCP protocol, `reis` for EIS (Emulated Input Sender) input control, `atspi` for AT-SPI2 accessibility tree traversal, `chromiumoxide` for CDP (Chrome DevTools Protocol) element discovery in Electron/Chromium apps, and `zbus` for KWin D-Bus IPC. Serves over stdio transport.

## Build & Lint

```bash
cargo build                # debug build
cargo build --release      # release build
cargo clippy               # lint (strict: unwrap/expect/todo/dead_code all denied)
```

No tests exist — binary-only project, no lib target.

## Clippy Rules

All denied: `unwrap_used`, `expect_used`, `todo`, `unimplemented`, `unreachable`, `as_conversions`, `wildcard_imports`, `wildcard_enum_match_arm`, `dead_code`. Use `map_err`/`ok_or_else`/match arms — never `unwrap()` or `expect()`. Numeric casts must use `try_from`/`try_into`, not `as`.

## Architecture

Everything lives in `src/main.rs` (~2300 lines). Key layers:

- **Input parsing** (top of file): `parse_combo`, `char_key`, `btn_code` — maps key names and characters to evdev keycodes via `keyboard-codes` crate.
- **KWin D-Bus proxies**: `KWinEis`, `KWinScreenShot2` — zbus proxy traits for EIS input and screenshots.
- **EIS input**: `Eis` struct holds EIS context, pointer/button/scroll/keyboard devices. Methods: `from_fd()`, `move_abs()`, `button()`, `scroll_discrete()`, `scroll_smooth()`, `key()`. Negotiation is blocking (`tokio::task::spawn_blocking`).
- **CDP integration**: `chromiumoxide::Browser` stored as `Option<Arc<Browser>>` in Session. `launch_app` always injects `--remote-debugging-port=<free_port>` and tries CDP connection (2s timeout) — Chromium apps respond, native apps ignore the flag. `find_ui_elements` dispatches via `match`: CDP path queries DOM for interactive elements, AT-SPI path traverses the accessibility tree.
- **KWallet emulator**: `KWalletEmulator` implements `org.kde.KWallet` on the container session bus at `/modules/kwalletd6`. `dump_host_wallet()` at `session_start` reads every folder/entry from the host's unlocked kwallet via the user's host session bus; the emulator serves that snapshot. Emits `walletOpened` / `walletAsyncOpened` signals so chromium's async open path resolves — sync method returns alone aren't enough. Container's real `kwalletd6` is prevented from auto-activating by bind-mounting `/dev/null` over its `.service` file.
- **NetworkManager proxy**: `xdg-dbus-proxy` is spawned on the host before bwrap, filtering the host system bus with `--talk=org.freedesktop.NetworkManager`. The filtered socket is bind-mounted into the container as `/run/dbus/system_bus_socket` so chromium's `NetworkChangeNotifier` reaches NM without broad system-bus access.
- **`Session` struct**: Owns the bwrap child process, bwrap stdin (for launch_app), D-Bus connections (kwin_conn, _proxy_conn, _wallet_conn — the wallet_conn is held so the claimed well-known name outlives session_start), EIS handle, host XDG dir, optional CDP browser handle, xdg-dbus-proxy child. Created by `session_start`, destroyed by `session_stop`.
- **`KwinMcp` struct**: `Arc<Mutex<Option<Session>>>` — the MCP server. Implements `ToolHandler` with 12 tools. `with_session()` gates all tools behind session existence.

## Container Architecture

`session_start` spawns an isolated session via **bubblewrap (bwrap)**:
- `--overlay-src $HOME --tmp-overlay $HOME` — overlayfs on user's home (default writable=false); `--bind / /` when `writable=true`
- `--die-with-parent` — auto-kills container if MCP server dies
- `--unshare-pid --unshare-uts --unshare-ipc` — namespace isolation (network is shared)
- `--dev-bind /dev/dri` and `--dev-bind /dev/uinput` — GPU and input device access
- `--bind {host_xdg_dir}` — shared directory for D-Bus socket
- `--ro-bind-try {host_xdg_dir}/system_bus_socket /run/dbus/system_bus_socket` — NetworkManager-only proxy socket from host
- `--ro-bind /dev/null` over `org.kde.kwalletd6.service`, `org.kde.secretservicecompat.service`, `org.freedesktop.impl.portal.desktop.kwallet.service`, `org.kde.secretprompter.service` — masks these dbus service files so the container's dbus-daemon cannot auto-activate the real kwalletd6/ksecretd; our emulator owns the wallet name exclusively

Inside the container, an inline bash entrypoint starts: dbus-daemon (socket in shared dir), KWin `--virtual --xwayland`, AT-SPI, PipeWire, WirePlumber. The host connects to the container's D-Bus via the shared socket. Apps are launched by writing commands to bwrap's stdin.

`session_stop` kills the bwrap process group (negative PID SIGTERM), kills the xdg-dbus-proxy child, and removes the host XDG dir.

## MCP Tools

`session_start` spawns an isolated KWin Wayland session (1280x800, XWayland, own D-Bus). All other tools require an active session. `session_stop` kills the process group. Input tools (`mouse_*`, `keyboard_*`) use window-relative coordinates — window position is added internally via `active_window_info()` KWin scripting. Screenshots go through `org.kde.KWin.ScreenShot2` D-Bus interface, returning raw ARGB32 converted to PNG. `accessibility_tree` traverses AT-SPI2 with configurable depth/filters. `find_ui_elements` searches by name/role — automatically uses CDP DOM queries for Chromium/Electron apps, AT-SPI for native apps. `launch_app` auto-detects Chromium apps by injecting `--remote-debugging-port` and attempting CDP connection. Chromium/Chrome in the container transparently read the host's kwallet data (cookies decrypt, saved passwords available) via the emulator — no prompts.

## Key Patterns

- All coordinates are window-relative, converted to absolute internally
- D-Bus screenshot returns raw ARGB32 premultiplied pixels, not PNG — the `screenshot` tool handles conversion
- EIS negotiation is blocking I/O, runs in `tokio::task::spawn_blocking`
- D-Bus socket at `{host_xdg_dir}/bus` — accessible from both host and container via bind-mount
- Container inherits all host config via overlayfs — no manual kwinrc/service configuration
- CDP port is reachable from host because network namespace is shared (no `--unshare-net`)
- `find_ui_elements` uses `match` to dispatch: `Some(browser)` → CDP DOM query, `None` → AT-SPI traversal
- Connections registered on container bus that need to outlive `session_start` must be stored in `Session` (e.g., `_wallet_conn`). Dropped connections release their claimed names and registered objects.
- Emulating a well-known D-Bus service that normally auto-activates (kwalletd6, ksecretd, etc.) requires masking its `.service` file with `--ro-bind /dev/null`. Otherwise the dbus-daemon races our `request_name` and spawns the real binary first.
