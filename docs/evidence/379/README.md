# Persistent directory verification

This extends the verification for PR #385 / issue #379 on top of 40c3b6d while
preserving its implementation and concise tool description.

The production stdio server passed all 25 checks in persistent_paths_e2e.rb on engi.
The committed results.txt records each check. The fixture used a fresh private HOME,
a filtered host bus, generated marker files, real KWin sessions, bubblewrap,
pasta, and systemd user scopes.

Additional coverage checks same, ancestor, and descendant overlap with
host_write_paths, then uses a separate valid host-write directory alongside two
persistent directories through all three session cycles and a server restart.
Both kinds of durable data survive session stop; ordinary HOME overlay data remains
private. The fixture also verifies source-replacement refusal and autoclean.

A tools/list schema guard now refuses an executable lacking persistent_paths before
starting a desktop. It prevents stale output from a shared Cargo build directory
from masquerading as this feature's server.

Reproduce with:

    ruby tests/persistent_paths_e2e.rb /absolute/path/to/built/kwin-mcp /private/new/evidence/directory

Use a new evidence directory for each run. Ruby syntax and git diff --check passed.
No real browser authentication, crash recovery, or concurrent browser profile use
is claimed; application shutdown and exclusive profile reuse remain caller duties.
