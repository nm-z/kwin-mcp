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

## File chooser capture identity

```bash
ruby tests/transient_chooser_e2e.rb /path/to/kwin-mcp /path/to/evidence candidate
```

This production-stdio test opens an owned fresh Chrome window and its native
file chooser. It requires a successful pixel-verified chooser image, then
closes the chooser during a pending capture and requires an explicit
`transient_capture_unverified` error without an image. It also checks the
restored parent and a region extending outside the window. `baseline` mode
records the older misleading capture for comparison. The script saves RPC
evidence and PNGs, stops its session, and removes its private HOME and temp
root. Chrome and the normal session dependencies are required.

The checked-in `evidence/396` images show the baseline race returning parent
pixels under the chooser's identity, and the candidate's verified chooser and
subsequent parent capture. The candidate rejects the race, so that response
intentionally has no PNG.
