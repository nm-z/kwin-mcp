# Display size of a `--listen` server's sessions (#984)

`systemd/kwin-mcp-listen.service` starts the listener as `kwin-mcp --listen ADDRESS --no-viewer --autoclean --ttl 120`, with no `--width`/`--height`. Every session it started was 3840x2160, the compiled default, while the stdio servers registered with `--width 1920 --height 1080` told agents the default was 1920x1080. The compiled default is now 1920x1080, so a stdio server and a listener without size options agree.

Both screenshots show a maximized Konsole in a session that a stdio server started on such a listener on engi, with no size in `session_start`:

| Build | `session_start` reported | Screenshot |
| --- | --- | --- |
| master 3be7567 | 3840x2160 | [`before.png`](before.png), 3840x2160 |
| this change | 1920x1080 | [`after.png`](after.png), 1920x1080 |

## Reproduce

The screenshots come from the real-session test, with `KWIN_MCP_E2E_EVIDENCE` naming a directory:

```sh
KWIN_MCP_E2E_EVIDENCE=<dir> cargo test --test remote_session_e2e -- a_listener_without_a_size_starts_1920x1080_desktops --exact --ignored
```

It also checks the compositor's `--width`/`--height`, the maximized window's geometry, a `session_start` that names 1280x720, and a listener started with `--width 1600 --height 900 --no-override`. The test that needs no KDE checks what a listener tells its peers at `initialize`:

```sh
cargo test --test remote_session_e2e -- a_listener_without_a_size_offers_the_stdio_default_of_1920x1080 --exact
```
