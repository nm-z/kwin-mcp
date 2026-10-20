# Distinct exports from concurrent sessions

Closes #410.

The current runtime already keeps multiple session handles live. Its existing acceptance case exported `/etc/hostname`, whose identical bytes in every session could conceal an export being routed to the wrong session.

This PR writes different session IDs to the same private `/tmp` pathname in two live desktops, then requires both exports to return the originating session's exact bytes. It also retains the existing window-list, stop, repeated-stop, and stale-handle checks.

On engi, the production stdio binary at `05b937d1c2ce50c52b80c878e0d1c12555a3ef44` passed: `s901516-1` exported its own ID after `s901516-2` started, and the later session exported its different ID. Both handles remained usable and all cleanup assertions passed. The fixture now selects the supported 60-second startup budget; an initial run at the default 20 seconds hit the loaded host's sandbox startup deadline before exercising session routing.

Validation: Ruby syntax and `ruby tests/session_registry_e2e.rb /path/to/kwin-mcp` passed. The regression changes acceptance coverage; runtime source is unchanged. Results are committed in `docs/verification/issue410/`.

### PNG integrity and stop-order coverage

The production lifecycle suite also captures different real kdialog windows in two live desktops, stores their PNGs under the same private `/tmp` pathname, and exports each through its originating session handle. Independent host reads require byte-for-byte equality with each source PNG. After stopping the later desktop, the earlier desktop exports its unchanged PNG again. Stopping the earlier desktop then makes that handle fail explicitly without writing the requested destination; both session work directories are removed.

On engi, `starting_another_session_preserves_existing_file_exports` passed against the production stdio binary at `05b937d1c2ce50c52b80c878e0d1c12555a3ef44`: 1 passed, 0 failed, 8.44 seconds, with two distinct live handles and three byte-exact PNG exports. The case uses a private HOME and fresh browser profiles. Lossless source PNGs and the command log accompany the existing session-token proof. Runtime source remains unchanged.

Run with `KWIN_MCP_E2E=1 KWIN_MCP_E2E_SERVER=/absolute/path/to/kwin-mcp cargo test --locked --test session_lifecycle_e2e starting_another_session_preserves_existing_file_exports -- --ignored --exact --nocapture --test-threads=1`.
