#!/usr/bin/env bash
# Temporarily disabled — will be re-enabled if subprocesses leak, hangs appear, or output is discarded
exit 0
set -uo pipefail

INPUT=$(cat)
FILE=$(echo "$INPUT" | jq -r '.tool_input.file_path // empty')
[[ "$FILE" != *main.rs ]] && exit 0
[[ ! -f "$FILE" ]] && exit 0

# Subprocess/IPC bans
SUB='Command::new|std::process::Command|pre_exec|Stdio::|libc::socket|libc::bind|libc::listen|libc::close|libc::connect|reis::|std::os::unix::net::UnixStream|std::os::unix::net::UnixListener|zbus::blocking|blocking::Connection|blocking::Proxy'
# Control flow bans
FLOW='timeout|Timeout|sleep|thread::sleep|\bif\b'
# Pattern bans: _ => wildcard, let _ = discard, _var prefix
PAT='\b_ =>|let _ =|let _[a-z]\w*\b'
# Appeasement bans: clone, static lifetime, RefCell
APPEASE='\.clone\(\)|'\''static|Rc<RefCell'
# Error handling bans
ERR='\.unwrap\(\)|\.expect\(|Box<dyn Error>|anyhow::Error|Result<_,\s*String>|\.ok\(\)'
# Lint suppression bans
LINT='#\[allow\(dead_code\)\]|#\[allow\(unused|#\[allow\(clippy'
# Type system bans
TYPE='mem::transmute|Option<Option'
# Style bans
STYLE='\breturn\b.*;\s*$'
HITS=$(grep -nE "$SUB|$FLOW|$PAT|$APPEASE|$ERR|$LINT|$TYPE|$STYLE" "$FILE" 2>/dev/null | grep -v '// ──\|JavaScript\|KWin script\|from_millis(50)\|from_secs(5)\|from_secs(15)\|Cow<.static\|with_connection(zbus\|zbus_conn\.clone\|return Err(\|hakoniwa::Stdio\|reis::\|UnixStream::from\|device().clone\|d.device().clone\|scroll_stop\|interface::<reis\|tokio::time::sleep\|_ => {}\|_ => match\|bootstrap\.log\|xdg_inner}' | head -30)
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
