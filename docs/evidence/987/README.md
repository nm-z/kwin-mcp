# `cargo fmt --check` and `cargo clippy --all-targets` on master (#987)

AGENTS.md lists `cargo fmt --all -- --check` and `cargo clippy --all-targets` as the checks a change is held to. On master 3be7567 both failed. Every check below ran on sentry (Arch Linux, x86-64), and each clippy run started with `cargo clean -p kwin-mcp -p screen-13-window`, so no result is a cached one.

| Toolchain | rustfmt | clippy | `cargo fmt --all -- --check`, master → this change | `cargo clippy --all-targets`, master → this change |
| --- | --- | --- | --- | --- |
| 1.97.1, rustup (engi's, the one #987 names) | 1.9.0 | 0.1.97 | exit 1, 536 hunks in 25 files → exit 0 | exit 101, 6 errors and 10 warnings → exit 0, nothing |
| 1.99.1, Arch `rust 1:1.99.1-1` (sentry's own) | 1.10.0 | 0.1.99 | exit 1, the same 536 hunks → exit 0 | exit 101, 6 errors and 44 warnings → exit 0, 34 warnings |
| 1.100.0, rustup (current stable) | 1.11.0 | 0.1.100 | exit 1, the same 536 hunks → exit 0 | exit 101, 6 errors and 45 warnings → exit 0, 35 warnings |

The warnings left under clippy 0.1.99 and 0.1.100 come from lints 0.1.97 does not have, at sites this change does not touch (#991). The repository has no CI and pins no toolchain. All three rustfmt versions format the lint commit into the committed style commit byte for byte.

## The two changes outside test code

### `PreparedPaste::Ready` holds a boxed `TextReceipt` (`large_enum_variant`)

`keyboard_paste` into a contenteditable, then into plain text beside it, in Chromium in a private 1280x720 session on sentry, with a debug `kwin-mcp` of each commit built there with 1.99.1:

| Build | Paste into the field | Paste onto plain text | After the first paste |
| --- | --- | --- | --- |
| master 3be7567 | `verified`, 17 of 17 characters observed | `unverified`, `paste_target_unobservable` | [`before.png`](before.png) |
| this change | `verified`, 17 of 17 characters observed | `unverified`, `paste_target_unobservable` | [`after.png`](after.png) |

The two builds' results are identical byte for byte, and the first paste is in the field in both screenshots.

### `install_local` cleans up through a let chain (`collapsible_if`)

`kwin-mcp-deploy install PKG engi DIR` on engi, with the binary of each commit built on sentry, in the three outcomes of the condition. The second and third installs fail on a symlink added to the package, and the third runs under `strace -e inject=unlinkat:error=EACCES`, so removing the staging directory fails too. Master's binary and this change's print the same:

```
== ok: $ kwin-mcp-deploy install pkg engi <w>/dest-ok
installed=engi:<w>/dest-ok mode=local
exit=0
staging dirs left: 0

== cleaned: $ kwin-mcp-deploy install pkg engi <w>/dest-cleaned
Error: "unsupported package entry: <w>/pkg/bin/zz-symlink"
exit=1
staging dirs left: 0
staging directory created: 1, removed (AT_REMOVEDIR on it): 1

== refused: $ strace -e inject=unlinkat:error=EACCES kwin-mcp-deploy install pkg engi <w>/dest-refused
could not remove local staging path <w>/dest-refused.incoming-<nonce>: Permission denied (os error 13)
Error: "unsupported package entry: <w>/pkg/bin/zz-symlink"
exit=1
staging dirs left: 1
```

## Reproduce

```sh
cargo clean -p kwin-mcp -p screen-13-window
cargo fmt --all -- --check
cargo clippy --all-targets --keep-going --message-format=short
```

Two checkouts that share one `CARGO_TARGET_DIR` can make clippy answer from the other checkout's cache, because cargo keys a path package's freshness to paths relative to its root and to their modification times: a master checkout once passed in 0.55 s here, answered from the cache of a checkout that had this change. Hence the `cargo clean -p` first.
