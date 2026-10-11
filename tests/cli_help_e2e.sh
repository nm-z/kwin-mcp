#!/usr/bin/env bash
set -euo pipefail

binary=${1:?usage: cli_help_e2e.sh /path/to/kwin-mcp /path/to/evidence}
evidence=${2:?provide an evidence directory}
mkdir -p "$evidence"
binary=$(realpath "$binary")

for flag in --help -h; do
    label=${flag#--}
    label=${label#-}
    timeout 10 "$binary" "$flag" > "$evidence/$label.stdout" 2> "$evidence/$label.stderr"
    [[ ! -s "$evidence/$label.stderr" ]]
    rg -q '^Usage: kwin-mcp' "$evidence/$label.stdout"
    for option in --help --describe --width --height --no-override --no-viewer --autoclean --ttl --memory-high --memory-max --memory-swap-max --stats; do
        rg -q -- "$option" "$evidence/$label.stdout"
    done
done
cmp "$evidence/help.stdout" "$evidence/h.stdout"

timeout 20 "$binary" --describe --width 1232 --height 924 > "$evidence/describe.json" 2> "$evidence/describe.stderr"
jq -e '.initialize.capabilities.tools and (.tools | type == "array") and (.tools | length > 0)' "$evidence/describe.json" > /dev/null

set +e
timeout 10 "$binary" --invalid-cli-help-test > "$evidence/invalid.stdout" 2> "$evidence/invalid.stderr"
status=$?
set -e
[[ $status -ne 0 && $status -ne 124 ]]
[[ ! -s "$evidence/invalid.stdout" ]]
rg -q "unknown argument '--invalid-cli-help-test'" "$evidence/invalid.stderr"
rg -q 'Usage: kwin-mcp' "$evidence/invalid.stderr"
printf 'help=PASS (exit 0)\nshort_help=PASS (exit 0)\ndescribe=PASS (exit 0, valid tool schemas)\ninvalid=PASS (exit %s)\n' "$status" | tee "$evidence/results.txt"
