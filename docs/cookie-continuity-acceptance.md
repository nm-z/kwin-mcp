# Automated cookie continuity acceptance for #245

Nate explicitly replaced the original fourteen-day Gmail `Profile 2` and
help.openai.com acceptance gate with automated cookie-persistence checks. This
instruction was recorded on October 10, 2026. There is no real-site acceptance
run, host-browser reservation, or login requirement. This change does not claim
that either real site stayed signed in for two weeks.

Keep PR #257 in draft and record the exact tested commit, executable hashes,
logs, and rendered screenshots. The worker does not mark the PR ready or merge it.

## Required automated evidence

All desktop gates use the candidate artifacts through the real production stdio
entrypoint: `session_start`, affected MCP operations, independent GUI/system-state
checks, and `session_stop`. Before compilation, compare `uptime`, `free -m`, and
`df -h` on engi and sentry; build on the host with more headroom. Wrap cargo build,
test, and clippy commands with `/home/nate/claude/luna/build-slot` and set a
private `CARGO_TARGET_DIR` under the checkout. Do not build on archy. If
`session_start` fails the 2 GiB overlay-space gate, identify the full filesystem
and its space consumers before reporting the failure. No candidate binary is
installed or deployed. Each fixture uses a private HOME and cookie state; actual
host browser profiles are not modified. Each private child server receives
`XDG_RUNTIME_DIR=/run/user/1000` and a filtered session-bus address through
`KWIN_MCP_TEST_DBUS_SESSION_BUS_ADDRESS`. The proxy must allow
`org.freedesktop.DBus` and `org.freedesktop.systemd1`, and deny
`org.kde.kwalletd6` and Secret Service so these checks do not copy the host
wallet. For example, use `xdg-dbus-proxy` with the user bus as its upstream,
`--filter`, `--see=org.freedesktop.DBus`, `--see=org.freedesktop.systemd1`, and
`--talk=org.freedesktop.systemd1`, then set the test variable to that proxy's
Unix socket. The fixtures preserve their private HOME/XDG data/config/cache/state
and no-repository setting; they do not change the agent's global environment or
host application state.

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
real path is verified again. Report every unrun or failed gate, keep the PR draft,
and do not merge it.
