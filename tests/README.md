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
