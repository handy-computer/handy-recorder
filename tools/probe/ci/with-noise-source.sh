#!/usr/bin/env bash
# Runs a command with the default input set to the monitor of a null sink
# playing noise, starting PulseAudio unless a server already runs.
#
#   tools/probe/ci/with-noise-source.sh <command...>
set -euo pipefail

# CPAL finds the server only through XDG_RUNTIME_DIR; otherwise it falls
# back to ALSA.
if [ ! -w "${XDG_RUNTIME_DIR:-}" ]; then
    XDG_RUNTIME_DIR=$(mktemp -d)
    export XDG_RUNTIME_DIR
fi

if ! pactl info >/dev/null 2>&1; then
    pulseaudio --daemonize=yes --exit-idle-time=-1
    for _ in $(seq 50); do
        pactl info >/dev/null 2>&1 && break
        sleep 0.1
    done
fi
pactl info | grep -E "Server Name|Server Version"

# pipewire-pulse answers before WirePlumber is up; retry for up to 10 s.
retry() {
    for _ in $(seq 100); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "gave up: $*" >&2
    return 1
}

pactl unload-module module-null-sink 2>/dev/null || true
retry pactl load-module module-null-sink sink_name=ci_noise
retry sh -c 'pactl list short sources | grep -q ci_noise.monitor'
pacat --playback --device=ci_noise --volume=32768 --latency-msec=50 --raw --format=s16le --rate=48000 --channels=2 </dev/urandom &
noise=$!
trap 'kill $noise 2>/dev/null || true' EXIT
retry sh -c 'pactl set-default-source ci_noise.monitor && [ "$(pactl get-default-source)" = ci_noise.monitor ]'
echo "default input: $(pactl get-default-source)"

"$@"
