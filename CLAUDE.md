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

Everything lives in `src/main.rs` (~634 lines). Key layers:

- **Input parsing** (top of file): `parse_int`, `parse_combo`, `char_key`, `modifier_code`, `special_key`, `btn_code` — maps key names and characters to evdev keycodes. `parse_int` handles Claude Code's number-as-string serialization quirk.
- **EIS negotiation**: `negotiate_eis`, `drain_eis_events`, `register_eis_device` — sets up pointer/keyboard devices with the EIS protocol via `reis`.
- **`Eis` struct**: Holds EIS context, pointer/button/scroll/keyboard devices, D-Bus connection. Methods: `connect()`, `move_abs()`, `button()`, `scroll_do()`, `key()`.
- **`Session` struct**: Owns the KWin child process, AT-SPI registry, D-Bus/ATSPI addresses, screenshot dir, EIS handle. Created by `session_start`, destroyed by `session_stop`.
- **`KwinMcp` struct**: `Arc<Mutex<Option<Session>>>` — the MCP server. Implements `ToolHandler` with 12 tools. `with_session()` gates all tools behind session existence.

## MCP Tools

`session_start` spawns an isolated KWin Wayland session (1920x1080, XWayland, own D-Bus). All other tools require an active session. `session_stop` kills the process group. Input tools (`mouse_*`, `keyboard_*`) use window-relative coordinates — window position is added internally via `kdotool::get_active_window_info()`. Screenshots go through `org.kde.KWin.ScreenShot2` D-Bus interface, returning raw ARGB32 converted to PNG. `accessibility_tree` traverses AT-SPI2 with configurable depth/filters.

## Key Patterns

- All coordinates are window-relative, converted to absolute internally
- D-Bus screenshot returns raw ARGB32 premultiplied pixels, not PNG — the `screenshot` tool handles conversion
- `session_start` runs in `tokio::task::spawn_blocking` because EIS negotiation is blocking I/O
- SIGCHLD is ignored at startup to prevent zombie accumulation
- `find_ui_elements` is stubbed out — not yet implemented
