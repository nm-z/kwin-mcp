#!/usr/bin/env bash
# Run only compilation/CPU gates here. Desktop regressions use returned artifacts on engi.
set -euo pipefail
cookie_build_jobs=${CARGO_BUILD_JOBS:-6}
cookie_target_root=${CARGO_TARGET_DIR:-target}
cookie_artifacts=target/cookie-verification
mkdir -p "$cookie_artifacts"

git rev-parse HEAD > "$cookie_artifacts/commit.txt"
cargo build --release --locked --bins -j "$cookie_build_jobs"
cargo test --release --locked --lib --bins -j "$cookie_build_jobs"
cargo test --release --locked --test session_lifecycle_e2e --no-run -j "$cookie_build_jobs" --message-format=json > "$cookie_artifacts/test-artifacts.jsonl"
cookie_test_binary=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "session_lifecycle_e2e" and .profile.test == true and .executable != null) | .executable' "$cookie_artifacts/test-artifacts.jsonl")
[[ -n "$cookie_test_binary" && "$cookie_test_binary" != *$'\n'* ]]
cargo clippy --release --locked --lib --bin kwin-mcp --test session_lifecycle_e2e -j "$cookie_build_jobs"

for cookie_binary in kwin-mcp kwin-viewer kwin-mcp-strict; do
  cp "$cookie_target_root/release/$cookie_binary" "$cookie_artifacts/$cookie_binary"
done
cp "$cookie_test_binary" "$cookie_artifacts/cookie-e2e-tests"
(
  cd "$cookie_artifacts"
  sha256sum kwin-mcp kwin-viewer kwin-mcp-strict cookie-e2e-tests > SHA256SUMS
)
