#!/bin/bash
# Container entrypoint: start services sequentially with readiness checks.
# Expects env: XDG_INNER (host-shared dir), RUNTIME_DIR (wayland runtime).
set -u
ulimit -c 0
mkdir -p "$RUNTIME_DIR" /tmp/cache
chmod 700 "$RUNTIME_DIR"
printf '#!/bin/sh\nexit 0\n' > /tmp/kdialog && chmod +x /tmp/kdialog

dbus-daemon --session --address="unix:path=${XDG_INNER}/bus" \
    --print-address=3 --print-pid=4 --nofork \
    3>"${XDG_INNER}/dbus.address" 4>"${XDG_INNER}/dbus.pid" 2>"${XDG_INNER}/dbus.log" &
dbus_pid=$!
n=0; while [ ! -s "${XDG_INNER}/dbus.address" ] && kill -0 "$dbus_pid" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done
if [ ! -s "${XDG_INNER}/dbus.address" ]; then echo 'dbus-daemon did not announce an address' >> "${XDG_INNER}/bootstrap.log"; wait "$dbus_pid" || true; exit 1; fi

KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 kwin_wayland --virtual --width 1920 --height 1080 2>"${XDG_INNER}/kwin.log" &
kwin_pid=$!
n=0; while [ ! -S "${RUNTIME_DIR}/wayland-0" ] && kill -0 "$dbus_pid" 2>/dev/null && kill -0 "$kwin_pid" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done
if ! kill -0 "$kwin_pid" 2>/dev/null; then echo 'kwin_wayland exited before creating wayland-0' >> "${XDG_INNER}/bootstrap.log"; wait "$kwin_pid" || true; exit 1; fi
if [ ! -S "${RUNTIME_DIR}/wayland-0" ]; then echo 'kwin_wayland did not create wayland-0' >> "${XDG_INNER}/bootstrap.log"; exit 1; fi

if ! dbus-update-activation-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR XDG_CURRENT_DESKTOP XDG_SESSION_TYPE PATH HOME USER QT_QPA_PLATFORM=wayland 2>>"${XDG_INNER}/bootstrap.log"; then
    echo 'dbus-update-activation-environment failed' >> "${XDG_INNER}/bootstrap.log"; exit 1
fi

pipewire 2>"${XDG_INNER}/pipewire.log" &
at-spi-bus-launcher 2>"${XDG_INNER}/atspi.log" &
wireplumber 2>"${XDG_INNER}/wireplumber.log" &

while read -r cmd; do eval "$cmd" & done
