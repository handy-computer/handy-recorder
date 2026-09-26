# handy-recorder-probe

Runs handy-recorder against real hardware.

```sh
cargo run -p handy-recorder-probe -- auto     # probes that need no operator
cargo run -p handy-recorder-probe -- all      # everything, asks before interactive ones
cargo run -p handy-recorder-probe -- <probe> [--device <id>] [--secs <n>] [--take-headset]
```

Results go to `tools/probe/results/`.

| Probe | Tests |
| --- | --- |
| `list` | devices, IDs, default, permission status |
| `baseline` | a recording is real, complete, the right length |
| `warm-cycles` | many short recordings on one open recorder |
| `two-clients` | two recorders on one device in one process |
| `second-process` | sharing the mic with another process |
| `virtual-disconnect` | Linux: a PulseAudio source removed while recording and idle |
| `disconnect` | device unplugged or turned off while recording and idle; reconnect |
| `bluetooth-handoff` | headset moved to a phone while recording and idle; bring back |
| `meeting-app` | recorders across a real call (Meet, Zoom); new ones during and after |
| `sleep` | sleep and wake while idle and while recording |
| `audio-service-restart` | OS audio service restarted mid-recording; reopen after |
| `default-change` | system default input changed mid-recording |
| `format-change` | macOS/Windows: device sample rate changed mid-recording |
| `permission` | microphone access denied |
| `soak` | 30 min open, a recording every 30 s: drift, memory, false failures |

Linux without hardware: `tools/probe/ci/pulseaudio.sh`, `tools/probe/ci/pipewire.sh`.
