# Container & HID Redesign Spec

## Problem

The current kwin-mcp architecture manually starts services (D-Bus, KWin, PipeWire, AT-SPI, WirePlumber, KWallet) inside a hakoniwa container via entrypoint.sh. Every new feature hits a wall because the container is too stripped down — missing services, D-Bus interfaces, device configurations that exist on the real system. The Mouse KCM can't find input devices because KWin's `--virtual` backend bypasses libinput entirely, leaving InputDeviceManager empty.

## Solution

Two independent changes:

1. **Container: bwrap + overlayfs on host `/`** — Replace hakoniwa + entrypoint.sh with bubblewrap. Overlay the entire host root so the container inherits all config, services, and state. Only display and HID differ from host. Delete entrypoint.sh entirely.

2. **HID: uinput + libinput bridge** — Create virtual input devices via `/dev/uinput`, bind-mount them into the container, run a standalone libinput context (path mode) that manages them and exposes `org.kde.KWin.InputDeviceManager` on the container's D-Bus. KCMs see real devices with real acceleration curves. KWin stays on `--virtual` for display.

## Requirements

1. Zero shell — no .sh files
2. Isolation — overlayfs, no fs leak to host
3. 1:1 UX fidelity — KCMs see real input devices via InputDeviceManager
4. Agent write boundary — explicit /tmp copy-out only
5. MCP server runs unprivileged — no sudo, group membership (input, video, uinput) is fine
6. HID isolation — host KWin ignores container's uinput devices (proven: no seat tag = not claimed)
7. No user setup beyond group membership

## Proven Facts (tested 2026-04-08)

- uinput devices created inside bwrap ARE kernel-global but host KWin does NOT claim them (no seat tag)
- `libinput debug-events --device /dev/input/eventN` works inside bwrap with bind-mounted device node
- Works with `--unshare-net` — libinput path mode needs no udev, no GROUP_UDEV events
- seatd runs unprivileged inside bwrap
- `systemd --user` refuses inside bwrap (needs sd_booted)
- KWin `--drm` fails (EACCES on amdgpu_query_info — DRM master issue)

## Architecture

```
Host Process (kwin-mcp binary, unprivileged)
│
├── Creates uinput mouse + keyboard via /dev/uinput
│   └── UI_SET_PHYS = "kwin-mcp/virtual-mouse"
│   └── Devices get NO seat tag → host KWin ignores
│
├── Spawns bwrap container:
│   ├── --overlay-src / --tmp-overlay /     (overlayfs on host root)
│   ├── --dev /dev                          (fresh devtmpfs)
│   ├── --dev-bind /dev/input/eventN        (bind uinput device nodes)
│   ├── --dev-bind /dev/dri                 (GPU for rendering)
│   ├── --dev-bind /dev/uinput              (for creating more devices)
│   ├── --unshare-pid --unshare-net --unshare-uts --unshare-ipc
│   ├── --proc /proc
│   └── --tmpfs /tmp
│
├── Inside container:
│   ├── dbus-daemon (session bus)
│   ├── KWin --virtual (display, screenshots, window management)
│   ├── libinput bridge (path mode on bind-mounted devices)
│   │   ├── Opens /dev/input/eventN directly (no udev)
│   │   ├── Applies acceleration curves via libinput API
│   │   └── Exposes org.kde.KWin.InputDeviceManager on D-Bus
│   ├── PipeWire (socket forwarded or started inside)
│   ├── AT-SPI (accessibility)
│   └── Apps launched via container exec
│
├── Host connects to container D-Bus:
│   ├── EIS for pixel-precise input injection (bypass acceleration)
│   ├── ScreenShot2 for screenshots
│   ├── KWin scripting for active window info
│   └── AT-SPI for accessibility tree
│
└── session_stop: kill container, destroy uinput devices
```

## Input Architecture

Two input paths coexist:

1. **EIS (existing)** — pixel-precise, bypasses acceleration. Used by `mouse_move`, `mouse_click`, etc. Unchanged from current.

2. **libinput bridge (new)** — uinput devices visible to InputDeviceManager. KCMs configure acceleration on these devices. When an agent wants accelerated input (testing mouse feel), events flow through this path.

The libinput bridge is a separate process inside the container (or a thread in the MCP server) that:
- Opens the bind-mounted `/dev/input/eventN` via `libinput_path_create_context()`
- Reads device properties (acceleration profiles, speed, scroll method)
- Exposes them on D-Bus as `org.kde.KWin.InputDeviceManager` interface
- When KCM writes a property change, applies it via `libinput_device_config_*()` API

## Container Startup Sequence

1. Create tmpdir for session (`/tmp/kwin-mcp-{pid}`)
2. Create uinput mouse + keyboard devices
3. Record their `/dev/input/eventN` paths
4. Spawn bwrap with overlayfs, bind-mount device nodes
5. Inside container: start dbus-daemon, write bus address to shared file
6. Inside container: start KWin `--virtual`
7. Inside container: start libinput bridge process
8. Inside container: start PipeWire, AT-SPI
9. Host reads bus address, connects D-Bus, EIS
10. Session ready

## What Gets Deleted

- `entrypoint.sh` (83 lines) — all service config/startup
- `hakoniwa` dependency — replaced by bwrap subprocess
- `rewrite_bus_address_for_host()` — bwrap overlay makes paths match
- Manual kwinrc/kscreenlockerrc/kwinrulesrc generation — inherited from host
- Manual dbus.conf generation — dbus-daemon with default config
- `startup_diagnostics()` — replaced by journalctl or direct log reading
- `teardown_container()` / container stdin protocol — replaced by process kill

## What Gets Added

- `evdev` crate — uinput device creation
- bwrap spawn logic (~80 lines replacing ~200 lines of hakoniwa setup)
- libinput bridge binary or module (~200-300 lines)
- Container startup orchestration (Rust, replacing entrypoint.sh)

## What's Unchanged

- EIS input injection (reis crate, all mouse_*/keyboard_* tools)
- Screenshot pipeline (ScreenShot2 D-Bus → ARGB32 → PNG)
- Accessibility tree traversal (atspi crate)
- Active window info (KWin scripting D-Bus)
- MCP protocol layer (rmcp)
- Input parsing (char_key, parse_combo, btn_code, etc.)

## LOC Impact

- Current: 1538 (1455 main.rs + 83 entrypoint.sh)
- After: ~1200-1300 (all Rust, 0 shell)
- Net: -200 to -300 lines
