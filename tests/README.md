# Real-system test storage

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
