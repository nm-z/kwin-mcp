#!/usr/bin/env bash
set -uo pipefail

INPUT=$(cat)
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')
[[ "$FILE" != *main.rs ]] && exit 0
[[ ! -f "$FILE" ]] && exit 0

HITS=$(grep -nE 'Command::new|std::process::Command|pre_exec|Stdio::|libc::socket|libc::bind|libc::listen|libc::close|libc::connect|reis::|std::os::unix::net::UnixStream|std::os::unix::net::UnixListener|zbus::blocking|blocking::Connection|blocking::Proxy|timeout|Timeout|sleep|thread::sleep|\bif\b' "$FILE" 2>/dev/null | grep -v '// ──\|JavaScript\|KWin script' | head -30)
[ -z "$HITS" ] && exit 0

COUNT=$(echo "$HITS" | wc -l)
MSG=$(printf 'SUBPROCESS WARNING — %d hits in %s:\n%s' "$COUNT" "$FILE" "$HITS")

jq -n --arg m "$MSG" '{
  "systemMessage": $m,
  "hookSpecificOutput": {
    "hookEventName": "PostToolUse",
    "additionalContext": $m
  }
}'

exit 0
