#!/bin/bash
set -eu
case "${1-}" in
--describe) kind=describe ;;
--sweep-workdirs)
if (( $# == 1 )); then kind=periodic-sweep; else kind=scoped-sweep; fi ;;
*) kind=server ;;
esac
read -r stat < "/proc/$BASHPID/stat"
read -r -a fields <<< "${stat##*) }"
{ printf '%s %s %s cwd=%q argc=%s argv=' "$kind" "$BASHPID" "${fields[19]}" "$PWD" "$#"; printf ' %q' "$@"; printf '\n'; } >> '/home/nate/builds/kwin-mcp-background/proof-home/shim-disconnect-3471994-1790855341654297459/background-invocations.log'
printf 'background invocation kind=%s pid=%s birth=%s argc=%s\n' "$kind" "$BASHPID" "${fields[19]}" "$#" >&2
if [[ $kind == server ]]; then
printf '%s %s\n' "$BASHPID" "${fields[19]}" > '/home/nate/builds/kwin-mcp-background/proof-home/shim-disconnect-3471994-1790855341654297459/background-server.identity'
while [[ ! -f '/home/nate/builds/kwin-mcp-background/proof-home/shim-disconnect-3471994-1790855341654297459/background-server.release' ]]; do
(( SECONDS < 180 )) || exit 124
/bin/sleep 0.02
done
fi
if [[ $kind != periodic-sweep ]]; then exec '/home/nate/kwin-mcp/target/release/kwin-mcp' "$@"; fi
trap '' TERM HUP
/bin/sleep 180 &
lease=$!
setsid /bin/bash '/home/nate/builds/kwin-mcp-background/proof-home/shim-disconnect-3471994-1790855341654297459/background-descendant.sh' </dev/null &
descendant=$!
read -r stat < "/proc/$BASHPID/stat"
read -r -a fields <<< "${stat##*) }"
printf '%s %s\n' "$BASHPID" "${fields[19]}" > '/home/nate/builds/kwin-mcp-background/proof-home/shim-disconnect-3471994-1790855341654297459/background-helper.identity'
printf 'background helper kind=%s pid=%s birth=%s descendant=%s lease=%s lifetime_s=180\n' "$kind" "$BASHPID" "${fields[19]}" "$descendant" "$lease" >&2
wait "$lease"
printf 'helper %s expired\n' "$BASHPID" >> '/home/nate/builds/kwin-mcp-background/proof-home/shim-disconnect-3471994-1790855341654297459/background-helper-expired.log'
exit 124
