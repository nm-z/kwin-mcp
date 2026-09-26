# KWin MCP development

Follow [AGENTS.md](AGENTS.md) for repository commands, style, and real-system test requirements.

`kwin-mcp` serves MCP over stdio. Each server process owns at most one isolated KWin session. `session_start` is idempotent; start, stop, and shutdown share a lifecycle gate. Startup resources remain owned until the `Session` is published, so cancellation can reap bwrap, D-Bus proxies, and the optional viewer. With `--autoclean`, workdir ownership ends only after removal succeeds.

Bubblewrap mounts the host root read-only and gives the container an overlay for `$HOME`. KWin runs on a virtual output. EIS supplies input, KWin D-Bus supplies window and screenshot operations, and AT-SPI supplies accessibility data. Mouse and screenshot coordinates are relative to the active window. `screenshot` uses `CaptureScreen("Virtual-0")` so popups composite into the frame, then crops to the active window by default; a `region` may extend past the window, and the metadata reports the window and the window-relative region the image covers.

`launch_app` runs a shell expression. A PATH directory scoped to that launch wraps known Chromium-family executable names. The wrapper adds missing Wayland, KWallet, and renderer-accessibility switches to the browser's argv; Chromium-family programs also get a CDP port. Bash handles compound commands and `nohup` or `timeout` wrappers. Absolute executable paths bypass the PATH wrapper, so callers using them must supply their own flags.

FUSE: `fusermount` binaries are setuid and cannot work under `no_new_privs` in a user namespace, and bwrap nests a second user namespace for `--uid` != 0 (devpts needs root), where capabilities cannot mount. When the host supports it (`/dev/fuse`, non-setuid bwrap, `setpriv`, `fusermount`), bwrap runs at uid 0 with `CAP_SYS_ADMIN` in one namespace; `kwin-mcp --fuse-helper` alone keeps it, and the entrypoint runs everything else through `unshare --user --map-user=<uid>` and `setpriv` with no capabilities. Sandbox `fusermount`/`fusermount3` are this binary acting as shims (see `src/fuse_bridge.rs`); the helper runs the real ones only to mount on user-owned directories or unmount FUSE mounts.

The container reaches permitted host KWallet methods through a filtered live D-Bus proxy. The server does not dump or snapshot wallet entries. `session_start` enables `org.a11y.Status.IsEnabled`; renderer accessibility exposes Chrome web content to AT-SPI even when CDP is unavailable.

The HOME overlay's lower layer is the live host HOME. SQLite databases a host process holds open in WAL mode (found through `/proc/*/fd` `-wal` descriptors) are snapshotted into the upper layer at `session_start` with SQLite's online backup and empty `-wal`/`-shm`, because a host-live WAL database read across the overlay reads as malformed.

The workdir and HOME overlay live on `/tmp`, which systemd mounts as tmpfs with a per-user quota (about 80% of its size), so EDQUOT can occur while `df` shows free space. `screenshot` writes atomically, never leaves a partial or stale PNG, returns the image inline when the file cannot be saved, and names the quota numbers in its error.

Chrome's file chooser is its own in-process GTK4 dialog, not the portal. Enter in its location bar cancels the dialog (the input's `cancel` event fires, nothing attaches) while zenity's GTK4 chooser accepts the same injected keys, so the documented sequence is Ctrl+L, path, Alt+O (the Open mnemonic).

Session writes never reach the host on their own. `export_file` reads a session path through `/proc/<bwrap child>/root` (so both HOME overlay files and the private `/tmp` work), copies it to a host path via a temporary file and rename, and compares it byte for byte; it refuses to overwrite unless asked and reports downloads still in progress.

The optional host viewer writes `viewer-status.json` in the workdir; `session_start` and `viewer_open` report its outcome (ready, starting, unavailable with a reason, disabled) without failing the session.

For remote MCP use, set the client's stdio command to SSH and run `kwin-mcp --no-viewer` on the remote host. SSH carries MCP messages; there is no network listener or remote viewer transport. The remote host needs KDE, bubblewrap, render and input devices, and an active user D-Bus.
