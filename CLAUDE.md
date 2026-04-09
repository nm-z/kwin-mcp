# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What This Is

MCP server for KDE Plasma 6 Wayland GUI automation. Single-binary Rust project using `rmcp` for MCP protocol, `reis` for EIS (Emulated Input Sender) input control, `atspi` for AT-SPI2 accessibility tree traversal, and `zbus` for KWin D-Bus IPC. Serves over stdio transport.

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

Everything lives in `src/main.rs` (~1333 lines). Key layers:

- **Input parsing** (top of file): `parse_combo`, `char_key`, `btn_code` — maps key names and characters to evdev keycodes via `keyboard-codes` crate.
- **KWin D-Bus proxies**: `KWinEis`, `KWinScreenShot2` — zbus proxy traits for EIS input and screenshots.
- **EIS input**: `Eis` struct holds EIS context, pointer/button/scroll/keyboard devices. Methods: `from_fd()`, `move_abs()`, `button()`, `scroll_discrete()`, `scroll_smooth()`, `key()`. Negotiation is blocking (`tokio::task::spawn_blocking`).
- **`Session` struct**: Owns the bwrap child process, bwrap stdin (for launch_app), D-Bus connection, EIS handle, host XDG dir. Created by `session_start`, destroyed by `session_stop`.
- **`KwinMcp` struct**: `Arc<Mutex<Option<Session>>>` — the MCP server. Implements `ToolHandler` with 12 tools. `with_session()` gates all tools behind session existence.

## Container Architecture

`session_start` spawns an isolated session via **bubblewrap (bwrap)**:
- `--overlay-src / --tmp-overlay /` — overlayfs on host root, writes are ephemeral
- `--die-with-parent` — auto-kills container if MCP server dies
- `--unshare-pid --unshare-uts --unshare-ipc` — namespace isolation
- `--dev-bind /dev/dri` and `--dev-bind /dev/uinput` — GPU and input device access
- `--bind {host_xdg_dir}` — shared directory for D-Bus socket

Inside the container, an inline bash entrypoint starts: dbus-daemon (socket in shared dir), KWin `--virtual --xwayland`, AT-SPI, PipeWire, WirePlumber. The host connects to the container's D-Bus via the shared socket. Apps are launched by writing commands to bwrap's stdin.

`session_stop` kills the bwrap process group (negative PID SIGTERM) and removes the host XDG dir.

## MCP Tools

`session_start` spawns an isolated KWin Wayland session (1221x977, XWayland, own D-Bus). All other tools require an active session. `session_stop` kills the process group. Input tools (`mouse_*`, `keyboard_*`) use window-relative coordinates — window position is added internally via `active_window_info()` KWin scripting. Screenshots go through `org.kde.KWin.ScreenShot2` D-Bus interface, returning raw ARGB32 converted to PNG. `accessibility_tree` traverses AT-SPI2 with configurable depth/filters. `find_ui_elements` searches by name/role.

## Key Patterns

- All coordinates are window-relative, converted to absolute internally
- D-Bus screenshot returns raw ARGB32 premultiplied pixels, not PNG — the `screenshot` tool handles conversion
- EIS negotiation is blocking I/O, runs in `tokio::task::spawn_blocking`
- D-Bus socket at `{host_xdg_dir}/bus` — accessible from both host and container via bind-mount
- Container inherits all host config via overlayfs — no manual kwinrc/service configuration
