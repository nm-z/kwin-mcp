# Real-system test storage

The Rust lifecycle harness creates a unique private HOME beneath `/var/tmp`
and removes it when the client is dropped. This keeps nested fixture Unix
socket paths within Linux's pathname limit even in a long checkout path.
Unlike `/tmp`, `/var/tmp` remains visible inside the isolated desktop.

Set `KWIN_MCP_TEST_TMPDIR` to override the fixture HOME parent. Use a short
absolute path outside `/tmp`; nested browser-cache socket fixtures need room
for the generated HOME name and their own subdirectories. This variable is
separate from `TMPDIR`, which selects the server's runtime workdir root.

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
