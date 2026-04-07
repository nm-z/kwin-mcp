# kwin-mcp (Rust)

MCP server for KDE Plasma 6 Wayland GUI automation. Rust implementation using `rmcp` + `ashpd` + `zbus`.

## XDG Desktop Portal

      RemoteDesktop
            org.freedesktop.portal.RemoteDesktop
      ScreenCast
            org.freedesktop.portal.ScreenCast
      Screenshot
            org.freedesktop.portal.Screenshot
      Account
            org.freedesktop.portal.Account
      Background
            org.freedesktop.portal.Background
      Camera
            org.freedesktop.portal.Camera
      Clipboard
            org.freedesktop.portal.Clipboard
      Documents
            org.freedesktop.portal.Documents
      DynamicLauncher
            org.freedesktop.portal.DynamicLauncher
      Email
            org.freedesktop.portal.Email
      FileChooser
            org.freedesktop.portal.FileChooser
      FileTransfer
            org.freedesktop.portal.FileTransfer
      GameMode
            org.freedesktop.portal.GameMode
      GlobalShortcuts
            org.freedesktop.portal.GlobalShortcuts
      Inhibit
            org.freedesktop.portal.Inhibit
      InputCapture
            org.freedesktop.portal.InputCapture
      Location
            org.freedesktop.portal.Location
      MemoryMonitor
            org.freedesktop.portal.MemoryMonitor
      NetworkMonitor
            org.freedesktop.portal.NetworkMonitor
      Notification
            org.freedesktop.portal.Notification
      OpenURI
            org.freedesktop.portal.OpenURI
      PowerProfileMonitor
            org.freedesktop.portal.PowerProfileMonitor
      Print
            org.freedesktop.portal.Print
      ProxyResolver
            org.freedesktop.portal.ProxyResolver
      Realtime
            org.freedesktop.portal.Realtime
      Secret
            org.freedesktop.portal.Secret
      Settings
            org.freedesktop.portal.Settings
      Trash
            org.freedesktop.portal.Trash
      Usb
            org.freedesktop.portal.Usb
      Wallpaper
            org.freedesktop.portal.Wallpaper

## Session Architecture

```
systemd --user
      dbus (session bus — user desktop)
            kwin-mcp ← asks systemd to start isolated session
      dbus (session bus — isolated)
            dbus-broker
            plasma-kwin_wayland
            pipewire
            wireplumber
            xdg-desktop-portal
            plasma-xdg-desktop-portal-kde
            at-spi-dbus-bus
```

kwin-mcp is 1 process, 0 children. systemd manages all isolated session services.
session_start creates the isolated bus via zbus_systemd StartTransientUnit.
session_stop tears it down via StopUnit.

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
