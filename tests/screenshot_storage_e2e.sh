#!/usr/bin/env bash
# Exercise a completely full workdir without filling the user's shared /tmp.
set -euo pipefail
server=$(realpath "${1:?server binary}")
runner=$(realpath "${2:?session_lifecycle_e2e binary}")
proof=$(realpath -m "${3:?evidence directory}")
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
mkdir -p "$repo/target" "$proof"
scratch=$(mktemp -d "$repo/target/screenshot-storage.XXXXXX")
# Session Unix socket paths must fit sockaddr_un.sun_path.
private_tmp=$(mktemp -d /tmp/k319.XXXXXX)
proxy_pid=
cleanup() {
    if [[ -n "$proxy_pid" ]]; then
        kill "$proxy_pid" || true
        wait "$proxy_pid" || true
    fi
    rm -rf -- "$scratch"
    rmdir -- "$private_tmp"
}
trap cleanup EXIT
mkdir "$scratch/homes"
# Keep systemd scope management available while denying wallet/Secret Service
# discovery and reads. The server's HOME below is also a private empty fixture.
runtime=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
bus=${DBUS_SESSION_BUS_ADDRESS:-unix:path=$runtime/bus}
xdg-dbus-proxy "$bus" "$scratch/bus" --filter \
    --talk=org.freedesktop.systemd1 --talk=org.a11y.Bus &
proxy_pid=$!
for (( attempt = 0; attempt < 50; attempt++ )); do
    [[ -S "$scratch/bus" ]] && break
    kill -0 "$proxy_pid"
    sleep 0.1
done
[[ -S "$scratch/bus" ]]
# The FUSE bridge currently copies the server image into its runtime directory.
# Allow that image plus 32 MiB of runtime data; keep the fill below 1 GiB.
capacity=$(( $(stat -c %s "$server") + 32 * 1024 * 1024 ))
if (( capacity > 1024 * 1024 * 1024 )); then
    printf 'Server is too large for the bounded tmpfs; strip debug info first.\n' >&2
    exit 1
fi
# A user namespace drops input-device access. Mount only in a private mount
# namespace, then restore the original user and groups before running the check.
# This requires passwordless sudo; no mount is added to the host namespace.
sudo -n unshare --mount --propagation private -- bash -c '
    set -euo pipefail
    mount -t tmpfs -o "size=$1,uid=$2,gid=$3,mode=0700" tmpfs "$4"
    exec setpriv --reuid "$2" --regid "$3" --init-groups env \
        TMPDIR="$4" KWIN_MCP_E2E_PRIVATE_TMPFS="$4" KWIN_MCP_E2E=1 \
        KWIN_MCP_E2E_SERVER="$6" KWIN_MCP_TEST_TMPDIR="$7" \
        KWIN_MCP_TEST_ARTIFACT_DIR="$8" XDG_RUNTIME_DIR="$9" \
        DBUS_SESSION_BUS_ADDRESS="${10}" \
        "$5" screenshot_at_full_workdir_keeps_desktop_inspection_available \
        --exact --ignored --nocapture --test-threads=1
' screenshot-storage "$capacity" "$(id -u)" "$(id -g)" "$private_tmp" \
    "$runner" "$server" "$scratch/homes" "$proof" "$runtime" "unix:path=$scratch/bus"
