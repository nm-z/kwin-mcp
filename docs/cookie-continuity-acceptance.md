# Cookie continuity acceptance for #245

PR #257 stays draft until lead review. Its candidate tests do not satisfy this
acceptance gate, and passing fewer than fourteen elapsed days does not close #245.

## Required observations

Use an immutable, reviewed candidate binary in a private evidence directory on
engi. Keep an independent durable `XDG_STATE_HOME` under that evidence directory
for the entire run; the same store must be used by all acceptance sessions.
The isolated desktop may read the real profile through its read-only lower layer.
All browser writes must remain in the session overlay or the private cookie store.
Do not install or deploy the candidate, or write into a real browser profile.

At each observation, start the production stdio server, call `session_start`, and
open Google Chrome with `--profile-directory='Profile 2'` at Gmail. Inspect rendered
pixels for an authenticated mailbox, then capture a screenshot. Open help.openai.com
with the corresponding Chrome Default profile, inspect its rendered authenticated
state, and capture a screenshot. Do not send mail, change support requests, or
perform account operations. Call `session_stop` and verify teardown after every run.

Run three times daily, including weekends, for at least fourteen elapsed days.
Proposed Pacific observation times are 07:30, 15:30, and 23:30. Record actual times;
a missing run is a missing observation, not a passing result. The final observation
must occur at least fourteen days after the initial authenticated observation.
The assigned deadline is October 24, 2026 at 23:30 Pacific. Meeting it therefore
requires the initial authenticated observation by October 10 at 23:30 Pacific.
If this is not possible, report the deadline risk to kwin-lead without changing it.

## Preconditions and handoffs

- Complete remote compilation and real production stdio tests for concurrent WAL
  snapshots and real-browser immediate-stop cookie flush first.
- Obtain kwin-lead's review of the exact candidate commit and binary hashes.
- Obtain CEO/Nate's acceptance decision to leave host Chrome Profile 2 unused for
  the entire interval, and to avoid using help.openai.com/auth.openai.com in the
  host Default profile. Host usage would invalidate the session-only control.
- If either copied login is already signed out, put that login page in the isolated
  viewer and ask Nate to log in there. Do not read or type credentials. Keep the
  session open during this handoff; stop it after the user finishes and evidence
  is captured. The two-week clock starts with successful authenticated pixels.

## Evidence for each run

Record the exact commit, executable hashes, candidate artifact paths, actual start
and stop timestamps, browser/profile names, cookie store path, screenshots, stdio
responses, server stderr, authenticated/signed-out outcome, and cleanup outcome.
Keep screenshots private: mailbox and support-page pixels may contain personal data.
Never attach cookie databases, cookie values, credentials, or session tokens to a PR.

A signed-out page, interrupted cadence, host use of a controlled profile/site, or
failed teardown must be reported promptly. Preserve the observation as failure or
inconclusive; do not silently restart the clock or mark it passing. The CEO decides
acceptance after reviewing the complete fourteen-day rendered record.
