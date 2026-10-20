# Distinct exports from concurrent sessions

Closes #410.

The current runtime already keeps multiple session handles live. Its existing acceptance case exported `/etc/hostname`, whose identical bytes in every session could conceal an export being routed to the wrong session.

This PR writes different session IDs to the same private `/tmp` pathname in two live desktops, then requires both exports to return the originating session's exact bytes. It also retains the existing window-list, stop, repeated-stop, and stale-handle checks.

On engi, the production stdio binary at `05b937d1c2ce50c52b80c878e0d1c12555a3ef44` passed: `s901516-1` exported its own ID after `s901516-2` started, and the later session exported its different ID. Both handles remained usable and all cleanup assertions passed. The fixture now selects the supported 60-second startup budget; an initial run at the default 20 seconds hit the loaded host's sandbox startup deadline before exercising session routing.

Validation: Ruby syntax and `ruby tests/session_registry_e2e.rb /path/to/kwin-mcp` passed. The regression changes acceptance coverage; runtime source is unchanged. Results are committed in `docs/verification/issue410/`.
