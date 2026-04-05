# kwin-mcp (Rust)

MCP server for KDE Plasma 6 Wayland GUI automation. Rust implementation using `rmcp` + `reis` + `zbus`.

## Show Accessibility Tree

Requires `jq`.

```bash
./show-tree.sh
```

Defaults to System Monitor. Override with environment variables if needed:

```bash
APP_COMMAND='QT_ACCESSIBILITY=1 QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 konsole' \
APP_NAME='org.kde.konsole' \
MAX_DEPTH=12 \
SHOW_ELEMENTS=true \
./show-tree.sh
```

## org.kde.KWin.ScreenShot2 — Complete Method Table

| Method | D-Bus Signature | What it does |
|---|---|---|
| `CaptureWindow` | `(s handle, a{sv} options, h pipe) → a{sv}` | Specific window by UUID |
| `CaptureActiveWindow` | `(a{sv} options, h pipe) → a{sv}` | Currently focused window |
| `CaptureScreen` | `(s name, a{sv} options, h pipe) → a{sv}` | Monitor by name (e.g. "HDMI-1") |
| `CaptureActiveScreen` | `(a{sv} options, h pipe) → a{sv}` | Current monitor |
| `CaptureArea` | `(i x, i y, u w, u h, a{sv} options, h pipe) → a{sv}` | Rectangle region |
| `CaptureInteractive` | `(u kind, a{sv} options, h pipe) → a{sv}` | User picks (0=window, 1=screen) |
| `CaptureWorkspace` | `(a{sv} options, h pipe) → a{sv}` | All screens composite (v3+) |

Pipe receives raw ARGB32 premultiplied pixels — not PNG. Return dict has `width`, `height`, `stride`, `format`, `scale`.

**Window handle:** UUID string like `"{a1b2c3d4-e5f6-...}"`

**Options dict keys:** `include-cursor`, `include-decoration`, `include-shadow`, `native-resolution`
