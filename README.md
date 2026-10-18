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

## Resize a maximized session window

To resize a maximized window with KWin's native controls, restore it with the session's configured restore/maximize shortcut through `keyboard_key` (the regression proof binds this to `super+Backspace`). Read its `x`, `y`, `w`, and `h` with `window_list`, then hold Super with `keyboard_press` (`key: "super"`) and call `mouse_drag` with `button: "right"` from ten pixels inside the lower-right corner to the desired lower-right point. `mouse_drag` coordinates are relative to the active window. Release Super with `keyboard_release` (`key: "super"`), then verify the geometry with `window_list` or screenshot metadata.

A successful `org.kde.kwin.Script.run` response does not verify the resulting window geometry. Check `window_list` or screenshot metadata before reporting a resize as successful.

`session_start` leaves the host viewer closed. Work without a viewer unless the user must act on the session (for example, Duo, OTP, CAPTCHA, or approval) or asks to watch. Before asking for information, put the relevant form or empty entry on screen, then call `viewer_open` with a short `reason` explaining what the user should do. Never assume the viewer is already open. You can keep your turn open and poll screenshots for completion instead of requiring a chat confirmation. Continue when the screen shows that the step is complete; the viewer may remain open so the user can watch, and the user can close it directly. Use `viewer_close` when the user asks. If a Duo push expires, tell the user in one line and leave the page on the resend option so they can retry; continue when the page advances. `session_start` reports the viewer as `closed`; `viewer_open` reports `ready` after the host renderer successfully submits the first PipeWire frame for presentation, or `unavailable` with a reason (for example, no active host Wayland session when serving over SSH). This reports renderer submission, not compositor acknowledgement. The old `--no-viewer` argument remains accepted for existing server commands.

Pass `--autoclean` to remove the server-owned workdir after stop, failed start, timeout, transport close, or SIGTERM/SIGINT/SIGHUP. Cleanup repairs permissions only inside that directory and does not follow symlinks. If removal fails, `session_stop` reports the error and keeps ownership for a retry. Without the flag, the workdir remains after stop.

On startup, `--autoclean` also removes a workdir left by a crashed server when this version's lease marker proves it opted into cleanup, its lock is free, its sandbox process group has exited, and no mount remains inside it. Older workdirs without a lease marker remain for individual review.

Pass `--ttl MINUTES` to tear down an idle session and its viewer after that many minutes without a tool call. The server stays available for the next `session_start`; `--ttl` implies `--autoclean`, so expiry also removes the workdir.
Each `session_start` attempt records its outcome, duration, server PID, commit, and failure reason in `$XDG_RUNTIME_DIR/kwin-mcp/session-starts.log` when `XDG_RUNTIME_DIR` is set. Run `kwin-mcp --stats [SECONDS]` to print successes and attempts in the requested window (3600 seconds by default), followed by the five most common failure reasons.
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

## Host window inventory and exact activation

`window_list` and `window_activate` operate only on this server's isolated KWin session. They cannot inspect or activate windows on the host desktop. When a task explicitly concerns an existing host window, use [kdotool](https://github.com/jinliu/kdotool) from a shell attached to that user's KDE Plasma 6 session. It uses KWin's scripting interface over the session D-Bus; it is a separate host utility and is not installed by kwin-mcp.

List every host window with its KWin ID, process ID, class, title, and geometry:

```bash
for id in $(kdotool search --id '.*'); do
  printf '\nID: %s\nPID: %s\nClass: %s\nTitle: %s\nGeometry:\n%s\n' \
    "$id" \
    "$(kdotool getwindowpid "$id")" \
    "$(kdotool getwindowclassname "$id")" \
    "$(kdotool getwindowname "$id")" \
    "$(kdotool getwindowgeometry "$id")"
done
```

The unfiltered search includes windows that are not currently visible, and the returned UUIDs are KWin window IDs, not X11 IDs. Process IDs are best-effort because some applications do not publish them. After matching the title/class/process information, pass the exact UUID from that inventory to activate one window:

```bash
kdotool windowactivate '{window-uuid}'
```

Activation switches to the window's virtual desktop when needed. Run these commands only when the requested task is about the host desktop: they query and activate real host windows. They require an installed kdotool and access to the active Plasma 6 session bus. See the [kdotool command reference](https://github.com/jinliu/kdotool/blob/master/README.md) and [KWin scripting API](https://develop.kde.org/docs/plasma/kwin/api/).

## Codex plugin

The plugin packages the [KWin MCP routing skill](skills/kwin-mcp/SKILL.md). Configure the MCP server separately; the plugin does not install a binary or declare an endpoint.

## Clipboard isolation

Each session has its own clipboard. In the local viewer, Ctrl+C or Ctrl+X copies to the host clipboard, and Ctrl+V or Shift+Insert pastes from the host clipboard. The viewer restores the session's prior selection afterward. Put anything the user should paste through the viewer on the **host** clipboard.

Use `clipboard_write` with `target: "session"` for agent input or `target: "host"` for local viewer paste. Provide exactly one of `text` or `generate_secret`. For example, `{"target":"host","generate_secret":{"length":20,"character_classes":["lowercase","uppercase","digits","symbols"]}}` generates a value using operating-system randomness and includes every selected class. The response contains only its target and character length.

`clipboard_read` reads session text only. Generated selections carry a secret marker, so this tool returns `text: null`, `protected: true`, and the length while that marker is present. This prevents an accidental tool readback; it does not restrict applications that can read the clipboard. Clipboard ownership survives server reload and ends when the server exits. Session clipboard ownership also ends when its session stops. The host target means the server's desktop, not a remote viewer client's desktop.

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

Each stdio server watches its launching client with a Linux pidfd. When that
client exits, all of the server's session scopes are stopped, including nested
KWin and D-Bus processes, even if another descendant keeps stdin open. The
watcher and session scope identity survive hot reload. Normal stdin EOF,
`session_stop`, and startup cancellation use the same scope cleanup path.

Each new sandbox runs in a systemd user scope with `MemoryHigh=3G`,
`MemoryMax=4G`, and `MemorySwapMax=1G`. Startup fails if the user manager
cannot create a scope. The limits cover the sandbox and its applications;
the host viewer and MCP server remain outside that scope.

For a gateway that needs more memory, set all three limits explicitly when
launching its server:

```bash
kwin-mcp --memory-high 8 --memory-max 10 --memory-swap-max 6
```

Values are whole GiB. `--memory-high 0` disables throttling while retaining
the RAM and swap limits. `--memory-swap-max 0` prevents new swap use.
`--memory-max` must be positive and at least `--memory-high`.
Existing sessions retain their current limits until their owner restarts them
or changes their scope properties.

`session_start` also accepts `memory_high`, `memory_max`, and
`memory_swap_max` in whole GiB. Each omitted value inherits the server's
default; zero disables the throttle or swap, while the RAM hard limit must
remain positive. For example:

```json
{"memory_high":8,"memory_max":10,"memory_swap_max":6}
```

The result and `session_list` report the actual byte limits and owned scope.
When the sandbox exits, the server tears down its resources and keeps a dead
entry until `session_stop`. Later calls return the exit cause and recovery
guidance. An OOM diagnosis includes the scope result, RAM/swap limits and
systemd's memory peak when available; a signal exit alone is never treated
as proof of an OOM kill. After upgrading an older server, scope identity and
limits are recovered from the sandbox PID cgroup; values that cannot be
recovered remain unknown. Stop the dead handle, start a new desktop without
`session_id`, and relaunch the applications to continue.

Configure MCP clients to launch the current `kwin-mcp` executable directly.
The historical `kwin-mcp-shim` has its own session listing and hides these
structured lifecycle diagnostics; it is no longer built by this repository.

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

Session storage: sockets and small files stay in `/tmp/kwin-mcp-<pid>-<number>` (RAM). The `$HOME` overlay upper, work and staging layers are on disk in `${XDG_CACHE_HOME:-~/.cache}/kwin-mcp/kwin-mcp-<pid>-<number>`, which the session itself sees as an empty directory. `session_start` refuses to start with less than 2 GiB free there, and `session_stop` (or the leaked-workdir sweep) deletes it. With `--autoclean`, failed startup also removes the disk overlay before removing the temporary workdir. If either removal fails, cleanup keeps ownership for a `session_stop` retry and reports the failing path.

`TMPDIR` overrides the runtime storage root. Linux pathname Unix sockets allow
at most 107 bytes, excluding the terminating NUL, for the entire socket path.
The session's D-Bus sockets have a stricter 99-byte limit imposed by
[libdbus](https://dbus.freedesktop.org/doc/api/html/group__DBusSysdeps.html).
`session_start` checks both constraints before creating its workdir or starting
processes. If a path is too long, the error names it, reports its byte length,
and asks for a shorter `TMPDIR`; set that variable before launching the server.
The limit includes the generated `kwin-mcp-<pid>-<number>` directory and socket
filename, and counts UTF-8 bytes rather than characters.

Temporary KWin scripts also live in that disk directory, exposed to the sandbox
through a read-only `kwin-scripts` mount in its runtime directory. A full `/tmp`
therefore does not prevent `window_list` or screenshot geometry queries in new
sessions. If saving `screenshot.png` fails, the default call reports a
`write_failed` storage error; retry with `inline=true` to receive the pixels
with `path: null` and `write_error`. Freeing storage allows later screenshots
to save normally. A session restored from an older server keeps its original
script storage until it is recreated.

Live host SQLite databases with open WAL files are mounted read-only with their
WAL and shared-memory files, except browser cookie jars. Startup mounts these
databases without copying their contents, and reads use the host inodes and
SQLite locks. Session applications cannot write them. Browser history databases
still start empty in the private overlay. Cookie jars remain writable in their
private session overlay; writes cannot reach the host jar.
Live database directory aliases also expose their siblings read-only under
`/run/kwin-mcp-host-sqlite/`. Ordinary sibling paths under HOME remain privately
writable. The aliases mirror hidden session directories and mask known host
Unix sockets; approved helper sockets use their separate exposure policy.
Symlinks reached through them resolve within the sandbox's mount
namespace; they do not restore the hidden host runtime bus, keyring, or p11-kit.
Startup timeout errors name the active preparation stage: host mounts, HOME
overlays, host sockets and processes, or browser profiles.

### Two-phase D-Bus startup

The private KWallet bus records `dbus-daemon` diagnostics in the session
workdir's `kwallet-bus.stderr`. If the daemon exits before readiness or its
socket does not appear within three seconds, `session_start` includes a bounded
stderr excerpt in the error before cleanup. `--autoclean` removes the log with
the failed session's workdir.

1. bwrap starts, dbus-daemon creates session bus
2. Host `proxy_conn` claims `org.kde.KWin`, registers InputDevice objects
3. Container starts KWin (gets unique name `:1.N`, not the well-known name)
4. Host discovers KWin's unique name by probing for EIS interface
5. Host `kwin_conn` connects to KWin via unique name for EIS/screenshots/scripting

This lets KCMs (like Mouse settings) see our virtual devices under `org.kde.KWin`, while the MCP server talks to the real KWin compositor via its unique bus name.

### HID isolation

Virtual input devices are created via `/dev/uinput` (requires `input` group). They are kernel-global but the host's KWin does not claim them (no seat tag assigned by udev). The devices are bind-mounted into the container and destroyed on session_stop.

Startup device errors identify the mouse or keyboard, the failed operation,
the `/dev/uinput` or sysfs path, and the original OS error. They also report
the failing server's numeric `Uid`, `Gid`, and supplementary `Groups` from
`/proc/self/status`, plus its user and mount namespace IDs. Compare these
with the launching shell when investigating access from an enclosing
namespace: a writable `/dev/uinput` check alone does not verify device
configuration, creation, or evdev discovery.

Run `ruby tests/uinput_startup_e2e.rb /path/to/kwin-mcp /path/to/evidence`
on a KDE host with `/dev/uinput`, an existing user runtime and bus, and
passwordless sudo to verify startup diagnostics through the stdio server.
The regression mounts failure fixtures in private mount namespaces and
restores the caller's credentials before launching the server.

All coordinates are window-relative — window position is added internally via KWin scripting. `screenshot` returns the active window by default, so a pixel read off the image is the `mouse_click` coordinate even for a small dialog away from the display origin; a cropped screenshot reports `region=[x1,y1,x2,y2]` and its pixel (px,py) is `mouse_click` (px+x1, py+y1).

### Host socket exposure

At `session_start`, active pathname sockets beneath `$HOME` and non-graphical user-runtime sockets are exposed automatically. Sockets owned by processes attached to the host display, desktop application scopes, input devices, or the desktop session slice remain isolated. Parent directories are mounted read-only, which prevents host file writes but does not restrict operations offered by each exposed socket protocol. Hidden parent mounts also expose sibling files through their internal `/run/kwin-mcp-host-sockets` paths, except the host session D-Bus socket and the `keyring` and `p11-kit` entries of the user runtime directory, which are masked and never exposed. Socket replacements at discovered names remain live; new socket names require a new session.

## Build

```bash
cargo build          # debug
cargo build --release
cargo clippy         # strict: unwrap/expect/todo/dead_code all denied
```

## Input timing

Agent mouse and keyboard tools accept `speed`, a positive timing multiplier
(default `1.0`). `keyboard_type` defaults to approximately 100 WPM; `speed: 2`
requests approximately twice that rate. Mouse tools also accept `target_width`,
the smaller target dimension in pixels (default `20`), for movement timing.

Pointer movement uses sampled Fitts-law durations, minimum-jerk progress,
curved paths, and short corrections on long moves. Typing samples per-bigram
hold times and intervals from a published-data fit, including overlapping keys
and word-boundary timing. Scrolls preserve the requested total in timed bursts.
See [profile sources and fitting method](data/README.md) for assumptions and
source-data terms. No host input is recorded.

## Input logs

`session_start` and `session_list` report each session's `input_log` path.
Agent input is appended immediately to JSONL under
`${XDG_STATE_HOME:-~/.local/state}/kwin-mcp/input/`, with file permissions `0600`.
Logs remain after session cleanup and continue in the same file after a server reload.
No export call is required.

Each raw input record includes monotonic and wall-clock nanoseconds, the MCP tool
name and request ID, and the motion, button, key, or scroll event. Motion records
include screen and window coordinates. Key records include characters when known,
hold times, and gaps. Scroll records identify pixels or discrete value120 units.
Each call ends with a summary of its pointer path and peak speed, typing timing,
and scroll totals and event rate. A flushed event means the EIS socket accepted
the data; it does not confirm that an application consumed it.

## Sessions and reloads

### Browser cookie continuity

When a session stops, kwin-mcp saves its Chromium profile cookie databases under
`${XDG_STATE_HOME:-~/.local/state}/kwin-mcp/browser-cookies`, keyed by browser and
profile. The directory is private (`0700`), and the databases are private (`0600`).
The next session merges these cookies into its isolated copy of the host profile.
For each cookie identity, the row with the newer `last_update_utc` wins; partitioned
cookies retain their partition identity. The host browser profile stays read-only.
History, cached pages, saved tabs, and other profile changes remain disposable.

Cookies keep their existing encryption and require the copied host wallet key.
Host cookie snapshots read only the database and WAL files, verify two identical
observations, and check the resulting private SQLite copy. The host database and
WAL together must fit within 64 MiB. If the files keep changing during the bounded
copy attempts, an active rollback journal remains, a host snapshot fails while a
saved jar is available, or the saved schema differs, kwin-mcp reports the cookie
problem in `session_start`'s `cookie_errors` when startup can continue. If a saved
jar has an incompatible schema, `session_start` uses the host snapshot; at
`session_stop`, kwin-mcp replaces the incompatible saved jar with the session
jar. Browser flush timeouts and cookie save failures are returned in
`session_stop`'s `cookie_errors`; a browser process enumeration failure is
reported there as well. Remove the profile's saved database from this directory
to discard its session cookie history. This does not sign out the host browser.


Point your MCP client at `target/release/kwin-mcp`. One server process owns all isolated desktops. Every `session_start` creates a new desktop and returns its ID; this tool does not accept `session_id`. Pass that ID to later calls; omitting it is permitted only when one session exists. `session_list` lists live desktops that may belong to other agents sharing the server. Use only IDs returned by your own `session_start` calls.

Session IDs belong to the server process that returned them. A DSH migration or client/server restart does not transfer sessions; when the old client connection closes, that server shuts down its desktops. An in-place source reload preserves its PID, pipes, and live sessions. The replacement server's `session_list` covers only its own process. The previous server records completed shutdown cleanup in `${XDG_STATE_HOME:-~/.local/state}/kwin-mcp/session-lifecycle.jsonl`; the log is capped at 1 MiB and retains recent records, so older cleanup details may be absent. An unavailable-ID error reports the previous process and cleanup record when available, and reports malformed records as errors. If an ID belongs to another process, retry the request there while it is running; otherwise call `session_start` without `session_id`, relaunch the applications, and restore the task.

Sessions run concurrently. Input requests retain arrival order within a desktop, reads run concurrently, and a shared Tower concurrency limit matches the machine thread count. App launches use a Tower timeout.

The server watches `src/`, `Cargo.toml`, `Cargo.lock`, `build.rs`, and its executable. A source change triggers a rebuild. A rebuilt executable replaces the server in place while preserving its PID, client pipes, and live desktops. The client receives `notifications/tools/list_changed`. Build diagnostics are in `${XDG_CACHE_HOME:-~/.cache}/kwin-mcp/build.log`. Set `KWIN_MCP_REPO` when the source is elsewhere.

## Setup

### Host runtime environment

The server resolves a missing `XDG_RUNTIME_DIR` to the logged-in user's
`/run/user/<effective UID>` directory. It must already exist, belong to that
user, and have mode `0700`. A missing `DBUS_SESSION_BUS_ADDRESS` uses that
directory's existing, user-owned `bus` socket. Explicit invalid overrides fail
with an error naming the variable before a sandbox is created.

These values are passed to the session setup and wallet snapshot lock without
changing the server's global environment. Graphical socket filtering uses the
socket owner's desktop variables, so a launcher with missing variables still
keeps known host graphical sockets and secret runtime entries isolated. Allowed
non-graphical sockets retain their existing same-name replacement behavior.

This requires an existing user login runtime and user bus. The strict launcher
continues to forward inherited desktop values only to its configured MCP server.

### Claude Code registration

Register the server once at **user scope** so it remains available when a session
changes projects or leaves your home directory. Use an absolute path to the
installed `kwin-mcp` binary. Keep `kwin-viewer` beside it when using the viewer:

```bash
claude mcp add --scope user --transport stdio kwin-mcp -- \
  /absolute/path/to/kwin-mcp \
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

### Interrupted desktop control connections

A broken control connection can leave the sandbox launcher running. Tool errors
report `session_channel_failed`, the operation, session ID and affected channel,
and preserve the original transport error. This condition does not establish
an OOM or sandbox exit. Stop that handle, start a new session, and relaunch the
applications. Check the previous application state before repeating input.

### Typing delivery receipts

`keyboard_type` submits paced press/release events and waits for a compositor
acknowledgement. Its `delivery` separates that acknowledgement from application
text verification. When the focused editable field exposes accessible text,
`verified` confirms the complete expected insertion; expected/observed character
counts accompany the receipt. For `atspi_rich_text`, the expected value is
verified in accessible descendants while `observed_field_characters` continues
to report the focused node's text length; an embedded-object placeholder can
therefore report one observed character alongside a verified full value.
Otherwise `unverified` warns that the application text has not been confirmed.
When a timed-out check has seen U+FFFC in the focused text or a descendant, it
reports `unverified` because embedded rich-text content may not be fully
exposed through AT-SPI. Otherwise, a field that still differs after the
bounded wait reports `pending_or_mismatched` as an error. Inspect the field
before sending or retrying: delayed input can otherwise be duplicated.
Passwords, terminals and Tab/Return actions do not expose an application-text
receipt.

Errors after submission retain requested/attempted/submitted progress when
available and explicitly disallow blind retry, including when the session dies
or writing the input summary fails. No partially submitted text is restarted.
