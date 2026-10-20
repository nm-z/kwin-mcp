# Startup timeout process evidence

Issue: https://github.com/nm-z/kwin-mcp/issues/450

Startup errors already preserved a bounded stderr tail. A sandbox that produces no stderr could still reach the D-Bus marker deadline without useful process evidence, after which cleanup removes the child and its log.

The change captures the owned startup child's PID, Linux state, and wait channel before cleanup. Reads are bounded and tolerate missing proc files. The snapshot includes neither process arguments nor environment data.

## Production validation

The fixture uses the existing debug hook to stop the actual owned sandbox child. Baseline `05b937d1c2ce50c52b80c878e0d1c12555a3ef44` reaches its real marker deadline and fails the new process-state assertion. [Baseline response](baseline-stopped.json).

The candidate reports the exact child as `T (stopped)`, with `do_signal_stop` as its wait channel. The fixture separately verifies teardown of the child, session handle, runtime directory, disk directory, and stderr file. [Candidate response](candidate-stopped.json).

All four production stdio cases passed on engi: invalid D-Bus configuration, noisy stderr, normal KWin/dialog/screenshot startup, and the stopped child. [Timings and outcomes](cases.txt). The normal case is shown in [normal.png](normal.png).

The frozen candidate was built from this patch on base `05b937d`. Its SHA-256 is `5fb8e2e7c1f5ae0130fab8905db8c9d4218701c26912a3f61ca7b19978d4773c`.

Commands:

    cargo build --locked --bin kwin-mcp -j1
    ruby tests/startup_diagnostics_e2e.rb /absolute/path/to/frozen-kwin-mcp /absolute/path/to/evidence
    cargo test --locked --bin kwin-mcp -j1
    cargo clippy --locked --bin kwin-mcp -j1
    rustfmt --edition 2024 --check --config skip_children=true src/main.rs

Build, all four real-session cases, clippy, and the main-source formatting check passed. Unit tests: 75 passed, 1 ignored.

The historical October 17 log establishes a marker deadline and failed prompt scope teardown, but does not establish the original cause. The added snapshot preserves the missing evidence for subsequent failures.
