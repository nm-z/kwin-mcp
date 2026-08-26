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

Pass `--autoclean` to remove the entire `/tmp/kwin-mcp-<pid>` session workdir. Cleanup ownership is claimed before `session_start` creates the directory and released only once the directory is gone, so it covers every terminal outcome: a successful stop, a start that fails, is cancelled, or hits the 20s hard limit, and the server exiting when the client never called `session_stop`.

| From | Event | To | Result |
| --- | --- | --- | --- |
| Idle | `session_start` claims the path | Owned | directory created under an owner |
| Owned | `session_start` succeeds | Owned | session runs, `session_stop` owns the delete |
| Owned | start fails, times out, or is cancelled with no session | Idle or Owned | deleted, or kept with a retry in the error |
| Owned | `session_stop` deletes the directory | Idle | `status=stopped` or `status=cleaned` with `workdir_removed` |
| Owned | `session_stop` cannot delete the directory | Owned | error naming the cause and the `session_stop` retry |
| Owned | transport closes | Idle or Owned | shutdown tears the session down and deletes |
| Idle | `session_stop` | Idle | `status=stopped` or `status=none`, as without the flag |

Before deleting, the server grants owner `rwx` to directories inside the owned workdir, so a mode-000 directory that a launched command created in the overlay cannot block the delete. The repair walks open descriptors with `O_PATH | O_NOFOLLOW` and chmods only what `fstat` proves is a directory, so symlinks are never followed or modified and nothing outside the validated workdir is touched, even while the tree is being rewritten concurrently. If a delete still fails, for example because a root-owned file sits inside, `session_stop` reports the error and keeps the workdir owned; call `session_stop` again to retry, and it reports `status=cleaned` once the directory is gone. Without the flag nothing is owned, and `session_stop` and server exit retain the existing workdir behavior.

## Strict host-GUI isolation

Normal Codex shell commands inherit the host desktop's Wayland, X11, and session-bus environment, so an accidental command can open or control a real host window. Launch Codex through `kwin-mcp-strict` to remove those channels from Codex and its shell tools while forwarding the original values only to the configured `kwin-mcp` stdio server:

```bash
# Assumes the MCP entry in config.toml is named "kwin-mcp".
target/release/kwin-mcp-strict --

# Forward Codex arguments after the separator.
target/release/kwin-mcp-strict -- --model gpt-5.6-terra
```

The launcher uses Codex's one-run `--config` overrides for `mcp_servers.<id>.env`, so it does not rewrite `~/.codex/config.toml`. Use `--mcp-server NAME` if the configured server has a different name, and `--codex PATH` if `codex` is not on `PATH`. The KWin MCP process retains the host-session values needed by its viewer, clipboard bridge, and wallet integration; apps continue to receive the isolated session's replacements.

Strict mode is fail-closed for inherited values and profile-based shell reinjection. Restoring normal host-desktop access requires an explicit opt-out from a host terminal:

```bash
target/release/kwin-mcp-strict --allow-host-gui --
```

This guards against accidental host GUI control; it is not a security sandbox for hostile code that deliberately reconstructs host socket paths. See the official [Codex MCP configuration](https://developers.openai.com/codex/mcp) and [CLI configuration overrides](https://developers.openai.com/codex/config-advanced) documentation for the underlying settings.

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

### Host socket exposure

At `session_start`, active pathname sockets beneath `$HOME` and non-graphical user-runtime sockets are exposed automatically. Sockets owned by processes attached to the host display, desktop application scopes, input devices, or the desktop session slice remain isolated. Parent directories are mounted read-only, which prevents host file writes but does not restrict operations offered by each exposed socket protocol. Hidden parent mounts also expose sibling files through their internal `/run/kwin-mcp-host-sockets` paths. Socket replacements at discovered names remain live; new socket names require a new session.

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
