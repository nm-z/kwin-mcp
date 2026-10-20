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
