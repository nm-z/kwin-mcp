# Issue #854: saved captures omit the pointer by default

The same real-system regression ran against the existing pre-fix binary
`v0.1.2.197 (8793bd8)` and the candidate `v0.1.2.209 (cc362b3 + this patch)`.
The pre-fix binary uses the same unconditional cursor default as issue #854's
reported version. Both runs used production stdio tools, a newly owned KWin
session, and a static local Chrome page, with the pointer inside the crop.

Host: engi, Linux 7.2.11, KWin 6.8.0, Mesa 26.2.5, AMD Navi 32 GPU.
The runner and binaries were copied to `/var/tmp/kwin-854-check` and run over
`ssh engi` because the worker's NoNewPrivs restriction prevents nested bwrap
startup (tracked separately in #856).

## Before and after

| Pre-fix `save_as` default | Fixed `save_as` default |
| --- | --- |
| ![Pointer baked into saved crop](before-saved-default.png) | ![Clean saved crop](after-saved-default.png) |

The fixed explicit `include_cursor: true` capture is
[after-explicit-cursor.png](after-explicit-cursor.png).

The old default is byte-identical to explicit `true`; the new default is
byte-identical to explicit `false`. The checked clean crop contains 375,000
identical background pixels; including the pointer changes 836 pixels.
The fixed explicit `true` image is also byte-identical to the old default.

## Regression command

```bash
KWIN_MCP_E2E=1 cargo test --test session_lifecycle_e2e \
  screenshot_can_omit_both_cursor_layers_without_moving_the_pointer -- --ignored --exact --nocapture
```

`KWIN_MCP_E2E_SERVER` selects the binary; `KWIN_MCP_E2E_PROOF_DIR` keeps PNGs
and metadata. The same compiled runner failed against the original binary
with `region: save_as default baked the pointer into the saved PNG` (exit
101), then passed against the candidate (exit 0). See
[baseline-test.log](baseline-test.log) and [candidate-test.log](candidate-test.log).

The candidate covers all nine saved combinations: full window, region crop,
and cursor crop, each with omitted/false/true `include_cursor`. It checks
saved/workdir/inline byte agreement, cursor metadata, an independently
verified uniform background, ordinary screenshots retaining their pointer,
and cursor-free pixels staying unchanged when the pointer moves.

Targeted `cargo clippy --bin kwin-mcp --test session_lifecycle_e2e` passed
with the existing `collapsible_if` and `large_enum_variant` warnings.
`-D warnings` fails on those existing warnings. Repository-wide
`cargo fmt --all -- --check` also reports existing formatting differences;
unrelated formatting was kept out of this patch. `git diff --check` passed.
