# Real-system test storage

`tests/browser_navigate_e2e.rs` drives `browser_navigate` in fresh sessions
against a local HTTP server that records what Chrome asked for, so each result
is checked against the request Chrome made: one test per report of #908 without
CDP (a page that keeps ctrl+l, a page that returns it late, another window
taking focus, an address of 3000 characters) and one with CDP. Run a case with
`systemd-run --user --quiet --wait --pipe --collect --same-dir -p RuntimeMaxSec=600
--setenv=KWIN_MCP_E2E_SERVER=<binary> <test exe> --ignored --test-threads=1 NAME`;
`KWIN_MCP_E2E_EVIDENCE=<directory>` keeps a screenshot of each landed page.

`ruby tests/native_paste_e2e.rb /path/to/kwin-mcp /absolute/evidence` exercises
native Ctrl+V and Shift+Insert through a fresh stdio/KWin/Chrome session. It checks
multiline UTF-8 field receipts and independently receives the complete values
through ordinary HTML form POSTs to a local Ruby fixture. It also verifies that
a generated-secret selection returns only protected/unverified paste metadata.
The fixture submits no external form and cleans up its own desktop.

The Rust lifecycle harness and Ruby fixtures use `KWIN_MCP_TEST_TMPDIR`,
then `TMPDIR`, then `/var/tmp` as the fixture parent. The lifecycle harness
creates a unique private HOME there and removes it when the client is dropped.
This keeps nested fixture Unix
socket paths within Linux's pathname limit even in a long checkout path.
Unlike `/tmp`, `/var/tmp` remains visible inside the isolated desktop.

Set `KWIN_MCP_TEST_TMPDIR` to override the fixture HOME parent. Use a short
absolute path outside `/tmp`; nested browser-cache socket fixtures need room
for the generated HOME name and their own subdirectories.

`KWIN_MCP_TEST_TMPDIR` takes precedence over `TMPDIR`, so fixture HOME storage
can be chosen independently of the server's runtime workdir root. The latter
still follows `TMPDIR`. Both overrides must name an existing absolute directory.
The nested-mount fixture uses a short generated name and checks its graphical
socket path against Linux's 107-byte limit with an actionable error.

For example, after building the production binary and lifecycle runner:

```bash
KWIN_MCP_E2E=1 cargo test --test session_lifecycle_e2e \
  session_follows_host_sqlite_wal_rotation -- --ignored --exact --nocapture
```

The desktop cases require the dependencies stated on each ignored test.

The same command works from a checkout whose shell has `NoNewPrivs=1`, such
as a sandboxed worker's: the runner and the server it starts inherit the flag
and the server starts sessions under it. `no_new_privs_e2e.rs` starts the
server through `setpriv --no-new-privs` and inside a `bwrap` user namespace
and checks a window, a screenshot, the session's network and files, and that
`session_stop` leaves no process, scope or directory behind:

```bash
KWIN_MCP_E2E=1 cargo test --test no_new_privs_e2e -- --ignored --test-threads=1
```

## Startup stderr without autoclean (#852)

```bash
cargo test --test startup_uid_map_e2e -- --ignored --test-threads=1 --nocapture
```

These production-stdio cases run the real server and pasta with a stand-in
`bwrap` that prints the refusal the real one prints where nested user
namespaces are forbidden (`NoNewPrivs` alone no longer causes one). They
require uinput access and the host's systemd user D-Bus. Both leave `XDG_RUNTIME_DIR` and `DBUS_SESSION_BUS_ADDRESS` unset to
exercise the validated host fallback from the issue.

The default mode must return the actual UID-map refusal, structured stderr and
namespace guidance while retaining its workdir. The `--autoclean` mode must
return the same diagnostics after deleting its workdir and disk twin. The tests
independently read the retained sandbox log, clear the failed session handle,
and require successful server exit on stdin EOF. All fixture files are private
and removed afterward; no desktop is started.

To show the regression against an older binary, set `KWIN_MCP_E2E_SERVER` to its
absolute path. Before the fix, the default case fails because its response omits
the stderr, while the autoclean case passes.

## System Monitor startup and Qt diagnostics

```bash
KWIN_MCP_E2E=1 cargo test --test session_lifecycle_e2e \
  systemmonitor_registers_on_the_private_app_bus_and_qt_failures_reach_stderr \
  -- --ignored --exact --nocapture
```

Requires `plasma-systemmonitor` and the normal KDE session dependencies. The
production stdio test launches System Monitor in a default session,
checks its window through KWin and a screenshot, checks private app service
ownership and wallet access, and requires a Qt platform failure to report its
reason on stderr. It also checks that the app bus differs from the host bus and
that the host system bus still denies name ownership. Set
`KWIN_MCP_TEST_ARTIFACT_DIR` to save RPC evidence and the screenshot. The same
test fails against the pre-fix server supplied with `KWIN_MCP_E2E_SERVER`.

## Saved screenshots and the pointer (#854)

```bash
KWIN_MCP_E2E=1 cargo test --test session_lifecycle_e2e \
  screenshot_can_omit_both_cursor_layers_without_moving_the_pointer -- --ignored --exact --nocapture
```

The production stdio test opens a static Chrome page and independently checks
that a clean crop contains only its background color. It compares saved PNG
bytes for omitted, false, and true `include_cursor` settings across full-window,
region, and cursor crops, and checks agreement with workdir and inline images.
Ordinary captures still show the pointer by default. Set
`KWIN_MCP_E2E_PROOF_DIR` to keep images and metadata, and `KWIN_MCP_E2E_SERVER`
to exercise a pre-fix binary with the same test (it must fail on the saved crop).

## What a session reaches and how its tools name things (#862, #864, #869, #870, #891)

```bash
systemd-run --user --wait --pipe --collect --same-dir --setenv=KWIN_MCP_E2E=1 \
  --setenv=KWIN_MCP_E2E_SERVER=target/release/kwin-mcp \
  target/release/deps/session_one_offs_e2e-<hash> --ignored --test-threads=1 --exact NAME
```

`tests/session_one_offs_e2e.rs` holds one production-stdio case per report; each
fails against a server built before its fix (`KWIN_MCP_E2E_SERVER`):

- `mdns_names`: the host's own `.local` name resolves in a session to the address
  the host gets. Needs avahi-daemon and nss-mdns on the host.
- `absent_host_socket`: a display-attached host process holds
  `~/.local/share/zed/zed-stable.sock`; the session sees no file there, a
  single-instance start (the test executable, run inside the session as
  `session_side_single_instance`) binds its own socket, and the host instance
  receives nothing.
- `session_keyring`: a private bus with an unlocked gnome-keyring stands in for
  the host Secret Service (`KWIN_MCP_SECRET_SERVICE_ADDRESS`). The session reads
  the host item with `secret-tool`, stores and changes items of its own, and the
  host keyring is unchanged afterwards; after `secret-tool lock` a new session
  copies nothing, says so, still stores its own items, and the host collection
  is still locked. Needs gnome-keyring-daemon and secret-tool.
- `renamed_element`: Chrome without a debugging port shows a `contenteditable`
  composer named by `aria-label`; the element search finds it by the name as
  typed (`...` for `…`), by `textbox` as well as `entry`, and by the tree's
  `entry <name>` line; after the page renames it, the earlier name still returns
  it with `earlier_name`.
- `concurrent_localhost`: two servers start sessions together, each with a host
  HTTP server on `127.0.0.1`; in each session 100 of 100 `curl
  http://localhost:PORT` requests succeed, Chrome shows the page, a server on
  `::1` only and one started after the session are reached too.

# DevTools placeholder receipt

Run `ruby tests/devtools_placeholder_e2e.rb /path/to/kwin-mcp /path/to/evidence`
on a KDE/KWin host. The fixture starts its own stdio server and isolated fresh
Chrome profile, opens DevTools Network, and locates the Filter field through
accessibility. It checks both the empty placeholder and a genuine `Filter`
value, independently reads each completed value through native copy and the
session clipboard, and saves screenshots and receipts. It closes only its own
session and removes its private HOME.

# Typing receipt diagnostics

Run `ruby tests/typing_receipts_e2e.rb /path/to/kwin-mcp /path/to/evidence`
on a KDE/KWin host. The isolated fresh-Chrome fixture checks a 71-character
address-bar value, the exact 75-character search query from issue 528, a
controlled 72-character limit, a zero-length web field, and a 1,700-character
multiline Markdown value. It independently copies application text through
native Ctrl+A/Ctrl+C and the session clipboard, records phase timings, saves
screenshots, and cleans up its own session.

## Typing focus changes

```bash
ruby tests/typing_focus_e2e.rb /path/to/kwin-mcp /path/to/evidence
```

This production-stdio regression creates fresh Chrome profiles in an owned
session and verifies normal typing by native copy. An application process
already launched inside the session waits for a fixture gate, then opens a
different window or a different tab after the journal confirms eight uppercase
key presses. The tab case requires the KWin window ID to remain unchanged.

Both cases require a partial `target_changed` error, flushed key releases in
the input journal, and correct lowercase typing afterward. Native copies from
both fields record the characters actually observed, including any character
that reached the new field before cancellation. A focused noneditable browser
button must fail preflight with zero key events. The script saves JSON, input
journal and screenshots, then stops its session and removes its private HOME
and temp root. Chrome and the ordinary desktop dependencies are required.

## Typing and paste targets

`tests/typing_targets_e2e.rs` has two ignored cases. The Chrome case types into
a number input, the in-page search bar and the DevTools console prompt, repeats
the first two after DevTools was opened and closed, checks that a read-only
input is refused by name and that a 360,000-character field is typed into and
reported `unverified`. The Konsole case pastes a command right after
`launch_app`, runs it and reads the file it writes, then pastes a control
character and expects the confirmation dialog in the result.
`KWIN_MCP_E2E_SERVER` selects the server binary and `KWIN_MCP_E2E_EVIDENCE` a
directory for screenshots. Chrome names a window's frame after the search bar
only some of the time, so that binding is covered by the unit test
`a_frame_renamed_by_its_application_is_bound_through_its_process` on a stand-in
accessibility bus.

## File chooser capture identity

```bash
ruby tests/transient_chooser_e2e.rb /path/to/kwin-mcp /path/to/evidence candidate
```

This production-stdio test opens an owned fresh Chrome window and its native
file chooser. It requires a successful pixel-verified chooser image, then
closes the chooser during a pending capture and requires an explicit
`transient_capture_unverified` error without an image. It also checks the
restored parent and a region extending outside the window. `baseline` mode
records the older misleading capture for comparison. The script saves RPC
evidence and PNGs, stops its session, and removes its private HOME and temp
root. Chrome and the normal session dependencies are required.

The checked-in `evidence/396` images show the baseline race returning parent
pixels under the chooser's identity, and the candidate's verified chooser and
subsequent parent capture. The candidate rejects the race, so that response
intentionally has no PNG.

# Remote viewer stream

```bash
cargo test --release --test remote_viewer_e2e -- --ignored --test-threads 1 \
  a_remote_session a_reexecution raw_frames every_way a_window_the_compositor \
  the_remote_viewer_bridges
cargo test --release --test remote_viewer_e2e -- --ignored two_viewers --nocapture
cargo test --release --test remote_viewer_e2e -- --ignored measure_the_stream --nocapture
```

Each test starts production servers from one private HOME that holds a shared
secret: a listener on a loopback port, the server that starts sessions on it,
and a third server whose isolated desktop receives the viewer windows, so
nothing opens on the tester's own desktop. `--link-mbps` on the listener makes
the loopback socket behave like a link of that speed. The tests read each
viewer's `viewer-stream.json` counters and logs. Build in release: the
software DSC codec is too slow unoptimized for the viewer's start timeout. Do
not edit `src/` or rebuild while they run: a server started from `target/`
rebuilds and re-executes itself when its sources change, in the middle of a
test's measurements, and the listener, which does not re-execute, would keep
running the old build.

- `a_remote_session_streams_changed_frames_to_a_viewer_on_the_callers_machine`
  (20 Mb/s link): a 1280x720 window gets DSC at the 6 bpp the link carries and,
  once the screen is still, the same frame as exact pixels; a 320x180 window
  gets raw frames; an idle screen sends none; agent typing sends some; keys
  typed into the window create a file in the session; `screenshot` still
  returns 1280x720; the sender stops when the viewer is killed, on
  `viewer_close`, and when the calling server is killed.
- `raw_frames_show_the_sessions_exact_pixels_and_a_still_dsc_frame_becomes_exact`:
  Gwenview shows a picture with every value of each channel in a 1280x720
  session, and the viewer window has the same size. The session's own
  `screenshot`, the frame the viewer received (written through
  `KWIN_VIEWER_STREAM_DUMP`) and a screenshot of the viewer's window on its
  desktop are compared pixel for pixel, over an unlimited link (raw) and over
  a 20 Mb/s link (DSC, then the settled exact frame). A 64x64 corner where the
  pointer is parked is left out, because the stream draws the pointer and a
  screenshot with `include_cursor=false` does not.
- `a_remote_session_and_its_viewer_survive_the_callers_reexecution` (#926): the
  calling server runs a private copy of the binary, and a new modification
  time on it makes the server re-execute itself, twice. Each time the listener
  logs no disconnect, the sandbox, the frame sender and the viewer keep their
  PIDs, the viewer is still the server's child, `session_list` shows the
  session with its host, `window_list` and `screenshot` answer on the same
  ID, typed text reaches the window as new presented frames, and
  `viewer_open` reports `already_open`, and no socket of the server is
  inheritable by its children. `session_stop` then ends all of it, and
  killing the server with a second session open makes the listener log the
  disconnect within 5 seconds and stop that session.
  `KWIN_MCP_TEST_EVIDENCE=DIR` keeps the window's and the session's
  screenshots after each re-execution and both logs; `evidence/926` holds one
  run's.
- `a_reexecution_into_a_binary_without_the_handover_closes_the_connection`:
  the server's binary is replaced by the same build with one byte of
  `remote::ADOPTS_CONTROL_MARK` changed, which the running server takes for a
  build that would not continue on its sockets. It re-executes without them:
  the listener logs the disconnect within 5 seconds and stops the session,
  `session_list` shows it as `connection_lost` with its host, and
  `session_stop` clears it.
- `every_way_a_stream_ends_leaves_nothing_running`: the sender killed, the
  viewer killed, the window closed, `viewer_close`, `session_stop`, and the
  listener killed each end both processes; a viewer stopped for six seconds
  holds its sender back without growing it and then resumes.
- `two_viewers_stay_stable_through_minutes_of_changes_resizes_and_mode_switches`
  (soak, four minutes; `KWIN_MCP_SOAK_SECONDS` changes that): two sessions and
  two viewers over a 40 Mb/s link, text scrolling in both, a resize every 15
  seconds that changes the mode, one stalled viewer. It asserts frames in
  every 5 second interval, no frame-socket or picture error in any log, mode
  switches and settles counted, no growth of sender or viewer memory between
  the halves of the run, and no viewer or sender process afterwards.
- `measure_the_stream_under_scrolling_text` prints, for each session height in
  `KWIN_MCP_STREAM_HEIGHTS` (default `1080,720`) and link speed in
  `KWIN_MCP_STREAM_LINKS` (default `1000,500,200,100`; `0` is unlimited), the
  chosen mode, bytes per frame, frames per second, capture-to-present latency,
  processor use of sender and viewer, and what the still frame became.
  `KWIN_MCP_TEST_BINARY` names another kwin-mcp build to measure.
- `a_window_the_compositor_does_not_show_still_presents_the_frames_it_receives`:
  the viewer window is minimized while text scrolls. It gets no frame
  callbacks then, and must go on presenting what arrives (`presented` in its
  counters keeps up with `frames`, counted as `presented_unshown`), return to
  the compositor's redraws when it is shown again, and close when its stream
  ends while it is minimized.
- `the_local_viewer_keeps_showing_a_changing_session`: the local viewer, which
  shares the window code, shows different pictures over time.
- `the_remote_viewer_bridges_the_callers_clipboard_and_gives_the_agent_its_selection_back`
  (needs KWrite): the isolated desktop that receives the windows stands for the
  caller's machine, so its clipboard is the caller's and the tester's own is
  never touched. Ctrl+V and Shift+Insert in the window paste that clipboard
  into a document in the remote session (read back from the file the session
  saves), Ctrl+C and Ctrl+X put the session's selection on it, and the agent's
  selection is back in the session after each. `clipboard_write target=host`
  on the remote session writes the caller's clipboard, and a generated secret
  reaches the session only through the viewer paste and is in no result and
  no log. A second session with its own viewer sees none of it. With
  `KWIN_MCP_TEST_EVIDENCE=DIR` it keeps screenshots of the window.

Every test that streams also checks that the window presents: frames that
arrive while `presented` stands still fail it.

What the isolated desktop cannot show is the tester's own: real outputs with
their scale and HDR mode, and the states in which that compositor repaints
nothing (outputs powered off, a locked screen). One test opens its window
there, for six seconds per session size:

```bash
KWIN_MCP_STREAM_HEIGHTS=1080,1350 KWIN_MCP_STREAM_EVIDENCE=/some/directory \
  cargo test --release --test remote_viewer_e2e -- --ignored the_viewer_presents --nocapture
```

`the_viewer_presents_the_frames_it_receives_on_the_testers_own_desktop` fails
when a second with frames has almost no presents, and with
`KWIN_MCP_STREAM_EVIDENCE` it copies the viewer's log and takes two screenshots
of the window (kdotool makes it the active window, spectacle captures it),
which must differ. `KWIN_MCP_REAL_DESKTOP_MOVE=X,Y` moves the window first, to
choose the output. Run it once with the screens on and once with them off
(`kscreen-doctor --dpms off`): the log says which presents the compositor did
not ask for. A 1920x1080 session on an output at scale 1.25 gives a window of
2400x1350 device pixels, larger than the session; a 2400x1350 session there is
shown one to one.

## Viewer target and session end

`tests/viewer_target_e2e.rs` covers the viewer's link to its session (#911)
and `session_stop`'s report of processes that are still exiting (#922).
`viewer_close_succeeds_for_a_session_whose_owner_is_gone` needs no desktop.
The others start real isolated desktops and open every viewer window inside
one of them:

```bash
KWIN_MCP_E2E=1 cargo test --release --test viewer_target_e2e -- --ignored --test-threads=1
```

`the_viewer_names_a_screencast_node_that_is_in_another_pipewire_daemon` starts
two private PipeWire daemons with their own WirePlumber and a bare
`kwin_wayland --virtual`, so it needs `pipewire`, `wireplumber` and
`dbus-run-session` on PATH. `a_confined_server_links_no_host_socket_and_its_viewer_opens`
runs the server under `unshare --user --map-current-user`, which needs
unprivileged user namespaces.
