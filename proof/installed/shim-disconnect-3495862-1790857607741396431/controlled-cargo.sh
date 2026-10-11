#!/bin/bash
set -eu
[[ ${1-} == build && ( $# == 1 || ( $# == 2 && ${2-} == --release ) ) ]] || exit 125
kind=cargo
read -r stat < "/proc/$BASHPID/stat"
read -r -a fields <<< "${stat##*) }"
{ printf '%s %s %s cwd=%q argc=%s argv=' "$kind" "$BASHPID" "${fields[19]}" "$PWD" "$#"; printf ' %q' "$@"; printf '\n'; } >> '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3495862-1790857607741396431/background-invocations.log'
printf 'background invocation kind=%s pid=%s birth=%s argc=%s\n' "$kind" "$BASHPID" "${fields[19]}" "$#" >&2
trap '' TERM HUP
/bin/sleep 180 &
lease=$!
setsid /bin/bash '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3495862-1790857607741396431/background-descendant.sh' </dev/null &
descendant=$!
read -r stat < "/proc/$BASHPID/stat"
read -r -a fields <<< "${stat##*) }"
printf '%s %s\n' "$BASHPID" "${fields[19]}" > '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3495862-1790857607741396431/background-helper.identity'
printf 'background helper kind=%s pid=%s birth=%s descendant=%s lease=%s lifetime_s=180\n' "$kind" "$BASHPID" "${fields[19]}" "$descendant" "$lease" >&2
wait "$lease"
printf 'helper %s expired\n' "$BASHPID" >> '/home/nate/builds/kwin-mcp-cancel-start/proof-home/shim-disconnect-3495862-1790857607741396431/background-helper-expired.log'
exit 124
