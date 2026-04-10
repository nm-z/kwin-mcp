# Writable Sessions

## Problem

Container sessions use an overlay on `$HOME` and `--ro-bind /` — all writes are ephemeral. When an agent installs an app (e.g. Obsidian AppImage) that requires GUI interaction to configure, the config files vanish when the session ends. Manually copying files from `/tmp` to the host after the session is slow and fragile.

## Design

`session_start` gains one optional boolean parameter: `writable` (default `false`).

### `writable: false` (default)

Current behavior. `--ro-bind / /`, `--overlay-src $HOME --tmp-overlay $HOME`. Everything ephemeral.

### `writable: true`

The host filesystem is writable inside the container. Agent writes persist to the host.

**$HOME:** `--bind $HOME $HOME` replaces `--overlay-src $HOME --tmp-overlay $HOME`.

**System paths:** `--bind` instead of inheriting `--ro-bind` for: `/opt`, `/usr/local`, `/usr/share/applications`, `/usr/share/icons`, `/etc`, `/var`. `/tmp` and `/run` remain `--tmpfs` (ephemeral) in both modes.

**Protected paths (read-only regardless):** The 9 files kwin-mcp configures for the virtual display session are `--ro-bind` over the real host files so the agent cannot modify them and the virtual session renders correctly:

- `~/.config/kwinrc` — window decorations, compositing
- `~/.config/kdeglobals` — scale, fonts, subpixel
- `~/.config/kwinrulesrc` — no-border maximized rule
- `~/.config/kscreenlockerrc` — screenlocker disabled
- `~/.config/kcmfonts` — forced 96 DPI
- `~/.config/fontconfig/fonts.conf` — hinting/antialias
- `/usr/share/defaults/at-spi2/accessibility.conf` — AT-SPI anonymous auth
- `/usr/share/fontconfig/conf.default/10-hinting-slight.conf` — system hinting override
- `/usr/share/fontconfig/conf.default/11-lcdfilter-default.conf` — system LCD override

### Implementation change

For the 6 `$HOME` config files, the entrypoint currently creates/patches them in-place. When `writable: true`, those writes would persist to the host. Instead:

1. Write the kwin-mcp config content to files in `host_xdg_dir` (already done for the 3 system configs).
2. For files created from scratch (kwinrc, kwinrulesrc, kscreenlockerrc, kcmfonts, fontconfig/fonts.conf): write to `host_xdg_dir`, add `--ro-bind` over the real path.
3. For kdeglobals (patched via sed): read the real file in Rust, apply patches, write patched version to `host_xdg_dir`, add `--ro-bind`.
4. Remove the corresponding writes from the entrypoint script (they're now handled by bwrap mounts).

This applies to both `writable: true` and `writable: false` — the entrypoint simplifies either way.

### Trust model

The MCP client shows `session_start(writable: true)` to the user before executing. The user approves or denies. One decision, one boolean.

### Tool response

Reports whether the session is writable or ephemeral so the agent knows.

## Decision record

Approach A (explicit `writable_paths` list) scored 93/100 vs Approach B (overlay + sync-back at session end) at 57/100 on project objectives. The `writable_paths` approach was then simplified to a single boolean after recognizing that path enumeration is unnecessary — the agent should write wherever it needs, and the server only needs to protect its own display configuration.
