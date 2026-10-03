#!/usr/bin/env bash
set -euo pipefail
source_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
binary=$(realpath "${1:-$source_root/target/release/kwin-mcp-shim}")
command -v strace >/dev/null
command -v timeout >/dev/null
command -v rg >/dev/null
command -v fd >/dev/null
scratch_parent=${XDG_CACHE_HOME:-$HOME/.cache}
mkdir -p "$scratch_parent"
scratch=$(mktemp -d "$scratch_parent/kwin-shim-cli.XXXXXX")
trap 'rm -rf -- "$scratch"' EXIT
mkdir "$scratch/runtime" "$scratch/cache"
mkfifo "$scratch/input"
exec 9<>"$scratch/input"

check_exit() {
    local label=$1 expected_status=$2 expected_text=$3 stream=$4
    shift 4
    local status=0
    # Keep stdin open: a relay must not pass merely because it sees EOF.
    timeout --kill-after=1s 3s \
        env XDG_RUNTIME_DIR="$scratch/runtime" XDG_CACHE_HOME="$scratch/cache" \
        KWIN_MCP_BINARY="$scratch/missing-server" KWIN_MCP_REPO="$scratch/no-repo" \
        strace -f -qq -e trace=process -o "$scratch/$label.trace" \
        "$binary" "$@" <"$scratch/input" \
        >"$scratch/$label.out" 2>"$scratch/$label.err" || status=$?
    if [[ $status != "$expected_status" ]]; then
        printf '%s: expected exit %s, got %s\n' "$label" "$expected_status" "$status" >&2
        return 1
    fi
    rg -Fq -- "$expected_text" "$scratch/$label.$stream"
    if [[ $stream == out ]]; then
        [[ ! -s $scratch/$label.err ]]
    else
        [[ ! -s $scratch/$label.out ]]
        rg -Fq 'Usage: kwin-mcp-shim' "$scratch/$label.err"
    fi
    # The one exec is the shim itself. No child, runtime thread, or helper starts.
    [[ $(rg -Fc 'execve(' "$scratch/$label.trace") == 1 ]]
    if rg -q '(clone3?|fork|vfork|execveat)\(' "$scratch/$label.trace"; then
        printf '%s: started another process or thread\n' "$label" >&2
        return 1
    fi
    [[ -z $(fd --hidden --no-ignore . "$scratch/runtime" "$scratch/cache") ]]
}

check_exit help 0 'Usage: kwin-mcp-shim' out --help
check_exit short-help 0 'Usage: kwin-mcp-shim' out -h
check_exit version 0 'kwin-mcp-shim ' out --version
check_exit unknown 2 "unknown argument '--unknown'" err --unknown
check_exit help-unknown 2 "unknown argument '--unknown'" err --help --unknown
check_exit positional 2 "unknown argument 'unexpected'" err unexpected
check_exit missing-width 2 '--width requires a value' err --width
check_exit missing-ttl 2 '--ttl requires a value' err --ttl
check_exit flag-as-value 2 '--width requires a value' err --width --unknown
check_exit session-options 0 'Usage: kwin-mcp-shim' out \
    --width 800 --height 600 --no-override --no-viewer --autoclean --ttl 1 \
    --memory-high 0 --memory-max 4 --memory-swap-max 1 --help
printf 'PASS: 10 CLI cases; no relay, child, runtime thread, or state files\n'
