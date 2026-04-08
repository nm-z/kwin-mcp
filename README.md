# kwin-mcp

MCP server for KDE Plasma 6 Wayland GUI automation. Single-binary Rust using `rmcp` + `reis` (EIS input) + `atspi` (accessibility tree) + `zbus` (D-Bus/KWin IPC) + `hakoniwa` (container isolation).

## Tools

| Tool | Description |
|---|---|
| `session_start` | Start an isolated KDE Wayland session. Must be called first. |
| `session_stop` | Tear down the session and all container processes. |
| `screenshot` | Capture the active window as PNG. |
| `accessibility_tree` | Traverse the AT-SPI2 accessibility tree with configurable depth/filters. |
| `find_ui_elements` | Find UI elements by role/name (stubbed). |
| `mouse_click` | Click at window-relative coordinates. |
| `mouse_move` | Move pointer to window-relative coordinates. |
| `mouse_scroll` | Scroll at window-relative coordinates. |
| `mouse_drag` | Drag from one window-relative position to another. |
| `keyboard_type` | Type a string of text. |
| `keyboard_key` | Press a key or key combo (e.g. `ctrl+c`, `Return`). |
| `launch_app` | Launch an application and wait for its window. |

## Session Architecture

```
kwin-mcp (host process)
  └── hakoniwa container
        ├── dbus-daemon        (isolated session bus, anonymous auth)
        ├── kwin_wayland       (virtual display 1221x977, XWayland)
        ├── pipewire
        ├── wireplumber
        ├── at-spi-bus-launcher
        └── kwalletd6
```

`session_start` spawns the hakoniwa container via `entrypoint.sh`. All input and D-Bus calls from the host go into the container over the shared `XDG_RUNTIME_DIR`. `session_stop` kills the container process group.

All coordinates are window-relative — window position is added internally via `kdotool`.

## Build

```bash
cargo build          # debug
cargo build --release
cargo clippy         # strict: unwrap/expect/todo/dead_code all denied
```

## org.kde.KWin.ScreenShot2 — Method Reference

| Method | D-Bus Signature | Description |
|---|---|---|
| `CaptureWindow` | `(s handle, a{sv} options, h pipe) → a{sv}` | Specific window by UUID |
| `CaptureActiveWindow` | `(a{sv} options, h pipe) → a{sv}` | Currently focused window |
| `CaptureScreen` | `(s name, a{sv} options, h pipe) → a{sv}` | Monitor by name |
| `CaptureActiveScreen` | `(a{sv} options, h pipe) → a{sv}` | Current monitor |
| `CaptureArea` | `(i x, i y, u w, u h, a{sv} options, h pipe) → a{sv}` | Rectangle region |
| `CaptureInteractive` | `(u kind, a{sv} options, h pipe) → a{sv}` | User picks (0=window, 1=screen) |
| `CaptureWorkspace` | `(a{sv} options, h pipe) → a{sv}` | All screens composite (v3+) |

Pipe receives raw ARGB32 premultiplied pixels. Return dict has `width`, `height`, `stride`, `format`, `scale`.

**Options dict keys:** `include-cursor`, `include-decoration`, `include-shadow`, `native-resolution`

## Screenshot dimensions

Anthropic resizes images with a long edge > 1568px before the model sees them. The virtual display (1221x977) is sized to fit within this limit with no resize needed.

| Aspect ratio | Max size (no resize) |
|---|---|
| 1:1 | 1092×1092 |
| 5:4 | 1221×977 |
| 4:3 | 1268×951 |
| 2:3 | 896×1344 |
| 1:2 | 784×1568 |

Token cost: ~1 token per 750 pixels. A 1221×977 screenshot costs ~1590 tokens.
