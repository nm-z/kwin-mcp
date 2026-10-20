# Import refresh verification

This completes PR #374 for issue #373 against master 05b937d1c2ce50c52b80c878e0d1c12555a3ef44.

## Current checks on engi

- The built production stdio server passed the real KDE session fixture:
  import_file_refreshes_host_notes_without_restarting_the_session
  (1 passed, 0 failed; 20.82 seconds).
- The fixture covers blank session notes, private edits, whiteouts, atomic host
  replacement, default overwrite refusal, independent export/readback, preservation
  of host files, absolute parent symlinks, private /tmp, and invalid/read-only destinations.
- The byte-comparison helper now requires EOF in both streams. Two focused
  file_content_tests pass, including a longer stale copy after source EOF.
- The included raw screenshot shows the refreshed fixture note in a real kdialog window.

The fixture uses a generated private HOME and an isolated desktop. It closes its own
session after verification. No running user desktop or application is restarted.

## Reproduce

Build the production server and session_lifecycle_e2e integration target. Set
KWIN_MCP_E2E=1, KWIN_MCP_E2E_SERVER to that built server, and
KWIN_MCP_TEST_ARTIFACT_DIR to the evidence directory. Run the integration executable:

    import_file_refreshes_host_notes_without_restarting_the_session --ignored --exact --nocapture --test-threads=1

Run the deterministic comparison regressions:

    cargo test --locked --offline --bin kwin-mcp file_content_tests

## Scope

Import refreshes one regular file in an existing writable session directory. Applications
must reload their own buffers. Obsidian and Full Calendar cache/render behavior is not
claimed; the GUI proof uses kdialog and file bytes are checked independently.
