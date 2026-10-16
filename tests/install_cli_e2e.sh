#!/usr/bin/env bash
set -euo pipefail

source_dir=$(realpath "${1:?usage: install_cli_e2e.sh /path/to/build-output /path/to/evidence}")
evidence=${2:?provide an evidence directory}
mkdir -p -- "$evidence"
evidence=$(realpath "$evidence")
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
scratch=$(mktemp -d)
trap 'rm -r -- "$scratch"' EXIT
bin_dir="$scratch/bin with spaces"
mkdir -p -- "$bin_dir" "$scratch/other-package/viewer"
printf '#!/usr/bin/env bash\nexit 0\n' > "$scratch/other-package/kwin-mcp"
chmod +x "$scratch/other-package/kwin-mcp"
printf 'preserve this directory\n' > "$scratch/other-package/viewer/sentinel"
ln -s -- "$scratch/other-package/kwin-mcp" "$bin_dir/kwin-mcp"
ln -s -- "$scratch/other-package/viewer" "$bin_dir/kwin-viewer"
sha256sum "$scratch/other-package/kwin-mcp" > "$evidence/original-package.sha256"

# An existing installation needs explicit consent; neither symlink is followed.
if PATH="$bin_dir:$PATH" bash "$repo_dir/scripts/install.sh" --source-dir "$source_dir" --bin-dir "$bin_dir" > "$evidence/conflict.stdout" 2> "$evidence/conflict.stderr"; then
    printf 'installer unexpectedly replaced an existing command\n' >&2
    exit 1
fi
rg -q -- '--replace' "$evidence/conflict.stderr"
[[ -L "$bin_dir/kwin-mcp" && -L "$bin_dir/kwin-viewer" ]]

PATH="$bin_dir:$PATH" bash "$repo_dir/scripts/install.sh" --source-dir "$source_dir" --bin-dir "$bin_dir" --replace > "$evidence/install.stdout" 2> "$evidence/install.stderr"
[[ ! -L "$bin_dir/kwin-mcp" && ! -L "$bin_dir/kwin-viewer" ]]
cmp "$source_dir/kwin-mcp" "$bin_dir/kwin-mcp"
cmp "$source_dir/kwin-viewer" "$bin_dir/kwin-viewer"
sha256sum --check "$evidence/original-package.sha256"
[[ $(<"$scratch/other-package/viewer/sentinel") == 'preserve this directory' ]]

# Resolve the installed command through PATH, as in issue #313, rather than
# invoking the source artifact by its absolute path.
PATH="$bin_dir:$PATH" timeout 10 kwin-mcp --help > "$evidence/help.stdout" 2> "$evidence/help.stderr"
[[ ! -s "$evidence/help.stderr" ]]
rg -q '^Usage: kwin-mcp' "$evidence/help.stdout"
PATH="$bin_dir:$PATH" timeout 20 kwin-mcp --describe > "$evidence/describe.json" 2> "$evidence/describe.stderr"
jq -e '.initialize.capabilities.tools and (.tools | type == "array") and (.tools | length > 0)' "$evidence/describe.json" > /dev/null

# Reject a silent source launcher before publishing either file.
mkdir -p -- "$scratch/silent-source"
ln -s -- "$source_dir/kwin-viewer" "$scratch/silent-source/kwin-viewer"
cp -- "$scratch/other-package/kwin-mcp" "$scratch/silent-source/kwin-mcp"
if PATH="$scratch/rejected:$PATH" bash "$repo_dir/scripts/install.sh" --source-dir "$scratch/silent-source" --bin-dir "$scratch/rejected" > "$evidence/silent.stdout" 2> "$evidence/silent.stderr"; then
    printf 'installer unexpectedly accepted empty help\n' >&2
    exit 1
fi
rg -q 'did not print kwin-mcp help' "$evidence/silent.stderr"
[[ ! -e "$scratch/rejected/kwin-mcp" && ! -e "$scratch/rejected/kwin-viewer" ]]
cat > "$scratch/silent-source/kwin-mcp" <<'EOF'
#!/usr/bin/env bash
if [[ $1 == --help ]]; then printf 'Usage: kwin-mcp\n'; fi
EOF
if PATH="$scratch/rejected:$PATH" bash "$repo_dir/scripts/install.sh" --source-dir "$scratch/silent-source" --bin-dir "$scratch/rejected" > "$evidence/silent-describe.stdout" 2> "$evidence/silent-describe.stderr"; then
    printf 'installer unexpectedly accepted empty schemas\n' >&2
    exit 1
fi
rg -q 'printed no --describe output' "$evidence/silent-describe.stderr"
[[ ! -e "$scratch/rejected/kwin-mcp" && ! -e "$scratch/rejected/kwin-viewer" ]]
cat > "$scratch/silent-source/kwin-mcp" <<'EOF'
#!/usr/bin/env bash
if [[ $1 == --help ]]; then printf 'Usage: kwin-mcp\n'; else printf '{}\n'; fi
EOF
if PATH="$scratch/rejected:$PATH" bash "$repo_dir/scripts/install.sh" --source-dir "$scratch/silent-source" --bin-dir "$scratch/rejected" > "$evidence/invalid-schemas.stdout" 2> "$evidence/invalid-schemas.stderr"; then
    printf 'installer unexpectedly accepted invalid schemas\n' >&2
    exit 1
fi
rg -q 'did not print valid MCP tool schemas' "$evidence/invalid-schemas.stderr"
[[ ! -e "$scratch/rejected/kwin-mcp" && ! -e "$scratch/rejected/kwin-viewer" ]]

# A completed copy must still report a different executable earlier on PATH.
if PATH="$scratch/other-package:$scratch/shadowed:$PATH" bash "$repo_dir/scripts/install.sh" --source-dir "$source_dir" --bin-dir "$scratch/shadowed" > "$evidence/shadowed.stdout" 2> "$evidence/shadowed.stderr"; then
    printf 'installer unexpectedly ignored PATH shadowing\n' >&2
    exit 1
fi
rg -q 'installed successfully, but PATH selects' "$evidence/shadowed.stderr"
rg -q 'first in PATH' "$evidence/shadowed.stderr"
cmp "$source_dir/kwin-mcp" "$scratch/shadowed/kwin-mcp"
printf 'conflict=PASS\nsymlink_targets_preserved=PASS\ninstalled_help=PASS\ninstalled_describe=PASS\nsilent_source_rejected=PASS\ninvalid_schemas_rejected=PASS\nshadowing_reported=PASS\n' | tee "$evidence/results.txt"
