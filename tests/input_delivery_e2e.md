# Input delivery regressions

Build the production server, then run the Ruby probe against that private binary:

```bash
cargo build --release --bin kwin-mcp
KWIN_MCP_E2E_SERVER="$PWD/target/release/kwin-mcp" \
  KWIN_MCP_TEST_ARTIFACT_DIR=/tmp/input-delivery-evidence \
  ruby tests/input_delivery_e2e.rb clear_retype
```

This mode covers [issue #288](https://github.com/nm-z/kwin-mcp/issues/288) through
production MCP stdio in a fresh 1920×1080 isolated desktop. It starts the actual
ChatGPT Electron executable with a private profile and installs a static,
synthetic document with CDP. It does not sign in or use a host browser session.
KDE, KWin, bubblewrap, input devices, and `/usr/lib/chatgpt/ChatGPT` are required.

The probe clicks the composer, types 250 ASCII characters, and immediately calls
`screenshot` before any accessibility read or sleep. It repeats capture in a
cropped region, copies the field through Ctrl+A/Ctrl+C, and requires
byte-identical text from the session clipboard. A sentinel replaces
the clipboard before each copy, so a stale value from the previous round cannot
pass. The typing receipt must also report complete submission, compositor
acknowledgement, and `retry_safe=false`.

It then clears with Ctrl+A/Delete, captures the cleared field, types the same
250 characters again, and repeats all checks. The initial receipt must verify
the exact field counts. After Delete, the receipt may instead report the narrow
`atspi_transformed_blank_line` ambiguity: one initial newline was removed by the
application, so the original expected count is 251 and the observed count is 250.
This must be `unverified` with inspection guidance, never a mismatch error or a
claim that the original newline was preserved. The independent copied value
must still equal all 250 requested bytes.

A final screenshot checks that capture continues working. A separate textarea
case enters a real newline with Return, types after it, and requires normal
verified delivery plus an exact copied value retaining that newline. This guards
against silently discarding meaningful newlines. PNGs, copied values,
accessibility trees, and RPC responses are saved in the artifact directory.
The desktop is stopped in an
`ensure` block. All text and clipboard operations belong to the private session;
no viewer is opened.

This supplements the `electron` mode's selected-field replacement coverage.
The production pacing and delivery acknowledgements landed in
[PR #274](https://github.com/nm-z/kwin-mcp/pull/274), after #288 was reported.
The regression does not deliberately stall the compositor or simulate host
resource exhaustion.
