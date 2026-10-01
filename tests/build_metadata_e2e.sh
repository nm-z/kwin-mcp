#!/bin/bash
set -euo pipefail
case "$(hostname -s)" in archy|sentry) ;; *) printf 'Run build verification on archy or sentry.\n' >&2; exit 2 ;; esac
source_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
proof_root=${KWIN_MCP_PROOF_DIR:?set a disk-backed proof directory}
mkdir -p "$proof_root"
proof_root=$(mktemp -d "$(realpath "$proof_root")/run-XXXXXX")
printf 'proof_run=%s\n' "$proof_root"
while IFS= read -r variable; do
  if [[ $variable == GIT_* ]]; then unset "$variable"; fi
done < <(compgen -e)
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
fixture=$(mktemp -d "$proof_root/metadata-XXXXXX")
cp "$source_root/build.rs" "$source_root/cursor_v6_fixed.svg" "$fixture/"
mkdir "$fixture/src" "$fixture/tests"
printf '%s\n' '[package]' 'name = "metadata-proof"' 'version = "0.0.0"' 'edition = "2024"' '[workspace]' > "$fixture/Cargo.toml"
printf '%s\n' 'fn main() { println!("{} {}", env!("GIT_HASH"), env!("BUILD_NUMBER")); }' > "$fixture/src/main.rs"
git -C "$fixture" init -q -b metadata-proof
git -C "$fixture" config user.name 'KWin build test'
git -C "$fixture" config user.email 'build-test@localhost'
git -C "$fixture" config core.hooksPath /dev/null
git -C "$fixture" add build.rs cursor_v6_fixed.svg Cargo.toml src/main.rs
git -C "$fixture" commit -qm 'Initial build metadata fixture'
git -C "$fixture" pack-refs --all --no-prune
export CARGO_BUILD_JOBS=1
unset CARGO_TARGET_DIR KWIN_MCP_BUILD_NUMBER
build() {
  local directory=$1 label=$2
  shift 2
  (cd "$directory" && nice -n19 ionice -c3 cargo "$@" --target-dir "$directory/target") > "$proof_root/$label.log" 2>&1
}
stamp() { "$1/target/release/metadata-proof"; }
assert_commit() {
  local directory=$1
  [[ $(stamp "$directory" | cut -d' ' -f1) == "$(git -C "$directory" rev-parse --short HEAD)" ]]
}
snapshot() {
  sha256sum "$1/target/release/metadata-proof"
  stat -c '%n %s %y' "$1/target/release/metadata-proof"
  cat "$1/.build_number"
}
build "$fixture" initial build --release
assert_commit "$fixture"
snapshot "$fixture" > "$proof_root/before.txt"
build "$fixture" unchanged build --release
snapshot "$fixture" > "$proof_root/unchanged.txt"
cmp "$proof_root/before.txt" "$proof_root/unchanged.txt"
git -C "$fixture" update-ref refs/tags/unrelated HEAD
git -C "$fixture" pack-refs --all --no-prune
build "$fixture" unrelated-packed-ref build --release
snapshot "$fixture" > "$proof_root/unrelated-packed-ref.txt"
cmp "$proof_root/before.txt" "$proof_root/unrelated-packed-ref.txt"
printf '%s\n' '#[test] fn test_only_change() { assert_eq!(2 + 2, 4); }' > "$fixture/tests/cache.rs"
build "$fixture" test-only test --release --no-run
snapshot "$fixture" > "$proof_root/test-only.txt"
cmp "$proof_root/before.txt" "$proof_root/test-only.txt"
old=$(stamp "$fixture")
printf '\n' >> "$fixture/src/main.rs"
build "$fixture" source-change build --release
[[ $(stamp "$fixture") != "$old" ]]
assert_commit "$fixture"
old=$(stamp "$fixture")
git -C "$fixture" commit --allow-empty -qm 'New release identity'
build "$fixture" ref-change build --release
assert_commit "$fixture"
[[ $(stamp "$fixture") != "$old" ]]
git -C "$fixture" pack-refs --all --prune
build "$fixture" packed-ref build --release
assert_commit "$fixture"
snapshot "$fixture" > "$proof_root/packed-before.txt"
build "$fixture" packed-unchanged build --release
snapshot "$fixture" > "$proof_root/packed-after.txt"
cmp "$proof_root/packed-before.txt" "$proof_root/packed-after.txt"
git -C "$fixture" commit --allow-empty -qm 'Update packed branch'
build "$fixture" packed-update build --release
assert_commit "$fixture"
worktree="$fixture-worktree"
git -C "$fixture" worktree add -q -b linked-proof "$worktree" HEAD
build "$worktree" worktree build --release
assert_commit "$worktree"
git -C "$worktree" commit --allow-empty -qm 'Linked branch release identity'
build "$worktree" linked-branch-update build --release
assert_commit "$worktree"
git -C "$worktree" checkout -q --detach
build "$worktree" detach build --release
assert_commit "$worktree"
snapshot "$worktree" > "$proof_root/detached-before.txt"
git -C "$fixture" update-ref refs/tags/detached-unrelated HEAD
git -C "$fixture" pack-refs --all --no-prune
build "$worktree" detached-unrelated-ref build --release
snapshot "$worktree" > "$proof_root/detached-after.txt"
cmp "$proof_root/detached-before.txt" "$proof_root/detached-after.txt"
git -C "$worktree" commit --allow-empty -qm 'Detached release identity'
build "$worktree" detached-update build --release
assert_commit "$worktree"
export KWIN_MCP_BUILD_NUMBER=197001
build "$worktree" explicit-number build --release
[[ $(stamp "$worktree" | cut -d' ' -f2) == 197001 ]]
snapshot "$worktree" > "$proof_root/explicit-before.txt"
build "$worktree" explicit-unchanged build --release
snapshot "$worktree" > "$proof_root/explicit-after.txt"
cmp "$proof_root/explicit-before.txt" "$proof_root/explicit-after.txt"
export KWIN_MCP_BUILD_NUMBER=invalid
if build "$worktree" invalid-number build --release; then
  printf 'Invalid release number was accepted.\n' >&2
  exit 1
fi
printf 'unchanged=PASS\nunrelated_packed_ref=PASS\ntest_only=PASS\nsource_change=PASS\nbranch_commit=PASS\npacked_ref=PASS\npacked_unchanged=PASS\npacked_update=PASS\nworktree=PASS\nlinked_branch_update=PASS\ndetach=PASS\ndetached_unrelated_ref=PASS\ndetached_update=PASS\nexplicit_number=PASS\nexplicit_unchanged=PASS\ninvalid_number=PASS\nfixture=%s\nworktree=%s\n' "$fixture" "$worktree" > "$proof_root/results.txt"
cat "$proof_root/results.txt"
