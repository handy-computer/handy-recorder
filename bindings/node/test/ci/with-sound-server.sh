#!/usr/bin/env bash
# Runs a command against a real Linux sound server whose default source is
# the monitor of a null sink playing quiet noise: real, non-zero audio with
# no sound card (the same setup as tools/probe/ci/).
#
#   with-sound-server.sh pulseaudio node smoke.mjs
#   with-sound-server.sh pipewire bun smoke.mjs
#
# Needs pactl/pacat (Ubuntu: pulseaudio-utils) and the server: `pulseaudio`,
# or `pipewire pipewire-pulse wireplumber dbus`. A non-root user.
set -euo pipefail

server=$1
shift

if [ "$server" = pipewire ] && [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
    exec dbus-run-session -- "$0" "$server" "$@"
fi
if [ ! -w "${XDG_RUNTIME_DIR:-}" ]; then
    XDG_RUNTIME_DIR=$(mktemp -d)
    export XDG_RUNTIME_DIR
fi

pids=()
trap 'kill "${pids[@]}" 2>/dev/null || true' EXIT
case $server in
    pulseaudio)
        pulseaudio --daemonize=yes --exit-idle-time=-1
        ;;
    pipewire)
        for daemon in pipewire wireplumber pipewire-pulse; do
            "$daemon" >/dev/null 2>&1 &
            pids+=($!)
        done
        ;;
    *)
        echo "unknown server $server" >&2
        exit 2
        ;;
esac
for _ in $(seq 100); do
    pactl info >/dev/null 2>&1 && break
    sleep 0.1
done
pactl info | grep -E "Server Name|Server Version"

# pipewire-pulse answers `pactl info` before WirePlumber, which manages
# devices and defaults, is up; until then setup fails "Not supported".
# Retry each step until it takes effect.
retry() {
    for _ in $(seq 100); do
        "$@" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    echo "gave up: $*" >&2
    return 1
}
retry pactl load-module module-null-sink sink_name=smoke_noise
retry sh -c 'pactl list short sources | grep -q smoke_noise.monitor'
pacat --playback --device=smoke_noise --volume=32768 --latency-msec=50 --raw --format=s16le --rate=48000 --channels=2 </dev/urandom &
pids+=($!)
retry sh -c 'pactl set-default-source smoke_noise.monitor && [ "$(pactl get-default-source)" = smoke_noise.monitor ]'
echo "default source: $(pactl get-default-source)"

"$@"
