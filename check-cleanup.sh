#!/bin/bash
set -euo pipefail
root=/home/nate/builds/kwin-mcp-background/proof
mapfile -t files < <(fd -a background-receipts.jsonl "$root")
((${#files[@]} > 0))
jq -r '.processes[] | [.pid,.starttime] | @tsv' "${files[@]}" | sort -u > "$root/recorded-identities.tsv"
jq -r '[.private_home,.tmpdir][]' "${files[@]}" | sort -u > "$root/recorded-paths.txt"
failed=0
count=0
while IFS=$'\t' read -r pid birth; do
  count=$((count+1))
  if [[ -r /proc/$pid/stat ]] && read -r stat < "/proc/$pid/stat"; then
    read -r -a fields <<< "${stat##*) }"
    if [[ ${fields[19]} == "$birth" ]]; then
      printf 'remaining_identity=%s:%s\n' "$pid" "$birth"
      failed=1
    fi
  fi
done < "$root/recorded-identities.tsv"
paths=0
while IFS= read -r path; do
  paths=$((paths+1))
  if [[ -e $path || -L $path ]]; then
    printf 'remaining_path=%s\n' "$path"
    failed=1
  fi
done < "$root/recorded-paths.txt"
date -u +checked_utc=%FT%TZ
printf 'recorded_identities=%s\nrecorded_paths=%s\ncleanup_failure=%s\n' "$count" "$paths" "$failed"
sha256sum /home/nate/kwin-mcp/target/release/kwin-mcp /home/nate/kwin-mcp/target/release/kwin-mcp-shim
exit "$failed"
