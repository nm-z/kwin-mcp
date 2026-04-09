# HID libinput Bridge Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Create virtual input devices via uinput, manage them with libinput in path mode inside the container, and expose `org.kde.KWin.InputDeviceManager` on D-Bus so KCMs see real devices with real acceleration curves.

**Architecture:** The MCP server creates uinput mouse/keyboard on the host via `/dev/uinput` (input group), records their `/dev/input/eventN` paths, bind-mounts them into the bwrap container. A libinput bridge process inside the container opens them via `libinput_path_create_context()`, reads properties, exposes them on D-Bus. Host KWin ignores the devices (no seat tag — verified empirically).

**Tech Stack:** `evdev` crate (uinput device creation), `input` crate or raw libinput FFI (path mode context), zbus (D-Bus InputDeviceManager interface), bwrap (container from Task 1).

**Depends on:** `2026-04-08-bwrap-container-migration.md` (container must be bwrap-based first)

---

### File Structure

- **Modify:** `Cargo.toml` — add `evdev` crate
- **Modify:** `src/main.rs` — add uinput device creation to `session_start`, add device paths to bwrap bind-mounts, destroy devices on `session_stop`
- **Create:** `src/libinput_bridge.rs` — standalone binary or module that opens devices via libinput path mode and exposes InputDeviceManager on D-Bus (this is a significant piece — scoping TBD based on KWin's actual InputDeviceManager D-Bus interface)

---

### Task 1: Add uinput device creation to session_start

**Files:**
- Modify: `Cargo.toml` — add `evdev` dep
- Modify: `src/main.rs` — Session struct, session_start, teardown

- [ ] **Step 1: Add evdev dependency**

```toml
evdev = { version = "0.13", features = ["uinput"] }
```

Note: verify the latest version and that `uinput` feature exists. The `evdev` crate's `UinputAbsSetup` and `VirtualDeviceBuilder` are in the `uinput` module.

- [ ] **Step 2: Add uinput device creation function**

Add to `src/main.rs`:

```rust
struct UinputDevices {
    mouse: evdev::uinput::VirtualDevice,
    keyboard: evdev::uinput::VirtualDevice,
    mouse_event_path: std::path::PathBuf,
    keyboard_event_path: std::path::PathBuf,
}

fn create_uinput_devices() -> anyhow::Result<UinputDevices> {
    use evdev::uinput::VirtualDeviceBuilder;
    use evdev::{AttributeSet, Key, RelativeAxisType};

    // Mouse
    let mut keys = AttributeSet::<Key>::new();
    keys.insert(Key::BTN_LEFT);
    keys.insert(Key::BTN_RIGHT);
    keys.insert(Key::BTN_MIDDLE);

    let mut rel = AttributeSet::<RelativeAxisType>::new();
    rel.insert(RelativeAxisType::REL_X);
    rel.insert(RelativeAxisType::REL_Y);
    rel.insert(RelativeAxisType::REL_WHEEL);
    rel.insert(RelativeAxisType::REL_HWHEEL);

    let mouse = VirtualDeviceBuilder::new()
        .map_err(|e| anyhow::anyhow!("uinput mouse builder: {e}"))?
        .name("kwin-mcp-virtual-mouse")
        .with_phys("kwin-mcp/mouse")?
        .with_keys(&keys)?
        .with_relative_axes(&rel)?
        .build()
        .map_err(|e| anyhow::anyhow!("uinput mouse create: {e}"))?;

    let mouse_syspath = mouse.enumerate().next()
        .ok_or_else(|| anyhow::anyhow!("no sysfs path for uinput mouse"))?;
    let mouse_event_path = find_event_path(&mouse_syspath)?;

    // Keyboard
    let mut kb_keys = AttributeSet::<Key>::new();
    for code in 1..=248 {
        if let Some(key) = Key::new(code) {
            kb_keys.insert(key);
        }
    }

    let keyboard = VirtualDeviceBuilder::new()
        .map_err(|e| anyhow::anyhow!("uinput keyboard builder: {e}"))?
        .name("kwin-mcp-virtual-keyboard")
        .with_phys("kwin-mcp/keyboard")?
        .with_keys(&kb_keys)?
        .build()
        .map_err(|e| anyhow::anyhow!("uinput keyboard create: {e}"))?;

    let kb_syspath = keyboard.enumerate().next()
        .ok_or_else(|| anyhow::anyhow!("no sysfs path for uinput keyboard"))?;
    let keyboard_event_path = find_event_path(&kb_syspath)?;

    Ok(UinputDevices {
        mouse,
        keyboard,
        mouse_event_path,
        keyboard_event_path,
    })
}

fn find_event_path(syspath: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    for entry in std::fs::read_dir(syspath)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("event") {
            return Ok(std::path::PathBuf::from(format!("/dev/input/{name_str}")));
        }
    }
    anyhow::bail!("no event device under {}", syspath.display())
}
```

Note: the evdev crate API may differ from what's shown. The implementer MUST check docs.rs/evdev for the actual API. `VirtualDeviceBuilder` and `with_phys` may have different signatures. The key: create a mouse with BTN_LEFT/RIGHT/MIDDLE + REL_X/Y/WHEEL, a keyboard with all key codes, and find their `/dev/input/eventN` paths.

- [ ] **Step 3: Create devices before bwrap spawn in session_start**

After the orphan check and before bwrap spawn:

```rust
eprintln!("session_start: creating uinput devices");
let uinput = create_uinput_devices().map_err(|e| ver_err(e.to_string()))?;
eprintln!(
    "session_start: uinput mouse={} keyboard={}",
    uinput.mouse_event_path.display(),
    uinput.keyboard_event_path.display(),
);
```

- [ ] **Step 4: Add device paths to bwrap args**

In the bwrap Command builder, add bind-mounts for the specific event devices:

```rust
cmd.args([
    "--dev-bind",
    &uinput.mouse_event_path.to_string_lossy(),
    &uinput.mouse_event_path.to_string_lossy(),
]);
cmd.args([
    "--dev-bind",
    &uinput.keyboard_event_path.to_string_lossy(),
    &uinput.keyboard_event_path.to_string_lossy(),
]);
```

- [ ] **Step 5: Store uinput devices in Session for cleanup**

```rust
struct Session {
    zbus_conn: zbus::Connection,
    eis: Eis,
    bwrap_child: std::process::Child,
    bwrap_stdin: std::process::ChildStdin,
    host_xdg_dir: std::path::PathBuf,
    uinput: UinputDevices,
}
```

The `UinputDevices` struct holds the `VirtualDevice` handles. When dropped, the evdev crate destroys the uinput devices automatically (RAII). So `teardown` dropping the Session destroys the devices.

- [ ] **Step 6: Verify host KWin ignores the devices**

After creating devices but before container spawn, verify:

```rust
// Sanity check: the devices should have no seat tag
eprintln!(
    "session_start: uinput devices created — host KWin should ignore (no seat tag)"
);
```

The actual verification was done empirically. No runtime check needed — it's a kernel behavior.

- [ ] **Step 7: Build and clippy**

```bash
cargo build 2>&1
cargo clippy 2>&1
```

- [ ] **Step 8: Smoke test**

Start session, verify:
- Two new devices appear in host `ls /dev/input/event*`
- `libinput list-devices` on host shows `kwin-mcp-virtual-mouse` and `kwin-mcp-virtual-keyboard`
- Host KWin's `qdbus6 org.kde.KWin /org/kde/KWin/InputDevice` does NOT list them
- After session_stop, devices disappear from `/dev/input/`

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -m "feat: create uinput virtual mouse+keyboard on session_start"
```

---

### Task 2: Verify libinput path mode inside container

**Files:** None — this is a verification task using the devices from Task 1.

- [ ] **Step 1: Manual test**

With a session running (uinput devices created, bwrap container up):

```bash
# From host, exec into container namespace and test libinput
# Find bwrap PID, use nsenter
BWRAP_PID=$(pgrep -f "bwrap.*overlay-src" | head -1)
nsenter -t $BWRAP_PID -m -p -- \
    libinput debug-events --device /dev/input/eventN 2>&1 | head -5
```

Expected: `DEVICE_ADDED kwin-mcp-virtual-mouse`

If nsenter doesn't work, add a test command to the bwrap entrypoint that runs libinput and logs output.

- [ ] **Step 2: Document results**

If it works: proceed to Task 3.
If it doesn't: debug — check device node permissions inside container, check libinput library availability.

---

### Task 3: Design and implement the InputDeviceManager D-Bus interface

This is the most complex task. It requires implementing the D-Bus interface that KDE's Mouse KCM expects. Before writing code, the implementer MUST:

1. Introspect the real `org.kde.KWin.InputDeviceManager` interface on the host:
   ```bash
   qdbus6 org.kde.KWin /org/kde/KWin/InputDevice org.freedesktop.DBus.Introspectable.Introspect
   ```
2. Introspect a specific device:
   ```bash
   qdbus6 org.kde.KWin /org/kde/KWin/InputDevice/event0 org.freedesktop.DBus.Introspectable.Introspect
   ```
3. Identify which properties the Mouse KCM reads/writes (pointerAcceleration, pointerAccelProfile, naturalScroll, etc.)

**Files:**
- Create: `src/input_bridge.rs` — D-Bus service implementing InputDeviceManager

This task is intentionally left at design level because the D-Bus interface must be discovered from the live system. The implementer should:

- [ ] **Step 1: Introspect host KWin InputDeviceManager**

Run the introspection commands above. Record the full XML interface definition.

- [ ] **Step 2: Identify KCM-critical properties**

Read the Mouse KCM source (likely at `/usr/share/kpackage/kcms/kcm_mouse/` or in KDE git) to determine which properties it reads and writes.

- [ ] **Step 3: Implement the D-Bus interface using zbus**

Create `src/input_bridge.rs` with a `#[zbus::interface]` implementation that:
- Exposes the same path structure (`/org/kde/KWin/InputDevice/{eventN}`)
- Exposes the same property names and types
- Backs properties with the actual libinput device state (via `input` crate or libinput FFI)
- Handles property writes by applying them to the libinput device

- [ ] **Step 4: Register the service on the container's D-Bus**

The bridge process connects to the container's session bus and registers at the well-known name and paths that the KCM expects.

- [ ] **Step 5: Test with the actual Mouse KCM**

```bash
# Inside container, launch System Settings
systemsettings kcm_mouse
```

Verify: device list shows `kwin-mcp-virtual-mouse`, acceleration curve editor is available.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: InputDeviceManager D-Bus bridge backed by libinput"
```

---

### Task 4: Wire bridge into session lifecycle

**Files:**
- Modify: `src/main.rs` — start bridge as part of session_start, stop on session_stop

- [ ] **Step 1: Start bridge in container entrypoint**

Add to the bwrap entrypoint command the bridge process startup (if it's a separate binary) or start it as a thread connected to the container's D-Bus.

- [ ] **Step 2: Verify full chain**

1. `session_start` → creates uinput devices → spawns bwrap → bridge starts inside
2. Agent calls `launch_app "systemsettings kcm_mouse"`
3. Take screenshot — Mouse KCM shows device list with `kwin-mcp-virtual-mouse`
4. Agent interacts with KCM controls
5. `session_stop` → everything cleaned up

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "feat: wire InputDeviceManager bridge into session lifecycle"
```
