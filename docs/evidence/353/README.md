# Socket pathname validation (#353)

Verified on engi with the production Rust stdio server, KDE/KWin, bubblewrap,
input devices, xdg-dbus-proxy and dbus-daemon. Every server used a private HOME;
a filtered host D-Bus proxy denied wallet access. No viewer was opened.

## Before

Baseline `3dffc29`, built from a separate clean checkout:

```bash
/home/nate/claude/luna/build-slot env CARGO_TARGET_DIR=/home/nate/work/luna/kwin-mcp-353/build cargo build --bin kwin-mcp --jobs 2
# From the candidate checkout, using the baseline executable:
ruby tests/socket_path_length_e2e.rb ../build/debug/kwin-mcp target/socket-path-baseline --reject-only
```

The build passed (2m 30s). The regression failed as expected: the generated
179-byte system proxy pathname produced `D-Bus proxy socket did not appear`.
There was no byte-count or TMPDIR guidance, and startup had already acquired
uinput devices. The server cleaned up on failure.

## Candidate

Candidate Rust changes are at `e3b46ba`; later evidence-only commits do not
change the executable. The package artifacts were cleared between checkouts
because Cargo initially reused the baseline executable from the shared target.
The final candidate's `--version` and SHA-256 were checked before running it.
Binary identities and exact error responses are in [results.json](results.json).

```bash
/home/nate/claude/luna/build-slot env CARGO_TARGET_DIR=/home/nate/work/luna/kwin-mcp-353/build cargo build --jobs 2
ruby tests/socket_path_length_e2e.rb ../build/debug/kwin-mcp target/socket-path-final
```

The final build passed (9.16s). All seven regression cases passed:

| Service socket pathname | Result |
| --- | --- |
| 180 ASCII bytes | Rejects the overlong system proxy path with the kernel socket constraint |
| 108 ASCII bytes | Rejects with the libdbus constraint |
| 107 ASCII bytes | Rejects with the libdbus constraint |
| 100 ASCII bytes | The 99-byte system proxy path fits; the longer service proxy path is rejected |
| 100 UTF-8 bytes, fewer than 100 characters | Rejects by byte count |
| 99 ASCII bytes | Real session starts; service socket exists, sandbox exists, `window_list` succeeds, stop removes runtime/disk directories and sandbox |
| 80 ASCII bytes | Same successful startup, inspection and cleanup |

Invalid paths returned in about 1 ms, with the offending pathname, byte count
and shorter-TMPDIR guidance. Each invalid case was retried in the same server:
the next session name was validated, no session was listed, and no runtime or
disk session directory remained.

An initial 107-byte positive case exposed libdbus's stricter portable 99-byte
limit: the 100-byte KWallet bus path failed even though it fit Linux's
`sockaddr_un`. A standalone private dbus-daemon confirmed `Socket name too long`.
The named limit in this fix follows
[libdbus's authoritative definition](https://dbus.freedesktop.org/doc/api/html/group__DBusSysdeps.html).
General loss of private daemon startup errors is tracked separately in #364.

## Repository checks

Run from the candidate checkout on engi:

```bash
/home/nate/claude/luna/build-slot env CARGO_TARGET_DIR=/home/nate/work/luna/kwin-mcp-353/build cargo test --jobs 2
/home/nate/claude/luna/build-slot env CARGO_TARGET_DIR=/home/nate/work/luna/kwin-mcp-353/build cargo clippy --all-targets --jobs 2
cargo fmt --all -- --check
rustfmt --edition 2024 --config skip_children=true --check src/main.rs
ruby -c tests/socket_path_length_e2e.rb
git diff --check
```

- `cargo test`: passed, 25 passed and 47 ignored across the targets. No failures.
  The ignored cases require additional real desktop/device/browser scenarios.
- `cargo clippy --all-targets`: passed (1m 33s), with the existing
  `collapsible_if` warning at `tests/session_lifecycle_e2e.rs:6351`.
- `cargo fmt --all -- --check`: exits 1 for existing formatting differences in
  unchanged `src/bin/kwin-viewer.rs` and `src/input_bridge.rs`.
- Changed-file rustfmt, Ruby syntax, and diff whitespace checks: passed.

Not verified: the full ignored real-system suite, other platforms, viewer or
capture behavior. This change validates newly generated session paths; it does
not change an existing session's storage root.
