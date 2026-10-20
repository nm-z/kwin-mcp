# Configured provider artifact installation

Issue: https://github.com/nm-z/kwin-mcp/issues/493

The new `--provider-bin` mode publishes the built server and adjacent viewer at an explicitly configured absolute executable path. Its success checks target that artifact independently of PATH. Existing staged publication and validation remain in place, and an existing artifact still requires `--replace`. Conflicting installation modes and symlink provider commands are refused.

## Installer checks

The existing installer regression passed, including all four new provider checks: exact artifact selection, preservation of an unrelated PATH command, symlink refusal, and invalid-mode refusal. [Results](installer-results.txt).

    bash tests/install_cli_e2e.sh /absolute/path/to/new-build /absolute/path/to/evidence

## Live old-to-new replacement

The production fixture used an isolated copy of baseline `05b937d1c2ce50c52b80c878e0d1c12555a3ef44` and replaced it through the actual installer with `b3a67e0c18f4b65d940fe3a90bae616342d3649b`. [Binary SHA-256 values](binaries.sha256).

It deliberately provided no `KWIN_MCP_BINARY` or `KWIN_MCP_REPO` override, so the server had to discover changes through its native executable path. PATH pointed at an unrelated command. Assertions verified the installed inode, unchanged PID and client pipes, completion of a request split across reload, two surviving dialogs with retained contents, subsequent input, unchanged unrelated PATH artifact, and cleanup.

Exactly one named production test passed in 21.72 seconds. [Output](production-result.txt). Lossless captures: [session A before](session-a-before.png), [session A after](session-a-after.png), and [session B after](session-b-after.png).

    KWIN_MCP_E2E=1 \
    KWIN_MCP_E2E_SERVER=/absolute/path/to/old/kwin-mcp \
    KWIN_MCP_E2E_INSTALL_SOURCE_DIR=/absolute/path/to/new-build \
    KWIN_MCP_TEST_ARTIFACT_DIR=/absolute/path/to/evidence \
    cargo test --locked --test session_lifecycle_e2e \
      configured_provider_install_preserves_pid_pipes_live_windows_and_partial_requests \
      -- --ignored --exact --nocapture --test-threads=1

This establishes compatibility for the two named builds. The historical `16a7357` executable was not exercised. The shared configured provider artifact and existing user desktops were not modified.
