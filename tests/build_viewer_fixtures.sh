#!/usr/bin/env bash
set -euo pipefail
# Run on the selected build host; copy outputs alongside the private server.
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
out=${1:?usage: build_viewer_fixtures.sh OUTPUT_DIRECTORY}
mkdir -p "$out"
protocol=${KWIN_MCP_FAKE_INPUT_XML:-/usr/share/plasma-wayland-protocols/fake-input.xml}
cc -std=c11 -Wall -Wextra -Werror "$repo/tests/fixtures/viewer_input_probe.c" -o "$out/viewer-input-probe" $(pkg-config --cflags --libs gtk4)
wayland-scanner client-header "$protocol" "$out/fake-input-client.h"
wayland-scanner private-code "$protocol" "$out/fake-input-code.c"
cc -std=c11 -D_DEFAULT_SOURCE -Wall -Wextra -Werror -I "$out" "$repo/tests/fixtures/viewer_touch.c" "$out/fake-input-code.c" -o "$out/viewer-touch" $(pkg-config --cflags --libs wayland-client)
