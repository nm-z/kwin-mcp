# kwin-mcp

MCP server for KWin Wayland GUI automation. Single-binary Rust using `rmcp` + `reis` (EIS input) + `atspi` (accessibility tree) + `zbus` (D-Bus/KWin IPC) + `evdev` (uinput virtual devices). Container isolation via bubblewrap and pasta.

The optional [KWin MCP skill](skills/kwin-mcp/SKILL.md) helps Codex choose between this server's isolated desktop and the user's current desktop. It requires a separately configured KWin MCP server.

## Tools

| Tool | Description |
|---|---|
| `session_start` | Start an isolated KDE Wayland session without opening a host viewer. Must be called first. |
| `session_stop` | Tear down the session and all container processes. |
| `viewer_open` | Open the live host viewer for the current session (reports ready, starting, or unavailable with the reason). |
| `viewer_close` | Close the host viewer without stopping the isolated session. |
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
| `export_file` | Copy a session file (e.g. a finished download) to a real host path and verify it byte for byte. |

`session_start` leaves the host viewer closed. Work without a viewer unless the user must act on the session (for example, Duo, OTP, CAPTCHA, or approval) or asks to watch. Call `viewer_open` for that step, keep the page open while polling with screenshots, and call `viewer_close` when the step is done. If a Duo push expires, tell the user in one line and leave the page on the resend option so they can retry; continue when the page advances. `session_start` reports the viewer as `closed`; `viewer_open` reports `ready`, `starting`, or `unavailable` with a reason (for example, no active host Wayland session when serving over SSH). The old `--no-viewer` argument remains accepted for existing server commands.

Pass `--autoclean` to remove the server-owned workdir after stop, failed start, timeout, transport close, or SIGTERM/SIGINT/SIGHUP. Cleanup repairs permissions only inside that directory and does not follow symlinks. If removal fails, `session_stop` reports the error and keeps ownership for a retry. Without the flag, the workdir remains after stop.

On startup, `--autoclean` also removes a workdir left by a crashed server when this version's lease marker proves it opted into cleanup, its lock is free, its sandbox process group has exited, and no mount remains inside it. Older workdirs without a lease marker remain for individual review.

Pass `--ttl MINUTES` to tear down an idle session and its viewer after that many minutes without a tool call. The server stays available for the next `session_start`; `--ttl` implies `--autoclean`, so expiry also removes the workdir.
Each non-idempotent `session_start` attempt records its outcome, duration, server PID, commit, and failure reason in `$XDG_RUNTIME_DIR/kwin-mcp/session-starts.log` when `XDG_RUNTIME_DIR` is set. Run `kwin-mcp --stats [SECONDS]` to print successes and attempts in the requested window (3600 seconds by default), followed by the five most common failure reasons.
To run the MCP server on another host, set the client's stdio command to `ssh -T HOST /path/to/kwin-mcp`. SSH carries MCP requests and screenshots while the isolated desktop runs on `HOST`. The remote host needs the same KWin, pasta, and device dependencies as a local session.

## Strict host-GUI isolation

Normal Codex shell commands inherit the host desktop's Wayland, X11, and session-bus environment, so an accidental command can open or control a real host window. Launch Codex through `kwin-mcp-strict` to remove those channels from Codex and its shell tools while forwarding the original values only to the configured `kwin-mcp` stdio server:

```bash
# Assumes the MCP entry in config.toml is named "kwin-mcp".
target/release/kwin-mcp-strict --

# Forward Codex arguments after the separator.
target/release/kwin-mcp-strict -- --model gpt-5.6-terra
```

The launcher uses Codex's one-run `--config` overrides for `mcp_servers.<id>.env`, so it does not rewrite `~/.codex/config.toml`. Use `--mcp-server NAME` if the configured server has a different name, and `--codex PATH` if `codex` is not on `PATH`. The KWin MCP process retains the host-session values needed by its viewer; apps continue to receive the isolated session's replacements.

Strict mode is fail-closed for inherited values and profile-based shell reinjection. Restoring normal host-desktop access requires an explicit opt-out from a host terminal:

```bash
target/release/kwin-mcp-strict --allow-host-gui --
```

This guards against accidental host GUI control; it is not a security sandbox for hostile code that deliberately reconstructs host socket paths. See the official [Codex MCP configuration](https://developers.openai.com/codex/mcp) and [CLI configuration overrides](https://developers.openai.com/codex/config-advanced) documentation for the underlying settings.

## Codex plugin

The plugin packages the [KWin MCP routing skill](skills/kwin-mcp/SKILL.md). Configure the MCP server separately; the plugin does not install a binary or declare an endpoint.

## Clipboard isolation

KWin MCP does not bridge clipboard contents between the host desktop and the isolated session. Each compositor keeps its own clipboard and primary selection; copying in one session does not overwrite or seed the other session.

## KWallet safety

A session never talks to the host wallet or Secret Service. `session_start` takes one read-only copy of the host KWallet, and the session's apps then use a session-local `org.kde.kwalletd6` served from memory; their writes stay in the session and end with it. The copy is taken only when `kwalletd6` and `ksecretd` already run and the wallet is unlocked: kwin-mcp never starts, unlocks or prompts a host wallet service, never calls `org.freedesktop.secrets`, and serializes copies across servers, at least 100 ms apart, within 12 s per `session_start`. If `kwalletd6` is missing or does not answer, the same wallet is read from `ksecretd` with the standard Secret Service calls, and copies in the next 60 s read `ksecretd` directly. If any check fails the session gets a disabled KWallet, `session_start` reports why, and `launch_app` of a Chromium-family browser says that the browser cannot decrypt the copied profile: Chrome drops every cookie it cannot decrypt (a recovered 3,606-cookie profile fell to 8 rows with the `basic` store, and kept 3,311 with the wallet). The host session D-Bus socket and the `keyring` and `p11-kit` entries of the user runtime directory are never reachable from a session.

## Session Architecture

Nested mounts under HOME require separate overlays. Regular files directly in
their ancestor directories are copied into the private session. For files larger
than 64 MiB, kwin-mcp first tries a copy-on-write reflink. If the filesystem cannot
clone the file, it uses a read-only bind instead of copying its data during
startup. Other files and directory overlays keep their existing write behavior.
`session_start` reports these fallback files in `oversized_read_only_files`.
To require private writable copies for specific files or directories, pass their
absolute paths under HOME in `writable_paths` when starting a new session.
Explicit copies still share the 20-second startup deadline; this option does not
make existing read-only host mounts writable or change a running session.

### Session memory limits

Each new sandbox runs in a systemd user scope with `MemoryHigh=3G`,
`MemoryMax=4G`, and `MemorySwapMax=1G`. Startup fails if the user manager
cannot create a scope. The limits cover the sandbox and its applications;
the host viewer and MCP server remain outside that scope.

For a gateway that needs more memory, set all three limits explicitly when
launching its server or shim:

```bash
kwin-mcp-shim --memory-high 8 --memory-max 10 --memory-swap-max 6
```

Values are whole GiB. `--memory-high 0` disables throttling while retaining
the RAM and swap limits. `--memory-swap-max 0` prevents new swap use.
`--memory-max` must be positive and at least `--memory-high`.
Existing sessions retain their current limits until their owner restarts them
or changes their scope properties.

```
kwin-mcp (host process)
  ├── proxy_conn (owns org.kde.KWin on container D-Bus)
  │     └── InputDeviceManager + InputDevice objects
  │         (KCMs see virtual mouse/keyboard here)
  ├── kwin_conn (talks to KWin via unique name)
  │     └── EIS, ScreenShot2, Scripting
  └── pasta private network namespace
        └── bwrap container (bubblewrap, overlayfs on $HOME)
              ├── dbus-daemon        (isolated session bus, anonymous auth)
              ├── kwin_wayland       (virtual display 1000x1000, XWayland)
              ├── pipewire + wireplumber
              ├── at-spi-bus-launcher
              └── uinput devices     (virtual mouse + keyboard, bind-mounted)
```

Session storage: sockets and small files stay in `/tmp/kwin-mcp-<pid>` (RAM). The `$HOME` overlay upper, work and staging layers are on disk in `${XDG_CACHE_HOME:-~/.cache}/kwin-mcp/kwin-mcp-<pid>`, which the session itself sees as an empty directory. `session_start` refuses to start with less than 2 GiB free there, and `session_stop` (or the leaked-workdir sweep) deletes it.

### Two-phase D-Bus startup

1. bwrap starts, dbus-daemon creates session bus
2. Host `proxy_conn` claims `org.kde.KWin`, registers InputDevice objects
3. Container starts KWin (gets unique name `:1.N`, not the well-known name)
4. Host discovers KWin's unique name by probing for EIS interface
5. Host `kwin_conn` connects to KWin via unique name for EIS/screenshots/scripting

This lets KCMs (like Mouse settings) see our virtual devices under `org.kde.KWin`, while the MCP server talks to the real KWin compositor via its unique bus name.

### HID isolation

Virtual input devices are created via `/dev/uinput` (requires `input` group). They are kernel-global but the host's KWin does not claim them (no seat tag assigned by udev). The devices are bind-mounted into the container and destroyed on session_stop.

All coordinates are window-relative — window position is added internally via KWin scripting. `screenshot` returns the active window by default, so a pixel read off the image is the `mouse_click` coordinate even for a small dialog away from the display origin; a cropped screenshot reports `region=[x1,y1,x2,y2]` and its pixel (px,py) is `mouse_click` (px+x1, py+y1).

### Host socket exposure

At `session_start`, active pathname sockets beneath `$HOME` and non-graphical user-runtime sockets are exposed automatically. Sockets owned by processes attached to the host display, desktop application scopes, input devices, or the desktop session slice remain isolated. Parent directories are mounted read-only, which prevents host file writes but does not restrict operations offered by each exposed socket protocol. Hidden parent mounts also expose sibling files through their internal `/run/kwin-mcp-host-sockets` paths, except the host session D-Bus socket and the `keyring` and `p11-kit` entries of the user runtime directory, which are masked and never exposed. Socket replacements at discovered names remain live; new socket names require a new session.

## Build

```bash
cargo build          # debug
cargo build --release
cargo clippy         # strict: unwrap/expect/todo/dead_code all denied
```

## Shim: hot reload and one session per agent

Run `kwin-mcp-shim --help` (or `-h`) for its options and
`kwin-mcp-shim --version` for its version. These commands exit without starting
the relay or any child process. Unknown options fail before relay startup.

Point your MCP client at `target/release/kwin-mcp-shim` instead of `kwin-mcp`, with the same arguments. The shim is launched once and never restarts. It runs each real `kwin-mcp` server as a child and relays MCP:

- **One session per agent.** Every `session_start` without a `session_id` gets its own child process, so each session has its own display, windows, keyboard focus and mouse. The result includes a `session_id` (for example `s12345`, matching `/tmp/kwin-mcp-12345`). Every tool accepts `session_id`. It may be omitted only while exactly one session is live; otherwise the call fails and lists the live ids. `session_list` shows every session and its owner. Each server's environment carries `KWIN_MCP_OWNER` (the agent process that started the shim, as `COMM pid PID`, plus `session ID` for Claude Code), so a cleanup selects one agent's servers with `grep -lz '^KWIN_MCP_OWNER=node pid 1234' /proc/*/environ` instead of by command line. Parallel subagents that share one MCP connection each call `session_start` and use their own id.
- **Hot reload, no reconnect.** The shim watches `src/`, `Cargo.toml`, `Cargo.lock` and `build.rs` next to its own binary and runs `cargo build` when they change (log: `~/.cache/kwin-mcp-shim/build.log`). When the `kwin-mcp` binary changes, from its build or anyone else's, it swaps in a fresh idle child and sends `notifications/tools/list_changed`. New sessions get the new build. Live sessions keep running on their original child until they stop, and tools they still serve stay published. Calling a tool newer than a session's build fails with a clear message.
- **Supervision.** A child that exits, or does not answer a ping for 90 seconds, is replaced. Its in-flight calls fail with a clear error, and processes left in its process session are killed. The shim is a child subreaper, so orphans are reaped. Every 5 minutes, and after each child exit, it runs `kwin-mcp --sweep-workdirs`. That removes leased autoclean workdirs whose sandbox is gone, and unleased `/tmp/kwin-mcp-<pid>` workdirs whose server has exited. Servers the shim did not start are never touched.

Changes to the shim itself still need a client reconnect. Keep it thin.

## Setup

### Claude Code registration

Register the shim once at **user scope** so it remains available when a session
changes projects or leaves your home directory. Use an absolute path to the
installed binary, with `kwin-mcp` and `kwin-viewer` installed beside it:

```bash
claude mcp add --scope user --transport stdio kwin-mcp -- \
  /absolute/path/to/kwin-mcp-shim \
  --width 1920 --height 1080 --autoclean --ttl 120
```

Claude Code stores this registration in the top-level `mcpServers` object in
`~/.claude.json`. See [Claude Code's MCP installation scopes](https://code.claude.com/docs/en/mcp#user-scope).

If an installation already has a user registration, keep that entry and migrate
the existing configuration:

1. Remove duplicate local registrations from `projects["<project path>"].mcpServers`
   in `~/.claude.json`, and duplicate project registrations from `.mcp.json` files,
   including `~/.mcp.json`. A duplicate may use another name, such as `kwin-rust`;
   compare its command and arguments before removing it.
2. Remove `kwin-mcp` from `projects["<project path>"].disabledMcpServers` in
   `~/.claude.json` wherever it was disabled. Preserve other disabled server names.
3. Start a new Claude Code session after changing the files. In `/mcp`, verify one
   enabled user-scope `kwin-mcp` server. Repeat from a directory outside the former
   project, then change the working directory in a session and confirm its tools
   remain available.

The expected configuration has one user-scope registration, no project or local
duplicate, and no workspace override disabling `kwin-mcp`.

### System dependencies

Add your user to these groups:
```
sudo usermod -aG input,uinput,video,render $USER
```

Requires: `bubblewrap` (bwrap), `passt` (pasta), and KWin running as a Wayland compositor. Each session has private loopback: ports the host listens on are forwarded to the host, so session apps reach the real local services at `127.0.0.1`; other ports stay private to the session, and only the session's CDP port is forwarded into the host. The sandbox keeps the host hostname so copied HOME profile locks do not look as if they belong to another computer. FUSE works inside the session when the host has `/dev/fuse`, a non-setuid `bwrap`, `setpriv`, and `fusermount`: AppImages mount normally and FUSE filesystems such as sshfs work, while apps still run without capabilities over a read-only host root. Without those, `launch_app` sets `APPIMAGE_EXTRACT_AND_RUN=1` so compatible AppImages run without FUSE. `launch_app` also selects the session's Xwayland display for X11 apps.

## Screenshot dimensions

Virtual display is 2000×1875 (3.75MP). All windows open maximized, no decorations, no shadows. Font hinting disabled, grayscale antialiasing, 96 DPI, scale 1.0.

Token cost: ~1 token per 750 pixels. A 2000×1875 screenshot costs ~5000 tokens.
