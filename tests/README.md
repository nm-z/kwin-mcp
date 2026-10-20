# Real-system test storage

`ruby tests/native_paste_e2e.rb /path/to/kwin-mcp /absolute/evidence` exercises
native Ctrl+V and Shift+Insert through a fresh stdio/KWin/Chrome session. It checks
multiline UTF-8 field receipts and independently receives the complete values
through ordinary HTML form POSTs to a local Ruby fixture. It also verifies that
a generated-secret selection returns only protected/unverified paste metadata.
The fixture submits no external form and cleans up its own desktop.

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
