#!/bin/bash
set -euo pipefail
release=$1
proof=$2
mkdir -p "$proof/bin" "$proof/home"
cp "$release/kwin-mcp-shim" "$proof/bin/kwin-mcp-shim"
export KWIN_MCP_BINARY="$release/kwin-mcp" KWIN_MCP_REPO=/nonexistent
coproc SHIM { HOME="$proof/home" "$proof/bin/kwin-mcp-shim" --autoclean 2>"$proof/stderr.log"; }
shim=$SHIM_PID
input=${SHIM[1]}
output=${SHIM[0]}
cleanup() { exec {input}>&- 2>/dev/null || true; wait "$shim" 2>/dev/null || true; }
trap cleanup EXIT
response() {
    local wanted=$1 line deadline=$((SECONDS+20))
    while (( SECONDS < deadline )); do
        if IFS= read -r -t 2 line <&"$output"; then
            if jq -e --argjson id "$wanted" '.id == $id' <<<"$line" >/dev/null; then
                printf '%s\n' "$line" > "$proof/response-$wanted.json"
                return 0
            fi
        elif ! kill -0 "$shim" 2>/dev/null; then
            return 1
        fi
    done
    return 1
}
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"shutdown-self-upgrade","version":"1"}}}' >&"$input"
response 1
printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"window_list","arguments":{}}}' >&"$input"
response 2
jq -e '.error.message | contains("no session")' "$proof/response-2.json" >/dev/null
deadline=$((SECONDS+20))
until rg -q 'child .* exited' "$proof/stderr.log"; do
    (( SECONDS < deadline )) || exit 1
    sleep 0.1
done
cp "$release/kwin-mcp-shim" "$proof/bin/next-shim"
mv "$proof/bin/next-shim" "$proof/bin/kwin-mcp-shim"
deadline=$((SECONDS+20))
until rg -q 're-executed after a rebuild' "$proof/stderr.log"; do
    (( SECONDS < deadline )) || exit 1
    sleep 0.1
done
printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"ping"}' >&"$input"
response 3
jq -e '.result == {}' "$proof/response-3.json" >/dev/null
exec {input}>&-
wait "$shim"
trap - EXIT
printf 'shim_pid=%s\nself_upgrade=true\npost_upgrade_ping=true\nshutdown_exit=0\n' "$shim" > "$proof/result.txt"
cat "$proof/result.txt"
