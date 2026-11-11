# Issue 855: System Monitor launch

Verified on engi with the production stdio server, Plasma 6, bubblewrap and a
fresh isolated desktop. The baseline binary is commit `cc362b3`; the candidate
contains this fix. Both ran the same committed regression test through the host
user manager because the dsh worker inherits `NoNewPrivs` (see #856 and #858).

- `baseline.json`: System Monitor exits 1, stderr is empty, KWin has no window.
  The intentional invalid Qt platform also loses its Qt diagnostic.
- `fixed.json`: System Monitor maps a window with the expected application
  class; the invalid Qt platform exits 134 and reports its missing plugin.
- `systemmonitor.png`: the candidate's window from the separate manual stdio
  reproduction, after its Overview page loaded. Some sensors are unavailable in
  the isolated desktop; this change does not expose the host sensor services.

The JSON files retain the relevant fields from the test's real RPC evidence.
The test additionally checks app service registration, distinct app/host bus
IDs, absence of the app's test service on the host bus, session wallet access,
and denied name ownership on the filtered host system bus.

Results:

- New regression against baseline: fails with the reported silent exit.
- New regression against candidate: passes.
- Existing `session_kwallet_is_session_local_and_never_opens_host_handles`: passes.
- `cargo clippy --bin kwin-mcp --test session_lifecycle_e2e`: passes with existing
  `large_enum_variant` and `collapsible_if` warnings.
- `git diff --check`: passes. Whole-repository rustfmt checking reports existing
  formatting differences in unrelated code, including the vendored window crate.
