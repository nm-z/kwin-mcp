# Resolve Issues #25 and #26

## Summary

Fix #25 first so the visible viewer is reliable, then fix #26 through the same real `session_start` boundary. Assume only KWin and Wayland, never Plasma, KDE session metadata, SDDM, or a particular login service.

No MCP schemas, TOML configuration, environment toggles, or new CLI flags will be added.

## Implementation Changes

### Host Wayland resolution, issue #25

- Add one typed host-display resolver with this exact precedence:
  1. Preserve an inherited `WAYLAND_DISPLAY`. Preserve inherited `XDG_RUNTIME_DIR`, or obtain only the missing runtime path from logind.
  2. Ask logind for the current user and its active display session. Require `Active=true`, `State=active`, and `Type=wayland`.
  3. Read `RuntimePath` from logind, connect directly to that user's systemd manager D-Bus, and read `WAYLAND_DISPLAY` from `org.freedesktop.systemd1.Manager.Environment`.
  4. If the user-manager value is absent or stale, select the lowest numeric `wayland-N` Unix socket directly under `RuntimePath`.
- Treat an inherited display as authoritative. If it is invalid, report that failure instead of replacing it with another display.
- Validate every resolved display as an existing Unix socket.
- Apply `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` only with `Command::env` on `kwin-viewer`. Never mutate process-global environment.
- Create `viewer.log` before resolution. Record resolution or spawn failure there while leaving MCP session startup functional.
- Collapse the duplicated wallet-failure/session-finalization branch so viewer resolution and session publication each occur once.

### Live home-socket exposure, issue #26

- Use the existing `procfs` dependency to read the host Unix socket table at `session_start`.
- Select named, unconnected Unix endpoints whose canonical parent is beneath canonical `$HOME` and whose live filesystem entry is a socket. Ignore abstract sockets and deduplicate paths.
- Leave sockets already beneath separately rebound nested mounts alone, since those directory mounts already preserve live replacement.
- For every other socket parent:
  - bind the host parent read-only at a numbered path under `/run/kwin-mcp-host-sockets`;
  - replace the socket entry in the applicable overlay upper layer or staging layer with a symlink to that hidden parent mount;
  - group sockets sharing one parent into one mount.
- Store generated links in session state. Remove only links still pointing to their generated targets during teardown or startup failure. Refuse to overwrite unrelated upper-layer entries.
- This follows replacement at socket paths discovered during `session_start`. Entirely new socket names require a fresh session.
- Generalize `OverlayPlan` and its Bubblewrap mount ordering. Do not add a socket proxy, watcher, retry path, or parallel mount implementation.
- Document that every active home socket is exposed automatically, including sensitive control sockets. Read-only mounting prevents host file writes but does not restrict operations available through each socket protocol. Hidden parent mounts also expose sibling originals through their internal paths.

## Interfaces and LOC Accounting

- Keep all existing MCP tool inputs and results unchanged.
- Keep existing width and height CLI behavior unchanged.
- Add no user configuration.
- Implement the two issues as separate logical changes and record each production Rust delta with `git diff --numstat`, allowing their net LOC impact to be reported independently.
- Reuse the current D-Bus, `procfs`, and overlay machinery. Add no dependency.

## End-to-End Test Plan

1. Build with `cargo build`; do not run Clippy unless separately requested.
2. Issue #25, inherited environment:
   - launch the production MCP entrypoint with the current Wayland environment;
   - call `session_start`, launch a real GUI app, and verify the host viewer visibly renders it and forwards interaction.
3. Issue #25, stripped environment:
   - launch a fresh production server with `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` removed;
   - verify logind and the user-manager environment recover the active KWin Wayland display through the same visible path.
4. Issue #25, numeric fallback:
   - save the exact user-manager `WAYLAND_DISPLAY`, temporarily unset it with restoration guaranteed on exit, and launch another stripped server;
   - verify the lowest numeric live `wayland-N` socket produces the same visible viewer behavior;
   - restore the original manager environment immediately.
5. Issue #26 baseline:
   - through the unfixed production MCP path, launch `konsole -e llm-mux` and capture the visible failure to reach `llm-mux.sock`.
6. Issue #26 fixed path:
   - repeat `session_start` and `launch_app` with `konsole -e llm-mux`;
   - verify the real `llm-mux` client visibly renders the daemon's provider/model snapshot. Do not use `llm-mux-tui`, which reads ledgers and does not exercise the socket.
7. Live replacement:
   - restart the real `llm-muxd.service` while keeping the KWin MCP session alive;
   - launch a fresh `llm-mux` client in that same session and verify it again renders the daemon snapshot.
8. Overlay regression:
   - save a file under `$HOME` through a real GUI action;
   - verify it exists inside the session but not in the host home, proving normal overlay writes remain isolated while socket access works.
9. Run each case separately, fix the observed cause, and rerun that exact real path before continuing. Logs, PIDs, exit codes, and socket-table entries are diagnostic evidence only, not behavioral proof.

## Assumptions

- KWin is the compositor and the session protocol is Wayland.
- All active pathname sockets beneath `$HOME` are intentionally exposed without an allowlist.
- Same-path socket replacement must remain live. New socket names appearing after startup require `session_stop` followed by `session_start`.
- The current llm-mux observer socket is the first real acceptance case.

## Implementation LOC

- Issue #25: 55 additions, 31 deletions, net +24 production Rust lines.
- Issue #26: 84 additions, 113 deletions, net -29 production Rust lines.
- Combined: 139 additions, 144 deletions, net -5 production Rust lines.
