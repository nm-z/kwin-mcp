# kwin-mcp

MCP server for KWin Wayland GUI automation. Single-binary Rust using `rmcp` + `reis` (EIS input) + `atspi` (accessibility tree) + `zbus` (D-Bus/KWin IPC) + `evdev` (uinput virtual devices). Container isolation via bubblewrap and pasta.

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
| `accessibility_tree` | Traverse the AT-SPI2 accessibility tree with configurable depth/filters, subtree continuation, and a bounded per-call time budget (5 seconds by default, up to 30 seconds). `show_links: true` adds each link's absolute target to its row; a level too large for one reply is returned a page of rows at a time (`offset`). |
| `find_ui_elements` | Search UI elements by name/role with bounding boxes. A link carries its absolute target as `href`. |
| `browser_read` | Read the page in the active tab of the focused Chromium-family window through CDP: its text, its outer HTML, or its links with their absolute targets, optionally scoped by a CSS selector, at most 30,000 characters per call with `next_offset` to continue. Never returns cookies, storage or the values of fields. |
| `browser_eval` | Run JavaScript in the page of the active tab of the focused Chromium-family window through CDP and return its value: a string as it is, anything else as JSON, paged like `browser_read`. It runs in an isolated world that sees the page's DOM but not its scripts' variables, and that the page's scripts do not see; there `document.cookie` reads empty, Web Storage, IndexedDB and Cache Storage are absent, and password fields read empty. For compact rows from a long listing page (#959). |
| `mouse_click` | Click at window-relative coordinates. |
| `browser_navigate` | Open a URL in the active tab of the focused Chromium-family window and verify where the tab landed. With CDP the tab is navigated directly. Without it, or with `method: address_bar`, the address bar is focused through accessibility, the URL is typed (pasted when longer than 128 characters, restoring the session clipboard), and Return is pressed only once the bar holds exactly the URL. Returns `title`, `url` and `method`, with `redirected: true` after a redirect. A download, Chrome's error page, a tab that kept its page, a tab switch or a search is an error with a `reason`. A URL without a scheme is opened as `http://`. Up to 2097152 characters with CDP, 512000 without. A bot-protected shop answered navigations through CDP with its error page and the same addresses typed with the page, although Chrome sends the same request headers either way (#958): `method: address_bar` types the address as a person does also when CDP is there. |
| `mouse_click_element` | Find an element by name/role at click time (as `find_ui_elements`) and click the center of its bounds; `index` picks among several. For pages whose layout shifts between steps. Activates another window of the session when the match is only there; a refusal says why and the next step. |
| `mouse_move` | Move pointer to window-relative coordinates. |
| `mouse_scroll` | Scroll at window-relative coordinates. Reports `view_changed` (false at the end of the content), and with CDP `scroll_y`, `scroll_max_y`, `at_end`. |
| `mouse_drag` | Drag from one window-relative position to another. |
| `keyboard_type` | Type a string of text. The result names the window that received the keys (`window`: id, title, `resourceClass`, pid, `hidden`), so text sent to a terminal running a full-screen program or to a hidden window can be told from lost input. |
| `keyboard_paste` | Paste UTF-8 text through the session clipboard with an acknowledged shortcut. |
| `keyboard_key` | Press a key or key combo (e.g. `ctrl+c`, `Return`). |
| `keyboard_press` / `keyboard_release` | Hold a key or combo down across calls, then release it with the same string. For a shortcut use `keyboard_key`: a modifier left held makes every typed character a shortcut (`h` becomes Ctrl+H), so `keyboard_type` refuses with `modifier_held` until it is released. |
| `launch_app` | Launch an application and wait for its window, or with `expect_window: false` return its session PID once the command is submitted. With no session it starts a default one and returns its `session_id`. `cdp: false` starts a browser without a debugging port (see [Browser CDP](#browser-cdp)). |
| `export_file` | Copy a session file (e.g. a download) to a real host path and verify it byte for byte, waiting for it first with `timeout_seconds`. |
| `import_file` | Refresh a running session's file from the host, with byte verification and explicit overwrite control. |

When a producer writes an artifact asynchronously, give `export_file` a
`timeout_seconds` (range 1–300; the default 0 copies what is there at once).
It then waits until the file exists as a regular file, no partial download of
it remains beside it (`.crdownload`, `.part`, `.download`), and its inode, size,
and modification time stay unchanged for `stable_for_ms` (default 500 ms, range
100–5000), and copies it in the same call. A timeout writes nothing and says
whether the file was `missing`, `downloading` or `changing`. This bounded
observation does not prove that the producer has exited. The wait has to run
inside kwin-mcp: the file is in the session's HOME overlay, which the agent's
own shell on the host does not see. The separate `wait_for_file` tool this
replaced was removed (#969).

## Uploading a host-created file

The browser and its file chooser read the **isolated session's filesystem**.
The same absolute path on the host can contain different bytes: HOME uses an
overlay, files can have startup-time copies or read-only mounts, and the
session has a private `/tmp`. A chooser accepting a path does not establish
that the selected file matches the host attachment.

Before selecting an attachment, call `import_file` with the host file and its
`session_path`: it copies the host bytes into the session and verifies them
byte for byte, so the chooser reads exactly the host file. It keeps a session
file that already exists unless `overwrite` is true.

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

Chrome's save dialog (Ctrl+S) is the same in-process GTK dialog, and nothing in
it is exposed through AT-SPI. It opens with only the stem of the suggested name
selected, so text typed there keeps the old extension; `keyboard_type` with
`replace=true` presses Ctrl+A first and replaces the whole name. The result is
`unverified` and says the field could not be read back: check it with a
screenshot. Return in the name field does not accept this dialog, also when
pressed twice and with no completion popup showing (the same keys accept
zenity's GTK chooser, so this is Chrome's dialog, not key delivery); use Alt+S,
the Save mnemonic, or click **Save**. To read a page's content, `browser_read`
needs no dialog at all.

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

A reply holds whole levels, as deep as fit in 30,000 characters. One level can be larger than that by itself: a `role` filter lifts every match to the first level, and a results page has several hundred links. That level (the first one, or with `root=ID` the one below the root) is returned a page of rows at a time: the rows that fit, each ending in `root=ID`, and a last line `+N more rows offset=K`, with `more_rows` and `next_offset` in the structured result. Call again with `offset=K` and the same arguments until no such line follows. Before this such a call returned an empty tree.

## Link targets and page content

A link's target is part of what the element tools report ([#947](https://github.com/nm-z/kwin-mcp/issues/947)):

- `find_ui_elements` gives every element of role `link` an `href`, the absolute URL the browser would open: a relative, scheme-relative or fragment-only `href` is resolved against its document (an iframe's links against the iframe's address), and `javascript:` and `mailto:` targets are returned as written. The `cdp` backend reads the element's resolved `href` property; the `atspi` backend reads the URI of the object's AT-SPI Hyperlink interface, which Chrome publishes already resolved (it puts no target in the object attributes). The two agree. An `<a>` without `href` and a `role=link` element have no `href` field. `compact=true` leaves `href` out: compact is the cheap list of click targets, and a tracking URL is longer than everything else it keeps. A target longer than 8,000 characters (a `data:` URL) is cut to its first and last 4,000 with `…[N more characters]…` between, and `href_chars` gives its length.
- `accessibility_tree` adds the target to link rows only with `show_links=true`, as a last field `href=URL`, so the default tree of a link-heavy page keeps its depth. A target longer than 1,000 characters is shown as its first and last 500 with `…[N more characters]…` between, the rule long names follow at 8,000; the element search returns that target whole. With `role=link` the links of a results page are one level, returned in pages as described above.
- `accessibility_tree` returns the window tree (AT-SPI: the browser frame, tab strip, toolbar and the page inside them, with AT-SPI role names) whether or not a CDP port is open. `source=page` returns the page tree of the active tab of the focused browser window through CDP, with the browser role names (`RootWebArea`, `StaticText`); a `root=ID` taken from a page tree continues in it. It needs `cdp_port` and refuses with the same reasons as `browser_read`.
- `browser_read` returns the page itself through CDP, without a save dialog: `format=text` (the text a reader sees), `html` (outer HTML without style sheets and without what scripts hold: code, and JSON data blocks, where a page keeps its state and tokens; only `application/ld+json` metadata stays), or `links` (one line per `a[href]`: its text, a tab, its absolute target; `strip_query=true` drops each target's `?query` and `#fragment`, where shops keep their tracking, and `dedupe=true` keeps one line per target, in the order targets first appear, named by the first of its links with a name). `selector` limits the read to the elements a CSS selector matches. A call returns at most `max_chars` characters (1 to 30,000, counted as JavaScript counts them) starting at `offset`, with `total_chars` and, while more remains, `next_offset`. It reads the active tab of the focused browser window and only its top document. It needs CDP: without a connection the error has `reason: cdp_unavailable` and says how to launch the browser so that one exists; the two tools above work either way. The connection launch_app made is not needed for good: the CDP tools connect again to the session's port when there is no connection (a server reload, which every install causes, keeps none) or the kept one stops answering (#957), and `reason: cdp_unresponsive` means the browser answered on neither; `browser_navigate` then types the address and says why in `cdp_skipped`. It does not read `chrome://` or extension pages.

`browser_read` reads the DOM only. It never reads `document.cookie`, `localStorage` or `sessionStorage`, and it returns no field's current value; in `html` the `value` attribute a page wrote on a password field is removed (also on a field that a "show password" switch turned into a text field, recognised by `autocomplete`, `name` or `id`, and inside a `<template>`). `format=links` lists every `a[href]` of the document, including links the accessibility tree leaves out (collapsed menus, links without a name), so it can list more targets than the tree does; every target the tree shows is in it.

`browser_eval` takes `expression`, JavaScript run as a script with `Runtime.evaluate` in a world `Page.createIsolatedWorld` makes for each call (DevTools names it "kwin-mcp browser_eval"): the value of its last statement is returned, top-level `await` works (REPL mode), and a promise is awaited, within 10 seconds. The world has the page's DOM and none of its scripts' variables, so a listing is turned into rows with selectors and attributes (`[...document.querySelectorAll('[data-listingid]')].map(card => card.dataset.listingid + '\t' + card.querySelector('h3')?.innerText).join('\n')`), and a page script that watches its own globals or prototypes sees nothing of the call. Before the expression runs, the world's `document.cookie` is made to read empty, `localStorage`, `sessionStorage`, `indexedDB`, `caches` and `cookieStore` to read `undefined`, and a password field's `value` (recognised as for `browser_read`) to read empty. The value comes back as it is when it is a string and as JSON otherwise (`type` names its JavaScript type; a DOM node has no JSON form), at most `max_chars` characters from `offset`, with `total_chars` and `next_offset`; every call runs the expression again. A thrown error is refused with `reason: exception` and its message.

## Element names in `accessibility_tree` and `find_ui_elements`

Both tools report the same objects, and a name copied from one works in the other:

- Text is compared case-insensitively with white space collapsed, typographic punctuation in its ASCII spelling (`…` and `...`, curly and straight quotes, dashes), invisible characters dropped, and the `\n` the tree prints for a line break treated as that line break.
- A role matches in either vocabulary: AT-SPI's `entry`, `push button`, `check box`, `radio button`, `combo box` and `menu item` are the page's `textbox`, `button`, `checkbox`, `radio`, `combobox` and `menuitem`, for the `role` filter and for a query that names a role. A query may also be the role followed by the name, as a tree line reads (`entry Ask anything…`).
- A field with no accessible name is named by its placeholder (`placeholder`, `aria-placeholder`), in both tools. For the page's own DOM (the `cdp` backend) the name follows the browser's order: `aria-labelledby`, `aria-label`, `<label>`, placeholder, `alt`, content (not for fields, whose content is their value), `title`; `contenteditable` elements are fields.
- A page can rename an element between two calls. Perplexity's composer sets a new `aria-label` every few seconds ("Ask anything…", "Type / for search modes", "Type @ for connectors"), so a name read from the tree was gone when it was searched for ([#891](https://github.com/nm-z/kwin-mcp/issues/891)). `find_ui_elements` remembers for five minutes the names each AT-SPI object was reported under by either tool; a query for one of them still returns the object, with its current `name` and the matched one as `earlier_name`. This applies to objects read through AT-SPI (the tree of a session without a CDP browser, and every search); names printed by a CDP tree are not remembered.

## Browser CDP

`launch_app` lists the exact browser commands available on the host. Chromium-family commands receive the session's forwarded CDP port, on the profile the command opens:

```text
google-chrome-stable https://example.com/
google-chrome-stable --profile-directory="Profile 2" https://example.com/
google-chrome-stable --user-data-dir="$HOME/.cache/kwin-mcp/cdp-profile" https://example.com/
```

The first two run on the session's copy of your signed-in Chrome profile (the `launch_app` description lists its profile directories), the third on a profile of its own. Browsing with those sign-ins is what the copy is for, and the server instructions and the `launch_app` description say so in one sentence each: an agent that read only cautions about cookies took the profile for off limits (#962). The session's cookies and site storage carry to the next session through kwin-mcp's state, and the host profile is never written (see Browser cookie continuity and Browser page storage continuity). The session assigns the CDP port. Do not pass `--remote-debugging-port`; if supplied, `launch_app` replaces it with the session port. A successful connection returns `cdp_port` in the structured result, and that port is forwarded to the host on `127.0.0.1`. For a session started with `host`, `cdp_port` is a port of the caller's own `127.0.0.1` too (#961): the caller's server listens there, the same number as on the session's machine when it is free, and carries each connection over a WebSocket of its own to the listener (path `/cdp`, the shared secret, the port in a header), which joins it to that port of its loopback; `cdp_forwarded_from` names the host and its port. The listener joins a peer only to the CDP port of one of its live sessions, never to another service on its loopback. The forward ends with the session and is kept, on the same port, when the caller's server re-executes itself; `browser_navigate`, `browser_read` and `browser_eval` do not need it, since the listener reaches its own port.

The port is forwarded to the host's `127.0.0.1` without authentication, so any program running as you can drive that browser, its signed-in profile included; pass `cdp: false` to `launch_app` to start a browser without a port (`browser_navigate` then goes through the address bar).

Chrome 136 and later, and Edge, [ignore remote-debugging switches for the default data directory](https://developer.chrome.com/blog/remote-debugging-port). They decide what is the default by comparing the directory they run on, resolved, with the path they derive from `CHROME_CONFIG_HOME`, as text. So for `google-chrome`, `google-chrome-stable`, `microsoft-edge`, `microsoft-edge-stable` and `msedge` started without `--user-data-dir`, the wrapper sets `CHROME_CONFIG_HOME` for that browser process alone to a directory in the session workdir (`browser-config/…`) whose `google-chrome` or `microsoft-edge` entry is a symlink to the real data directory, and passes the real path as `--user-data-dir`. The browser runs on the same directory as before, with the same lock, cookies, extensions and profiles. Nothing is copied, the session's other programs do not see the variable, and a browser that Chrome itself starts inherits it and reaches the real directories through the same symlinks. The real directory is `${CHROME_CONFIG_HOME:-${XDG_CONFIG_HOME:-$HOME/.config}}/<name>`, resolved; a `--user-data-dir` in a distribution launcher's flags file (Arch's `~/.config/chrome-flags.conf`) is followed instead of doubled. Chromium, Brave and Vivaldi open the port on their default directory by themselves and are started as before, as is a program named `chrome`.

Should a later Chrome resolve that symlink and refuse the port, `launch_app` reports `cdp_refused` once; after the browser is closed and launched again, the session gives it a default directory with no profile in it, which also opens the port (checked on Chrome 156 and, headless, on Edge 157), at the cost that a browser started by that browser without `--user-data-dir` gets a new profile.

Google Chrome's branded builds ignore `--load-extension`. When `launch_app` sees that switch with `google-chrome` or `google-chrome-stable`, it sets `extension_warning` in the result and explains the supported path: open `chrome://extensions`, enable Developer mode, choose **Load unpacked**, and select the extension directory in the isolated session. Passing `--disable-features=DisableLoadExtensionCommandLineSwitch` does not restore command-line loading; use unbranded Chromium if command-line loading is required.

## Resize a maximized session window

Maximization is applied only when a window first appears; it does not force the window to remain at the display size. To restore it through KWin's window menu, send `keyboard_key` with `key: "alt+F3"`, inspect the menu, and click its checked **Maximize** item. Wait for `window_list` to report the restored geometry: the menu action and application resize complete asynchronously. The session's configured restore/maximize shortcut is another option. Read `x`, `y`, `w`, and `h` with `window_list`, then hold Super with `keyboard_press` (`key: "super"`) and call `mouse_drag` with `button: "right"` from ten pixels inside the lower-right corner to the desired lower-right point. `mouse_drag` coordinates are relative to the active window. Release Super with `keyboard_release` (`key: "super"`), then verify the geometry with `window_list` or screenshot metadata.

Session windows have no title bar or border (`window_mode` removes decorations in both modes), so a drag from a window edge has nothing to grab. In observed runs on a native-mode window, Super+right-button drag (above) resized it, while a drag from the corner pixel, Alt+right-button drag and Alt+F8 with arrow keys did not. `screenshot` returns "target changed or moved during capture" while the active window's geometry is still changing; capture again once it has stopped.

KWin's own menus have no accessibility tree, but each menu and submenu is a `popup: true` window in `window_list`, so a call after a hover or click shows whether the submenu opened. In observed runs hovering the arrow end of a submenu row opened it, while clicking the row's text did not always.

A successful `org.kde.kwin.Script.run` response does not verify the resulting window geometry. Check `window_list` or screenshot metadata before reporting a resize as successful.

`session_start` leaves the host viewer closed. Work without a viewer unless the user must act on the session (for example, Duo, OTP, CAPTCHA, or approval) or asks to watch. Before asking for information, put the relevant form or empty entry on screen, then call `viewer_open` with a short `reason` explaining what the user should do. Never assume the viewer is already open. You can keep your turn open and poll screenshots for completion instead of requiring a chat confirmation. Continue when the screen shows that the step is complete. Viewer input is forwarded immediately. If it arrives during an agent mouse or keyboard operation, the remaining agent operation is cancelled and reports `retry_safe: false` because partial input may already have arrived; inspect the target before retrying. Agent mouse and keyboard calls return a retry-safe error for one second after the latest viewer input; retry after one second without viewer input. The viewer may remain open while agent input resumes, and the user can close it directly. Use `viewer_close` when the user asks. If a Duo push expires, tell the user in one line and leave the page on the resend option so they can retry; continue when the page advances. `session_start` reports the viewer as `closed`; `viewer_open` reports `ready` after the host renderer successfully submits the first PipeWire frame for presentation, or `unavailable` with a reason (for example, no active host Wayland session when serving over SSH). This reports renderer submission, not compositor acknowledgement. The old `--no-viewer` argument remains accepted for existing server commands.

**Starting kwin-mcp from a sandboxed client.** A client that runs inside its own sandbox (for example a Codex worker under `bwrap`) starts the server with `NoNewPrivs=1`, as does anything that client starts in turn (a mux, its shims, a test runner). `session_start` works there: the server starts the session's namespaces in an order that needs no privilege from `exec` (see `src/netns_holder.rs`), inside the client's sandbox, with the same scope, limits and teardown. The session is built on what that sandbox sees, not on the host outside it. `session_start` still fails with `setting up uid map: Operation not permitted` or `cannot create the session's user and network namespaces` when the client's sandbox forbids nested user namespaces altogether (`bwrap --disable-userns`, a seccomp filter, `user.max_user_namespaces=0`); the error reports the settings it saw. Start the server outside such a sandbox: set the client's server command to `systemd-run --user --quiet --wait --pipe --collect /path/to/kwin-mcp --width 1920 --height 1080 …` (`--pipe` keeps the stdio transport; the unit runs from the user manager).

**Viewer on a host with no desktop.** `viewer_open` shows the session on the screen of the host the server runs on. When that host has no desktop session (a headless build or GPU box, or a worker that cannot reach one), it reports `unavailable` with the reason and a ready command: `kwin-viewer --remote HOST /tmp/kwin-mcp-<pid>-<n> WIDTH HEIGHT`. Run that command on the machine whose screen should show the session, inside its logged-in desktop session (from an SSH shell there it finds the user's `wayland-N` socket in `XDG_RUNTIME_DIR` or `/run/user/<uid>` by itself; with no live socket it says so). That machine must reach HOST over SSH without a prompt (`BatchMode=yes`, so key or agent authentication; a password prompt makes it fail immediately), and `kwin-viewer` must exist at the same path on HOST (set `KWIN_VIEWER_REMOTE_BIN` otherwise). The viewer shows the session and forwards input to it; closing it or stopping the session ends it. The server reaches the desktop even when logind lists no active Wayland session, as long as a live `wayland-N` socket is in the runtime directory.

Pass `--autoclean` to remove the server-owned workdir and its disk-backed overlay cache after stop, failed start, timeout, transport close, launching-client death, or SIGTERM/SIGINT/SIGHUP. Cleanup repairs permissions only inside those directories and does not follow symlinks. If either removal fails, `session_stop` reports the failing path and keeps ownership for a retry. Without the flag, the workdir remains after stop. Because `host_shared_dir` is that workdir, files host tools wrote there are deleted with it; `session_start` reports `workdir_removed_on_stop: true` (and `ttl_minutes`) when this server runs with `--autoclean` or `--ttl`, so copy outputs elsewhere before `session_stop`.

On startup, `--autoclean` also removes a workdir left by a crashed server when this version's lease marker proves it opted into cleanup, its lock is free, its sandbox process group has exited, and no mount remains inside it. Older workdirs without a lease marker remain for individual review.

Pass `--ttl MINUTES` to tear down an idle session and its viewer after that many minutes without a tool call. The server stays available for the next `session_start`; `--ttl` implies `--autoclean`, so expiry also removes the workdir.
Each `session_start` attempt records its outcome, duration, server PID, commit, and failure reason in `$XDG_RUNTIME_DIR/kwin-mcp/session-starts.log` when `XDG_RUNTIME_DIR` is set. Run `kwin-mcp --stats [SECONDS]` to print successes and attempts in the requested window (3600 seconds by default), followed by the five most common failure reasons.
To run the MCP server on another host, set the client's stdio command to `ssh -T HOST /path/to/kwin-mcp`. SSH carries MCP requests and screenshots while the isolated desktop runs on `HOST`. The remote host needs the same KWin, pasta, and device dependencies as a local session.

### Sessions on another machine (`--listen` and `session_start` `host`)

One server can also place individual sessions on other machines over the LAN, without SSH. Each machine that should host sessions runs one WebSocket listener:

```bash
install -d -m 700 ~/.config/kwin-mcp
(umask 077; head -c 32 /dev/urandom | base64 > ~/.config/kwin-mcp/remote-secret)
kwin-mcp --listen 192.168.1.20 --autoclean --ttl 120
```

To keep the listener running across reboots, install [`systemd/kwin-mcp-listen.service`](systemd/kwin-mcp-listen.service) as a user unit, put the address in `~/.config/kwin-mcp/listen.env` as `KWIN_MCP_LISTEN=192.168.1.20`, then run `systemctl --user enable --now kwin-mcp-listen` and, on a machine nobody logs in to, `loginctl enable-linger`. The unit holds no session alive: each session still ends with its owner's connection or its `--ttl`.

`--listen ADDRESS[:PORT]` takes the IP address to bind (default port 47913) and serves kwin-mcp peers instead of an MCP client on stdio; the other options (`--ttl`, `--autoclean`, `--width`, memory limits) apply to the sessions it starts. `--keep-sessions` is rejected with it. `--link-mbps N` sets the link speed its viewer frames are budgeted against (see **Viewer** below).

Then pass `host` to `session_start` on the local server: a hostname or IP address with an optional `:port`, for example `{"host": "sentry"}` or `{"host": "192.168.1.20:47913"}`. The local server connects to that listener (one connection per host, reused by later starts), and the listener starts the desktop as its own child process exactly as a local `session_start` does. The returned `session_id` is the listener's; every later call with it is routed over the socket and its result returned unchanged. `session_list` gives each session a `host`: this machine's hostname for local sessions, the `name:port` passed to `session_start` for remote ones.

**Shared secret.** Both sides read `${XDG_CONFIG_HOME:-~/.config}/kwin-mcp/remote-secret`: one line of printable ASCII, in a regular file owned by the user with mode 0600. Copy the same file to every machine. The listener refuses to start when the file is missing, empty, or readable by group or others; it accepts only peers whose WebSocket handshake presents the secret (`Authorization: Bearer`, compared in constant time) and answers others with 401. The secret is never logged. The connection is plain `ws://`, so the secret and all session traffic cross the network unencrypted: bind a LAN address and do not expose the port beyond a trusted network.

**Host needs.** A machine that hosts sessions needs `kwin`, `passt` and `bubblewrap`, read/write access to `/dev/uinput` for the listener's user, and a DRM device under `/dev/dri`. On a compute-only GPU host with no DRM driver loaded (NVIDIA without `nvidia-drm`), KWin composites with QPainter and screen capture is cancelled; `session_start` then returns `graphics_warning`. Load the software device instead: `echo vgem | sudo tee /etc/modules-load.d/vgem.conf`, a udev rule `SUBSYSTEM=="drm", DRIVERS=="vgem", MODE="0666"`, then `sudo modprobe vgem`.

**Lifetime.** The listener keeps no session alive by itself. A session it starts belongs to the connection that asked for it: `session_stop` and `--ttl` end it as they do locally, and when that WebSocket drops — the local server's MCP client closed, the local server exited or was killed, or the machine vanished (TCP keepalive notices within about 25 seconds) — the listener stops every session that connection started, as a stdio server does when its client exits. The local server closes its remote connections when its own client closes, even with `--keep-sessions`. An in-place reload of the local server (it re-executes itself when its binary changes, which every install causes) keeps them: once no request to the listener is under way and the control socket stands between two WebSocket messages, the new process image inherits the socket's descriptor and continues on the same TCP connection without a new handshake, so the listener sees no disconnect, the session IDs keep working, `session_list` still shows the sessions with their `host`, and an open viewer window, with its own frame socket, runs on. The listener has no part in this and no grace period. The connections close with the old image as before, and the local server's log says why, when a connection is not between two messages within 2 seconds, when a request to a listener has not returned after 30 seconds, or when the new binary is a build without this handover (an older one installed over a newer one). The descriptor becomes inheritable only in the exec call itself, so no process the server starts ever holds the connection, and a server that exits or is killed hands nothing over. After a drop, calls with the old ID report `remote_connection_lost`, `session_list` shows `connection_lost`, and `session_stop` clears the handle. The next `session_start` with that `host` dials the listener again, also when the drop was the listener's own restart, and a start whose connection ends before the listener answers goes once more on a new connection; the sessions of the ended connection stay `connection_lost` (#960). A peer reaches only the sessions its own connection started; `session_attach` and forwarding to other processes are unavailable over the socket.

**Paths.** A remote session's files are on the remote machine: `host_shared_dir`, screenshot `path`s, `export_file` destinations and `import_file` sources all name that machine's filesystem. Use `screenshot` with `inline=true` to receive the image itself.

**Viewer.** `viewer_open` on a remote session opens the viewer window on the local machine (the one whose server received the call), and `viewer_close` closes it; both report `ready`, `unavailable` with a reason, or `closed` as they do for a local session, plus the `host`. The local server starts `kwin-viewer --stream HOST:PORT DIR`, which opens a second WebSocket to the listener (path `/frames`, same shared secret) carrying a one-time ticket; the `viewer_open` call forwarded on the control socket carries the same ticket, which binds the frame socket to that session of that control connection. The listener hands the socket to `kwin-viewer --stream-serve`, a child process beside the session that captures its PipeWire stream. No SSH is involved, and MCP calls stay on the control socket, so frames never delay a tool call.

The stream puts colour accuracy first, then reasonable use of the hardware, then stability. It has no frame-rate target.

- Frames go out as binary messages only when the screen changed: PipeWire delivers on damage, and a delivery whose pixels equal the last one is dropped. One frame is on its way at a time: the sender takes the newest capture once the last byte of the previous frame has left for the viewer (the kernel reports no unsent bytes for the socket), so nothing queues behind a slow or stalled viewer. The socket's kernel buffers are held to 1 MiB.
- The viewer reports its drawable size in physical pixels at start and on every resize. Each frame is scaled down to that size with an area filter before it is sent, and never scaled up past the session size, so a frame of one viewer size and mode always has the same length: `24 + width * height * 3` bytes raw, `24 +` the DSC picture length otherwise.
- Budget: the speed of the interface that holds the socket's local address (`/sys/class/net/<if>/speed`) times 0.9; loopback is unlimited; an interface that reports no speed counts as 1000 Mb/s. `kwin-mcp --listen ... --link-mbps N` replaces the speed of every interface the listener sends on. The kernel paces each frame socket (`SO_MAX_PACING_RATE`) so that the streams on an interface stay inside its budget together, which leaves a tenth of the link to everything else and makes `--link-mbps` on loopback or a faster link behave like a link of that speed.
- Several viewers on one interface share its budget. The frame senders are separate processes, so the listener runs a scheduler for each interface it sends on (`LinkScheduler` in `src/main.rs`; the rule is `Shares` in `src/links.rs`): a sender tells it when a frame goes onto the wire and when its last byte has left, on a socket that is the sender's standard output, and the listener answers each change with what every sender on that interface may send now.
  - Pace: the budget is divided equally among the streams that have a frame on the wire at that moment, and each one's socket is re-paced at once, also in the middle of a frame. The paces of the sending streams therefore add up to the budget, frames of different streams go out at the same time, a stream alone has the whole budget, and the part of a stream that stops sending is taken over immediately.
  - Share: what the mode rule below works with. A stream counts as streaming from its second frame within 250 ms of the one before until 250 ms after its last, and a stream's share is the budget divided among itself and the other streaming streams. One frame after a still screen (a click, a blinking cursor) therefore changes nobody's mode, and neither do the gaps between the frames of a scrolling screen. When the share changes, the sender's measured link speed changes in the same proportion at once, so the next frame is decided for the share the stream has (four frames in a row are still needed to change the mode), and the sender tells the viewer (`share` text message).
  - A stream whose viewer reports a slower link of its own (see paths below) gets no more than that link carries, and the rest goes to the others.
  - One viewer on one interface behaves as before: its pace and its share are the budget. A sender whose listener runs no scheduler (an older listener that is still running after an install) paces itself to the whole budget.
- Paths: when the caller's machine has more than one interface that reaches the listener (an interface that is up, has a carrier and holds an address in the listener's subnet, or the one the routing table uses), each viewer's frame socket is put on one of them, and stays there.
  - Choice (`choose` in `src/links.rs`): a new stream takes the path that gives it the largest fair share, which is the path's usable speed divided by the streams it would then carry, and no more than the sending machine's interface gives one more stream (streams of one machine leave it on one interface whatever path they arrive on, so a second path gains them nothing). Usable speed is 0.9 times a wired interface's speed and 0.5 times the rate a wireless interface receives at (read from the kernel's nl80211; the rest is the medium's overhead). A wired path wins ties and is always taken while it carries no stream, so the first stream is on the wire; a wireless path is used only when its fair share is larger than the wired one's, never when it is slower than a fair share of the wire. With a 1000 Mb/s wire and Wi-Fi receiving at 864 Mb/s (432 usable), the second stream stays on the wire (450) and the third goes to Wi-Fi (432 against 300).
  - The streams of every kwin-mcp server of the user are counted: each stream viewer records its path in `$XDG_RUNTIME_DIR/kwin-mcp-streams/` under a lock, and files of processes that are gone are ignored.
  - Binding: a socket on the routing table's own path is not bound at all. Any other is bound to the path's address before it connects, and to the interface with `SO_BINDTODEVICE`, which Linux allows without privileges since 5.7 for a socket not yet bound to a device (verified on 7.2). With the address alone, the listener sends the frames to that address, but what the viewer sends (acknowledgements, input) leaves on the interface the routing table picks: on one subnet Linux routes by destination, not by source address. Whether frames addressed to an interface's address also arrive on that interface is the receiving machine's ARP policy (`arp_ignore=1` answers for an address only on its own interface). A viewer on a path of its own choosing tells the sender that path's usable speed (`path` text message), and the sender paces to no more.
  - A stream moves only when its path dies: the interface disappears from the usable paths (checked once a second), or the socket ends with a timeout or an unreachable network or host, while another path exists. A socket the session's machine closed or reset ended over a working path, and closes the window as before. The viewer asks the server that started it for a new ticket (a Unix socket beside its log, answered only for the same user), opens a frame socket on another path, and the listener starts a new frame sender on it in place of the old one. The window stays open with the picture it shows until the first frame arrives on the new socket; it repeats its drawable size and the pointer position to the new sender (which pauses agent input for a moment, as any viewer input does), and input typed in between is dropped. No frame is ever split across paths.
  - `KWIN_VIEWER_STREAM_PATHS=FILE` replaces the machine's interfaces with the paths listed in a JSON file (`{"paths":[{"interface","source","mbps","wireless"}], "registry": DIR}`), for tests.
  - Not built: the session's machine does not offer its own other addresses, so when that machine has two interfaces all its streams still leave on the one its routing table picks; and streams from different machines that arrive on one interface of the caller are not scheduled against each other there (each sender paces to its own interface's budget).
- Mode: raw frames are the session's exact pixels (3 bytes each: red, green, blue), and they are what goes out whenever the link carries them at a usable rate. The sender measures the link (`link`: bits per second, from each frame of at least 64 KiB and the time until its last byte left; it starts at the budget and moves a quarter of the way to each measurement) and its own time to pack a raw frame and to encode a DSC frame (per pixel, smoothed the same way; while raw is too slow and no DSC frame gives the encode time, one frame is encoded just to measure it, before the first decision that needs it and again after every 32 raw frames). With `raw_time = pack + pixels * 24 / link`, `rate` = the highest whole DSC rate from 6 to 12 bpp whose frame crosses the link within 100 ms, and `dsc_time = encode + pixels * rate / link`:
  1. raw while `raw_time <= 100 ms`;
  2. from raw to DSC when `raw_time > 100 ms` and `dsc_time <= 0.75 * raw_time` (DSC is used only when it delivers at least a quarter sooner);
  3. from DSC back to raw when `raw_time <= 90 ms` or `dsc_time >= raw_time`;
  4. a change needs the rule to ask for it on 4 frames in a row; the first frame of a new size decides at once by 1 and 2;
  5. the DSC rate follows the link alone, never the encode time: it rises when 95% of the link carries a higher whole rate and falls when the link no longer carries the current one, each on 4 frames in a row. A slow CPU never lowers it; when encoding is too slow for rule 2 or 3 the frame goes out raw instead.

  Examples at 1920x1080 (6,220,800 bytes raw): a 1 Gb/s link carries a raw frame in 55 ms, so every frame is exact; at 500 Mb/s raw takes 111 ms and DSC at 12 bpp 55 ms plus the encode, which is a quarter sooner only when the encode takes under 28 ms; at 200 Mb/s raw takes 276 ms and the link carries 8 bpp (92 ms); at 100 Mb/s it carries 6 bpp (138 ms).
- Settle to exact: when the last frame went out as DSC and the screen then stays unchanged for 150 ms, the same frame goes out once more raw (flag bit 0 of the header's second byte marks it), so a still screen is always shown with exact pixels. A settle frame is not a change of mode.
- The viewer uploads each frame to an `R8G8B8A8_UNORM` image and blits it to a `B8G8R8A8_UNORM` (or `R8G8B8A8_UNORM`) swapchain in the sRGB non-linear colour space, chosen explicitly: no transfer curve or colour-space conversion is applied, and a frame of the window's own size is copied texel for texel (nearest filter; other sizes are interpolated). A raw frame at the session's size therefore reaches the screen bit for bit.
- A window the compositor shows redraws on its frame callbacks, once per refresh. A compositor that is not showing the window (it is minimized, on another virtual desktop, or the outputs are powered off) sends none, so nothing asks the window to redraw. The viewer, local or remote, then draws and presents a frame that has waited 100 ms on its own, about seven times a second, through a mailbox swapchain, whose presents never wait for the compositor to show the previous one. The window's content is therefore current in a screenshot or a thumbnail and when it shows again, and a stream that ends still closes it. The log line counts these presents (`N presented (M of them while the window was not shown)`), as does `presented_unshown` in `viewer-stream.json`: frames arriving with nothing presented means the window is stuck, not hidden.
- Viewer input (keys, buttons, motion, scroll) returns on the frame socket as 17-byte records and is applied at once; it pauses and cancels agent input exactly as local viewer input does. At most 4096 records wait for a host that has stopped reading; beyond that the viewer closes instead of dropping one. Num Lock is not synchronized.
- Clipboard: the remote viewer bridges it as the local viewer does, between the remote session and the machine the window is on (the caller's). Ctrl+V or Shift+Insert in the window sends that machine's clipboard text ahead of the key, and the sender makes it the session's selection for that paste. Ctrl+C or Ctrl+X (also Ctrl+Insert and Shift+Delete) asks the sender for the session's selection once the key has changed it (the sender waits up to 10 seconds), and the window puts the answer on its machine's clipboard. After either, the sender puts the agent's selection back, so `clipboard_read` and the agent's own pastes see what the agent left there.
  - What crosses: UTF-8 text only, as JSON text messages on the frame socket (`clipboard_paste` and `clipboard_copy` from the window, `clipboard` from the sender), sent in order with the key records. One text is at most 1,048,576 bytes (1 MiB). A longer text, or a selection that is not text (an image, files), is refused whole, never cut: the paste then inserts nothing, not the agent's selection, and a copy leaves the caller's clipboard as it was. The local viewer carries every MIME type; the remote one carries text.
  - **The frame socket is plain `ws://`, so clipboard text, including a password pasted through the window, crosses the LAN unencrypted** and can be read by anyone who can capture traffic between the two machines. It is written to no log and returned in no tool result.
  - Isolation: a frame socket belongs to one session (its ticket) and one window, and clipboard messages travel on no other socket, so a window bridges its own session only. A window takes a `clipboard` text only as the answer to the last copy shortcut pressed in it, within 15 seconds. A session selection that carries the generated-secret marker is the agent's own, not something an application copied: the sender never sends it in answer to a copy.
  - Either machine may run an older build. The sender announces the bridge in its `hello` (`clipboard_text_bytes`), a window sends no request to a sender that does not, an older sender ignores text messages, and both ends ignore text message types they do not know. Without the bridge the shortcuts act inside the session only, as before. The window also runs unbridged, and says so in its log, when its compositor offers no `ext_data_control_v1`.
- `screenshot` is unaffected: it captures at full session resolution on the session's machine and returns over the control socket.
- When the window closes, the frame socket ends and the sender exits; when the sender ends, the window closes; when the control connection ends, the listener stops the session and its sender. The local server closes the window when the session stops or the server exits.

The viewer logs one line per second while frames arrive and one when they stop (`viewer.log` in the directory `viewer_open` reports), and writes the same counters to `viewer-stream.json` beside it: frames, bytes, size, mode of the last frame, `stream_mode` (the mode a settle frame aside), bits per pixel, mode switches, settles, the swapchain format, and for the last second frames per second, megabits per second and capture-to-present latency. Latency compares the two machines' wall clocks, so it is exact on one machine and as good as their clock synchronization otherwise. Each line ends with the path and the share, for example `path enp10s0 (192.168.1.211, 1000 Mb/s) from sentry:47913 (enp0s25, 1000 Mb/s); share 450 Mb/s of 900 Mb/s among 2 streams`, and `viewer-stream.json` has them as `path` (`interface`, `source`, `link_mbps`, `wireless`, `bound`: `route`, `device` or `address`, `sender_link`, `budget_mbps`, `share_mbps`, `streams`, `reconnects`). `viewer_open` returns the same object as `viewer.path`. The sender logs each mode or rate change with the times the rule compared, each change of its share, and once a second its scale, pack or encode and wire times and what it sent of its share (`36 Mb/s, its share of 72 Mb/s among 2 streams on lo, --link-mbps 80`), in the session directory's `viewer.log` on its machine. With `KWIN_VIEWER_STREAM_DUMP=FILE` in its environment, the viewer writes the frame it last presented to `FILE` as a PNG whenever a second passes without a new one.

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

## Clipboard isolation

KWin MCP operates its own isolated desktop session. The viewer displays and
controls that session; it does not turn the tools into controls for the user's
active desktop session. Both clipboard tools require a live KWin MCP session,
including `clipboard_write` with `target: "host"`.

| Operation | Clipboard and scope |
|---|---|
| `clipboard_write`, `target: "session"` | The selected isolated session's CLIPBOARD and PRIMARY, used for agent input. |
| `clipboard_write`, `target: "host"` | The Wayland clipboard on the desktop of the server that received the call, which is where `viewer_open` opens the window. The viewer's copy/paste bridge uses it. For a session started with `host` it is still that calling machine's clipboard, never the session machine's. |
| `clipboard_read` | The selected isolated session's text selection only; for a session started with `host`, the remote session's. |
| Viewer Ctrl+C / Ctrl+X | Copies or cuts from the isolated application to the clipboard of the desktop the viewer window is on. |
| Viewer Ctrl+V / Shift+Insert | Pastes the clipboard of the desktop the viewer window is on into the isolated application. |

`host` does not mean the desktop of a `kwin-viewer --remote` window (the SSH
viewer, which has no clipboard bridge) or whichever active
desktop a user names. A request to change the user's active desktop clipboard
outside this live session/viewer workflow should use the appropriate separate
desktop mechanism; starting a KWin MCP session does not change that request's
destination.

The viewer restores the isolated session's prior selection after a clipboard handoff: the local viewer itself, and for a session started with `host` the frame sender beside the session.

**Sessions started with `host`.** The user and the viewer window are on the calling machine, so the two targets live on different machines:

- `clipboard_write` with `target: "host"` is carried out by the calling server and is not forwarded. The text, or the secret `generate_secret` asks for, is made and written on the calling machine and is in no message to the session's machine. It crosses the network only when the user pastes it in the viewer window: then it travels on the frame socket (plain `ws://`, unencrypted on the LAN) to the frame sender, which makes it the session's selection for that one paste and then restores the agent's. See the clipboard item under [Sessions on another machine](#sessions-on-another-machine---listen-and-session_start-host) for the 1 MiB limit and what else crosses.
- `clipboard_write` with `target: "session"` is forwarded like any other call. With `generate_secret` only the parameters cross the control socket: the listener generates the secret on the session's machine, where it is written, and answers with the length alone. A `text` argument crosses the control socket as every tool argument does, unencrypted.
- `clipboard_read` is forwarded and returns the remote session's text over the control socket; a generated secret is withheld there as it is locally.
- A generated secret is never logged, by either server or either end of the viewer stream, and is in no tool result.
- A `--listen` server refuses `clipboard_write` with `target: "host"` from a peer (`reason: host_clipboard_is_the_callers`). Calling servers older than the bridge forwarded that call, which wrote the clipboard of a desktop where nobody was waiting for the text; update the calling machine.

A session write fills both CLIPBOARD and PRIMARY, so X11 applications that paste from PRIMARY can consume it. In XTerm, the default fast paste shortcut is Shift+Insert; Ctrl+V is not bound to paste by default. Provide exactly one of `text` or `generate_secret`. For example, `{"target":"host","generate_secret":{"length":20,"character_classes":["lowercase","uppercase","digits","symbols"]}}` generates a value using operating-system randomness and includes every selected class. The response contains only its target and character length.

`clipboard_read` reads session text only. Generated selections carry a secret marker, so this tool returns `text: null`, `protected: true`, and the length while that marker is present. This prevents an accidental tool readback; it does not restrict applications that can read the clipboard. Clipboard ownership survives server reload and ends when the server exits. Session clipboard ownership also ends when its session stops. The host target means the desktop of the server that received the call. Text a viewer shortcut copied to that desktop is owned by the viewer window's process and ends with it unless a clipboard manager keeps it.

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

A session never talks to the host wallet or Secret Service. `session_start` takes one read-only copy of the host KWallet, and the session's apps then use a session-local `org.kde.kwalletd6` served from memory; their writes stay in the session and end with it. The copy is taken only when `kwalletd6` and `ksecretd` already run and the wallet is unlocked: kwin-mcp never starts, unlocks or prompts a host wallet service, never asks `org.freedesktop.secrets` about the wallet, and serializes copies across servers, at least 100 ms apart, within 12 s per `session_start`. If `kwalletd6` is missing or does not answer, the same wallet is read from `ksecretd` with the standard Secret Service calls, and copies in the next 60 s read `ksecretd` directly. If any check fails the session gets a disabled KWallet, `session_start` reports why, and `launch_app` of a Chromium-family browser says that the browser cannot decrypt the copied profile: Chrome drops every cookie it cannot decrypt (a recovered 3,606-cookie profile fell to 8 rows with the `basic` store, and kept 3,311 with the wallet). The host session D-Bus socket and the `keyring` and `p11-kit` entries of the user runtime directory are never reachable from a session.

### Secret Service

Applications that keep credentials in the desktop keyring through `org.freedesktop.secrets` (libsecret and keytar clients such as GitHub Copilot CLI, `gh`, Python `keyring`) get a session-local Secret Service on the same private bus, under the wallet's rules. `session_start` takes one read-only copy of the unlocked collections of the host's Secret Service, whichever daemon owns `org.freedesktop.secrets` (gnome-keyring, or `ksecretd` on a KDE-only host), and reports it as `secrets` (`mode`, `items`, `reason`). It sends only `OpenSession("plain")`, property reads, `GetSecrets` and `Session.Close`, one after the other on one connection, never starts the service, and never calls `Unlock` or a prompt: a locked collection or item is left out and named in `reason`. A connection whose request was not answered is kept open until the server exits, because gnome-keyring exits when a client disappears mid-request ([#156](https://github.com/nm-z/kwin-mcp/issues/156)). Everything after the copy is answered from memory: items a session stores, changes or deletes stay in the session and end with it, so a sign-in made in a session goes to a keyring instead of a plain text file, and nothing a session does reaches the host's keyring. The `default` alias always resolves (to a session-only collection when the host's default collection was not copied), and only the `plain` session algorithm is offered; clients that ask for the Diffie-Hellman one fall back to it. When the host collection is locked, as after a keyring daemon restart, apps in the session are signed out until they sign in there: unlock the keyring on the host and start a new session to carry the host's sign-ins over.

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
exact `session_id` and the `owner_pid` encoded in that handle.
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
`session_start` counts these fallback files in `host_access.oversized_read_only_files`
and lists them in the `host_access.report` file.
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

`session_start` counts the live databases it discovers and the oversized files
it binds read-only, and names no host path for them in its result (#957):
`host_access.sqlite` has the number of databases per mode (`live_read_only`,
`private_writable_snapshot`, `shared_host`), `host_access.oversized_read_only_files`
the number of such files, and `host_access.notes` each database whose requested
access could not be given, with why. `host_access.report` is a JSON file in the
session's workdir with the lists themselves: `sqlite_access` (`databases`, at
most 128, each with its `path` and `mode`, and `total_databases`) and
`oversized_read_only_files`. Empty results (`cookie_errors`, `host_access`,
`host_write_paths`, `runtime_root_fallback`) are left out. A `launch_app` result for a window it found
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

Each new sandbox runs in a systemd user scope with `MemoryHigh=6G`,
`MemoryMax=8G`, and `MemorySwapMax=2G`, enough for Chrome on a real profile
with about 15 tabs (#839); smaller caps throttled every call there. Startup fails if the user manager
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

The dead entry keeps what the teardown would otherwise delete: `exit_summary`
(which of `kwin_wayland` and the sandbox's top process ended first, and what
the exit status says), `ended_first`, `kwin_exit_status`, `exit_code` or
`exit_signal`, and `sandbox_log_tail`, the last 2 KiB of the session's stderr.
Every later call for that session returns them, and the lifecycle record keeps
the summary and the end of the log as `detail`. pasta, the top process of a
session's scope, exits with status 0 when it receives SIGTERM or SIGQUIT, so
`exit status: 0` with `kwin_wayland` still running means the session was ended
from outside the server (a signal to its processes, or its scope being
stopped), not that the desktop finished. `scope_properties` holds only
`LoadState` once systemd has forgotten the scope: its other properties would be
defaults, and `Result=success` among them says nothing.

`session_stop` answers within 30 seconds. It normally takes a fraction of a
second; a stop that took over 2 seconds returns `stages`, the time of each
teardown stage, and names the slowest in its text. The graceful part (KWin
closes browser windows within 3 s, CDP within 2 s, browsers exit and flush
cookies within 2 s and 6 s more after SIGTERM) ends at the latest 15 seconds
before the limit; the session's processes are then killed and its files
deleted. When the limit comes first, the answer is `status: cleanup_pending`
with `session_status: stopped`, `unfinished.stage` and whether every process
has already ended; the cleanup goes on by itself and the next `session_stop`
reports how it ended.

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

Launched applications share a private app bus with the session-local KWallet.
They can register service names and communicate with other session apps; this
bus has no connection to host desktop services. The host system-bus proxy keeps
its NetworkManager filter. Qt applications log to stderr so launch failures can
report their diagnostics even without a journal socket in the sandbox.

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

At `session_start`, active pathname sockets beneath `$HOME` and non-graphical user-runtime sockets are exposed automatically. Sockets owned by processes attached to the host display, desktop application scopes, input devices, or the desktop session slice remain isolated. Beneath `$HOME` such a socket is absent in the session, not replaced by a placeholder: a whiteout in the overlay's upper layer hides the name, so an application that looks for its single-instance socket (Zed's `zed-stable.sock`) sees no running instance, starts its own, and creates its own socket in the overlay ([#869](https://github.com/nm-z/kwin-mcp/issues/869)). Only where there is no overlay to hide it in (a `host_write_paths` directory, a read-only bind) is the name masked with `/dev/null`. The host's Avahi socket (`/run/avahi-daemon/socket`) is bound read-only when it exists, so `.local` names resolve in a session as on the host; it answers lookups only. Parent directories are mounted read-only, which prevents host file writes but does not restrict operations offered by each exposed socket protocol. Hidden parent mounts also expose sibling files through their internal `/run/kwin-mcp-host-sockets` paths, except the host session D-Bus socket and the `keyring` and `p11-kit` entries of the user runtime directory, which are masked and never exposed. Socket replacements at discovered names remain live; new socket names require a new session.

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
- `screenshot` replaces `screenshot.png` in the workdir on every call; pass `save_as` with a new absolute host path to keep a copy. With `save_as`, the pointer is omitted by default from the saved copy, workdir PNG, and inline image; pass `include_cursor: true` to include it. Without `save_as`, the pointer is included by default. This also applies to region and cursor crops. `export_file` refuses a host directory that does not exist unless `create_parents: true`.
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

## DSC codec

`src/dsc.rs` (`kwin_mcp::dsc`) is a software VESA Display Stream Compression (DSC 1.1 / 1.2a) encoder and decoder for 8-bit RGB 4:4:4 frames, used by the remote viewer stream (`src/viewer_stream.rs`). It takes `BGRx` or `RGBx` rows with a stride and writes a constant-size bitstream: every frame of one size and rate is exactly `chunk_size * slice_height * slices` bytes (1920x1080 at 8 bpp is 2,073,600 bytes), with the slices of a row interleaved line by line as DSC transports send them. `Pps` is the standard 128-byte picture parameter set the decoder needs.

```rust
let mut encoder = Encoder::new(Config::new(1920, 1080)?)?;   // 8 bpp, 4 slices wide, 108 lines high
let stats = encoder.encode_parallel(&Frame { data, stride, format: PixelFormat::Bgrx }, &mut out, threads)?;
let decoder = Decoder::new(Pps::from_bytes(&encoder.pps().to_bytes())?)?;
decoder.decode_parallel(&out, &mut pixels, stride, PixelFormat::Bgrx, threads)?;
encoder.set_bits_per_pixel_x16(104)?;                        // 6.5 bpp from the next frame on; send the new PPS
```

The rate is set in 1/16 bpp steps from 6 to 12 bpp. `FrameStats` reports per frame the mean quantization parameter (`complexity_permille`), the share of groups reproduced exactly at almost no cost (`flatness_permille`) and the share padded to hold the constant rate (`stuffing_permille`). Slices are independent, so `encode_parallel` and `decode_parallel` spread them over the calling thread and up to `threads - 1` worker threads and give the same bytes as the single-threaded calls. The workers are one pool per process, shared by every `Encoder` and `Decoder`: at most one thread per available core counting the caller, started the first time they are needed, asleep between frames, and stopped and joined when the last codec object is dropped. Each thread takes the next slice when it finishes one. An `Encoder` or `Decoder` keeps its per-thread line and bitstream buffers between frames (also across `set_bits_per_pixel_x16`), so a stream should keep one object rather than build one per frame.

The slice loops exist in up to four copies that give identical bytes: portable code (the only one off x86-64), SSE2, SSE4.2 and AVX2. The three components of a pixel are predicted and quantized side by side in 16-bit lanes, the colour history is searched 8 or 16 entries at a time, and colour conversion works on 8 pixels at a time; the SSE4.2 and AVX2 copies are additionally compiled for those instruction sets. The fastest copy the CPU supports is chosen when a codec object is created; `KWIN_DSC_SIMD=scalar`, `sse2` or `sse4.2` selects a lower one, for comparing hosts or isolating a fault.

The tests compare bitstreams with the VESA C reference model (v1.67, `DSC_model_20211213`) and every copy of the slice loops with the portable one. `cargo test --lib dsc_dump_vectors -- --ignored` with `KWIN_DSC_DUMP_DIR` and `KWIN_DSC_RC_DIR` (the model's directory of `rc_*.cfg` files) writes the inputs, this codec's output and model configurations to rerun that comparison; `cargo test --release --lib dsc_timing -- --ignored --nocapture` prints encode and decode times per 1920x1080 and 1280x720 frame, on one thread and on the pool, for every copy the CPU can run, with the load average. Only 8 bpc RGB 4:4:4 at a constant rate is implemented: no 10 bpc and above, YCbCr, 4:2:2, 4:2:0 or VBR.

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

### Browser page storage continuity

Sites that keep a sign-in in Local Storage or IndexedDB rather than a cookie stay
signed in from one session to the next as well. At `session_start` each Chromium
profile's `Local Storage/leveldb` and `IndexedDB` are copied into the session's
disk (tables as hard links, which LevelDB never rewrites) and mounted over the
profile's own as the read-only lower layer of a separate overlay, so the session's
browser writes only into that overlay. At `session_stop` kwin-mcp saves what the
session changed under `${XDG_STATE_HOME:-~/.local/state}/kwin-mcp/page-storage`
(private, `0700`/`0600`), and the next session starts from the host's stores with
those changes applied. The host profile stays read-only.

Local Storage is carried key by key: a saved key records the value the session
started with and the value it left. The next session takes the saved value where
the host still holds the starting value; where the host changed the key as well,
the later change wins. An IndexedDB origin is carried whole: the saved copy is
used while the host has not written that origin since the session started from
it, and otherwise when it is newer than the host's. Stores that cannot be copied
(for example past a 512 MiB copy limit when the session disk is on another file
system than the profile) keep working as before without continuity, and the
reason is reported in `cookie_errors`. Remove a profile's directory under
`page-storage` to discard what sessions saved for it.


Point your MCP client at `target/release/kwin-mcp`. One server process owns its isolated desktops. Every `session_start` creates a new desktop and returns its ID; this tool does not accept `session_id`. Use only IDs returned by your own `session_start` calls. Pass that ID to later calls; omitting it is permitted only when one session exists. `session_list` lists live desktops owned by that server. Requests carrying a live session ID owned by another kwin-mcp process running as the same user on the same host are forwarded to that process through a private Unix socket.

Session IDs belong to the server process that returned them. A DSH migration or client/server restart does not transfer sessions; when the old client connection closes, that server shuts down its desktops, unless it was started with `--keep-sessions MINUTES`. With that flag a server whose client closed (the usual way a host respawns it) stays up while it owns a running session and serves it, through its control socket, to the kwin-mcp the host starts next: calls with the old session ID are forwarded to it as for any other live same-user server, `session_stop` ends it, and it exits when no session is running or every session has been idle for MINUTES. A signal, or the process that launched the server exiting, still stops everything at once, and `--ttl` still expires idle sessions. An in-place source reload preserves its PID, pipes, and live sessions, including the sessions it started on other machines with `host` and their viewer windows. Calls with a session ID from another live same-user server on this host are automatically forwarded after checking the target PID and effective UID. The replacement server's `session_list` covers only its own process. Session stops, idle TTL expirations, and completed server-shutdown cleanup are recorded in `${XDG_STATE_HOME:-~/.local/state}/kwin-mcp/session-lifecycle-v2.jsonl`; the log is capped at 1 MiB and retains recent records, so older details may be absent. An unavailable-ID error includes `session_status`: `stopped` or `expired` with the recorded end time and reason when known, `owned_by_another_process` when the ID belongs elsewhere but its final state is unknown, or `not_found`. A request without an ID when no session is active reports `none_running`. These responses also report whether the ID is available in the current process and provide recovery guidance. TTL-expired handles appear as `expired` in the current process's `session_list` and can be cleared with `session_stop`. If the owning process is unavailable, call `session_start` without `session_id`, relaunch the applications, and restore the task.

A server that exits says why. On SIGTERM, SIGINT or SIGHUP it logs the sender
(`shutdown: SIGTERM from PID 1234 (pkill, uid 1000)`), answers every call that
is still running, and every call that still arrives, with a
`server_shutting_down` error carrying that cause, and only then stops its
sessions and closes its transport. The cause is kept as `detail` in the
lifecycle record of each session it stopped, so a later call with one of those
IDs, which reaches the server the host started next, is told that the owner
exited, what ended it, and to call `session_start` again. A server that is
killed outright records nothing; the later call still names the exited owner.
A server that re-executes itself for a new binary finishes the calls other
servers forwarded to it first, and a forwarding server retries for up to
5 seconds while the owner is running, so such a call is slower, not lost.

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

Requires: `bubblewrap` (bwrap), `passt` (pasta), and KWin running as a Wayland compositor. Each session has private loopback: TCP ports the host listens on (on a loopback or wildcard address) are forwarded to the host, so session apps reach the real local services; other ports stay private to the session, and only the session's CDP port is forwarded into the host. A port is offered in the session only on the loopback address of a family the host accepts it on (`127.0.0.1` for a server bound to `127.0.0.1` or `0.0.0.0`, `::1` for one bound to `::1`, both for `::`), so `localhost` works whichever family the server uses: the session's kernel refuses the other family and the client falls back, as on the host. kwin-mcp forwards these connections itself (`kwin-mcp --loopback-forwarder`, one per session, logging to `loopback-forwarder.log` in the workdir), because pasta's own forwarding offers every port on both families and resets what it cannot deliver, which failed most `localhost` requests to an IPv4-only server ([#782](https://github.com/nm-z/kwin-mcp/issues/782), [#862](https://github.com/nm-z/kwin-mcp/issues/862)). A listener the host starts after `session_start` is reachable within about a quarter of a second; a port a session process listens on itself is not forwarded. `session_start` reports `host_loopback_warning` if the forwarder could not start. UDP ports are still forwarded by pasta. The sandbox keeps the host hostname so copied HOME profile locks do not look as if they belong to another computer. FUSE works inside the session when the host has `/dev/fuse`, a non-setuid `bwrap`, `setpriv`, and `fusermount`: AppImages mount normally and FUSE filesystems such as sshfs work, while apps still run without capabilities over a read-only host root. Without those, `launch_app` sets `APPIMAGE_EXTRACT_AND_RUN=1` so compatible AppImages run without FUSE. `launch_app` also selects the session's Xwayland display for X11 apps.

## Screenshot dimensions

Apps in the session see a private D-Bus session bus that carries their services and the session-local KWallet, not KWin, so KDE's Spectacle (which needs `org.kde.KWin.ScreenShot2` there) exits with "KWin compositor not available". Use the `screenshot` tool, which captures the session's virtual output through the server's own KWin connection.

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

### Startup budgets and browser profiles

`session_start` accepts `startup_timeout_seconds` from 1 to 300 (default 60).
The budget includes waiting for the lifecycle gate and preparing host profiles;
it does not disable bounded startup or cleanup. Increase it explicitly when a
large profile set needs more preparation time.

Every session inherits the host's Chromium profiles and their cookies. When the
server starts, it reads Chrome's `Local State` and lists each profile's
directory, name and last use in the `launch_app` description, most recently
used first, so an agent can pick one with `--profile-directory="DIR"` without
looking. The list is read once per server process, not per session, and is cut
to fit the 2048-character description limit.

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
read, and a delayed real sandbox to verify operation diagnostics, deferred
cleanup, and configurable timeout bounds.

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

### Launches without a session, slow starts and late windows

`launch_app` without `session_id` on a server that has no session starts a
default desktop first and launches in it. The result then carries `session_id`,
`session_started: true`, a `session_note` and the new session's
`host_shared_dir`, size and wallet state under `session`; `viewer: true` also
opens the viewer once the launch has its window. The implicit start happens only
when the server uses no session at all: with a live session the call runs there
as before, an unknown `session_id` is still an error, and a server whose only
sessions are on another machine or attached answers `session_id_required` with
their IDs. Call `session_start` yourself for any non-default option.

Each launch names itself in the environment of its script's shell
(`KWIN_MCP_LAUNCH`), so the session's processes can be traced to the launch that
started them through `/proc` inside the sandbox. When the deadline passes with
the command still running and no window, `launch_app` samples the launch's
processes for two seconds:

| `reason` | `launch_processes.state` | Meaning |
| --- | --- | --- |
| `slow_start` | `busy` | The processes are using CPU: the application is still starting. Do not launch it again. |
| `window_not_observed` | `idle` | The processes use no CPU: nothing shows a window is coming (a background or non-GUI command, or one blocked on something). |
| `window_not_observed` | absent | The sandbox's process table could not be read. |

Either way the launch stays on record (`late_window_tracked: true`) for ten
minutes or until its processes are gone. `window_list` reports it under
`pending_launches`, and once its window maps, lists that window with `launch`
(`command`, `seen_after_seconds`, `launch_gave_up_after_seconds`,
`attributed_by`). `attributed_by` is `process` when the window's process was
started by the launch, and `timing` when the process names no launch and the
window mapped while this was the oldest launch still waiting. A later
`launch_app` call does not take such a window as its own unless its command
exited 0, which is a hand-off to the earlier launch's process.

A browser command that exits 0 handed its request to the running browser. A new
window or a title that changes and holds is reported as before. If neither
happens within three seconds, the result is still `reused_existing`, with
`reuse_evidence`: `window_changed` (exactly one existing browser window changed
its title, even briefly, or was raised), `active_browser_window` (no window
changed and a browser window has the focus, which is where a browser opens a
handed-over request and where Chrome's profile picker stays), or
`only_browser_window`. Several changed windows remain `unattributed`. A second
start of another single-instance application (`plasma-systemmonitor`, `dolphin`)
is reported the same way with `same_application`, when a window named after the
command's program was already open.

Applications launched in a session share a private D-Bus session bus (the one
that carries the session-local KWallet): they can own names and call each other,
and a single-instance KDE application can register itself. Host services are not
on that bus, and nothing is activated on it, so `plasma-systemmonitor` opens with
its sensors unavailable unless `ksystemstats` is started in the session first
(`launch_app` with `expect_window: false`); CPU, GPU, memory and network then
report, disks do not.

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
frame origin once the frame is paired with a KWin window of matching dimensions.
This also handles Qt/Wayland providers that report a local origin for AT-SPI Screen
extents. A frame is paired with the one window whose caption is its name. A frame
named unlike its window (Chrome's profile picker is "Who's using Chrome?" in a
window captioned "Google Chrome") is paired with the window of the same process and
size that no other unpaired frame could be; `window_association` then says
`process_and_frame_size`. Frames whose caption several windows share
(`caption_shared_by_windows`: two browser windows on one page), other frames that
stay ambiguous, and unmatched dimensions do not produce usable mouse coordinates. The active window's elements are searched
and listed first, so a time limit cuts off other windows. `role: "text"` includes
static text, labels, paragraphs and headings. With a `role` and `exact` or
`whole_word`, a control of that role is also returned for a label inside it
(`matched_label`): the menu item named "All settings Shift+Ctrl+," for "All
settings". CDP bounds remain explicitly labelled CSS viewport pixels: browser
chrome is not part of that viewport, so those bounds are never advertised as
window-relative input coordinates. Native accessibility is queried even when CDP
is connected, making browser chrome attributable too.

Visibility is not clickability. CDP performs a read-only center hit test with
`elementFromPoint`; covered controls cannot be marked clickable. Native matches
report compositor-window coverage where known but leave in-application occlusion
and positive clickability unknown. Inspect the screenshot for those cases.

`mouse_click_element` clicks a match shown in the active window, a control before
plain text that mentions the same words. When the only shown matches are in another
window of the session it activates that window first and reports it as
`activated_window`. A refusal carries `reason: "no_clickable_element"`, a `cause`
(`role_excluded`, `exact_excluded`, `role_and_exact_excluded`, `not_shown`,
`other_window`, `covered`, `page_dom_only`, `frame_not_paired_with_window`,
`index_out_of_range`, `search_unfinished`, `no_match`), up to five elements it saw
in `seen`, and a message with the next step: for a role filter or `exact` that left
out an element carrying the text, it names that element's role and whole name.

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
