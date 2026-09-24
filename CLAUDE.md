# KWin MCP development

Follow [AGENTS.md](AGENTS.md) for repository commands, style, and real-system test requirements.

`kwin-mcp` serves MCP over stdio. Each server process owns at most one isolated KWin session. `session_start` is idempotent; start, stop, and shutdown share a lifecycle gate. Startup resources remain owned until the `Session` is published, so cancellation can reap bwrap, D-Bus proxies, and the optional viewer. With `--autoclean`, workdir ownership ends only after removal succeeds.

Bubblewrap mounts the host root read-only and gives the container an overlay for `$HOME`. KWin runs on a virtual output. EIS supplies input, KWin D-Bus supplies window and screenshot operations, and AT-SPI supplies accessibility data. Mouse and screenshot coordinates are relative to the active window. `screenshot` uses `CaptureScreen("Virtual-0")` so popups appear in the PNG.

`launch_app` runs a shell expression. A PATH directory scoped to that launch wraps known Chromium-family executable names. The wrapper adds missing Wayland, KWallet, and renderer-accessibility switches to the browser's argv; Chromium-family programs also get a CDP port. Bash handles compound commands and `nohup` or `timeout` wrappers. Absolute executable paths bypass the PATH wrapper, so callers using them must supply their own flags.

The container reaches permitted host KWallet methods through a filtered live D-Bus proxy. The server does not dump or snapshot wallet entries. `session_start` enables `org.a11y.Status.IsEnabled`; renderer accessibility exposes Chrome web content to AT-SPI even when CDP is unavailable.

For remote MCP use, set the client's stdio command to SSH and run `kwin-mcp --no-viewer` on the remote host. SSH carries MCP messages; there is no network listener or remote viewer transport. The remote host needs KDE, bubblewrap, render and input devices, and an active user D-Bus.
