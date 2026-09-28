#!/usr/bin/env bash
# Runs the hardware-free probes, `virtual-disconnect` included, against a
# PulseAudio server playing noise (with-noise-source.sh). Run from the
# repository root: tools/probe/ci/pulseaudio.sh
set -euo pipefail

out=$(mktemp)
trap 'rm -f "$out"' EXIT

"$(dirname "$0")/with-noise-source.sh" cargo run --release -p handy-recorder-probe -- auto | tee "$out"
# INFO rather than PASS means the PulseAudio host wasn't used.
if ! grep -Eq '^virtual-disconnect +PASS' "$out"; then
    echo "virtual-disconnect did not PASS: was the PulseAudio host used?" >&2
    exit 1
fi
