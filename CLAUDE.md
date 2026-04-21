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
- **KWin D-Bus proxies**: `KWinEis`, `KWinScreenShot2` — zbus proxy traits for EIS input and screenshots. `KWinScreenShot2` exposes both `CaptureWindow` (per-toplevel framebuffer — misses popup surfaces) and `CaptureScreen` (composited output — includes xdg_popup menus). The `screenshot` tool uses `CaptureScreen("Virtual-0")` so Chrome/Qt popup menus appear in captures; `CaptureWindow` silently drops them because popups are separate Wayland surfaces from the toplevel.
- **EIS input**: `Eis` struct holds EIS context, pointer/button/scroll/keyboard devices. `serial` is `AtomicU32` advancing per-frame via `next_serial()`; `start: Instant` feeds `now_us()` for CLOCK_MONOTONIC μs timestamps — libei requires both (reis-0.6.1/src/eiproto_ei.rs:1408). Before the fix KWin silently dropped keyboard frames that used `(frozen_handshake_serial, 0)`. Methods: `from_fd()`, `move_abs()`, `button()`, `scroll_discrete()`, `scroll_smooth()`, `key()`. Negotiation is blocking (`tokio::task::spawn_blocking`).
- **CDP integration**: `chromiumoxide::Browser` stored as `Option<Arc<Browser>>` in Session. `launch_app` always injects `--remote-debugging-port=<free_port>` and tries CDP connection (2s timeout) — Chromium apps respond, native apps ignore the flag. It also auto-injects `--ozone-platform=wayland` for any chrome/chromium/edge/brave/vivaldi/electron/code command that doesn't already specify an ozone platform. Without this flag, Chromium picks XWayland and its popup menus (xdg_popup) never composite into KWin's output — Alt+F appears to do nothing visible even though the accelerator fires. `find_ui_elements` dispatches via `match`: CDP path queries DOM for interactive elements, AT-SPI path traverses the accessibility tree.
- **KWallet emulator**: `KWalletEmulator` implements `org.kde.KWallet` on the container session bus at `/modules/kwalletd6`. `dump_host_wallet()` at `session_start` reads every folder/entry from the host's unlocked kwallet via the user's host session bus; the emulator serves that snapshot. Emits `walletOpened` / `walletAsyncOpened` signals so chromium's async open path resolves — sync method returns alone aren't enough. Container's real `kwalletd6` is prevented from auto-activating by bind-mounting `/dev/null` over its `.service` file.
- **NetworkManager proxy**: `xdg-dbus-proxy` is spawned on the host before bwrap, filtering the host system bus with `--talk=org.freedesktop.NetworkManager`. The filtered socket is bind-mounted into the container as `/run/dbus/system_bus_socket` so chromium's `NetworkChangeNotifier` reaches NM without broad system-bus access.
- **`Session` struct**: Owns the bwrap child process, bwrap stdin (for launch_app), D-Bus connections (kwin_conn, _proxy_conn, _wallet_conn — the wallet_conn is held so the claimed well-known name outlives session_start), EIS handle, host XDG dir, optional CDP browser handle, xdg-dbus-proxy child. Created by `session_start`, destroyed by `session_stop`.
- **`KwinMcp` struct**: `Arc<Mutex<Option<Session>>>` — the MCP server. Implements `ToolHandler` with 12 tools. `with_session()` gates all tools behind session existence.

## Container Architecture

`session_start` spawns an isolated session via **bubblewrap (bwrap)**. It is **idempotent**: if a session is already running it returns `status=already_running` with the existing bus/workdir, no teardown. To restart, call `session_stop` first. bwrap args:

- `--overlay-src $HOME --tmp-overlay $HOME` — overlayfs on user's home (default writable=false); `--bind / /` when `writable=true`
- `--die-with-parent` — auto-kills container if MCP server dies
- `--unshare-pid --unshare-uts --unshare-ipc` — namespace isolation (network is shared)
- `--dev-bind /dev/dri` and `--dev-bind /dev/uinput` — GPU and input device access
- `--tmpfs /tmp` and `--tmpfs /run` — **applied unconditionally, AFTER the writable bind**. bwrap arg order = mount order, so these tmpfs mounts stack on top of the host root. Result: container `/tmp` and `/run` are always isolated RAM-only filesystems, invisible from the host, even with `writable=true`. This is intentional (socket/lockfile collision prevention for X11, PipeWire, Qt, Chromium). Writable mode only gives host-real access to everything *outside* `/tmp` and `/run` — i.e., `$HOME`, `/etc`, `/opt`, etc.
- `--bind {host_xdg_dir}` — shared directory for D-Bus socket (bind-mounted *into* the tmpfs-hidden `/tmp/kwin-mcp-<pid>/` path)
- `--ro-bind-try {host_xdg_dir}/system_bus_socket /run/dbus/system_bus_socket` — NetworkManager-only proxy socket from host
- `--ro-bind /dev/null` over `org.kde.kwalletd6.service`, `org.kde.secretservicecompat.service`, `org.freedesktop.impl.portal.desktop.kwallet.service`, `org.kde.secretprompter.service` — masks these dbus service files so the container's dbus-daemon cannot auto-activate the real kwalletd6/ksecretd; our emulator owns the wallet name exclusively

Inside the container, an inline bash entrypoint starts: dbus-daemon (socket in shared dir), KWin `--virtual --xwayland --width {VIRTUAL_SCREEN_WIDTH} --height {VIRTUAL_SCREEN_HEIGHT}`, AT-SPI, PipeWire, WirePlumber. The host connects to the container's D-Bus via the shared socket. Apps are launched by writing commands to bwrap's stdin.

`session_stop` kills the bwrap process group (negative PID SIGTERM), kills the xdg-dbus-proxy child, and calls `cleanup_stale_session_files` on the host XDG dir — removes sockets (`bus`, `wayland-0*`, `pipewire-0*`, `system_bus_socket`), ready-files (`dbus-ready`, `bridge-ready`), stale entrypoint-created dirs (`at-spi`, `dbus-1`, `dconf`, `doc`), any `script_*.js` leftovers, and `screenshot.png`. The dir itself and its `tmp/` subdirectory are preserved. The `tmp/` subdir is the agent's persistent scratch — host and container share it bidirectionally (live, same inode via the existing `--bind {host_xdg_dir} {host_xdg_dir}`). Scratch survives `session_stop` and crashes; past session dirs accumulate until host reboot. `session_start` runs the same cleanup on entry (idempotent), then `mkdir_p`'s the `tmp/` subdir if missing.

## Top-of-file constants

All tunables live in a single grouped block at the top of `main.rs` (lines ~33-95). Edit these, not inline values:

- **Kernel/protocol**: `LINUX_KEY_LEFTSHIFT` (evdev 42), `EIS_CAPS_KBD_POINTER` (0b011 = keyboard+pointer bitfield for KWin EIS)
- **Timings**: `STARTUP_TIMEOUT/POLL`, `EIS_NEGOTIATION_TIMEOUT/POLL`, `DBUS_PROXY_TIMEOUT/POLL`, `KWIN_NAME_PROBE_TIMEOUT`, `ATSPI_TRAVERSAL_TIMEOUT`, `INPUT_EVENT_DELAY` (used for click/drag/key pacing), `DRAG_STEPS` (mouse_drag interpolation count), `SCROLL_SMOOTH_PIXELS_PER_TICK`, `LAUNCH_POLL_INTERVAL`, `LAUNCH_WINDOW_POLLS`, `CDP_CONNECT_POLLS`
- **Virtual screen**: `VIRTUAL_SCREEN_WIDTH`/`VIRTUAL_SCREEN_HEIGHT` — threaded through the bash entrypoint's `kwin_wayland --width/--height` AND the server's `with_instructions` description string
- **Display/fonts**: `KDE_SCALE_FACTOR`, `KDE_FORCE_FONT_DPI`, `KDE_HINT_STYLE`, `KDE_SUB_PIXEL`, `UI_FONT_FAMILY`/`UI_FONT_SIZE`/`UI_FONT_SIZE_SMALL`, `FIXED_FONT_FAMILY`/`FIXED_FONT_SIZE`, `FONT_WEIGHT_REGULAR`/`FONT_WEIGHT_BOLD`. Inline comments list valid tokens. The `qt_font_spec()` helper encodes the Qt KConfig font-string format (`family,size,-1,5,weight,0,0,0,0,0,0,0,0,0,0,1[,Bold]`) — every kdeglobals replacement goes through it.

Server name/version uses `env!("CARGO_PKG_NAME"/"CARGO_PKG_VERSION")` so MCP `get_info()` tracks `Cargo.toml` automatically.

## MCP Tools

`session_start` spawns an isolated KWin Wayland session (`VIRTUAL_SCREEN_WIDTH`x`VIRTUAL_SCREEN_HEIGHT`, XWayland, own D-Bus) and is idempotent. All other tools require an active session. `session_stop` kills the process group. Input tools (`mouse_*`, `keyboard_*`) use window-relative coordinates — window position is added internally via `active_window_info()` KWin scripting. Screenshots go through `org.kde.KWin.ScreenShot2` D-Bus interface, returning raw ARGB32 converted to PNG. `accessibility_tree` traverses AT-SPI2 with configurable depth/filters. `find_ui_elements` searches by name/role — automatically uses CDP DOM queries for Chromium/Electron apps, AT-SPI for native apps. `launch_app` auto-detects Chromium apps by injecting `--remote-debugging-port` and attempting CDP connection. Chromium/Chrome in the container transparently read the host's kwallet data (cookies decrypt, saved passwords available) via the emulator — no prompts.

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
- Wayland popups (xdg_popup) are separate surfaces from their parent toplevel. `org.kde.KWin.ScreenShot2.CaptureWindow` captures only the toplevel framebuffer, so dropdown menus, context menus, and tooltips silently disappear. Use `CaptureScreen("Virtual-0")` to composite the whole output.
- Chromium/Chrome must launch with `--ozone-platform=wayland` in this container. On XWayland the browser still runs, but its popup menus never composite (Alt+F and right-click dropdowns appear to do nothing). `launch_app` auto-appends the flag for chrome/chromium/edge/brave/vivaldi/electron commands that don't already set `--ozone-platform`.
