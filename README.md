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
| `accessibility_tree` | Traverse the AT-SPI2 accessibility tree with configurable depth/filters, subtree continuation, and a bounded per-call time budget (5 seconds by default, up to 30 seconds). |
| `find_ui_elements` | Search UI elements by name/role with bounding boxes. |
| `mouse_click` | Click at window-relative coordinates. |
| `mouse_move` | Move pointer to window-relative coordinates. |
| `mouse_scroll` | Scroll at window-relative coordinates. |
| `mouse_drag` | Drag from one window-relative position to another. |
| `keyboard_type` | Type a string of text. |
| `keyboard_key` | Press a key or key combo (e.g. `ctrl+c`, `Return`). |
| `launch_app` | Launch an application and wait for its window. |
| `wait_for_file` | Wait for a session file to appear and remain stable before exporting it. |
| `export_file` | Copy a session file (e.g. a finished download) to a real host path and verify it byte for byte. |

When a producer writes an artifact asynchronously, call `wait_for_file` with
its absolute session path before `export_file`. The file is ready when it is a
regular file whose inode, size, and modification time remain unchanged for
`stable_for_ms` (default 500 ms, range 100–5000), within `timeout_seconds`
(default 30, range 1–300). A timeout distinguishes a missing file from one
that is still changing. This bounded observation does not prove that the
producer has exited.

## Large accessibility trees

`accessibility_tree` has a five-second default budget. Set `timeout_seconds` from 1 to 30 when a large page needs more traversal time. If the result is incomplete, continue from a node marked `root=ID` by passing that ID as `root` and using a smaller `max_depth`. The `root` parameter is part of the MCP tool schema; refresh the client's tool list if it is missing.

## Browser CDP

`launch_app` lists the exact browser commands available on the host. Chromium-family commands receive the session's forwarded CDP port. Google Chrome and Edge need a non-default user data directory; for example:

```text
google-chrome-stable --user-data-dir="$HOME/.cache/kwin-mcp/cdp-profile" https://example.com/
```

The session assigns the CDP port. Do not pass `--remote-debugging-port`; if supplied, `launch_app` replaces it with the session port. A successful connection returns `cdp_port` in the structured result, and that port is forwarded to the host on `127.0.0.1`. Chrome 136 and later ignore remote-debugging switches for the default data directory, so use a private, non-default profile as shown in [Chrome's remote debugging guidance](https://developer.chrome.com/blog/remote-debugging-port).

Google Chrome's branded builds ignore `--load-extension`. When `launch_app` sees that switch with `google-chrome` or `google-chrome-stable`, it sets `extension_warning` in the result and explains the supported path: open `chrome://extensions`, enable Developer mode, choose **Load unpacked**, and select the extension directory in the isolated session. Passing `--disable-features=DisableLoadExtensionCommandLineSwitch` does not restore command-line loading; use unbranded Chromium if command-line loading is required.

## Resize a maximized session window

To resize a maximized window with KWin's native controls, restore it with the session's configured restore/maximize shortcut through `keyboard_key` (the regression proof binds this to `super+Backspace`). Read its `x`, `y`, `w`, and `h` with `window_list`, then hold Super with `keyboard_press` (`key: "super"`) and call `mouse_drag` with `button: "right"` from ten pixels inside the lower-right corner to the desired lower-right point. `mouse_drag` coordinates are relative to the active window. Release Super with `keyboard_release` (`key: "super"`), then verify the geometry with `window_list` or screenshot metadata.

A successful `org.kde.kwin.Script.run` response does not verify the resulting window geometry. Check `window_list` or screenshot metadata before reporting a resize as successful.

`session_start` leaves the host viewer closed. Work without a viewer unless the user must act on the session (for example, Duo, OTP, CAPTCHA, or approval) or asks to watch. Before asking for information, put the relevant form or empty entry on screen, then call `viewer_open` with a short `reason` explaining what the user should do. Never assume the viewer is already open. You can keep your turn open and poll screenshots for completion instead of requiring a chat confirmation. Continue when the screen shows that the step is complete. Viewer input is forwarded immediately. If it arrives during an agent mouse or keyboard operation, the remaining agent operation is cancelled and reports `retry_safe: false` because partial input may already have arrived; inspect the target before retrying. Agent mouse and keyboard calls return a retry-safe error for one second after the latest viewer input; retry after one second without viewer input. The viewer may remain open while agent input resumes, and the user can close it directly. Use `viewer_close` when the user asks. If a Duo push expires, tell the user in one line and leave the page on the resend option so they can retry; continue when the page advances. `session_start` reports the viewer as `closed`; `viewer_open` reports `ready` after the host renderer successfully submits the first PipeWire frame for presentation, or `unavailable` with a reason (for example, no active host Wayland session when serving over SSH). This reports renderer submission, not compositor acknowledgement. The old `--no-viewer` argument remains accepted for existing server commands.

Pass `--autoclean` to remove the server-owned workdir and its disk-backed overlay cache after stop, failed start, timeout, transport close, launching-client death, or SIGTERM/SIGINT/SIGHUP. Cleanup repairs permissions only inside those directories and does not follow symlinks. If either removal fails, `session_stop` reports the failing path and keeps ownership for a retry. Without the flag, the workdir remains after stop.

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

Use `clipboard_write` with `target: "session"` for agent input or `target: "host"` for local viewer paste. A session write fills both CLIPBOARD and PRIMARY, so X11 applications that paste from PRIMARY can consume it. In XTerm, the default fast paste shortcut is Shift+Insert; Ctrl+V is not bound to paste by default. Provide exactly one of `text` or `generate_secret`. For example, `{"target":"host","generate_secret":{"length":20,"character_classes":["lowercase","uppercase","digits","symbols"]}}` generates a value using operating-system randomness and includes every selected class. The response contains only its target and character length.

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
Explicit copies share `startup_timeout_seconds` (20 seconds by default); this option does not
make existing read-only host mounts writable or change a running session.

### Live application state (Codex / ChatGPT desktop)

Codex needs to write its SQLite state, logs and thread history. These can exceed
the 64 MiB copy limit, and databases open in WAL mode on the host are normally
read-only in a session regardless of size. A private `writable_paths` copy also
cannot deliver a message to the real host thread.

To run against live host state, explicitly share the application state directory
when starting a **new** session. For example, with HOME `/home/nate`:

```json
{"host_write_paths":["/home/nate/.codex"]}
```

Then use `launch_app` with `chatgpt`. Use the actual state directory if Codex has
a custom `CODEX_HOME`. Each selected directory is bound directly into the session;
its databases, WAL, shared memory and newly created sidecars use the host inodes
and SQLite locks. Startup does not copy this subtree or impose the staging size
limit. Writes, messages and deletions in these directories persist on the host,
including after `session_stop`. Do not use this option for disposable work.

`host_write_paths` accepts existing directories strictly beneath HOME. It rejects
symlinked paths, parent traversal and HOME itself. Selections cannot overlap
browser profile directories, Chrome's cache, saved browser-cookie state, session
storage or the host runtime directory, including their ancestors and descendants.
They also cannot overlap private `writable_paths`. Existing host Unix sockets
within shared directories remain blocked. Other HOME paths retain their private
overlays. `session_start` and `session_list` report the selected directories; the
setting survives server reload.
Omitting the option retains the default isolation.

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
Input and screenshot calls sample the session cgroup before and after the
operation. When RAM reaches `memory.high`, the cgroup's `memory.events high`
counter increases during the call, or `memory.pressure full avg10` reaches 10%,
the result includes warning text and a `memory_warning` field when structured
content is available, with the RAM, swap, and pressure values available.
Screenshot timeout errors name active memory pressure as a likely cause.
When the sandbox exits, the server tears down its resources and keeps a dead
entry until `session_stop`. Later calls return the exit cause and recovery
guidance. An OOM diagnosis includes the scope result, RAM/swap limits and
systemd's memory peak when available; a signal exit alone is never treated
as proof of an OOM kill. After upgrading an older server, scope identity and
limits are recovered from the sandbox PID cgroup; values that cannot be
recovered remain unknown. Stop the dead handle, start a new desktop without
`session_id`, and relaunch the applications to continue.

Every handle `session_list` shows is accepted by `session_stop`: a running desktop
is stopped, and a dead, expired, or blocked-start handle is cleared with its
lifecycle record. A `session_start` whose host call stays blocked past its budget
is listed as `start_blocked`; `session_stop` answers `cleanup_pending` until that
call returns, then clears the handle. A stop of an ID whose record is already
cleared is a confirmed no-op.

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
requests approximately twice that rate. A 900-character comment can therefore
need roughly two minutes at the default rate. For a longer comment, pass a
larger multiplier such as `keyboard_type({"text":"...","speed":4})`. On engi, a real
900-character Chrome textarea probe completed in 28.63 seconds at that rate
and independently copied back the exact text. Inspect the delivery result and
the field before submitting. Run the synthetic probe with
`ruby tests/input_delivery_e2e.rb long_comment` using the server/artifact
variables documented in [the input-delivery tests](tests/input_delivery_e2e.md).
Mouse tools also accept `target_width`,

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


Point your MCP client at `target/release/kwin-mcp`. One server process owns its isolated desktops. Every `session_start` creates a new desktop and returns its ID; this tool does not accept `session_id`. Use only IDs returned by your own `session_start` calls. Pass that ID to later calls; omitting it is permitted only when one session exists. `session_list` lists live desktops owned by that server. Requests carrying a live session ID owned by another kwin-mcp process running as the same user on the same host are forwarded to that process through a private Unix socket.

Session IDs belong to the server process that returned them. A DSH migration or client/server restart does not transfer sessions; when the old client connection closes, that server shuts down its desktops. An in-place source reload preserves its PID, pipes, and live sessions. Calls with a session ID from another live same-user server on this host are automatically forwarded after checking the target PID and effective UID. The replacement server's `session_list` covers only its own process. Session stops, idle TTL expirations, and completed server-shutdown cleanup are recorded in `${XDG_STATE_HOME:-~/.local/state}/kwin-mcp/session-lifecycle-v2.jsonl`; the log is capped at 1 MiB and retains recent records, so older details may be absent. An unavailable-ID error includes `session_status`: `stopped` or `expired` with the recorded end time and reason when known, `owned_by_another_process` when the ID belongs elsewhere but its final state is unknown, or `not_found`. A request without an ID when no session is active reports `none_running`. These responses also report whether the ID is available in the current process and provide recovery guidance. TTL-expired handles appear as `expired` in the current process's `session_list` and can be cleared with `session_stop`. If the owning process is unavailable, call `session_start` without `session_id`, relaunch the applications, and restore the task.

Completed server-shutdown records are also mirrored to the legacy
`session-lifecycle.jsonl` format for older servers. Explicit stops and expiry
records stay in the versioned file. When a mixed legacy log is found, the server
moves its newer event kinds under a file lock and keeps a bounded recovery copy
as `session-lifecycle-v1-migration.jsonl`. Each history file remains capped at
1 MiB. Unknown future terminal kinds in the versioned file report an unknown
status without falsely claiming a stop or invalidating unrelated history.

Sessions run concurrently. Input requests retain arrival order within a desktop, reads run concurrently, and a shared Tower concurrency limit matches the machine thread count. App launches use a Tower timeout.

The server watches `src/`, `Cargo.toml`, `Cargo.lock`, `build.rs`, and its executable. A source change triggers a rebuild. A rebuilt executable replaces the server in place while preserving its PID, client pipes, and live desktops. The client receives `notifications/tools/list_changed`. Build diagnostics are in `${XDG_CACHE_HOME:-~/.cache}/kwin-mcp/build.log`. Set `KWIN_MCP_REPO` when the source is elsewhere.

## Setup

### Install the Rust executables

Build this checkout, then install its server and viewer together:

```bash
cargo build --release --bins
bash scripts/install.sh
hash -r
kwin-mcp --version
kwin-mcp --help
kwin-mcp --describe
```

The installer requires Bash, GNU coreutils, and `jq`. It defaults to `~/.local/bin`
and checks that the source server prints help, valid MCP schemas, and identical
`--version` / `-V` identities before installing either executable. Both version
flags must print one line with the Rust package version, build number, and Git
commit, such as `kwin-mcp v0.1.2.651 (a34524a)`. Installation reports this identity
along with the selected PATH executable. Use `--bin-dir DIR` for a different
destination, or `--source-dir DIR` for a different build output directory.
Put the destination first in `PATH`. If another directory still supplies the
`kwin-mcp` command, installation reports an actionable error after copying the
executables; use their absolute paths or fix `PATH` and clear the shell command cache.

An existing destination requires explicit replacement:

```bash
type -a kwin-mcp
bash scripts/install.sh --replace
hash -r
kwin-mcp --version
kwin-mcp --help
kwin-mcp --describe
```

`--replace` replaces the destination file or symlink itself. It preserves the
target of an old symlink, including a uv-managed console script. The separately
published Python/PyPI package also named `kwin-mcp` belongs to
[isac322/kwin-mcp](https://github.com/isac322/kwin-mcp); `uv tool install kwin-mcp`
does not install this Rust server. If `--version`, `--help`, or `--describe`
succeeds with empty output, inspect `type -a kwin-mcp` for that conflicting
command and install the Rust executables with the steps above. If you no longer need the Python
tool, run `uv tool uninstall kwin-mcp` before installing the Rust executables.
To keep both projects, use separate installation directories and absolute
commands: uv can recreate or remove its console entry point during later package
operations. Configure MCP clients with the absolute installed Rust path, such
as `~/.local/bin/kwin-mcp` expanded to your home directory.

MCP clients may still name an older absolute build-artifact path even after PATH
is corrected. Check the configured command as well as `kwin-mcp --version`.
For an existing provider, verify an old-to-new in-place reload before replacing
its executable; the production regression accepts
`KWIN_MCP_E2E_REEXEC_REPLACEMENT=/path/to/new/kwin-mcp` to check two live desktops,
PID, pipes, input, and partially buffered requests. Keep the server and viewer
from the same installation. Agent input is refused while an open viewer's status
is stale; finish any current user interaction, then use `viewer_open` to refresh
it. Read-only inspection remains available during that refresh step.

To check installation behavior against built artifacts without changing your
real installation, run `bash tests/install_cli_e2e.sh target/release /tmp/kwin-install-evidence`.
It installs into temporary directories and exercises help, schemas, and both
version flags through `PATH`, including rejection of silent version entrypoints.

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

Inline PNGs request original-detail presentation on supporting Codex clients
through the image block's `_meta["codex/imageDetail"]`. The hint does not change
the image bytes, capture dimensions, or click coordinates. Other clients may
ignore it. If a preview appears blank or loses small text, inspect the saved PNG
at original detail or request a narrow `region` crop before diagnosing a
compositor failure. An inline image and its saved PNG can contain identical,
correct pixels even when a client presents them differently (see #435).


Virtual display is 2000×1875 (3.75MP). By default all windows open maximized. `session_start` with `window_mode: "native"` maps each window at the size its app requests instead, so a relaunched app reopens at its own size. Windows have no decorations or shadows in either mode. Font hinting disabled, grayscale antialiasing, 96 DPI, scale 1.0.

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

After an editable field is cleared, AT-SPI may expose one blank newline that the
application removes when typing starts. If the entire resulting field matches
the requested text, `atspi_transformed_blank_line` reports `unverified` with
inspection guidance. It retains the original expected and observed counts;
preservation of that initial newline is ambiguous. A newline retained by the
application still follows the normal exact-text verification path.

Errors after submission retain requested/attempted/submitted progress when
available and explicitly disallow blind retry, including when the session dies
or writing the input summary fails. No partially submitted text is restarted.

The [input delivery regression](tests/input_delivery_e2e.md) covers clearing and
retyping the same text followed immediately by repeated screenshots.

The complete verification phase after compositor acknowledgement has a
12-second deadline, including any fallback focus observation. Expiry returns
`unverified` with method `atspi_verification_timeout`; it does not submit the
text again. `timings_ms` separates key submission, compositor acknowledgement,
and field verification. This is a verification deadline, not a deadline for
the whole typing operation: long paced input can take longer. Tab/newline
input remains explicitly unverified because those keys can change focus or
submit a field.

Chrome's address bar can return an empty AT-SPI readback despite accepting
the URL. When an editable control belongs to browser chrome outside a web
document and its nonempty expected value remains unobservable as empty,
`atspi_browser_toolbar` reports `unverified` without treating the observation
limitation as a confirmed input failure. A readable full URL still verifies
normally. An empty web-page field remains a mismatch. Inspect the address bar
before pressing Enter or retrying.

#### Interpreting character-count mismatches

For `atspi_focused_text`, `verification_context` identifies the observed count
as the raw Unicode scalar count returned by the focused AT-SPI text object.
Comparison removes embedded-object markers (`U+FFFC`); the reported observed
count is taken before that normalization. The verifier does not apply Gmail
search-chip rules or remove ordinary punctuation.

A count difference does not identify which characters changed or why. Inspect
the field and, where supported, use native copy to compare its exact value
before retrying. Accessibility can expose an incomplete or transformed
representation; its count does not independently establish the application's
submitted value.

For [#528](https://github.com/nm-z/kwin-mcp/issues/528), the retained journal
records the exact 75-character query, including flushed press/release events
for both hyphens and the underscore. The original observed field value was
unavailable in the evidence inspected. That evidence does not establish Gmail
chip normalization, punctuation loss, or the cause of the discrepancy. The
native regression verifies all 75 characters in an unrestricted search field
and deliberately limits another to 72: that mismatch removes the final `:1d`,
showing why the count alone cannot identify the missing characters.

### Startup budgets and fresh browser profiles

`session_start` accepts `startup_timeout_seconds` from 1 to 300 (default 20).
The budget includes waiting for the lifecycle gate and preparing host profiles;
it does not disable bounded startup or cleanup. Increase it explicitly when a
large profile set needs more preparation time.

For a terminal-only task or a browser that should start without inherited
Chromium profiles, use `browser_profiles: "fresh"`. The recognized Chromium
configuration directories start empty in that session, profile/cookie copying
is skipped, and cookies from those fresh profiles are not persisted between
sessions. Host files are unchanged. The default `"inherit"` preserves the
existing profile and cookie-continuity behavior. The response reports both the
profile mode and startup budget. This option does not bypass general HOME
mount/process discovery or unblock unrelated hung filesystems.

Browser preparation timeouts identify the current operation and pathname,
without printing file contents. Preparation checks its deadline between
operations. If an operation is blocked in the kernel, cleanup retains ownership
until that operation returns; it then stops preparation and removes the owned
storage. Do not remove that storage manually while the operation still uses it.

When sandbox startup fails, the response preserves available sandbox stderr
before autoclean removes its log. Long logs include only a bounded tail and a
truncation flag. The D-Bus readiness marker is published only after its socket
exists; a sandbox that exits before a startup signal reports its exit status.
Run `ruby tests/startup_diagnostics_e2e.rb SERVER EVIDENCE` to exercise real
D-Bus failures, noisy diagnostics, normal KWin startup, and cleanup.

Run `ruby tests/startup_profiles_e2e.rb SERVER EVIDENCE` for the real-stdio
regression. It uses synthetic private profiles, a controlled blocked Preferences
read, terminal launch, and a delayed real sandbox to verify profile isolation,
operation diagnostics, deferred cleanup, and configurable timeout bounds.

## Release packaging and deployment

The shipped `kwin-mcp-deploy` CLI builds committed source on the least busy eligible
sentry/archy host, stages checksummed release packages, and explicitly installs
new release directories on engi, archy or benji. Engi builds require an explicit
host override and always use nice 19; selected engi builds and installs run
locally when invoked on engi, without an SSH listener. Unreachable benji is skipped. No command
activates a release or restarts services. Packages include optional OAuth gateway
and Cloudflare tunnel user unit templates; the gateway executable and credentials
are external prerequisites.

See [deployment instructions](docs/deployment.md) for commands, private build and
install paths, runtime compatibility requirements and unit configuration.
