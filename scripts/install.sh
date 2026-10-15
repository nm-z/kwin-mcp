#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bash scripts/install.sh [OPTIONS]

Install the built Rust server and viewer as real executables.

  --source-dir DIR  Build output directory (default: repository target/release)
  --bin-dir DIR     Installation directory (default: $HOME/.local/bin)
  --replace         Replace existing files or symlinks at the destination
  -h, --help        Print this help

Build first with: cargo build --release --bins
Requires Bash, GNU coreutils, and jq.
The installer checks --help and --describe before replacing any executable.
It exits unsuccessfully if PATH still selects a different kwin-mcp afterward.
EOF
}

fail() {
    printf 'kwin-mcp install: %s\n' "$*" >&2
    exit 1
}

repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
source_dir="$repo_dir/target/release"
bin_dir=${HOME:+$HOME/.local/bin}
replace=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --source-dir|--bin-dir)
            [[ $# -ge 2 && -n $2 ]] || fail "$1 requires a directory"
            if [[ $1 == --source-dir ]]; then
                source_dir=$2
            else
                bin_dir=$2
            fi
            shift 2
            ;;
        --replace) replace=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown argument '$1'; use --help" ;;
    esac
done
[[ -n $bin_dir ]] || fail 'HOME is unset; provide --bin-dir'
command -v jq > /dev/null || fail 'jq is required to validate --describe output; install jq and rerun'

# Prepare both files before publishing either. Rename the directory entry rather
# than writing through an existing symlink into another package's installation.
for name in kwin-viewer kwin-mcp; do
    [[ -f "$source_dir/$name" && -x "$source_dir/$name" ]] ||
        fail "missing executable $source_dir/$name; run cargo build --release --bins"
    destination="$bin_dir/$name"
    if [[ -e $destination || -L $destination ]]; then
        [[ ! -d $destination || -L $destination ]] || fail "$destination is a directory"
        $replace || fail "$destination already exists; inspect it and rerun with --replace to replace this entry (symlink targets are preserved)"
    fi
done
mkdir -p -- "$bin_dir"
bin_dir=$(cd -- "$bin_dir" && pwd -P)
stage=$(mktemp -d "$bin_dir/.kwin-mcp-install.XXXXXX")
trap 'rm -r -- "$stage"' EXIT
for name in kwin-viewer kwin-mcp; do
    install -m 755 -- "$source_dir/$name" "$stage/$name"
done

if ! timeout 10 "$stage/kwin-mcp" --help </dev/null > "$stage/help.stdout" 2> "$stage/help.stderr"; then
    fail 'the source executable failed --help; build the Rust server from nm-z/kwin-mcp'
fi
help_text=$(<"$stage/help.stdout")
[[ $help_text == *'Usage: kwin-mcp'* ]] ||
    fail 'the source executable did not print kwin-mcp help; build the Rust server from nm-z/kwin-mcp'
if ! timeout 20 "$stage/kwin-mcp" --describe </dev/null > "$stage/describe.stdout" 2> "$stage/describe.stderr"; then
    fail 'the source executable failed --describe; build the Rust server from nm-z/kwin-mcp'
fi
[[ -s "$stage/describe.stdout" ]] ||
    fail 'the source executable printed no --describe output; build the Rust server from nm-z/kwin-mcp'
if ! jq -e '.initialize.capabilities.tools and (.tools | type == "array") and (.tools | length > 0)' "$stage/describe.stdout" > /dev/null 2> "$stage/describe.validation.stderr"; then
    fail 'the source executable did not print valid MCP tool schemas; build the Rust server from nm-z/kwin-mcp'
fi

for name in kwin-viewer kwin-mcp; do
    if $replace; then
        mv -fT -- "$stage/$name" "$bin_dir/$name"
    else
        mv -nT -- "$stage/$name" "$bin_dir/$name"
        [[ ! -e "$stage/$name" ]] || fail "$bin_dir/$name appeared during installation; inspect it and rerun with --replace"
    fi
done
printf 'Installed %s and %s\n' "$bin_dir/kwin-mcp" "$bin_dir/kwin-viewer"
resolved=$(type -P kwin-mcp || true)
if [[ -z $resolved || ! $resolved -ef "$bin_dir/kwin-mcp" ]]; then
    fail "installed successfully, but PATH selects ${resolved:-no kwin-mcp command}. Put $bin_dir first in PATH, clear your shell command cache (hash -r in Bash), or invoke $bin_dir/kwin-mcp by its absolute path"
fi
printf 'PATH selects %s. Clear any caller shell command cache (hash -r in Bash).\n' "$resolved"
