#!/bin/bash
# End-to-end MCP tool test — mirrors manual testing exactly.
# Uses a persistent coprocess, no sleeps, blocking reads with deadlines.
set -euo pipefail

SHOW_LOG=false
BINARY="./target/debug/kwin-mcp"
for arg in "$@"; do
    case "$arg" in
        --log) SHOW_LOG=true ;;
        *) BINARY="$arg" ;;
    esac
done
PASS=0 FAIL=0 ID=0
FAIL_MSGS=""

# Snapshot existing kwin/dbus processes before test
PRE_KWIN=$(ps -eo pid,cmd | grep "kwin_wayland.*--virtual" | grep -v grep | awk '{print $1}' | sort || true)
PRE_DBUS=$(ps -eo pid,cmd | grep "dbus-daemon.*\.tmp" | grep -v grep | awk '{print $1}' | sort || true)
PRE_KWIN_COUNT=$(echo "$PRE_KWIN" | grep -c . || true)
PRE_DBUS_COUNT=$(echo "$PRE_DBUS" | grep -c . || true)
PRE_TOTAL=$((PRE_KWIN_COUNT + PRE_DBUS_COUNT))
echo "== pre-existing stragglers: $PRE_TOTAL ($PRE_KWIN_COUNT kwin, $PRE_DBUS_COUNT dbus-daemon) =="

coproc MCP { "$BINARY" 2>/tmp/kwin-mcp-e2e-stderr.log; }
MCP_PID=${MCP_PID}
echo "== MCP server PID: $MCP_PID =="
cleanup() { kill $MCP_PID 2>/dev/null; wait $MCP_PID 2>/dev/null; }
trap cleanup EXIT

send() { echo "$1" >&"${MCP[1]}"; }
recv() {
    local line
    read -r -t "${2:-30}" line <&"${MCP[0]}" || { echo "TIMEOUT"; return 1; }
    echo "$line"
}

pass() { PASS=$((PASS + 1)); }
fail() { FAIL=$((FAIL + 1)); FAIL_MSGS="${FAIL_MSGS}  FAIL: $1\n"; }

# ── 1. Initialize ──
echo "== Initialize =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"e2e-test\",\"version\":\"1\"}}}"
RESP=$(recv 10)
echo "$RESP" | jq -e '.result.protocolVersion' >/dev/null 2>&1 && pass "protocolVersion present" || fail "protocolVersion missing"
echo "$RESP" | jq -re '.result.serverInfo.name' 2>/dev/null | grep -q "kwin-mcp" && pass "server name is kwin-mcp" || fail "wrong server name"
echo "$RESP" | jq -e '.result.capabilities.tools' >/dev/null 2>&1 && pass "tool capabilities present" || fail "no tool capabilities"
send '{"jsonrpc":"2.0","method":"notifications/initialized"}'

# ── 2. tools/list ──
echo "== tools/list =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/list\",\"params\":{}}"
RESP=$(recv 10)

TOOL_COUNT=$(echo "$RESP" | jq '.result.tools | length' 2>/dev/null)
[ "$TOOL_COUNT" = "12" ] && pass "12 tools registered" || fail "expected 12 tools, got $TOOL_COUNT"

for tool in session_start session_stop screenshot mouse_click mouse_move mouse_scroll mouse_drag keyboard_type keyboard_key launch_app accessibility_tree find_ui_elements; do
    echo "$RESP" | jq -e --arg t "$tool" '.result.tools[] | select(.name == $t)' >/dev/null 2>&1 && pass "tool '$tool' exists" || fail "tool '$tool' missing"
done

# Schema: no bare 'true' values (the bug that broke tool registration)
BAD=$(echo "$RESP" | jq '[.result.tools[].inputSchema.properties // {} | to_entries[] | select(.value == true)] | length' 2>/dev/null || echo "0")
[ "$BAD" = "0" ] && pass "no bare 'true' in schemas" || fail "$BAD properties have bare 'true'"

# FlexInt type check
XTYPE=$(echo "$RESP" | jq -r '.result.tools[] | select(.name == "mouse_click") | .inputSchema.properties.x.type[0]' 2>/dev/null)
[ "$XTYPE" = "integer" ] && pass "FlexInt emits integer type" || fail "FlexInt type is '$XTYPE'"

# Annotations
echo "$RESP" | jq -e '.result.tools[] | select(.name == "screenshot") | .annotations.readOnlyHint == true' >/dev/null 2>&1 && pass "screenshot is read-only" || fail "screenshot missing read-only"
echo "$RESP" | jq -e '.result.tools[] | select(.name == "session_stop") | .annotations.destructiveHint == true' >/dev/null 2>&1 && pass "session_stop is destructive" || fail "session_stop missing destructive"

# ── 3. session_start ──
echo "== session_start =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"session_start\",\"arguments\":{}}}"
RESP=$(recv 30)
TEXT=$(echo "$RESP" | jq -r '.result.content[0].text' 2>/dev/null)
[ -n "$TEXT" ] && [ "$TEXT" != "null" ] && pass "session_start succeeds" || fail "session_start failed: $(echo "$RESP" | head -c 200)"
SCRDIR_PID=$(echo "$TEXT" | sed -n 's/.*socket=wayland-mcp-\([0-9]*\).*/\1/p')
LOG="/tmp/kwin-mcp-${SCRDIR_PID}/kwin-mcp.log"
echo "== runtime log: $LOG =="
echo "$RESP" | jq -e '.result.isError == false' >/dev/null 2>&1 && pass "isError is false" || fail "isError not false"
echo "$TEXT" | grep -q "kwin-mcp v" && pass "version stamp in output" || fail "no version stamp"

# ── 4. launch_app ──
echo "== launch_app =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"launch_app\",\"arguments\":{\"command\":\"xterm\"}}}"
RESP=$(recv 10)
echo "$RESP" | jq -re '.result.content[0].text' 2>/dev/null | grep -q "launched pid=" && pass "launch_app returns PID" || fail "launch_app failed"

# ── 4b. check Xwayland is running ──
echo "== xwayland check =="
KWIN_PID=$(echo "$TEXT" | sed -n 's/.*pid=\([0-9]*\).*/\1/p')
sleep 2
XWAYLAND_COUNT=$(ps --ppid "$KWIN_PID" -o cmd 2>/dev/null | grep -c Xwayland || true)
[ "$XWAYLAND_COUNT" -gt 0 ] && pass "Xwayland running (children of kwin $KWIN_PID)" || fail "Xwayland not running (0 children of kwin $KWIN_PID)"

# ── 5. screenshot ──
echo "== screenshot =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"screenshot\",\"arguments\":{}}}"
RESP=$(recv 15)
if echo "$RESP" | jq -e '.error' >/dev/null 2>&1 || echo "$RESP" | jq -e '.result.isError == true' >/dev/null 2>&1; then
    fail "screenshot — No active window (apps don't create windows in isolated session)"
else
    pass "screenshot returns content"
fi

# ── 6. mouse_move (expected fail — needs active window for coord offset) ──
echo "== mouse_move =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"mouse_move\",\"arguments\":{\"x\":500,\"y\":500}}}"
RESP=$(recv 10)
if echo "$RESP" | jq -e '.error' >/dev/null 2>&1; then
    fail "mouse_move — No active window"
else
    pass "mouse_move works"
fi

# ── 7. mouse_click (expected fail — same) ──
echo "== mouse_click =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"mouse_click\",\"arguments\":{\"x\":500,\"y\":500}}}"
RESP=$(recv 10)
if echo "$RESP" | jq -e '.error' >/dev/null 2>&1; then
    fail "mouse_click — No active window"
else
    pass "mouse_click works"
fi

# ── 8. mouse_scroll (expected fail — same) ──
echo "== mouse_scroll =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"mouse_scroll\",\"arguments\":{\"x\":500,\"y\":500,\"delta\":3}}}"
RESP=$(recv 10)
if echo "$RESP" | jq -e '.error' >/dev/null 2>&1; then
    fail "mouse_scroll — No active window"
else
    pass "mouse_scroll works"
fi

# ── 9. mouse_drag (expected fail — same) ──
echo "== mouse_drag =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"mouse_drag\",\"arguments\":{\"from_x\":100,\"from_y\":100,\"to_x\":200,\"to_y\":200}}}"
RESP=$(recv 10)
if echo "$RESP" | jq -e '.error' >/dev/null 2>&1; then
    fail "mouse_drag — No active window"
else
    pass "mouse_drag works"
fi

# ── 10. keyboard_type ──
echo "== keyboard_type =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"keyboard_type\",\"arguments\":{\"text\":\"hello\"}}}"
RESP=$(recv 10)
echo "$RESP" | jq -re '.result.content[0].text' 2>/dev/null | grep -q "typed: hello" && pass "keyboard_type works" || fail "keyboard_type failed"

# ── 11. keyboard_key ──
echo "== keyboard_key =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"keyboard_key\",\"arguments\":{\"key\":\"Return\"}}}"
RESP=$(recv 10)
echo "$RESP" | jq -re '.result.content[0].text' 2>/dev/null | grep -q "key: Return" && pass "keyboard_key works" || fail "keyboard_key failed"

# ── 12. accessibility_tree (expected fail — AT-SPI registry not running) ──
echo "== accessibility_tree =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"accessibility_tree\",\"arguments\":{\"max_depth\":2}}}"
RESP=$(recv 10)
if echo "$RESP" | jq -e '.error' >/dev/null 2>&1; then
    fail "accessibility_tree — AT-SPI registry not running in isolated session"
else
    pass "accessibility_tree works"
fi

# ── 13. find_ui_elements (expected fail — not implemented) ──
echo "== find_ui_elements =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"find_ui_elements\",\"arguments\":{\"query\":\"button\"}}}"
RESP=$(recv 10)
if echo "$RESP" | jq -e '.error' >/dev/null 2>&1; then
    fail "find_ui_elements — not yet implemented"
else
    pass "find_ui_elements works"
fi

# ── 14. session_stop ──
echo "== session_stop =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"session_stop\",\"arguments\":{}}}"
RESP=$(recv 30 || echo "TIMEOUT")
echo "$RESP" | jq -re '.result.content[0].text' 2>/dev/null | grep -q "stopped pid=" && pass "session_stop works" || fail "session_stop failed: $RESP"

# ── 15. tools after stop should error ──
echo "== post-stop error check =="
ID=$((ID + 1))
send "{\"jsonrpc\":\"2.0\",\"id\":$ID,\"method\":\"tools/call\",\"params\":{\"name\":\"screenshot\",\"arguments\":{}}}"
RESP=$(recv 10)
if echo "$RESP" | jq -e '.error' >/dev/null 2>&1 || echo "$RESP" | jq -e '.result.isError == true' >/dev/null 2>&1; then
    pass "tools error after session_stop"
else
    fail "tools should error after session_stop"
fi

# ── 16. orphan check — no stragglers after session_stop ──
echo "== orphan check =="
# Compare against pre-test snapshot — only count NEW processes
POST_KWIN=$(ps -eo pid,cmd | grep "kwin_wayland.*--virtual" | grep -v grep | awk '{print $1}' | sort || true)
POST_DBUS=$(ps -eo pid,cmd | grep "dbus-daemon.*\.tmp" | grep -v grep | awk '{print $1}' | sort || true)
NEW_KWIN=$(comm -13 <(echo "$PRE_KWIN") <(echo "$POST_KWIN") | wc -l)
NEW_DBUS=$(comm -13 <(echo "$PRE_DBUS") <(echo "$POST_DBUS") | wc -l)
TOTAL_NEW=$((NEW_KWIN + NEW_DBUS))
if [ "$TOTAL_NEW" -eq 0 ]; then
    pass "no new orphans after session_stop (0 stragglers)"
else
    fail "found $TOTAL_NEW new stragglers ($NEW_KWIN kwin, $NEW_DBUS dbus-daemon)"
    comm -13 <(echo "$PRE_KWIN") <(echo "$POST_KWIN") | xargs -r -I{} ps -p {} -o pid,cmd || true
    comm -13 <(echo "$PRE_DBUS") <(echo "$POST_DBUS") | xargs -r -I{} ps -p {} -o pid,cmd || true
fi

# ── Results ──
echo ""
echo "═══════════════════════════════════════════"
if [ "$FAIL" -gt 0 ]; then
    echo "  FAILURES:"
    printf "%b" "$FAIL_MSGS"
    if $SHOW_LOG && [ -f "$LOG" ]; then
        echo ""
        echo "  LOG (errors only):"
        grep -i "error\|fatal\|fail\|crash" "$LOG" 2>/dev/null | head -20 | sed 's/^/  /'
    fi
fi
# Count stale X lock files
STALE_LOCKS=0
for f in /tmp/.X*-lock; do
    [ -f "$f" ] || continue
    pid=$(cat "$f" 2>/dev/null | tr -d ' ')
    if [ -n "$pid" ] && ! ps -p "$pid" > /dev/null 2>&1; then
        STALE_LOCKS=$((STALE_LOCKS + 1))
    fi
done

echo ""
echo "  COUNTS:"
echo "    pre-existing stragglers: $PRE_TOTAL ($PRE_KWIN_COUNT kwin, $PRE_DBUS_COUNT dbus)"
echo "    new stragglers:          $TOTAL_NEW ($NEW_KWIN kwin, $NEW_DBUS dbus)"
echo "    stale X lock files:      $STALE_LOCKS"
echo ""
echo "  $PASS pass, $FAIL fail"
echo "═══════════════════════════════════════════"
[ "$FAIL" -eq 0 ] || exit 1
