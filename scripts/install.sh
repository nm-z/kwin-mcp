#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: bash scripts/install.sh [OPTIONS]

Install the built Rust server and viewer as real executables.

  --source-dir DIR  Build output directory (default: repository target/release)
  --bin-dir DIR     Installation directory (default: $HOME/.local/bin)
  --provider-bin FILE  Install to this absolute configured kwin-mcp artifact path
  --replace         Replace existing files or symlinks at the destination
  -h, --help        Print this help

Build first with: cargo build --release --bins
Requires Bash, GNU coreutils, and jq.
The installer checks --help, --describe, and both version flags before replacing
any executable. It reports the installed package version, build, and Git commit.
Normally it exits unsuccessfully if PATH selects a different kwin-mcp afterward.
With --provider-bin, it verifies that exact MCP provider command instead of PATH.
The provider must be a real kwin-mcp file, not a symlink; its adjacent viewer is
installed too. Use --replace for an existing provider after verifying reload
compatibility. No provider is restarted and no MCP client config is changed.
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
provider_bin=
bin_dir_explicit=false
while [[ $# -gt 0 ]]; do
    case $1 in
        --source-dir|--bin-dir|--provider-bin)
            [[ $# -ge 2 && -n $2 ]] || fail "$1 requires a directory"
            if [[ $1 == --source-dir ]]; then
                source_dir=$2
            elif [[ $1 == --provider-bin ]]; then
                [[ -z $provider_bin ]] || fail '--provider-bin may be supplied only once'
                provider_bin=$2
            else
                bin_dir=$2
                bin_dir_explicit=true
            fi
            shift 2
            ;;
        --replace) replace=true; shift ;;
        -h|--help) usage; exit 0 ;;
        *) fail "unknown argument '$1'; use --help" ;;
    esac
done
if [[ -n $provider_bin ]]; then
    $bin_dir_explicit && fail '--provider-bin and --bin-dir cannot be combined'
    [[ $provider_bin == /* && ${provider_bin##*/} == kwin-mcp ]] ||
        fail '--provider-bin requires an absolute path ending in /kwin-mcp'
    [[ ! -L $provider_bin ]] ||
        fail 'the configured provider command is a symlink; use its resolved executable path so existing providers observe the replacement'
    bin_dir=${provider_bin%/*}
fi
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
for flag in --version -V; do
    if ! timeout 10 "$stage/kwin-mcp" "$flag" </dev/null > "$stage/$flag.stdout" 2> "$stage/$flag.stderr"; then
        fail "the source executable failed $flag; build the Rust server from nm-z/kwin-mcp"
    fi
    version_text=$(<"$stage/$flag.stdout")
    version_pattern='^kwin-mcp v[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+ \([[:xdigit:]]+\)$'
    [[ ! -s "$stage/$flag.stderr" && $(wc -l < "$stage/$flag.stdout") -eq 1 && $version_text =~ $version_pattern ]] ||
        fail "the source executable did not print an identifiable Rust version/build/commit for $flag; a same-named uv/PyPI entrypoint may be selected. Build the Rust server from nm-z/kwin-mcp"
done
cmp -s "$stage/--version.stdout" "$stage/-V.stdout" ||
    fail 'the source executable printed different --version and -V identities; build the Rust server from nm-z/kwin-mcp'

for name in kwin-viewer kwin-mcp; do
    if $replace; then
        mv -fT -- "$stage/$name" "$bin_dir/$name"
    else
        mv -nT -- "$stage/$name" "$bin_dir/$name"
        [[ ! -e "$stage/$name" ]] || fail "$bin_dir/$name appeared during installation; inspect it and rerun with --replace"
    fi
done
printf 'Installed %s and %s\n' "$bin_dir/kwin-mcp" "$bin_dir/kwin-viewer"
if [[ -n $provider_bin ]]; then
    printf 'Configured MCP provider command: %s\n' "$bin_dir/kwin-mcp"
    printf 'Version: %s\n' "$version_text"
    printf 'Existing compatible providers can reload this artifact in place; verify their session IDs and build identity before resuming work.\n'
    exit 0
fi
resolved=$(type -P kwin-mcp || true)
if [[ -z $resolved || ! $resolved -ef "$bin_dir/kwin-mcp" ]]; then
    fail "installed successfully, but PATH selects ${resolved:-no kwin-mcp command}. Put $bin_dir first in PATH, clear your shell command cache (hash -r in Bash), or invoke $bin_dir/kwin-mcp by its absolute path"
fi
printf 'PATH selects %s. Clear any caller shell command cache (hash -r in Bash).\n' "$resolved"
printf 'Version: %s\n' "$version_text"
