# bwrap Container Migration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace hakoniwa + entrypoint.sh with bubblewrap (bwrap) using overlayfs on host root, inheriting all host config and services instead of rebuilding from scratch.

**Architecture:** Spawn `bwrap` as a subprocess with `--overlay-src / --tmp-overlay /` for ephemeral writes, bind-mount GPU and specific device nodes, run dbus-daemon + KWin + services inside via a Rust-built argument list (no shell). Host connects to container's D-Bus for EIS, screenshots, and a11y. Container teardown kills the bwrap process group.

**Tech Stack:** bwrap 0.10+ (system binary), `std::process::Command` for subprocess, zbus for D-Bus, reis for EIS, existing rmcp/atspi/png crates unchanged.

**Spec:** `docs/superpowers/specs/2026-04-08-container-and-hid-redesign.md`

---

### File Structure

- **Modify:** `src/main.rs` — replace hakoniwa container setup in `session_start`, replace `Session` struct fields, replace `teardown`/`teardown_container`, delete `rewrite_bus_address_for_host`, delete `startup_diagnostics`
- **Modify:** `Cargo.toml` — remove `hakoniwa` dep
- **Delete:** `entrypoint.sh`
- **Modify:** `CLAUDE.md` — update architecture docs

---

### Task 1: Remove hakoniwa, add bwrap spawn helper

**Files:**
- Modify: `Cargo.toml` — remove hakoniwa
- Modify: `src/main.rs:428-435` — change Session struct
- Modify: `src/main.rs:497-525` — replace teardown functions
- Modify: `src/main.rs:846-1006` — replace session_start container setup

- [ ] **Step 1: Update Cargo.toml**

Remove the `hakoniwa` dependency line:

```toml
# DELETE this line:
hakoniwa = "1.4"
```

- [ ] **Step 2: Replace Session struct**

Replace the current `Session` struct at `src/main.rs:428`:

```rust
struct Session {
    zbus_conn: zbus::Connection,
    eis: Eis,
    bwrap_child: std::process::Child,
    host_xdg_dir: std::path::PathBuf,
}
```

- [ ] **Step 3: Replace teardown functions**

Delete `teardown_container()` (lines 497-516) and `teardown()` (lines 518-525). Replace with:

```rust
fn teardown(mut sess: Session) {
    // Kill bwrap process group — bwrap is the group leader,
    // this kills all children (dbus, kwin, etc.)
    let pid = sess.bwrap_child.id();
    if let Ok(pid_i32) = i32::try_from(pid) {
        // Negative PID = kill process group
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-pid_i32),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
    let _ = sess.bwrap_child.wait();
    if let Err(e) = std::fs::remove_dir_all(&sess.host_xdg_dir) {
        eprintln!("teardown cleanup: {e}");
    }
}
```

- [ ] **Step 4: Replace session_start container setup**

Delete everything from the hakoniwa container build (line ~846 `let mut container = hakoniwa::Container::new()`) through EIS connection (line ~984). Replace with bwrap spawn. The new code goes after `host_xdg_dir` creation and orphan check:

```rust
// Build bwrap command
let bus_socket = host_xdg_dir.join("bus");
let kwin_socket_name = format!("wayland-mcp-{pid}");
let entrypoint = format!(
    concat!(
        "mkdir -p /tmp/xdg && ",
        "export XDG_RUNTIME_DIR=/tmp/xdg && ",
        "export WAYLAND_DISPLAY={kwin_sock} && ",
        "export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 && ",
        "dbus-daemon --session --address=unix:path=/tmp/xdg/bus ",
        "--print-address --nofork &>/dev/null & ",
        "sleep 0.3 && ",
        "cp /tmp/xdg/bus {bus_path} && ",
        "KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 ",
        "KWIN_WAYLAND_NO_PERMISSION_CHECKS=1 ",
        "kwin_wayland --virtual --no-lockscreen ",
        "--width 1221 --height 977 ",
        "--socket {kwin_sock} &>/tmp/xdg/kwin.log & ",
        "sleep 0.5 && ",
        "/usr/lib/at-spi-bus-launcher --launch-immediately &>/dev/null & ",
        "pipewire &>/dev/null & ",
        "wireplumber &>/dev/null & ",
        "cat"  // block until stdin closes
    ),
    kwin_sock = kwin_socket_name,
    bus_path = host_xdg_dir.join("bus-address").display(),
);

let mut cmd = std::process::Command::new("bwrap");
cmd.args([
    "--unshare-pid",
    "--unshare-uts",
    "--unshare-ipc",
    "--overlay-src", "/",
    "--tmp-overlay", "/",
    "--dev", "/dev",
    "--dev-bind", "/dev/dri", "/dev/dri",
    "--dev-bind", "/dev/uinput", "/dev/uinput",
    "--proc", "/proc",
    "--bind", &host_xdg_dir.to_string_lossy(), &host_xdg_dir.to_string_lossy(),
    "--",
    "bash", "-c", &entrypoint,
]);
cmd.stdin(std::process::Stdio::piped());
cmd.stdout(std::process::Stdio::null());
cmd.stderr(std::process::Stdio::inherit());

eprintln!("session_start: spawning bwrap container");
let bwrap_child = cmd.spawn().map_err(|e| ver_err(format!("bwrap spawn: {e}")))?;
eprintln!("session_start: bwrap pid={}", bwrap_child.id());
```

Note: This still uses a `bash -c` inline string for the entrypoint. This is NOT a .sh file — it's a Rust-built command string passed to bwrap. A future task can replace this with a Rust helper binary if needed.

- [ ] **Step 5: Update D-Bus connection to use bwrap's bus**

Replace the bus address reading. The container writes the address to `host_xdg_dir/bus-address` which is bind-mounted into the container. The host reads it directly — no `rewrite_bus_address_for_host` needed because the overlay makes paths match:

```rust
let bus_address_path = host_xdg_dir.join("bus-address");
eprintln!("session_start: waiting for D-Bus address at {}", bus_address_path.display());
let bus_addr = match wait_for_nonempty_file(
    &bus_address_path,
    "D-Bus address",
    std::time::Instant::now() + STARTUP_TIMEOUT,
).await {
    Ok(addr) => addr,
    Err(e) => {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-i32::try_from(bwrap_child.id()).unwrap_or(0)),
            nix::sys::signal::Signal::SIGTERM,
        );
        let _ = bwrap_child.wait();
        let _ = std::fs::remove_dir_all(&host_xdg_dir);
        return Err(ver_err(e));
    }
};
```

Wait — the bus is at `unix:path=/tmp/xdg/bus` inside the container. That path is on the overlay. The host can't access it at `/tmp/xdg/bus` because `/tmp` is a different mount namespace. We need to either:
- Bind-mount the bus socket to the shared `host_xdg_dir` path, OR
- Have the container create the bus socket inside `host_xdg_dir` which is bind-mounted

The entrypoint above copies the bus address file to `host_xdg_dir/bus-address`. But the actual socket (`/tmp/xdg/bus`) is not accessible from the host. We need the D-Bus daemon to listen on a path inside the bind-mounted `host_xdg_dir`:

Update the entrypoint dbus-daemon line:

```
"dbus-daemon --session --address=unix:path={bus_sock} ",
"--print-address --nofork &>/dev/null & ",
```

Where `bus_sock = host_xdg_dir.join("bus").display()`. Since `host_xdg_dir` is bind-mounted into the container at the same path, the socket is accessible from both sides.

Then the bus address is simply `format!("unix:path={}", bus_socket.display())`.

- [ ] **Step 6: Delete dead code**

Remove:
- `rewrite_bus_address_for_host()` function (~12 lines)
- `startup_diagnostics()` function (~15 lines)
- All `hakoniwa` imports and types
- `container_stdin` references in Session and tools

- [ ] **Step 7: Delete entrypoint.sh**

```bash
git rm entrypoint.sh
```

- [ ] **Step 8: Update launch_app**

Current `launch_app` writes to `container_stdin`. With bwrap, use a different mechanism. The simplest: the bwrap container shares the `host_xdg_dir`, so write a command file there and have a watcher inside the container exec it. Alternatively, use `nsenter` via the bwrap PID.

Replace the stdin write in `launch_app` with:

```rust
// Write command to a FIFO that the container watches
let cmd_path = sess.host_xdg_dir.join("launch-cmd");
std::fs::write(&cmd_path, &params.command).map_err(KwinError::from)?;
```

And add to the entrypoint a watcher loop:

```
"while true; do ",
"  inotifywait -e modify {cmd_path} 2>/dev/null && ",
"  cmd=$(cat {cmd_path}) && ",
"  eval \"$cmd &\" ; ",
"done &",
```

This is getting complex. Simpler approach: the entrypoint `cat` on stdin is still available — bwrap_child.stdin is piped. Keep the stdin protocol:

```rust
// In launch_app, same as current but using bwrap_child.stdin
let stdin = sess.bwrap_child.stdin.as_mut().ok_or_else(|| {
    McpError::internal_error("container stdin not available", None)
})?;
writeln!(stdin, "{}", params.command).map_err(KwinError::from)?;
stdin.flush().map_err(KwinError::from)?;
```

This requires `Session.bwrap_child` to be mutable, which means `launch_app` needs `&mut self` or the child stdin is stored separately. Store stdin separately:

```rust
struct Session {
    zbus_conn: zbus::Connection,
    eis: Eis,
    bwrap_child: std::process::Child,
    bwrap_stdin: std::process::ChildStdin,
    host_xdg_dir: std::path::PathBuf,
}
```

Take stdin from child at spawn time: `let bwrap_stdin = bwrap_child.stdin.take().ok_or(...)?;`

And in the entrypoint, the final `cat` reads stdin lines and execs them:

```
"while IFS= read -r cmd; do eval \"$cmd &\"; done"
```

- [ ] **Step 9: Build and verify compilation**

```bash
cargo build 2>&1
```

Expected: compiles with no hakoniwa references, bwrap spawn logic in place.

- [ ] **Step 10: Run clippy**

```bash
cargo clippy 2>&1
```

Expected: zero warnings (all clippy denies pass).

- [ ] **Step 11: Manual smoke test**

Start the MCP server, call `session_start`, verify:
- bwrap process spawns
- D-Bus connection established
- KWin running (screenshot works)
- `launch_app` works (app appears)
- `session_stop` kills everything cleanly
- Host `/tmp/kwin-mcp-*` cleaned up

- [ ] **Step 12: Commit**

```bash
git add -A
git commit -m "refactor: replace hakoniwa with bwrap, delete entrypoint.sh"
```

---

### Task 2: Update CLAUDE.md

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update architecture section**

Replace references to hakoniwa with bwrap. Update the entrypoint.sh references. Update the Session struct description.

- [ ] **Step 2: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md for bwrap architecture"
```
