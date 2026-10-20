#!/bin/bash
# PolicyKit is intentionally absent from the session's filtered system bus.
# Preserve its diagnostic in the session log instead of interleaving it with a
# terminal application's screen. Every other diagnostic and the agent's status
# retain their ordinary behavior.
filter_registration_diagnostic() {
    # This subprocess must not keep --notify-fd (or any caller-owned descriptor)
    # alive after the real agent exits. The real process keeps its own copies.
    local descriptor path line
    for path in /proc/self/fd/*; do
        descriptor=${path##*/}
        if [[ $descriptor =~ ^[0-9]+$ ]] && (( descriptor > 2 )); then
            { exec {descriptor}>&-; } 2>/dev/null
        fi
    done
    umask 077
    while IFS= read -r line || [[ -n $line ]]; do
        if [[ $line == 'Error registering authentication agent: GDBus.Error:org.freedesktop.DBus.Error.ServiceUnknown: org.freedesktop.DBus.Error.ServiceUnknown (g-dbus-error-quark, 2)' ]] &&
            [[ -n ${XDG_RUNTIME_DIR:-} ]] &&
            printf '%s\n' "$line" >> "$XDG_RUNTIME_DIR/polkit-agent.log"; then
            continue
        fi
        printf '%s\n' "$line" >&2
    done
}
exec -a pkttyagent /run/kwin-mcp-pkttyagent/real "$@" 2> >(filter_registration_diagnostic)
