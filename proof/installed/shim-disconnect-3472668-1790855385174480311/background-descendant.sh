#!/bin/bash
set -eu
trap '' TERM HUP
/bin/sleep 180 &
lease=$!
read -r stat < "/proc/$BASHPID/stat"
read -r -a fields <<< "${stat##*) }"
printf '%s %s\n' "$BASHPID" "${fields[19]}" > '/home/nate/builds/kwin-mcp-background/proof-home/shim-disconnect-3472668-1790855385174480311/background-descendant.identity'
printf 'background descendant pid=%s birth=%s lease=%s lifetime_s=180\n' "$BASHPID" "${fields[19]}" "$lease" >&2
wait "$lease"
printf 'descendant %s expired\n' "$BASHPID" >> '/home/nate/builds/kwin-mcp-background/proof-home/shim-disconnect-3472668-1790855385174480311/background-helper-expired.log'
exit 124
