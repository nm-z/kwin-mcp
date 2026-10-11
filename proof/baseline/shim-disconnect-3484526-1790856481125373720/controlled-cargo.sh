#!/bin/bash
set -eu
[[ ${1-} == build && ( $# == 1 || ( $# == 2 && ${2-} == --release ) ) ]] || exit 125
read -r stat < "/proc/$BASHPID/stat"
read -r -a fields <<< "${stat##*) }"
kind=cargo-start
printf -v arguments ' %q' "$@"
printf '%s %s %s unix_s=%s cwd=%q argc=%s argv=%s\n' "$kind" "$BASHPID" "${fields[19]}" "$EPOCHREALTIME" "$PWD" "$#" "$arguments" >> '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856481125373720/held-start-invocations.log'
printf '%s %s\n' "$BASHPID" "${fields[19]}" > '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856481125373720/held-start-cargo.identity'
while [[ ! -f '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856481125373720/held-start-cargo.release' ]]; do
(( SECONDS < 180 )) || exit 124
/bin/sleep 0.02
done
kind=cargo-release
printf -v arguments ' %q' "$@"
printf '%s %s %s unix_s=%s cwd=%q argc=%s argv=%s\n' "$kind" "$BASHPID" "${fields[19]}" "$EPOCHREALTIME" "$PWD" "$#" "$arguments" >> '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856481125373720/held-start-invocations.log'
printf '%s %s\n' "$BASHPID" "${fields[19]}" > '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856481125373720/held-start-cargo.finished'
exit 0
