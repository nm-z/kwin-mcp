#!/usr/bin/env bash
set -euo pipefail

binary=${1:?usage: cli_version_e2e.sh /path/to/kwin-mcp expected-git-commit /path/to/evidence}
commit=${2:?provide the expected embedded Git commit}
evidence=${3:?provide an evidence directory}
binary=$(realpath "$binary")
mkdir -p "$evidence"

for flag in --version -V; do
    label=${flag#--}
    label=${label#-}
    timeout 10 "$binary" "$flag" > "$evidence/$label.stdout" 2> "$evidence/$label.stderr"
    [[ ! -s "$evidence/$label.stderr" ]]
    [[ $(wc -l < "$evidence/$label.stdout") -eq 1 ]]
    rg -q '^kwin-mcp v[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+ \([[:xdigit:]]+\)$' "$evidence/$label.stdout"
    [[ $(sed -n 's/.*(\([^)]*\)).*/\1/p' "$evidence/$label.stdout") == "$commit" ]]
done
cmp "$evidence/version.stdout" "$evidence/V.stdout"
timeout 10 "$binary" --help > "$evidence/help.stdout" 2> "$evidence/help.stderr"
[[ ! -s "$evidence/help.stderr" ]]
rg -q -- '--version' "$evidence/help.stdout"
printf 'version=PASS (exit 0, package/build/commit)\nshort_version=PASS (identical, exit 0)\nhelp_documents_version=PASS\n' | tee "$evidence/results.txt"
