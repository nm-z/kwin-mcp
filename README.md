# kwin-mcp

MCP server for KWin Wayland GUI automation. Single-binary Rust using `rmcp` + `reis` (EIS input) + `atspi` (accessibility tree) + `zbus` (D-Bus/KWin IPC) + `evdev` (uinput virtual devices). Container isolation via bubblewrap and pasta.

The optional [KWin MCP skill](skills/kwin-mcp/SKILL.md) helps Codex choose between this server's isolated desktop and the user's current desktop. It requires a separately configured KWin MCP server.

## Tools

| Tool | Description |
|---|---|
| `session_start` | Start an isolated KDE Wayland session without opening a host viewer. Must be called first. Returns `host_shared_dir`, the one `/tmp` directory shared with the host. |
| `session_stop` | Tear down the session and all container processes. |
| `viewer_open` | Open the live host viewer for the current session (reports ready, starting, or unavailable with the reason). |
| `viewer_close` | Close the host viewer without stopping the isolated session. |
| `screenshot` | Capture the active window as PNG. |
| `window_list` | List all isolated-session windows, including hidden prompts and modal relationships. `showing_desktop: true` (with `hidden: true` on each window) means KDE show desktop (`super+d`) hides them without minimizing them; `screenshot` and `keyboard_type` warn about the same state. |
| `window_activate` | Reveal and focus a window by the ID returned from `window_list`. |
| `accessibility_tree` | Traverse the AT-SPI2 accessibility tree with configurable depth/filters, subtree continuation, and a bounded per-call time budget (5 seconds by default, up to 30 seconds). |
| `find_ui_elements` | Search UI elements by name/role with bounding boxes. |
| `mouse_click` | Click at window-relative coordinates. |
| `browser_navigate` | Open a URL in the focused Chromium-family window: ctrl+l, type it, Delete (drops the address bar's inline completion), Return. Returns the new window title, and the page `url` when CDP is connected (`matched: false` plus a warning if it is not the address typed). |
| `mouse_click_element` | Find an element by name/role at click time (as `find_ui_elements`) and click the center of its bounds; `index` picks among several. For pages whose layout shifts between steps. |
| `mouse_move` | Move pointer to window-relative coordinates. |
| `mouse_scroll` | Scroll at window-relative coordinates. Reports `view_changed` (false at the end of the content), and with CDP `scroll_y`, `scroll_max_y`, `at_end`. |
| `mouse_drag` | Drag from one window-relative position to another. |
| `keyboard_type` | Type a string of text. The result names the window that received the keys (`window`: id, title, `resourceClass`, pid, `hidden`), so text sent to a terminal running a full-screen program or to a hidden window can be told from lost input. |
| `keyboard_paste` | Paste UTF-8 text through the session clipboard with an acknowledged shortcut. |
| `keyboard_key` | Press a key or key combo (e.g. `ctrl+c`, `Return`). |
| `keyboard_press` / `keyboard_release` | Hold a key or combo down across calls, then release it with the same string. For a shortcut use `keyboard_key`: a modifier left held makes every typed character a shortcut (`h` becomes Ctrl+H), so `keyboard_type` refuses with `modifier_held` until it is released. |
| `launch_app` | Launch an application and wait for its window, or with `expect_window: false` return its session PID once the command is submitted. |
| `wait_for_file` | Wait for a session file to appear and remain stable before exporting it. |
| `export_file` | Copy a session file (e.g. a finished download) to a real host path and verify it byte for byte. |
| `import_file` | Refresh a running session's file from the host, with byte verification and explicit overwrite control. |

When a producer writes an artifact asynchronously, call `wait_for_file` with
its absolute session path before `export_file`. The file is ready when it is a
regular file whose inode, size, and modification time remain unchanged for
`stable_for_ms` (default 500 ms, range 100–5000), within `timeout_seconds`
(default 30, range 1–300). A timeout distinguishes a missing file from one
that is still changing. This bounded observation does not prove that the
producer has exited.

## Uploading a host-created file

The browser and its file chooser read the **isolated session's filesystem**.
The same absolute path on the host can contain different bytes: HOME uses an
overlay, files can have startup-time copies or read-only mounts, and the
session has a private `/tmp`. A chooser accepting a path does not establish
that the selected file matches the host attachment.

Before selecting an attachment, call `wait_for_file` with its absolute
`session_path` and compare the returned `size_bytes` with the known host file
size. `status: "ready"` means stable, and can include a zero-byte file; it does
not compare the host and session contents. If exact content identity matters,
also compare the file hashes using the session application or terminal and
the host source.

If the session copy is missing, empty, or different, transfer the intended
bytes into a file in the current session before selecting it. Alternatively,
start a separate session after the host file exists, with that absolute file
under HOME in `writable_paths` to request a private writable copy. For example:

```json
{"writable_paths":["/home/nate/Uploads/proof.png"]}
```

Keep the current session alive while preserving any work it contains. Verify
the new session's file size before upload. Host `/tmp/proof.png` is not the
session's `/tmp/proof.png`; copying into host `/tmp` alone is not a transfer.
`export_file` moves a session result to the host and does not import an
attachment.

For Chrome's chooser, click the file input, press Ctrl+L, enter the verified
absolute session path, and use Alt+O. Then verify the application's selected
filename and byte size or upload outcome before submitting the form.

## Transient-window screenshots

For an active dialog, modal, or transient window, `screenshot` compares the
composited image with a separate KWin `CaptureWindow` reference before returning
an image. The comparison uses the opaque client interior, excluding a 97-pixel
square around each sampled cursor position and an eight-pixel border. Success includes `transient_pixel_check` with the
reference window ID and pixel counts. Qt's native-word and byte-ordered 32-bit
capture formats are normalized before comparison and PNG encoding.

If the dialog disappears, its identity or geometry changes, its pixels differ,
or a sufficient opaque interior cannot be compared, the tool returns
`transient_capture_unverified` without an image. Wait at least the reported
`retry_after_ms` and observe again. `settle.settled` alone only describes an
unchanging sequence of frames; it cannot prove that a dialog was drawn.

The returned image still comes from the composited screen so it can include
popups and regions extending outside the client window. A changing or
translucent dialog, or a popup covering its interior, can therefore require a
retry. The reference comparison does not prove that application content is
semantically up to date.

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

Maximization is applied only when a window first appears; it does not force the window to remain at the display size. To restore it through KWin's window menu, send `keyboard_key` with `key: "alt+F3"`, inspect the menu, and click its checked **Maximize** item. Wait for `window_list` to report the restored geometry: the menu action and application resize complete asynchronously. The session's configured restore/maximize shortcut is another option. Read `x`, `y`, `w`, and `h` with `window_list`, then hold Super with `keyboard_press` (`key: "super"`) and call `mouse_drag` with `button: "right"` from ten pixels inside the lower-right corner to the desired lower-right point. `mouse_drag` coordinates are relative to the active window. Release Super with `keyboard_release` (`key: "super"`), then verify the geometry with `window_list` or screenshot metadata.

Session windows have no title bar or border (`window_mode` removes decorations in both modes), so a drag from a window edge has nothing to grab. In observed runs on a native-mode window, Super+right-button drag (above) resized it, while a drag from the corner pixel, Alt+right-button drag and Alt+F8 with arrow keys did not. `screenshot` returns "target changed or moved during capture" while the active window's geometry is still changing; capture again once it has stopped.

KWin's own menus have no accessibility tree, but each menu and submenu is a `popup: true` window in `window_list`, so a call after a hover or click shows whether the submenu opened. In observed runs hovering the arrow end of a submenu row opened it, while clicking the row's text did not always.

A successful `org.kde.kwin.Script.run` response does not verify the resulting window geometry. Check `window_list` or screenshot metadata before reporting a resize as successful.

`session_start` leaves the host viewer closed. Work without a viewer unless the user must act on the session (for example, Duo, OTP, CAPTCHA, or approval) or asks to watch. Before asking for information, put the relevant form or empty entry on screen, then call `viewer_open` with a short `reason` explaining what the user should do. Never assume the viewer is already open. You can keep your turn open and poll screenshots for completion instead of requiring a chat confirmation. Continue when the screen shows that the step is complete. Viewer input is forwarded immediately. If it arrives during an agent mouse or keyboard operation, the remaining agent operation is cancelled and reports `retry_safe: false` because partial input may already have arrived; inspect the target before retrying. Agent mouse and keyboard calls return a retry-safe error for one second after the latest viewer input; retry after one second without viewer input. The viewer may remain open while agent input resumes, and the user can close it directly. Use `viewer_close` when the user asks. If a Duo push expires, tell the user in one line and leave the page on the resend option so they can retry; continue when the page advances. `session_start` reports the viewer as `closed`; `viewer_open` reports `ready` after the host renderer successfully submits the first PipeWire frame for presentation, or `unavailable` with a reason (for example, no active host Wayland session when serving over SSH). This reports renderer submission, not compositor acknowledgement. The old `--no-viewer` argument remains accepted for existing server commands.

**Starting kwin-mcp from a sandboxed client.** A client that runs inside its own sandbox (for example a Codex worker under `bwrap`) starts the server with `NoNewPrivs=1`, and then `session_start` fails with `bwrap: setting up uid map: Operation not permitted`. `NoNewPrivs` alone causes it: `pasta` runs the sandbox command with only the few capabilities it kept (`CAP_NET_ADMIN`, `CAP_SYS_ADMIN`, `CAP_NET_BIND_SERVICE`), and under `NoNewPrivs` an `exec` cannot regain `CAP_SETUID`/`CAP_SETFCAP`, which `bwrap` needs to map ids (`setpriv --no-new-privs pasta --config-net -- bwrap --unshare-user --ro-bind / / true` fails the same way on a plain host, and a capability added to the sandbox does not help). Start the server from the user manager instead: set the client's server command to `systemd-run --user --quiet --wait --pipe --collect /path/to/kwin-mcp --width 1920 --height 1080 …` (`--pipe` keeps the stdio transport; the unit does not inherit the sandbox's `NoNewPrivs`). The error reports the `NoNewPrivs` value it saw.

**Viewer on a host with no desktop.** `viewer_open` shows the session on the screen of the host the server runs on. When that host has no desktop session (a headless build or GPU box, or a worker that cannot reach one), it reports `unavailable` with the reason and a ready command: `kwin-viewer --remote HOST /tmp/kwin-mcp-<pid>-<n> WIDTH HEIGHT`. Run that command on the machine whose screen should show the session, inside its logged-in desktop session (from an SSH shell there it finds the user's `wayland-N` socket in `XDG_RUNTIME_DIR` or `/run/user/<uid>` by itself; with no live socket it says so). That machine must reach HOST over SSH without a prompt (`BatchMode=yes`, so key or agent authentication; a password prompt makes it fail immediately), and `kwin-viewer` must exist at the same path on HOST (set `KWIN_VIEWER_REMOTE_BIN` otherwise). The viewer shows the session and forwards input to it; closing it or stopping the session ends it. The server reaches the desktop even when logind lists no active Wayland session, as long as a live `wayland-N` socket is in the runtime directory.

Pass `--autoclean` to remove the server-owned workdir and its disk-backed overlay cache after stop, failed start, timeout, transport close, launching-client death, or SIGTERM/SIGINT/SIGHUP. Cleanup repairs permissions only inside those directories and does not follow symlinks. If either removal fails, `session_stop` reports the failing path and keeps ownership for a retry. Without the flag, the workdir remains after stop. Because `host_shared_dir` is that workdir, files host tools wrote there are deleted with it; `session_start` reports `workdir_removed_on_stop: true` (and `ttl_minutes`) when this server runs with `--autoclean` or `--ttl`, so copy outputs elsewhere before `session_stop`.

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

KWin MCP operates its own isolated desktop session. The viewer displays and
controls that session; it does not turn the tools into controls for the user's
active desktop session. Both clipboard tools require a live KWin MCP session,
including `clipboard_write` with `target: "host"`.

| Operation | Clipboard and scope |
|---|---|
| `clipboard_write`, `target: "session"` | The selected isolated session's CLIPBOARD and PRIMARY, used for agent input. |
| `clipboard_write`, `target: "host"` | The Wayland clipboard on the desktop where the server runs, used by the local viewer's copy/paste bridge. |
| `clipboard_read` | The selected isolated session's text selection only. |
| Local viewer Ctrl+C / Ctrl+X | Copies or cuts from the isolated application to the server host clipboard. |
| Local viewer Ctrl+V / Shift+Insert | Pastes the server host clipboard into the isolated application. |

`host` does not mean a remote viewer client's desktop or whichever active
desktop a user names. A request to change the user's active desktop clipboard
outside this live session/viewer workflow should use the appropriate separate
desktop mechanism; starting a KWin MCP session does not change that request's
destination.

The local viewer restores the isolated session's prior selection after a clipboard handoff.

A session write fills both CLIPBOARD and PRIMARY, so X11 applications that paste from PRIMARY can consume it. In XTerm, the default fast paste shortcut is Shift+Insert; Ctrl+V is not bound to paste by default. Provide exactly one of `text` or `generate_secret`. For example, `{"target":"host","generate_secret":{"length":20,"character_classes":["lowercase","uppercase","digits","symbols"]}}` generates a value using operating-system randomness and includes every selected class. The response contains only its target and character length.

`clipboard_read` reads session text only. Generated selections carry a secret marker, so this tool returns `text: null`, `protected: true`, and the length while that marker is present. This prevents an accidental tool readback; it does not restrict applications that can read the clipboard. Clipboard ownership survives server reload and ends when the server exits. Session clipboard ownership also ends when its session stops. The host target means the server's desktop, not a remote viewer client's desktop.

Agent `keyboard_key` calls for Ctrl+V or Shift+Insert now return a `paste` receipt.
`shortcut_acknowledged` records the compositor's acknowledgment of the native
shortcut; `editable_value` separately reports whether the focused field contains
the expected insertion. Multiline UTF-8 clipboard text can be verified without
converting it to generated keystrokes. A verified value is still followed by
`form_submission: "not_observed"`: an application-managed editor may maintain
additional form state, so verify the application's submission result separately.

If the field is inaccessible, the text exceeds the one MiB verification limit,
or the bounded readback cannot establish the result, the receipt is unverified.
A readable mismatch is an error. Both require inspection before another paste;
blind retries can duplicate text. Verification has a 12-second outer deadline
after compositor acknowledgment. Generated-secret selections are pasted natively
but are not read back; their receipt contains no expected or observed field value
or count. These receipts apply to the session clipboard used by agent shortcuts,
not the viewer's host-clipboard paste path.

## KWallet safety

A session never talks to the host wallet or Secret Service. `session_start` takes one read-only copy of the host KWallet, and the session's apps then use a session-local `org.kde.kwalletd6` served from memory; their writes stay in the session and end with it. The copy is taken only when `kwalletd6` and `ksecretd` already run and the wallet is unlocked: kwin-mcp never starts, unlocks or prompts a host wallet service, never calls `org.freedesktop.secrets`, and serializes copies across servers, at least 100 ms apart, within 12 s per `session_start`. If `kwalletd6` is missing or does not answer, the same wallet is read from `ksecretd` with the standard Secret Service calls, and copies in the next 60 s read `ksecretd` directly. If any check fails the session gets a disabled KWallet, `session_start` reports why, and `launch_app` of a Chromium-family browser says that the browser cannot decrypt the copied profile: Chrome drops every cookie it cannot decrypt (a recovered 3,606-cookie profile fell to 8 rows with the `basic` store, and kept 3,311 with the wallet). The host session D-Bus socket and the `keyring` and `p11-kit` entries of the user runtime directory are never reachable from a session.

## Session Architecture

The isolated system bus does not provide PolicyKit authentication. When an
application starts `pkttyagent`, its known missing-service registration diagnostic
is saved to `polkit-agent.log` in the session `workdir` instead of being printed
over the application's terminal UI. The agent still returns its original failure
status. Other errors remain on stderr, and the original diagnostic also remains
on stderr if the session log cannot be written. This does not grant access to the
host PolicyKit service. The wrapper closes inherited descriptors in its stderr
filter so `--notify-fd` readiness cannot be held open by the filter process.

### Host file changes and refreshing a running session

Treat `session_start` as the boundary for the initial HOME view. HOME uses a
private writable overlay over host files, not a continuously synchronized copy.
Some untouched host files can appear live, but session copies, deletions
(whiteouts), split-mount staging copies, and application caches can hide later
host additions or edits. Changes to the host lower layer also do not reliably
notify applications watching the overlay. A path appearing in Obsidian's Quick
Switcher does not establish that its editor or calendar cache has current bytes.

After creating or editing a host note, explicitly import it into the existing
desktop. For example, call `import_file` with:

```json
{"session_id":"<your session ID>","host_path":"/home/<user>/vault/calendar/event.md","overwrite":true}
```

`session_path` defaults to `host_path`; set it to a different absolute file path
when needed (including a path in the session's private `/tmp`). Its parent
directory must already exist and be writable inside the session. The tool copies
one regular file, verifies its bytes, then atomically replaces the session path
through the mounted filesystem. It does not modify the overlay backing layers
directly. It refuses an existing session file unless `overwrite` is true; that
option deliberately discards the session's private version of this file.
Imported files have private `0600` permissions. Read-only session mounts remain
read-only. Outside explicitly shared `host_write_paths`, imports into HOME stay
private to the session and host originals are unchanged. Imports into a shared
directory write directly to live host state.

The result confirms filesystem bytes, not application state. Save or discard
unsaved editor changes before importing, then reopen/reload the note in
Obsidian and reset the calendar's event cache if necessary. If an application
still holds stale data, restart that application inside the same desktop.
Stopping and starting the session remains an option when a fresh HOME view is
preferred over preserving session edits. Use `export_file` to save any session
files that must survive that restart.

### Reusing an explicitly authorized existing desktop

`session_start` always creates a new isolated desktop. To reach an application
already open in another live same-user kwin-mcp session, first obtain the user's
authorization for that specific session, then call `session_attach` with its
exact `session_id`, the `owner_pid` encoded in that handle,
`user_authorized: true`, and an `application` description of the authorization.
Discovering another session does not authorize operating it.

Attachment verifies the owner's Unix UID, process start time, authenticated
control socket and running-session record. It returns the existing sandbox PID,
work directory and scope, creates no desktop, and copies no credentials or browser
profiles. Subsequent tools use that same `session_id`; the attaching process pins
the owner's process identity and rejects a changed or unreachable owner before
forwarding. Reattach after restarting or reloading the attaching server.

The original server retains lifecycle ownership. Closing the attaching server
does not stop the existing desktop. `session_list` continues to list this server's
owned desktops; the attachment response identifies the selected foreign desktop.
Only stop an attached session when the user requests it. An exited owner or a
stopped session cannot be revived by attachment.

Nested mounts under HOME require separate overlays. Regular files directly in
their ancestor directories are copied into the private session. For files larger
than 64 MiB, kwin-mcp first tries a copy-on-write reflink. If the filesystem cannot
clone the file, it uses a read-only bind instead of copying its data during
startup. Other files and directory overlays keep their existing write behavior.
`session_start` reports these fallback files in `oversized_read_only_files`.
To require private writable copies for specific files or directories, pass their
absolute paths under HOME in `writable_paths` when starting a new session.
Explicit copies share `startup_timeout_seconds` (60 seconds by default); this option does not
make existing read-only host mounts writable or change a running session.

### Live application state (Codex / ChatGPT desktop)

Codex needs to write its SQLite state, logs and thread history. These can exceed
the 64 MiB copy limit, and databases open in WAL mode on the host are normally
read-only in a session regardless of size. For a disposable Codex session, request
a private writable snapshot of the state directory at startup. For HOME
`/home/nate`, use:

```json
{"writable_paths":["/home/nate/.codex"]}
```

Live SQLite databases in that selection are copied from consistent committed
database/WAL observations without opening the host shared-memory file for writing.
The private databases can be reopened by later application processes, and new
databases, WAL files and shared-memory files stay in the session. These changes
are discarded when the session stops; they cannot deliver a message to the real
host thread. Each initial database plus WAL must fit the 64 MiB snapshot limit.
An inconsistent, oversized or otherwise failed explicit snapshot fails startup
with its preparation error; it does not silently substitute a read-only database.

`session_start` lists the live databases it discovers. When every one is
`live_read_only` the result has one list of paths, `live_ro_dbs` (at most 128
entries; `live_ro_dbs_total` appears when there are more). When any database has
another mode (`private_writable_snapshot`, `shared_host`) or a note, the result
has `sqlite_access` instead, with a `mode` per database and `total_databases`.
Empty results (`cookie_errors`, `oversized_read_only_files`, `host_write_paths`,
`runtime_root_fallback`) are left out. A `launch_app` result for a window it found
carries the window, its title and how it was found (`window_outcome`); `cdp_port`,
`warning`, `extension_warning` and the window-baseline fields appear only when they
have something to report. If an
application reports SQLite error 14 or cannot open its state, check this policy
and the startup error before concluding that its host database is damaged.

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

Session storage: sockets and small files stay in `/tmp/kwin-mcp-<pid>-<number>` (RAM; see [Host visibility](#host-visibility) for what the host can reach). The `$HOME` overlay upper, work and staging layers are on disk in `${XDG_CACHE_HOME:-~/.cache}/kwin-mcp/kwin-mcp-<pid>-<number>`, which the session itself sees as an empty directory. `session_start` refuses to start with less than 2 GiB free there (pass `disk_min_free_mib` to accept less headroom; session HOME writes fail once the disk fills), and `session_stop` (or the leaked-workdir sweep) deletes it. With `--autoclean`, failed startup also removes the disk overlay before removing the temporary workdir. If either removal fails, cleanup keeps ownership for a `session_stop` retry and reports the failing path.

`TMPDIR` overrides the runtime storage root. Linux pathname Unix sockets allow
at most 107 bytes, excluding the terminating NUL, for the entire socket path.
The session's D-Bus sockets have a stricter 99-byte limit imposed by
[libdbus](https://dbus.freedesktop.org/doc/api/html/group__DBusSysdeps.html).
The limit includes the generated `kwin-mcp-<pid>-<number>` directory and socket
filename, and counts UTF-8 bytes rather than characters. When the session's
longest socket path under `TMPDIR` would exceed 99 bytes (for example a GPU
queue or build staging `TMPDIR`), `session_start` puts the workdir under `/tmp`
instead and reports `runtime_root_fallback` with the original `TMPDIR`; the
returned `workdir` and `host_shared_dir` are the paths to use. `session_start`
still checks both constraints before creating its workdir or starting
processes, and names any path that does not fit with its byte length.

Temporary KWin scripts also live in that disk directory, exposed to the sandbox
through a read-only `kwin-scripts` mount in its runtime directory. A full `/tmp`
therefore does not prevent `window_list` or screenshot geometry queries in new
sessions. If saving `screenshot.png` fails, the default call reports a
`write_failed` storage error; retry with `inline=true` to receive the pixels
with `path: null` and `write_error`. Freeing storage allows later screenshots
to save normally. A session restored from an older server keeps its original
script storage until it is recreated.

By default, live host SQLite databases with open WAL files are mounted read-only
with their WAL and shared-memory files, except browser cookie jars. Explicit
`writable_paths` selections instead receive private snapshots as described above;
`host_write_paths` selections use the shared host directory. Startup mounts
default read-only databases without copying their contents, and reads use the host inodes and
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

Screenshots check the active window identity and geometry before and after the frame is captured. A changed target returns `capture_target_changed` with `expected_window` and `observed_window`; if the target cannot be read afterward, the reason is `capture_target_unavailable`. Neither failure returns or saves an image. Inspect `window_list`, activate the intended window, and capture again. Successful metadata records `target_verification.method=active_window_before_and_after` and the final observation. These two observations detect a different or moved target at the end of capture; they do not establish that focus never briefly changed and returned between observations.


### Host socket exposure

At `session_start`, active pathname sockets beneath `$HOME` and non-graphical user-runtime sockets are exposed automatically. Sockets owned by processes attached to the host display, desktop application scopes, input devices, or the desktop session slice remain isolated. Parent directories are mounted read-only, which prevents host file writes but does not restrict operations offered by each exposed socket protocol. Hidden parent mounts also expose sibling files through their internal `/run/kwin-mcp-host-sockets` paths, except the host session D-Bus socket and the `keyring` and `p11-kit` entries of the user runtime directory, which are masked and never exposed. Socket replacements at discovered names remain live; new socket names require a new session.

### Host visibility

The [live Unix socket regression](docs/shared-ipc-511/README.md) verifies shared
connections, the overlay backing-path failure, private `/tmp`, and cleanup.

The session and the host share the paths listed here. Other session writes stay in the session: `/tmp` is private, and writes under `$HOME` go to an overlay on the host.

- `host_shared_dir` is the directory `session_start` and running entries in `session_list` return; it equals `workdir`, `/tmp/kwin-mcp-<pid>-<number>`. It is bind-mounted at the same absolute path inside the session and is the session's `XDG_RUNTIME_DIR`. It is the only `/tmp` directory shared with the host, so sockets, FIFOs, logs and output that host processes must read or connect to belong there. Host tools use the same path.
- The rest of `/tmp` inside the session is a private tmpfs. Its files and sockets are invisible to the host, and host `/tmp` contents other than `host_shared_dir` are hidden from the session. `launch_app` checks literal absolute path arguments and reports one that the session cannot see; its `path_not_visible` error data names `host_shared_dir`. To run or read a host build artifact, copy it into `host_shared_dir`, which the session sees immediately at the same path, or copy it into the session with `import_file` (for example back to its own path after a session-local move hid it).
- `$HOME` inside the session is an overlay. Reads see the live host home. Writes go to `${XDG_CACHE_HOME:-~/.cache}/kwin-mcp/<name>/overlay-upper` on the host and do not appear at the host's `$HOME`; `export_file` copies one out, and `host_write_paths` at `session_start` makes a directory's writes reach the host. A host file replaced after `session_start` (an editor's save, for example) can stay the old copy inside the session. Before each `launch_app` command runs, its result's `home_overlay` lists the absolute arguments under `$HOME` whose writes stay in the session (`overlay_paths`), the command's working directory when that is in the overlay, and the file arguments, absolute or relative to that directory, that the session sees a different size or modification time for (`different_copies`); the result text names both. `import_file` with `overwrite: true` brings in the host version. A Unix socket that a session process creates under `$HOME` cannot be connected from the host: `connect()` fails with `Connection refused` even though the file exists in the overlay. Put host-facing sockets under `host_shared_dir`.
- `/sys/fs/cgroup` is read-only in the session, so `mkdir` under it fails with `Read-only file system`. A program that needs a writable delegated cgroup runs on the host, under `systemd-run --user --scope -p Delegate=yes`, and reaches session processes through sockets in `host_shared_dir`.
- The session has no systemd user manager or user bus: `systemd-run --user` and `systemctl --user` fail with `Failed to connect to user scope bus`. Exposing the host's manager would let session processes start host units, so it stays outside the sandbox. Bound a command inside the session with `timeout` or `prlimit` (the session's own scope already applies `memory_high`/`memory_max`), or run the bounded unit on the host with `systemd-run --user` and connect it to the session through `host_shared_dir`.
- Session stdout is discarded, except for `launch_app` with `expect_window: false`: that command's stdout goes to `stdout_path`, a `launch-<n>.stdout` file in `host_shared_dir` that the host can read at the same path while the command runs, and the result quotes its last 4 KiB as `stdout_tail` when the command has already exited. Session stderr, including each application's stderr, goes to `sandbox.log` in `${XDG_CACHE_HOME:-~/.cache}/kwin-mcp/<name>/`, a host-only directory.
- Session apps share the host's network and, with inherited profiles, the user's signed-in accounts. A sync client inside the session (Obsidian Sync, a browser's sync, a cloud-drive app) uploads what it sees in the overlay, and the same account's host client then writes that version to real host files. Because files created or edited on the host after `session_start` can look stale or empty inside the session, such a client can overwrite host data this way (#581). Use `import_file` before opening a host-edited file, start the session after host edits, or keep sync off for vaults and profiles opened in a session.
- `screenshot` replaces `screenshot.png` in the workdir on every call; pass `save_as` with a new absolute host path to keep a copy. `export_file` refuses a host directory that does not exist unless `create_parents: true`.
- `launch_app` with `expect_window: false` returns as soon as a command is submitted, for a command that backgrounds itself or never maps a window. It reports the command shell's PID inside the session (not a host PID) and `exit_status` if the command has already exited. Without it, a command that is still running when `window_timeout_seconds` ends with no new window is reported as an error (`window_outcome: "timeout"`, `reason: "window_not_observed"`, `command_submitted: true`, `retry_safe: false`); the command keeps running, so inspect `window_list` before any retry. A command that exits with status 0 without a window still succeeds with `window_outcome: "exited"`. A window that already existed is attributed to the launch (`window_outcome: "reused_existing"`) only after the command has exited with status 0, the way a second launch hands off to a running instance; while the command is still running, an older window that becomes active (focus returning after another window closed, say) is not its window, and the launch keeps waiting for a new one.
- `/etc/ssh/ssh_config.d/*.conf` files that ssh would reject inside the sandbox (root-owned, which the sandbox shows with an unmapped owner, or group- or other-writable) are replaced by copies owned by the session user with mode 600. The copies are taken at `session_start`, so host edits to those files apply to the next session.

For a host helper that must reach a session daemon, start the daemon inside the session with its socket under `host_shared_dir`, and run the client on the host with the same path:

```bash
# Inside the session, through launch_app with expect_window: false
mkdir -p "$XDG_RUNTIME_DIR/obs"
<daemon> "$XDG_RUNTIME_DIR/obs/daemon.sock" >"$XDG_RUNTIME_DIR/obs/daemon.log" 2>&1 &

# On the host, using the host_shared_dir from session_start
systemd-run --user --scope -q -p Delegate=yes <client> /tmp/kwin-mcp-<pid>-<number>/obs/daemon.sock
```

## Build

```bash
cargo build          # debug
cargo build --release
cargo clippy         # strict: unwrap/expect/todo/dead_code all denied
```

## Chrome Load unpacked

Chrome's **Load unpacked** dialog selects a directory. Use this procedure instead
of assuming the file-upload Ctrl+L / file path / Alt+O sequence proves that an
extension loaded:

1. Make the extension directory, including `manifest.json`, available **inside
   the isolated session**. A directory created on the host after `session_start`
   may be hidden by the session's HOME overlay; create it through `launch_app`
   in the session or confirm the session can read it.
2. Launch Chrome with a separate profile for a probe, for example
   `google-chrome-stable --user-data-dir="$HOME/.cache/kwin-extension-probe" --no-first-run`.
   Open `chrome://extensions`, enable **Developer mode**, then click **Load unpacked**.
3. In the chooser, send `keyboard_key` with `ctrl+l`, then `keyboard_type` with
   the absolute extension directory path (for example,
   `/tmp/kwin-extension-probe/`). Inspect a screenshot to confirm the
   path. This chooser may report `delivery.status: unverified` when accessible
   field text is unavailable; inspect the path before retrying the input.
4. Click the chooser's top-right **Open**. The bottom **Select Folder** control
   selects the chooser mode; it is not the accept button. If **Open** is disabled,
   inspect the path and confirm the directory exists inside the session.
5. Verify the named extension card appears and is enabled in `chrome://extensions`.
   A dismissed chooser or an **Extension loaded** toast alone does not verify
   that the extension is active. For an extension with a background service or
   relay, also verify its expected operation.
   Use a screenshot for this check: Chrome internal pages can return no
   matching elements from `find_ui_elements` even when the card is visible.

See the [raw chooser and enabled-card proof](docs/evidence/375/README.md) for a
generated extension loaded through the production stdio server.

The [#375 follow-up](https://github.com/nm-z/kwin-mcp/issues/375#issuecomment-6202937506)
confirmed that `--disable-extensions-except` caused a load toast without the
expected extension card or relay connection. For manual loading, launch the
owned probe browser without that flag; if it is already running, restart only
that owned browser. This launch configuration failure does not demonstrate an
input-delivery failure.

## Fast clipboard input

Use `keyboard_paste({"text":"..."})` for long text, Unicode, or terminal commands
that do not need paced character events. Focus the target first. The call writes
the session clipboard and PRIMARY selection, sends Shift+Insert, and waits for
KWin to acknowledge the shortcut. Both selections retain the pasted text; the
host clipboard remains separate.

For a terminal command, paste without a trailing newline, inspect the terminal,
and use `keyboard_key({"key":"Return"})` when you intend to execute it. Pasted
newlines are clipboard content, but applications can interpret them as
submission; this tool never adds a Return key automatically.

The receipt reports `clipboard_characters`, `shortcut_submitted`,
`submission_complete`, and `compositor_acknowledged` separately. These establish
the offered clipboard length and input submission. `application_text` is
`verified` only when the focused field was read before and after the paste through
accessibility and the pasted text appeared (`delivery.method` is `atspi_focused_text`
or `atspi_rich_text`); otherwise it is `unverified` (terminals, and fields AT-SPI
cannot read). `form_submission` is always `not_observed`: this tool does not confirm
command execution. Check the field, terminal output, or a command-produced file
before continuing. `retry_safe: false` means
inspect before repeating a paste, since it can duplicate text or commands.

Run the production-stdio regression with
`ruby tests/keyboard_paste_e2e.rb /absolute/path/to/kwin-mcp /absolute/evidence/directory`.
It checks fast 120-character terminal input, UTF-8 and multiline clipboard
content, a native Qt field, and empty-input rejection using independently
exported application results. `--baseline` records the old paced-input timing
and fails when the paste tool is absent.

## Input timing

Paced mouse and keyboard tools accept `speed`, a positive timing multiplier
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

## Typing target changes

For single-line printable ASCII in a Chromium-family window, `keyboard_type`
requires an observable focused editable target before sending keys. If that
preflight fails, `text_target_unavailable` reports zero submitted characters
and a safe retry after inspecting and focusing the intended field. Password
fields and terminals retain their explicit unverified paths; Tab/Return input
can intentionally change the focused field and is not field-verified.

While typing, the server watches the original accessible object's focus and
visibility when available, and the active KWin window identity. For single-line
browser text it also watches the window caption, because Chromium can retain
focus/visibility states on a background tab's old field. A caption update can
therefore conservatively stop typing even when the same editor remains open. A lost target
or unavailable bounded observation cancels the remaining keystrokes, releases
keys held by this operation, and returns `target_changed` or
`target_unverifiable` with partial submission counts and `retry_safe: false`.
Inspect both the original field and the newly focused surface before retrying.

These checks are polling observations, not an atomic reservation of focus.
Some input may already have arrived when a change is detected. The counters
describe submitted key presses, not application receipt, and a successful
editable-value check does not establish application form submission.

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

### Investigating extra or duplicated characters

A `keyboard_type` mismatch reports a difference between the expected field
value and the accessibility observation. It does not identify the source of
an extra character. Compositor acknowledgement confirms processing of the
submitted input events; applications can still repeat, transform, or misdirect
them. Inspect the field before submitting or retrying. Preserve the typing
response and screenshot. Where native copy is available, select and copy the
field into the session clipboard and compare its exact text with the request.
Character counts alone do not identify which character differs.

For a harmless reproduction in a dedicated session, use the `input_log` path
reported by `session_start` or `session_list`. Correlate the `call_start` and
`call_summary` records by request ID, tool, and monotonic timestamps, then
inspect the ordered key events and `typing.keys` in the summary:

- Compare character-bearing key-down events with the requested text, keeping
  modifier events separate. Two generated key-down events at the duplicated
  position are evidence that the server emitted the duplication.
- Check corresponding releases, `hold_ns`, `since_last_release_ns` on events,
  and `gap_before_ns` in the summary. An unusually long hold can make key repeat
  relevant; interpret it against the compositor's actual repeat settings.
- Check each event's delivery outcome. `flushed` describes transport delivery,
  not a guarantee that one application character resulted from each event.
  An uncertain outcome also makes the delivered sequence inconclusive.

The summary's `typing.characters` and `character_count` describe generated
key-down events, not a separate reading of the application's field.

Viewer activity is coarse evidence. The server checks it during operations
and can stop with `viewer_input_interrupted`; the quiet period prevents
immediate agent input after detected user activity. The activity file's
modification time does not record which characters the viewer sent. Detected
overlap establishes concurrent activity, while absence of an interruption
does not establish the origin of an extra character.

To isolate the viewer, reproduce in a fresh empty test field after a confirmed
`viewer_close`, with no other input producer controlling that session. Type
the same harmless text once and independently inspect or copy the result.
Reproduction with the viewer closed excludes that viewer for that run.
Reproduction only with the viewer open narrows the conditions, but timing or
application behavior can still be involved.

Retain the server commit, application version, viewer state, exact request and
observed text, typing receipt, screenshots, and only the relevant journal
interval. If the generated sequence is correct and the evidence cannot
distinguish application repeat, another input producer, or viewer input,
report the discrepancy as unattributed. The extra `s` reported in
[#529](https://github.com/nm-z/kwin-mcp/issues/529) does not by itself establish
which source produced it.

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

Session IDs belong to the server process that returned them. A DSH migration or client/server restart does not transfer sessions; when the old client connection closes, that server shuts down its desktops, unless it was started with `--keep-sessions MINUTES`. With that flag a server whose client closed (the usual way a host respawns it) stays up while it owns a running session and serves it, through its control socket, to the kwin-mcp the host starts next: calls with the old session ID are forwarded to it as for any other live same-user server, `session_stop` ends it, and it exits when no session is running or every session has been idle for MINUTES. A signal, or the process that launched the server exiting, still stops everything at once, and `--ttl` still expires idle sessions. An in-place source reload preserves its PID, pipes, and live sessions. Calls with a session ID from another live same-user server on this host are automatically forwarded after checking the target PID and effective UID. The replacement server's `session_list` covers only its own process. Session stops, idle TTL expirations, and completed server-shutdown cleanup are recorded in `${XDG_STATE_HOME:-~/.local/state}/kwin-mcp/session-lifecycle-v2.jsonl`; the log is capped at 1 MiB and retains recent records, so older details may be absent. An unavailable-ID error includes `session_status`: `stopped` or `expired` with the recorded end time and reason when known, `owned_by_another_process` when the ID belongs elsewhere but its final state is unknown, or `not_found`. A request without an ID when no session is active reports `none_running`. These responses also report whether the ID is available in the current process and provide recovery guidance. TTL-expired handles appear as `expired` in the current process's `session_list` and can be cleared with `session_stop`. If the owning process is unavailable, call `session_start` without `session_id`, relaunch the applications, and restore the task.

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

To update a configured absolute MCP command that is separate from PATH, install
its artifact explicitly (the adjacent viewer is updated with it):

```bash
bash scripts/install.sh --source-dir target/release \
  --provider-bin /absolute/configured/path/kwin-mcp --replace
```

This mode validates and reports the configured command instead of treating an
unrelated PATH entry as an installation failure. It does not change client
configuration or restart providers. The provider path must be a real file;
for a symlink, inspect its resolved executable and use that artifact path so
existing processes observe the replacement. Verify compatible in-place reload
with owned test sessions before replacing a live artifact.

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

For a configured artifact, also exercise the actual installer across two owned
live desktops. [The provider installation regression](docs/provider-install-493/README.md)
documents the old-server/new-build command and verifies the installed inode,
stable PID and pipes, partially buffered input, surviving windows, and cleanup.

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

Requires: `bubblewrap` (bwrap), `passt` (pasta), and KWin running as a Wayland compositor. Each session has private loopback: ports the host listens on are forwarded to the host, so session apps reach the real local services at `127.0.0.1`; other ports stay private to the session, and only the session's CDP port is forwarded into the host. Address the service in the family it listens on: `localhost` resolves to `::1` first in the session, and pasta resets that connection (instead of refusing it, so clients do not fall back) when the host service listens only on IPv4, so use `127.0.0.1` (or `[::1]` for an IPv6-only service). A listener the host starts after `session_start` is picked up after about a second. See [#782](https://github.com/nm-z/kwin-mcp/issues/782). The sandbox keeps the host hostname so copied HOME profile locks do not look as if they belong to another computer. FUSE works inside the session when the host has `/dev/fuse`, a non-setuid `bwrap`, `setpriv`, and `fusermount`: AppImages mount normally and FUSE filesystems such as sshfs work, while apps still run without capabilities over a read-only host root. Without those, `launch_app` sets `APPIMAGE_EXTRACT_AND_RUN=1` so compatible AppImages run without FUSE. `launch_app` also selects the session's Xwayland display for X11 apps.

## Screenshot dimensions

Apps in the session see a D-Bus session bus that carries only KWallet, not KWin, so KDE's Spectacle (which needs `org.kde.KWin.ScreenShot2` there) exits with "KWin compositor not available". Use the `screenshot` tool, which captures the session's virtual output through the server's own KWin connection.

File screenshot responses expose capture metadata in `structuredContent`: the
PNG path, dimensions, window, region, and optional settling information. Inline
responses always include the PNG image and the same metadata as JSON text.
Clients that consume structured metadata can request
`{"inline": true, "structured_metadata": true}` to also receive it in
`structuredContent`. This is opt-in because some clients, including Claude Code,
hide image content when a result contains `structuredContent`; leave the option
unset in those clients. File captures always retain structured metadata. If saving
the file fails, an inline capture still returns the image, with `path: null` and
`write_error` in its metadata.

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
exposed through AT-SPI. When multiple accessibility windows have the same
title and bounds as the active window (for example, two maximized Chrome
windows showing the same page), AT-SPI can read stale focus from the other
window. An unmatched receipt then reports `unverified` with method
`atspi_ambiguous_window` after a 2-second check, not a delivery error.
Otherwise, a field that still differs after the bounded 10-second wait reports
`pending_or_mismatched` as an error. Inspect the active field before sending
or retrying: delayed input can otherwise be duplicated.
Passwords, terminals and Tab/Return actions do not expose an application-text
receipt.

`keyboard_key` waits for a KWin compositor receipt and includes
`compositor_acknowledged: true`, but its `application_effect: "unverified"`
does **not** confirm that the intended browser or application action happened.
Chrome DevTools shortcuts (`Ctrl+Shift+I`, `Ctrl+Shift+J`, `F12`) include an
inspection warning even when browser chrome, rather than the page, has focus:
Chrome may handle those shortcuts itself. Verify by screenshot; docked
DevTools do not create another `window_list` window.

For a focused terminal, `keyboard_type({"text":"command\n","speed":4})`
requests faster paced input; the final newline also submits the command. Inspect
`submission_complete`, `submitted_characters`, and
`delivery.compositor_acknowledged` to distinguish completed input from a failed
submission. A terminal's `delivery.status: "unverified"` means its application
text was not read back; it is not a reason to resend a completed command.
Confirm execution by inspecting terminal output or a known result file. For
result files, wait for readiness and export the file to check its contents.
The [accelerated terminal probe](tests/input_delivery_e2e.md#accelerated-terminal-command-476)
checks this route with a 120-character command including Return.

After an editable field is cleared, AT-SPI may expose one blank newline that the
application removes when typing starts. If the entire resulting field matches
the requested text, `atspi_transformed_blank_line` reports `unverified` with
inspection guidance. It retains the original expected and observed counts;
preservation of that initial newline is ambiguous. A newline retained by the
application still follows the normal exact-text verification path.

Chromium can also expose a placeholder or accessible label as if it were the
field's current text. If that initial text matches the label or placeholder
and the complete resulting field equals the submitted text,
`atspi_placeholder_transition` reports `unverified` without counting the
ambiguous initial label as missing input. A real existing value equal to the
label still follows ordinary exact-text verification when it remains in the
field. Inspect the result before sending; do not retype an already complete
value merely because placeholder preservation could not be verified.

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

`session_start` accepts `startup_timeout_seconds` from 1 to 300 (default 60).
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
exists; a sandbox that exits before a startup signal reports its exit status. A readiness timeout also reports the owned sandbox PID, its process state, and its kernel wait channel before cleanup. This distinguishes a stopped launcher from a running or blocked one even when stderr is empty. Only bounded process-state fields are captured; command lines and environments are not included.
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

### Launch failure recovery

Every error produced during `launch_app` includes the launch `stage`,
`command_submitted`, `operation`, `retry_safe`, and `recovery_action`. More specific
error data (such as a path-not-visible reason) is preserved. Command-script write
errors also include the exact `path` and numeric `os_error`; quota diagnostics do
not treat free filesystem blocks as proof of available per-user quota.

A pre-submission failure may be retried only after correcting its cause. Once any
submission is confirmed, a later error cannot authorize replay: inspect
`window_list` and `screenshot` in that same session before launching again. An
error does not automatically stop the desktop or rerun the command.

The real stdio regression `tests/launch_failure_context_e2e.rb` makes only its own
session workdir temporarily unwritable, verifies that no command ran, restores its
permissions, and verifies a new window in the same session. It does not fill a
host disk or change a user quota.

### Attributable element lookup

`find_ui_elements` accepts optional `role` and `visible_only` filters, `exact`
(match the whole name or role), `limit` (1 to 200, default 50) and `compact`. `role:
"button"` includes native push/toggle buttons but not a label that merely mentions
a button. Structured `elements` carry backend, role, name, bounds, states, `value`
(the text of an entry or spin button, cut at 200 characters, never a password) and the
index of the element's `surface` (the application frame or page, listed once in
`surfaces`). `matches` counts every match found and `more` says how many the limit
left out. `window_id` appears only for a window other than `active_window_id`, and
`visible: false`, `enabled: false`, `occluded: true`, `incomplete` and `errors` only
when they apply. `compact: true` returns role, name, bounds and
`mouse_coordinates_usable` only. The `query`, `results`, `role`, `visible_only`,
`server_pid`, `sampled_at_milliseconds` and `continuation` keys and the per-element
`window`, `coordinate_space` and `raw_atspi_bounds` were removed: on a page with 200
matches the result went from 377,870 to 14,587 bytes. Both accessibility
backends can report the same physical control; these are attributed observations,
not a count of unique physical widgets.

Use a match's `bounds` with `mouse_click` only when `mouse_coordinates_usable` is
true. Native coordinates are normalized against the accessibility provider's own
frame origin after a unique title match and matching frame dimensions. This also
handles Qt/Wayland providers that report a local origin for AT-SPI Screen extents.
Ambiguous window titles or unmatched dimensions do not produce usable mouse
coordinates. CDP bounds remain explicitly labelled CSS viewport pixels: browser
chrome is not part of that viewport, so those bounds are never advertised as
window-relative input coordinates. Native accessibility is queried even when CDP
is connected, making browser chrome attributable too.

Visibility is not clickability. CDP performs a read-only center hit test with
`elementFromPoint`; covered controls cannot be marked clickable. Native matches
report compositor-window coverage where known but leave in-application occlusion
and positive clickability unknown. Inspect the screenshot for those cases.

The result records the active window ID, serving PID and sample time. It is a
snapshot, not a window lock: re-query after navigation, scrolling, layout changes,
or changing windows. Page/node/match caps and timeouts report incomplete results
instead of silently discarding already collected matches.

`ruby tests/ui_match_context_e2e.rb SERVER EVIDENCE baseline|candidate` exercises
native controls, real Chrome HTML with a covered button, and same-user session
forwarding through the production stdio entrypoint. It uses fresh profiles and
its own desktop; it sends no provider requests.

Set `KWIN_PROOF_ZOOM=1` for the additional 175% browser-zoom reproduction from
#407. It places an Add-a-comment field and a separate milestone field in a local
page, then compares the original unlabelled DOM coordinates with the revised
window-relative native match. The browser reports which field received the exact
input through its window title. The original coordinates select the milestone
field; the revised coordinates select the comment field while leaving the
milestone empty. Caption confirmation is read-only and bounded; input is never
replayed while waiting for the compositor to publish the changed caption.
