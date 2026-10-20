# Shared Unix socket verification (#511)

On engi, the production stdio server starts a fresh private HOME and KWin session.
A Ruby helper creates one live Unix listener under HOME and another beneath the
returned workdir. The host reaches the shared listener and reads its unique
payload. The host sees the HOME socket in overlay-upper but a relative-path
connection returns ECONNREFUSED; using a relative path rules out the Unix socket
address length limit. A separate /tmp marker remains private. The helper exits,
the session stops, and autoclean removes the owned workdir.

Run `ruby tests/shared_ipc_e2e.rb SERVER EVIDENCE_DIR` on a supported KDE host.
The baseline is master 05b937d. Its real connection checks pass, but the final
assertion fails because the explicit host_shared_dir metadata is absent. The
candidate supplies that alias consistently in session_start and session_list.
The JSON files contain the actual outcomes; no host profile or service is changed.

The final run uses merged head b3a67e0, retaining the existing provider/launch
changes from PR #517. The server unit suite passed 77 cases (1 ignored), and
`cargo clippy --locked --bin kwin-mcp -j1` passed with existing warnings.
