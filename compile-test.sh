#!/bin/bash
set -euo pipefail
source_file=${1:?test source path}
output_file=${2:?output path}
compiler=${3:-rustc}
extra=()
case "$compiler" in
  rustc) ;;
  clippy-driver) extra=(--emit=metadata -Dwarnings) ;;
  *) exit 2 ;;
esac
mkdir -p "$(dirname "$output_file")" /home/nate/builds/kwin-mcp-background/proof-home
/usr/bin/time -f 'elapsed_seconds=%e' env \
  'CARGO_TARGET_TMPDIR=/home/nate/builds/kwin-mcp-background/proof-home' \
  'CARGO_BIN_EXE_kwin-mcp=/home/nate/kwin-mcp/target/release/kwin-mcp' \
  'CARGO_BIN_EXE_kwin-mcp-shim=/home/nate/kwin-mcp/target/release/kwin-mcp-shim' \
  nice -n 19 ionice -c3 "$compiler" --edition\=2024 --crate-name shim_disconnect_e2e --test -C opt-level\=3 -C embed-bitcode\=no -L dependency\=/home/nate/builds/kwin-mcp-lifecycle/target/release/deps --deny\=dead_code --deny\=clippy::wildcard_enum_match_arm --deny\=clippy::unwrap_used --deny\=clippy::expect_used --deny\=clippy::todo --deny\=clippy::unimplemented --deny\=clippy::unreachable --deny\=clippy::as_conversions --deny\=clippy::wildcard_imports --extern nix\=/home/nate/builds/kwin-mcp-lifecycle/target/release/deps/libnix-4a80a9629c3fe004.rlib --extern serde_json\=/home/nate/builds/kwin-mcp-lifecycle/target/release/deps/libserde_json-094e7e8acca71ad1.rlib --extern procfs\=/home/nate/builds/kwin-mcp-lifecycle/target/release/deps/libprocfs-93f255fbf80e72d6.rlib "${extra[@]}" "$source_file" -o "$output_file"
