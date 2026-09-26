#!/usr/bin/env bash
# Runs the probes that need no hardware against a PulseAudio server: CI's
# check of the PulseAudio host, including `virtual-disconnect`. Starts
# plain PulseAudio unless a server already answers (`pipewire.sh` starts
# pipewire-pulse and then runs this).
#
# The input is the monitor of a null sink playing quiet noise: real,
# non-zero audio, from a source that honors latency requests like a sound
# card's. (module-sine-source does not: it delivers 350 ms blocks whatever
# is asked for.)
#
# Needs `pulseaudio` and `pactl`/`pacat` (Ubuntu: pulseaudio
# pulseaudio-utils) and a non-root user. Run from the repository root:
#   tools/probe/ci/pulseaudio.sh
set -euo pipefail

# CPAL's PulseAudio client finds the server only through the environment
# (XDG_RUNTIME_DIR, PULSE_RUNTIME_PATH, PULSE_SERVER); libpulse, and so
# pactl, also look elsewhere. Without it the library falls back to ALSA,
# through the server's ALSA plugin, and every probe but
# `virtual-disconnect` still passes.
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

pactl unload-module module-null-sink 2>/dev/null || true
pactl load-module module-null-sink sink_name=probe_noise >/dev/null
pacat --playback --device=probe_noise --volume=32768 --latency-msec=50 --raw --format=s16le --rate=48000 --channels=2 </dev/urandom &
noise=$!
out=$(mktemp)
trap 'kill $noise 2>/dev/null || true; rm -f "$out"' EXIT
pactl set-default-source probe_noise.monitor

cargo run --release -p handy-recorder-probe -- auto | tee "$out"
# `virtual-disconnect` reports INFO, not FAIL, when the library is not on
# the PulseAudio host; here that means the check above did not hold.
if ! grep -Eq '^virtual-disconnect +PASS' "$out"; then
    echo "virtual-disconnect did not PASS: was the PulseAudio host used?" >&2
    exit 1
fi
