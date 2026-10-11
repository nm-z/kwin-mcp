#!/bin/bash
# Exercise the compiled CLI with actual artifacts and private remote release directories.
set -euo pipefail
cli=${KWIN_MCP_DEPLOY_BIN:?set the compiled deploy executable}
artifacts=${KWIN_MCP_DEPLOY_ARTIFACTS:?set the actual release artifact directory}
proof=${KWIN_MCP_DEPLOY_PROOF:?set an existing private evidence directory on engi}
[[ $(hostname -s) == engi ]]
proof=$(realpath "$proof")
run=$(mktemp -d "$proof/deploy-e2e-XXXXXX")
printf 'evidence=%s\n' "$run"
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
(cd "$run/package" && sha256sum -c SHA256SUMS) > "$run/checksums.log"
expect_failure existing-package "$cli" package "$artifacts" "$run/package"
expect_failure missing-artifacts "$cli" package "$run/nonexistent" "$run/missing-package"
expect_failure prohibited-builder "$cli" install "$run/package" sentry "$run/prohibited"
expect_failure relative-destination "$cli" install "$run/package" engi relative
expect_failure root-destination "$cli" install "$run/package" engi /
expect_failure invalid-command "$cli" nonsense
# A preexisting destination models a selected release; it must remain byte identical.
mkdir "$run/old-release"
printf 'old release sentinel\n' > "$run/old-release/kwin-mcp"
sha256sum "$run/old-release/kwin-mcp" > "$run/old-before.txt"
expect_failure old-release "$cli" install "$run/package" engi "$run/old-release"
sha256sum "$run/old-release/kwin-mcp" > "$run/old-after.txt"
cmp "$run/old-before.txt" "$run/old-after.txt"
"$cli" install "$run/package" engi "$run/installed" > "$run/install.log" 2>&1
(cd "$run/installed" && sha256sum -c SHA256SUMS) > "$run/installed-checksums.log"
# Installed tool runs independently from its original artifacts.
"$run/installed/bin/kwin-mcp-deploy" --help > "$run/installed-help.log"
cmp "$run/help.log" "$run/installed-help.log"
printf 'tamper\n' >> "$run/package/bin/kwin-mcp"
expect_failure corrupted-package "$cli" install "$run/package" engi "$run/corrupted-install"
[[ ! -e "$run/corrupted-install" ]]
cmp "$run/old-before.txt" "$run/old-after.txt"
[[ -z $(find "$run" -maxdepth 1 -name '*.incoming-*' -print -quit) ]]
# Remove a later input after remote staging exists to exercise a real transfer failure.
cp "$artifacts/kwin-mcp" "$run/package/bin/kwin-mcp"
(
  for attempt in {1..2000}; do
    if compgen -G "$run/failed-transfer.incoming-*" > /dev/null; then
      rm "$run/package/DEPLOYMENT.md"
      exit 0
    fi
    sleep 0.01
  done
  exit 1
) > "$run/transfer-interruption.log" 2>&1 &
interrupter=$!
expect_failure transfer-failure "$cli" install "$run/package" engi "$run/failed-transfer"
wait "$interrupter"
[[ ! -e "$run/failed-transfer" ]]
[[ -z $(find "$run" -maxdepth 1 -name '*.incoming-*' -print -quit) ]]
sha256sum "$run/old-release/kwin-mcp" > "$run/old-after-failure.txt"
cmp "$run/old-before.txt" "$run/old-after-failure.txt"
printf 'package=PASS\nchecksums=PASS\nreject_existing=PASS\ninvalid_arguments=PASS\ninstall=PASS\ncorruption=PASS\ntransfer_failure_cleanup=PASS\nold_release_preserved=PASS\n' | tee "$run/results.txt"
