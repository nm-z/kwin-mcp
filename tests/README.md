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
# Typing receipt diagnostics

Run `ruby tests/typing_receipts_e2e.rb /path/to/kwin-mcp /path/to/evidence`
on a KDE/KWin host. The isolated fresh-Chrome fixture checks a 71-character
address-bar value, the exact 75-character search query from issue 528, a
controlled 72-character limit, a zero-length web field, and a 1,700-character
multiline Markdown value. It independently copies application text through
native Ctrl+A/Ctrl+C and the session clipboard, records phase timings, saves
screenshots, and cleans up its own session.
