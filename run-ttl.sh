#!/bin/bash
set -euo pipefail
mode=${1:?baseline or candidate}
root=/home/nate/builds/kwin-mcp-ttl-retire
case "$mode" in
  baseline) bin=/home/nate/kwin-mcp/target/release ;;
  candidate) bin=/home/nate/builds/kwin-mcp-lifecycle/target/release ;;
  *) exit 2 ;;
esac
export XDG_RUNTIME_DIR=/run/user/1000
export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus
export WAYLAND_DISPLAY=wayland-0
export KWIN_MCP_E2E=1
export KWIN_MCP_E2E_SHIM="$bin/kwin-mcp-shim"
export KWIN_MCP_E2E_SERVER="$bin/kwin-mcp"
export KWIN_MCP_PROOF_DIR="$root/proof/$mode-ttl"
mkdir -p "$KWIN_MCP_PROOF_DIR"
date -u +%FT%TZ
uptime
sha256sum "$KWIN_MCP_E2E_SHIM" "$KWIN_MCP_E2E_SERVER"
test_binary=/home/nate/builds/kwin-mcp-lifecycle/target/release/deps/shim_disconnect_e2e-6af1ac13d1318a65
"$test_binary" --exact shim_ttl_retires_two_headless_servers_on_one_connection --ignored --nocapture
if [[ $mode == candidate ]]; then
  "$test_binary" --exact standalone_ttl_retains_stdio_and_accepts_a_new_headless_session --ignored --nocapture
fi
