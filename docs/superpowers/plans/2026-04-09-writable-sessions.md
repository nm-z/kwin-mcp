# Writable Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `writable: bool` parameter to `session_start` so agent writes persist to host, while protecting the 9 kwin-mcp display config files.

**Architecture:** When `writable: true`, bwrap uses `--bind` instead of `--overlay-src/--tmp-overlay` for `$HOME` and adds `--bind` for system paths (`/opt`, `/usr/local`, `/usr/share/applications`, `/usr/share/icons`, `/etc`, `/var`). The 6 `$HOME` config files that the entrypoint currently writes in-place are moved to Rust code — written to `host_xdg_dir` and `--ro-bind` mounted over their real paths. This applies in both modes, simplifying the entrypoint.

**Tech Stack:** Rust, bwrap, existing crate deps only

**Spec:** `docs/superpowers/specs/2026-04-09-writable-sessions-design.md`

---

### Task 1: Add `writable` field to `SessionStartParams`

**Files:**
- Modify: `src/main.rs:734-735`

- [ ] **Step 1: Add the field**

```rust
#[derive(Deserialize, schemars::JsonSchema, Default)]
struct SessionStartParams {
    /// When true, agent writes persist to the host filesystem.
    /// When false (default), all writes are ephemeral.
    #[serde(default)]
    writable: bool,
}
```

- [ ] **Step 2: Build and lint**

Run: `cargo clippy`
Expected: clean (the field is read in Task 3)

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat: add writable field to SessionStartParams"
```

---

### Task 2: Move $HOME config writes from entrypoint to Rust + --ro-bind

The entrypoint script currently writes 6 files in-place inside the container. Move these to Rust code that writes them to `host_xdg_dir` before bwrap starts, then `--ro-bind` mounts them over the real paths. This protects them in both writable and ephemeral modes and simplifies the entrypoint.

**Files:**
- Modify: `src/main.rs:858-966` (config writes + entrypoint + bwrap args)

- [ ] **Step 1: Write kwinrc to host_xdg_dir**

After the existing `atspi_conf_path` write (line 871), add:

```rust
let kwinrc_path = host_xdg_dir.join("kwinrc");
std::fs::write(&kwinrc_path,
    "[org.kde.kdecoration2]\nBorderSize=None\nShadowSize=0\n\n\
     [Compositing]\nLockScreenAutoLockEnabled=false\n"
).map_err(|e| ver_err(format!("write kwinrc: {e}")))?;
```

- [ ] **Step 2: Write kwinrulesrc to host_xdg_dir**

```rust
let kwinrulesrc_path = host_xdg_dir.join("kwinrulesrc");
std::fs::write(&kwinrulesrc_path,
    "[1]\nDescription=No decorations, maximized\nnoborder=true\nnoborderrule=2\n\
     maximizehoriz=true\nmaximizehorizrule=2\nmaximizevert=true\nmaximizevertrule=2\n\
     wmclassmatch=0\n\n[General]\ncount=1\nrules=1\n"
).map_err(|e| ver_err(format!("write kwinrulesrc: {e}")))?;
```

- [ ] **Step 3: Write kscreenlockerrc to host_xdg_dir**

```rust
let kscreenlockerrc_path = host_xdg_dir.join("kscreenlockerrc");
std::fs::write(&kscreenlockerrc_path,
    "[Daemon]\nAutolock=false\nLockOnResume=false\nTimeout=0\n"
).map_err(|e| ver_err(format!("write kscreenlockerrc: {e}")))?;
```

- [ ] **Step 4: Write kcmfonts to host_xdg_dir**

```rust
let kcmfonts_path = host_xdg_dir.join("kcmfonts");
std::fs::write(&kcmfonts_path,
    "[General]\nforceFontDPI=96\n"
).map_err(|e| ver_err(format!("write kcmfonts: {e}")))?;
```

- [ ] **Step 5: Write fontconfig/fonts.conf to host_xdg_dir**

```rust
let fonts_conf_path = host_xdg_dir.join("fonts.conf");
std::fs::write(&fonts_conf_path,
    "<?xml version=\"1.0\"?>\n\
     <!DOCTYPE fontconfig SYSTEM \"urn:fontconfig:fonts.dtd\">\n\
     <fontconfig>\n\
     <match target=\"font\">\n\
     <edit name=\"hinting\" mode=\"assign\"><bool>false</bool></edit>\n\
     <edit name=\"hintstyle\" mode=\"assign\"><const>hintnone</const></edit>\n\
     <edit name=\"antialias\" mode=\"assign\"><bool>true</bool></edit>\n\
     <edit name=\"rgba\" mode=\"assign\"><const>none</const></edit>\n\
     </match>\n\
     </fontconfig>\n"
).map_err(|e| ver_err(format!("write fonts.conf: {e}")))?;
```

- [ ] **Step 6: Build kdeglobals by reading + patching the real file**

The entrypoint currently uses `sed -i` on the host's kdeglobals. Do this in Rust instead:

```rust
let real_kdeglobals = std::path::Path::new(&home).join(".config/kdeglobals");
let mut kdeglobals_content = std::fs::read_to_string(&real_kdeglobals).unwrap_or_default();
let replacements = [
    ("ScaleFactor=", "ScaleFactor=1"),
    ("ScreenScaleFactors=", "ScreenScaleFactors="),
    ("XftHintStyle=", "XftHintStyle=hintnone"),
    ("XftSubPixel=", "XftSubPixel=none"),
    ("font=", "font=Noto Sans,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
    ("menuFont=", "menuFont=Noto Sans,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
    ("smallestReadableFont=", "smallestReadableFont=Noto Sans,12,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
    ("toolBarFont=", "toolBarFont=Noto Sans,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
    ("activeFont=", "activeFont=Noto Sans,14,-1,5,700,0,0,0,0,0,0,0,0,0,0,1,Bold"),
    ("fixed=", "fixed=Hack,14,-1,5,400,0,0,0,0,0,0,0,0,0,0,1"),
];
for (prefix, replacement) in replacements {
    let new_content = kdeglobals_content
        .lines()
        .map(|line| {
            if line.starts_with(prefix) { replacement.to_owned() }
            else { line.to_owned() }
        })
        .collect::<Vec<_>>()
        .join("\n");
    kdeglobals_content = new_content;
}
let kdeglobals_path = host_xdg_dir.join("kdeglobals");
std::fs::write(&kdeglobals_path, &kdeglobals_content)
    .map_err(|e| ver_err(format!("write kdeglobals: {e}")))?;
```

- [ ] **Step 7: Add --ro-bind mounts for all 6 files to bwrap args**

After the existing 3 system `--ro-bind` lines (line 964-966), add:

```rust
"--ro-bind", &kwinrc_path.display().to_string(), &format!("{home}/.config/kwinrc"),
"--ro-bind", &kdeglobals_path.display().to_string(), &format!("{home}/.config/kdeglobals"),
"--ro-bind", &kwinrulesrc_path.display().to_string(), &format!("{home}/.config/kwinrulesrc"),
"--ro-bind", &kscreenlockerrc_path.display().to_string(), &format!("{home}/.config/kscreenlockerrc"),
"--ro-bind", &kcmfonts_path.display().to_string(), &format!("{home}/.config/kcmfonts"),
"--ro-bind", &fonts_conf_path.display().to_string(), &format!("{home}/.config/fontconfig/fonts.conf"),
```

- [ ] **Step 8: Remove config writes from entrypoint script**

Remove these lines from the entrypoint string (lines 881-899):
- `mkdir -p "$HOME/.config/fontconfig"` and the `printf ... > fonts.conf` line
- `rm -rf "$HOME/.cache/fontconfig" && fc-cache -f` line
- `mkdir -p "$HOME/.config"` line
- `printf ... > kwinrc` line
- All 6 `sed -i ... kdeglobals` lines
- `printf ... > kcmfonts` line
- `printf ... > kscreenlockerrc` line
- `printf ... > kwinrulesrc` line

Add `export FONTCONFIG_CACHE=/tmp/fontconfig-cache` to the entrypoint env exports, and keep `mkdir -p /tmp/fontconfig-cache && fc-cache -f 2>/dev/null` in the entrypoint. This ensures the container rebuilds its font cache from the --ro-bind mounted fonts.conf into ephemeral /tmp, not the host's ~/.cache/fontconfig (which would corrupt host font rendering in writable mode).

The entrypoint should now only contain: env exports (including FONTCONFIG_CACHE), fc-cache, ATSPI env, dbus-daemon start, dbus wait loop, bridge-ready wait, kwin_wayland launch, dbus-update-activation-environment, at-spi-bus-launcher, pipewire, wireplumber, and the stdin read loop.

- [ ] **Step 9: Build and lint**

Run: `cargo clippy`
Expected: clean

- [ ] **Step 10: Commit**

```bash
git add src/main.rs
git commit -m "refactor: move entrypoint config writes to Rust + --ro-bind mounts"
```

---

### Task 3: Implement writable mode bwrap args

**Files:**
- Modify: `src/main.rs` (bwrap arg construction, ~lines 949-968)

- [ ] **Step 1: Read the writable param**

After `let home = ...` (line 948), add:

```rust
let writable = params.writable;
```

- [ ] **Step 2: Change $HOME mount based on writable flag**

Replace:
```rust
"--ro-bind", "/", "/",
"--overlay-src", &home, "--tmp-overlay", &home,
```

With:
```rust
if writable {
    cmd.args(["--bind", "/", "/"]);
    // /tmp and /run stay ephemeral regardless
} else {
    cmd.args([
        "--ro-bind", "/", "/",
        "--overlay-src", &home, "--tmp-overlay", &home,
    ]);
}
```

Note: when `writable: true`, the entire root is `--bind` (read-write). The `--tmpfs /tmp` and `--tmpfs /run` lines that come later override `/tmp` and `/run` back to ephemeral. The `--ro-bind` lines for the 9 config files override those specific paths back to read-only. The `--dev /dev` line overrides `/dev`. So the effective result is: everything writable except `/tmp`, `/run`, `/dev`, and the 9 protected config files.

Refactor the remaining bwrap args that were previously in the single `.args([...])` call to work with both branches. The args after the root/home mount stay the same in both modes:

```rust
cmd.args([
    "--dev", "/dev",
    "--dev-bind", "/dev/dri", "/dev/dri",
    "--dev-bind", "/dev/uinput", "/dev/uinput",
    "--dev-bind", &mouse_evdev_str, &mouse_evdev_str,
    "--dev-bind", &kbd_evdev_str, &kbd_evdev_str,
    "--proc", "/proc",
    "--tmpfs", "/tmp",
    "--tmpfs", "/run",
    "--bind", &xdg_dir_str, &xdg_dir_str,
    "--ro-bind", &atspi_conf_path.display().to_string(), "/usr/share/defaults/at-spi2/accessibility.conf",
    "--ro-bind", &fc_hinting_str, "/usr/share/fontconfig/conf.default/10-hinting-slight.conf",
    "--ro-bind", &fc_lcd_str, "/usr/share/fontconfig/conf.default/11-lcdfilter-default.conf",
    "--ro-bind", &kwinrc_path.display().to_string(), &format!("{home}/.config/kwinrc"),
    "--ro-bind", &kdeglobals_path.display().to_string(), &format!("{home}/.config/kdeglobals"),
    "--ro-bind", &kwinrulesrc_path.display().to_string(), &format!("{home}/.config/kwinrulesrc"),
    "--ro-bind", &kscreenlockerrc_path.display().to_string(), &format!("{home}/.config/kscreenlockerrc"),
    "--ro-bind", &kcmfonts_path.display().to_string(), &format!("{home}/.config/kcmfonts"),
    "--ro-bind", &fonts_conf_path.display().to_string(), &format!("{home}/.config/fontconfig/fonts.conf"),
    "--", "bash", "-c", &entrypoint,
]);
```

- [ ] **Step 3: Build and lint**

Run: `cargo clippy`
Expected: clean

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat: implement writable mode bwrap args"
```

---

### Task 4: Update tool response to report writable status

**Files:**
- Modify: `src/main.rs:1161-1168` (structured_result JSON)

- [ ] **Step 1: Add writable field to response JSON**

Change the `structured_result` call:

```rust
Ok(structured_result(&peer, msg, serde_json::json!({
    "status": "started",
    "version": format!("v{}.{}", env!("CARGO_PKG_VERSION"), env!("BUILD_NUMBER")),
    "commit": env!("GIT_HASH"),
    "bus": bus_name,
    "kwin_unique": kwin_unique_name,
    "workdir": workdir,
    "writable": writable,
})).await)
```

- [ ] **Step 2: Include writable in the text message**

Update the `msg` format string to mention writable status:

```rust
let msg = format!(
    "{version_stamp} — session started bus={bus_name} kwin={kwin_unique_name} writable={writable}"
);
```

- [ ] **Step 3: Update the tool description**

Update the `#[rmcp::tool]` attribute on `session_start`:

```rust
#[rmcp::tool(
    name = "session_start",
    description = "Start an isolated KDE Wayland session. Set writable=true to persist writes to host filesystem (default: false, all writes are ephemeral). Must be called before any other tool."
)]
```

- [ ] **Step 4: Build and lint**

Run: `cargo clippy`
Expected: clean

- [ ] **Step 5: Commit**

```bash
git add src/main.rs
git commit -m "feat: report writable status in session_start response"
```

---

### Task 5: Smoke test

- [ ] **Step 1: Build release**

Run: `cargo build --release`
Expected: clean build

- [ ] **Step 2: Test ephemeral mode (default)**

Start session with default params. Verify overlay behavior is unchanged — writes inside container don't persist to host.

- [ ] **Step 3: Test writable mode**

Start session with `writable: true`. Verify:
1. Agent can write a file to `$HOME` and it persists on host after session_stop
2. Agent can write to `/opt` and it persists
3. The 9 protected config files are read-only inside the container (writes fail)
4. `/tmp` and `/run` are still ephemeral

- [ ] **Step 4: Verify version increment**

Check that the version in session_start response has incremented.

- [ ] **Step 5: Commit any fixes**

```bash
git add -A
git commit -m "fix: smoke test fixes for writable sessions"
```
