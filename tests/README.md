# Real-system test storage

`ruby tests/native_paste_e2e.rb /path/to/kwin-mcp /absolute/evidence` exercises
native Ctrl+V and Shift+Insert through a fresh stdio/KWin/Chrome session. It checks
multiline UTF-8 field receipts and independently receives the complete values
through ordinary HTML form POSTs to a local Ruby fixture. It also verifies that
a generated-secret selection returns only protected/unverified paste metadata.
The fixture submits no external form and cleans up its own desktop.

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
