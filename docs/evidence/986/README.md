# launch_app of a browser the session's machine lacks (#986)

sentry (192.168.1.20) has Chromium 156 (`/usr/bin/chromium`) and no Google Chrome. A stdio server on engi, which has `google-chrome-stable` and `google-chrome`, started a session there with `session_start` `host` and ran the issue's command, `launch_app google-chrome-stable https://example.com/`. The listener on sentry (port 47986) and the stdio server on engi were private instances with a HOME of their own, both of the build in the table.

| Build | `session_start` | `launch_app google-chrome-stable https://example.com/` | Screenshot |
| --- | --- | --- | --- |
| master 3be7567 | no browser list | `failed: command not found (exit status 127)` after 0.2 s, no window | [`before.png`](before.png) |
| 53f7f13 | `browsers: ["chromium"]`, `browsers=chromium` in its text | a Chromium window after 1.3 s, `cdp_port` 46291, `browser_substitute: {"requested": "google-chrome-stable", "ran": "chromium"}`, and in the text: `google-chrome-stable is not installed on sentry, so chromium ran in its place` | [`after.png`](after.png) |

`before.png` is a Konsole in the master session: on sentry the same command exits 127, `/usr/bin/google-chrome-stable` does not exist and `/usr/bin/chromium` does. `after.png` is the window `launch_app` reported, `Example Domain - Chromium`. In the same session [`after-konsole.png`](after-konsole.png) shows that sentry still has no `google-chrome-stable` and that the browser running is `/usr/lib/chromium/chromium` with the wrapper's switches and `--remote-debugging-port=46291`, the `cdp_port` of the result.

Also in that session:

- `browser_read` returned the page's text through that port: "This domain is for use in documentation examples without needing permission. …".
- `launch_app firefox https://example.com/` still exits 127, and the result adds `Installed browser commands on sentry: chromium` and `browsers: ["chromium"]`.
- `launch_app google-chrome-stable --headless … --dump-dom https://example.com/` with `expect_window: false` names the substitute the same way.
- The `launch_app` description the engi server gave its client says `Installed browser commands on engi: google-chrome-stable, google-chrome.` (2,025 characters, both Chrome profiles still listed; master: `Installed browser commands: google-chrome-stable, google-chrome.`, 2,017).
