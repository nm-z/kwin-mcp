#!/bin/bash
# Exercise the compiled CLI with release artifacts and private local release directories on engi.
set -euo pipefail
cli=${KWIN_MCP_DEPLOY_BIN:?set the compiled deploy executable}
artifacts=${KWIN_MCP_DEPLOY_ARTIFACTS:?set the actual release artifact directory}
proof=${KWIN_MCP_DEPLOY_PROOF:?set an existing private evidence directory on engi}
[[ $(hostname -s) == engi ]]
proof=$(realpath "$proof")
run=$(mktemp -d "$proof/deploy-e2e-XXXXXX")
printf 'evidence=%s\n' "$run"
transport_log="$run/transport.log"
no_remote="$run/no-remote"
mkdir "$no_remote"
for transport in ssh scp; do
  cat > "$no_remote/$transport" <<'SH'
#!/bin/bash
printf '%s %s\n' "${0##*/}" "$*" >> "$KWIN_MCP_DEPLOY_TRANSPORT_LOG"
exit 70
SH
  chmod +x "$no_remote/$transport"
done
export KWIN_MCP_DEPLOY_TRANSPORT_LOG="$transport_log"
export PATH="$no_remote:$PATH"
expect_failure() {
  local label=$1
  shift
  if "$@" > "$run/$label.log" 2>&1; then
    printf 'Unexpected success: %s\n' "$label" >&2
    exit 1
  fi
}
"$cli" --help > "$run/help.log"
"$cli" package "$artifacts" "$run/package" > "$run/package.log" 2>&1
"$cli" inspect "$run/package" > "$run/inspect.log"
(cd "$run/package" && sha256sum -c SHA256SUMS) > "$run/checksums.log"
expect_failure existing-package "$cli" package "$artifacts" "$run/package"
expect_failure missing-artifacts "$cli" package "$run/nonexistent" "$run/missing-package"
expect_failure prohibited-builder "$cli" install "$run/package" sentry "$run/prohibited"
expect_failure relative-destination "$cli" install "$run/package" engi relative
expect_failure root-destination "$cli" install "$run/package" engi /
expect_failure invalid-command "$cli" nonsense
source_root=$(cd -- "$(dirname -- "$0")/.." && pwd)
expect_failure invalid-jobs env CARGO_BUILD_JOBS=0 "$cli" build "$source_root" "$run/invalid-jobs"
expect_failure invalid-builder env KWIN_MCP_DEPLOY_BUILD_HOSTS=benji "$cli" build "$source_root" "$run/invalid-builder"
expect_failure invalid-build-root env KWIN_MCP_DEPLOY_BUILD_ROOT=relative "$cli" build "$source_root" "$run/invalid-root"
# Simulate an unreachable benji without relying on an SSH listener or fleet state.
if ! ssh -o BatchMode=yes -o ConnectTimeout=5 benji true > "$run/benji-probe.log" 2>&1; then
  "$cli" install "$run/package" benji "$run/benji-private" > "$run/benji-skip.log" 2>&1
  rg -q 'benji unreachable; skipped' "$run/benji-skip.log"
fi
: > "$transport_log"
KWIN_MCP_DEPLOY_BUILD_HOSTS='engi archy' \
KWIN_MCP_DEPLOY_BUILD_ROOT="$run/build-root" \
CARGO_BUILD_JOBS=1 \
  "$cli" build "$source_root" "$run/local-build-package" > "$run/local-build.log" 2>&1
rg -q 'builder=engi mode=local' "$run/local-build.log"
if rg -q 'engi' "$transport_log"; then
  printf 'Local engi build invoked SSH/SCP: %s\n' "$transport_log" >&2
  exit 1
fi
[[ -x "$run/local-build-package/bin/kwin-mcp" ]]
: > "$transport_log"
"$cli" install "$run/local-build-package" engi "$run/local-build-install" > "$run/local-build-install.log" 2>&1
(cd "$run/local-build-install" && sha256sum -c SHA256SUMS) > "$run/local-build-install-checksums.log"
rg -q 'mode=local' "$run/local-build-install.log"
[[ $(stat -c '%a' "$run/local-build-install") == 700 ]]
[[ ! -s "$transport_log" ]]
: > "$transport_log"
# A preexisting destination models a selected release; it must remain byte identical.
mkdir "$run/old-release"
printf 'old release sentinel\n' > "$run/old-release/kwin-mcp"
sha256sum "$run/old-release/kwin-mcp" > "$run/old-before.txt"
expect_failure old-release "$cli" install "$run/package" engi "$run/old-release"
sha256sum "$run/old-release/kwin-mcp" > "$run/old-after.txt"
cmp "$run/old-before.txt" "$run/old-after.txt"
"$cli" install "$run/package" engi "$run/installed" > "$run/install.log" 2>&1
(cd "$run/installed" && sha256sum -c SHA256SUMS) > "$run/installed-checksums.log"
rg -q 'mode=local' "$run/install.log"
[[ ! -s "$transport_log" ]]
# Installed tool runs independently from its original artifacts.
"$run/installed/bin/kwin-mcp-deploy" --help > "$run/installed-help.log"
cmp "$run/help.log" "$run/installed-help.log"
printf 'tamper\n' >> "$run/package/bin/kwin-mcp"
expect_failure corrupted-package "$cli" install "$run/package" engi "$run/corrupted-install"
[[ ! -e "$run/corrupted-install" ]]
cmp "$run/old-before.txt" "$run/old-after.txt"
[[ -z $(find "$run" -maxdepth 1 -name '*.incoming-*' -print -quit) ]]
# Occupy the final path after local staging begins; install must preserve it and clean its stage.
cp "$artifacts/kwin-mcp" "$run/package/bin/kwin-mcp"
(
  for attempt in {1..2000}; do
    if compgen -G "$run/raced-release.incoming-*" > /dev/null; then
      mkdir "$run/raced-release"
      printf 'concurrent destination sentinel\n' > "$run/raced-release/sentinel"
      exit 0
    fi
    sleep 0.01
  done
  exit 1
) > "$run/transfer-interruption.log" 2>&1 &
interrupter=$!
if "$cli" install "$run/package" engi "$run/raced-release" > "$run/destination-race.log" 2>&1; then
  wait "$interrupter"
  printf 'Unexpected success: destination-race\n' >&2
  exit 1
fi
wait "$interrupter"
[[ -f "$run/raced-release/sentinel" ]]
printf 'concurrent destination sentinel\n' > "$run/raced-release-expected.txt"
cmp "$run/raced-release-expected.txt" "$run/raced-release/sentinel"
[[ -z $(find "$run" -maxdepth 1 -name 'raced-release.incoming-*' -print -quit) ]]
sha256sum "$run/old-release/kwin-mcp" > "$run/old-after-failure.txt"
cmp "$run/old-before.txt" "$run/old-after-failure.txt"
printf 'local_build=PASS\nlocal_install=PASS\npackage=PASS\nchecksums=PASS\nreject_existing=PASS\ninvalid_arguments=PASS\ncorruption=PASS\ndestination_race_cleanup=PASS\nold_release_preserved=PASS\n' | tee "$run/results.txt"
