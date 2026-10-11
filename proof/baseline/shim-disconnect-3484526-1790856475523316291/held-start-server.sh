#!/bin/bash
set -eu
read -r stat < "/proc/$BASHPID/stat"
read -r -a fields <<< "${stat##*) }"
case "${1-}" in
--describe) kind=describe ;;
--sweep-workdirs) kind=sweep ;;
*) kind=server ;;
esac
printf -v arguments ' %q' "$@"
printf '%s %s %s unix_s=%s cwd=%q argc=%s argv=%s\n' "$kind" "$BASHPID" "${fields[19]}" "$EPOCHREALTIME" "$PWD" "$#" "$arguments" >> '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856475523316291/held-start-invocations.log'
if [[ $kind == server ]]; then
printf '%s %s\n' "$BASHPID" "${fields[19]}" > '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856475523316291/held-start-server.identity'
while [[ ! -f '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856475523316291/held-start-server.release' ]]; do
(( SECONDS < 180 )) || exit 124
/bin/sleep 0.02
done
kind=server-delegate
printf -v arguments ' %q' "$@"
printf '%s %s %s unix_s=%s cwd=%q argc=%s argv=%s\n' "$kind" "$BASHPID" "${fields[19]}" "$EPOCHREALTIME" "$PWD" "$#" "$arguments" >> '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3484526-1790856475523316291/held-start-invocations.log'
fi
exec '/home/nate/kwin-mcp/target/release/kwin-mcp' "$@"
