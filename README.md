# kwin-mcp

MCP server for KWin Wayland GUI automation. Single-binary Rust using `rmcp` + `reis` (EIS input) + `atspi` (accessibility tree) + `zbus` (D-Bus/KWin IPC) + `evdev` (uinput virtual devices). Container isolation via bubblewrap.

## Tools

| Tool | Description |
|---|---|
| `session_start` | Start an isolated KDE Wayland session. Must be called first. |
| `session_stop` | Tear down the session and all container processes. |
| `screenshot` | Capture the active window as PNG. |
| `window_list` | List all isolated-session windows, including hidden prompts and modal relationships. |
| `window_activate` | Reveal and focus a window by the ID returned from `window_list`. |
| `accessibility_tree` | Traverse the AT-SPI2 accessibility tree with configurable depth/filters. |
| `find_ui_elements` | Search UI elements by name/role with bounding boxes. |
| `mouse_click` | Click at window-relative coordinates. |
| `mouse_move` | Move pointer to window-relative coordinates. |
| `mouse_scroll` | Scroll at window-relative coordinates. |
| `mouse_drag` | Drag from one window-relative position to another. |
| `keyboard_type` | Type a string of text. |
| `keyboard_key` | Press a key or key combo (e.g. `ctrl+c`, `Return`). |
| `launch_app` | Launch an application and wait for its window. |

Pass `--no-viewer` when starting `kwin-mcp` to suppress only the host preview window. The isolated session and all MCP tools remain available; without the flag, the viewer still opens normally.

## Session Architecture

```
kwin-mcp (host process)
  ├── proxy_conn (owns org.kde.KWin on container D-Bus)
  │     └── InputDeviceManager + InputDevice objects
  │         (KCMs see virtual mouse/keyboard here)
  ├── kwin_conn (talks to KWin via unique name)
  │     └── EIS, ScreenShot2, Scripting
  └── bwrap container (bubblewrap, overlayfs on $HOME)
        ├── dbus-daemon        (isolated session bus, anonymous auth)
        ├── kwin_wayland       (virtual display 1000x1000, XWayland)
        ├── pipewire + wireplumber
        ├── at-spi-bus-launcher
        └── uinput devices     (virtual mouse + keyboard, bind-mounted)
```

### Two-phase D-Bus startup

1. bwrap starts, dbus-daemon creates session bus
2. Host `proxy_conn` claims `org.kde.KWin`, registers InputDevice objects
3. Container starts KWin (gets unique name `:1.N`, not the well-known name)
4. Host discovers KWin's unique name by probing for EIS interface
5. Host `kwin_conn` connects to KWin via unique name for EIS/screenshots/scripting

This lets KCMs (like Mouse settings) see our virtual devices under `org.kde.KWin`, while the MCP server talks to the real KWin compositor via its unique bus name.

### HID isolation

Virtual input devices are created via `/dev/uinput` (requires `input` group). They are kernel-global but the host's KWin does not claim them (no seat tag assigned by udev). The devices are bind-mounted into the container and destroyed on session_stop.

All coordinates are window-relative — window position is added internally via KWin scripting.

### Home socket exposure

At `session_start`, every active pathname socket beneath `$HOME` is exposed automatically. This includes sensitive control sockets. Parent directories are mounted read-only, which prevents host file writes but does not restrict operations offered by each socket protocol. Hidden parent mounts also expose sibling files through their internal `/run/kwin-mcp-host-sockets` paths. Socket replacements at discovered names remain live; new socket names require a new session.

## Build

```bash
cargo build          # debug
cargo build --release
cargo clippy         # strict: unwrap/expect/todo/dead_code all denied
```

## Setup

Add your user to these groups:
```
sudo usermod -aG input,uinput,video,render $USER
```

Requires: `bubblewrap` (bwrap) and KWin running as a Wayland compositor.

## Screenshot dimensions

Virtual display is 2000×1875 (3.75MP). All windows open maximized, no decorations, no shadows. Font hinting disabled, grayscale antialiasing, 96 DPI, scale 1.0.

Token cost: ~1 token per 750 pixels. A 2000×1875 screenshot costs ~5000 tokens.
