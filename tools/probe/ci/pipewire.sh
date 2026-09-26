#!/usr/bin/env bash
# Runs `pulseaudio.sh` against PipeWire's PulseAudio server
# (pipewire-pulse), what most current distributions run: PipeWire,
# WirePlumber (which links streams to nodes) and pipewire-pulse, started
# here with no user session. With no sound card, PipeWire's dummy driver
# runs the graph.
#
# Needs `pipewire`, `pipewire-pulse`, `wireplumber`, `dbus` (for
# `dbus-run-session`) and `pactl`/`pacat` (Ubuntu: pulseaudio-utils), a
# non-root user, and no sound server running. Server logs go to
# tools/probe/results/. Run from the repository root:
#   tools/probe/ci/pipewire.sh
set -euo pipefail

# WirePlumber wants a session bus.
if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    exec dbus-run-session -- "$0" "$@"
fi
if [ ! -w "${XDG_RUNTIME_DIR:-}" ]; then
    XDG_RUNTIME_DIR=$(mktemp -d)
    export XDG_RUNTIME_DIR
fi

logs=tools/probe/results
mkdir -p "$logs"
pids=()
for server in pipewire wireplumber pipewire-pulse; do
    "$server" >"$logs/$server.log" 2>&1 &
    pids+=($!)
done
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT

# Ready once pipewire-pulse answers and WirePlumber has linked the default
# nodes; the pulseaudio.sh check would otherwise start plain PulseAudio.
for _ in $(seq 100); do
    pactl info 2>/dev/null | grep -q "PipeWire" && break
    sleep 0.1
done
if ! pactl info 2>/dev/null | grep -q "PipeWire"; then
    echo "pipewire-pulse did not come up; see $logs/*.log" >&2
    exit 1
fi

"$(dirname "$0")/pulseaudio.sh"
