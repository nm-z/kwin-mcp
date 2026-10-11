# Automated cookie continuity acceptance for #245

Nate explicitly replaced the original fourteen-day Gmail `Profile 2` and
help.openai.com acceptance gate with automated cookie-persistence checks. This
instruction was communicated through kwin-lead/CEO on October 10, 2026. There is
no real-site acceptance run, host-browser reservation, or login requirement.
This change does not claim that either real site stayed signed in for two weeks.

PR #257 stays draft until kwin-lead reviews the exact tested commit, executable
hashes, logs, and rendered screenshots. Lead marks ready and closes #245 after
reviewing the authorized automated evidence; the CEO alone reviews and merges.
The assigned candidate and acceptance deadlines are October 11 at 02:30 Pacific.

## Required automated evidence

All desktop gates use the returned candidate artifacts on engi through the real
production stdio entrypoint: `session_start`, affected MCP operations, independent
GUI/system-state checks, and `session_stop`. Compilation and CPU checks use plain
`dsh build`. No candidate binary is installed or deployed. Each fixture uses a
private HOME and cookie state; actual host browser profiles are not modified.
Each private child server receives `XDG_RUNTIME_DIR=/run/user/1000` and
`DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus` explicitly. The fixtures
preserve their private HOME/XDG data/config/cache/state and no-repository setting;
they do not change the agent's global environment or host application state.

| Regression | Required observations |
| --- | --- |
| `browser_cookies_continue_across_sessions_without_host_writes` | EXCLUSIVE WAL capture without host SHM access, cookie continuity through two session lifetimes, independent browser profiles, newer host row wins, unchanged lower database/WAL bytes, private file permissions, and rendered screenshot. |
| `browser_cookies_snapshot_concurrent_wal_commits_and_checkpoints` | Startup during concurrent atomic commits and WAL truncation/checkpoints, coherent transaction generations, full database integrity, a live captured generation, WAL removal/rotation on writer close, unchanged lower bytes after writer stops, and rendered screenshot. |
| `browser_cookie_deletion_survives_stale_sessions_and_expiry_is_pruned` | Concurrent session stops preserve deletions and newer rows, complete partition/scheme/port identity is preserved, expired rows including zero expiry are removed, session-only/future cookies remain, unrelated SQLite databases are skipped, the private store is hidden, stale host rows do not resurrect deletions, and genuinely newer host rows can recreate an identity. |
| `tests/browser_cookie_flush_e2e.rb` | Actual local Google Chrome receives persistent and session-only HTTP cookies; stop is requested less than ten seconds after each Set-Cookie response; subsequent isolated Chrome sessions send both values back over HTTP. Capture rendered set/restored pages, verify the lower fixture profile is unchanged, and stop all sessions. |

Run the three Rust regressions sequentially from the compiled integration test
artifact with `KWIN_MCP_E2E=1`, `KWIN_MCP_E2E_SERVER` pointing to the exact private
server artifact, and `KWIN_MCP_TEST_ARTIFACT_DIR` pointing to a private evidence
directory. Run the Ruby browser regression with the same server/artifact variables.
Inspect rendered screenshots; a passing process exit alone is insufficient.
Test clients explicitly track and stop their own session IDs on failure as well
as success. Independently verify that no test sandbox/server remains afterward.

## Deletion and expiration semantics

The complete startup jar is retained privately as the session's baseline. If a
cookie identity present in that baseline is missing when the session stops, a
private tombstone suppresses that baseline `last_update_utc` and older versions.
This prevents the frozen host jar or a stale concurrent session from restoring
the removed row. A cookie with a newer update timestamp wins regardless of stop
order. Absence alone cannot establish when an unseen concurrent update occurred,
so tombstones target observed versions instead of assigning a guessed deletion
timestamp. Tombstones use the complete unique cookie identity, including partition,
scheme, and port fields, and are durably published before the saved jar is replaced.

Expired rows with `has_expires` set and an elapsed `expires_utc` are removed during
restore and save; session-only cookies are retained. A database schema mismatch
does not trigger a guessed migration. Startup preserves a valid private host
snapshot if the saved schema is incompatible, while save preserves the prior
stored jar and reports the incompatibility. Non-cookie SQLite databases inside a
browser configuration remain private and are skipped by the continuity store.

## Evidence and failure handling

Record the exact commit, executable hashes, artifact provenance, verification
commands, start/stop times, screenshots, stdio outcomes, stderr, lower-profile
comparisons, and cleanup results. Do not attach real cookie databases, cookie
values, credentials, or session tokens to a PR. Local fixture values are synthetic.

A failed check remains failed until its concrete cause is corrected and the affected
real path is verified again. Do not omit a required gate or mark ready without lead
review. Do not remove board items or extend deadlines before lead authorization.
