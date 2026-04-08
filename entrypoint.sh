#!/bin/bash
# Container entrypoint: start services sequentially with readiness checks.
# Expects env: XDG_INNER (host-shared dir).
set -u
ulimit -c 0
mkdir -p "$XDG_INNER" /tmp/cache /tmp/config /tmp/.X11-unix
chmod 1777 /tmp/.X11-unix
chmod 700 "$XDG_INNER"
printf '#!/bin/sh\nexit 0\n' > /tmp/kdialog && chmod +x /tmp/kdialog

# Session environment — XDG_RUNTIME_DIR points to the shared dir so all
# sockets (wayland, at-spi, dbus) are visible from the host.
export PATH="/tmp:/usr/bin:/usr/sbin:/bin:/sbin:/usr/lib:/usr/libexec:/usr/lib/at-spi2-core"
export HOME="${HOME:-/home/${USER:-user}}"
export USER="${USER:-user}"
export XDG_RUNTIME_DIR="$XDG_INNER"
export XDG_CONFIG_HOME=/tmp/config
export XDG_CACHE_HOME=/tmp/cache
export XDG_DATA_HOME=/tmp/state
export XDG_SESSION_TYPE=wayland
export XDG_CURRENT_DESKTOP=KDE
export WAYLAND_DISPLAY=wayland-0
export KDE_DEBUG=0
export QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1

# KWin config: no decorations, no shadows for clean screenshots
cat > /tmp/config/kwinrc <<'EOF'
[org.kde.kdecoration2]
BorderSize=None
ShadowSize=0

[Compositing]
LockScreenAutoLockEnabled=false
EOF
cat > /tmp/config/kscreenlockerrc <<'EOF'
[Daemon]
Autolock=false
LockOnResume=false
Timeout=0
EOF
cat > /tmp/config/kwinrulesrc <<'EOF'
[1]
Description=No decorations
noborder=true
noborderrule=2
wmclassmatch=0

[General]
count=1
rules=1
EOF

# D-Bus
dbus-daemon --session --address="unix:path=${XDG_INNER}/bus" \
    --print-address=3 --print-pid=4 --nofork \
    3>"${XDG_INNER}/dbus.address" 4>"${XDG_INNER}/dbus.pid" 2>"${XDG_INNER}/dbus.log" &
dbus_pid=$!
n=0; while [ ! -s "${XDG_INNER}/dbus.address" ] && kill -0 "$dbus_pid" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done
if [ ! -s "${XDG_INNER}/dbus.address" ]; then echo 'dbus-daemon did not announce an address' >> "${XDG_INNER}/bootstrap.log"; wait "$dbus_pid" || true; exit 1; fi
export DBUS_SESSION_BUS_ADDRESS="$(cat "${XDG_INNER}/dbus.address")"

# KWin compositor
KWIN_SCREENSHOT_NO_PERMISSION_CHECKS=1 kwin_wayland --virtual --xwayland --width 1920 --height 1080 2>"${XDG_INNER}/kwin.log" &
kwin_pid=$!
echo "$dbus_pid $kwin_pid" > "${XDG_INNER}/pids"
n=0; while [ ! -S "${XDG_INNER}/wayland-0" ] && kill -0 "$dbus_pid" 2>/dev/null && kill -0 "$kwin_pid" 2>/dev/null && [ $n -lt 300 ]; do sleep 0.05; n=$((n+1)); done
if ! kill -0 "$kwin_pid" 2>/dev/null; then echo 'kwin_wayland exited before creating wayland-0' >> "${XDG_INNER}/bootstrap.log"; wait "$kwin_pid" || true; exit 1; fi
if [ ! -S "${XDG_INNER}/wayland-0" ]; then echo 'kwin_wayland did not create wayland-0' >> "${XDG_INNER}/bootstrap.log"; exit 1; fi

if ! dbus-update-activation-environment WAYLAND_DISPLAY XDG_RUNTIME_DIR XDG_CURRENT_DESKTOP XDG_SESSION_TYPE PATH HOME USER 2>>"${XDG_INNER}/bootstrap.log"; then
    echo 'dbus-update-activation-environment failed' >> "${XDG_INNER}/bootstrap.log"; exit 1
fi

# Supporting services
pipewire 2>"${XDG_INNER}/pipewire.log" &
pw_pid=$!
ATSPI_DBUS_IMPLEMENTATION=dbus-daemon at-spi-bus-launcher 2>"${XDG_INNER}/atspi.log" &
atspi_pid=$!
wireplumber 2>"${XDG_INNER}/wireplumber.log" &
echo "$dbus_pid $kwin_pid $pw_pid $atspi_pid $!" >> "${XDG_INNER}/pids"

# Ready — wait for app-launch commands on stdin (one per line)
while read -r cmd; do
    $cmd &
done
