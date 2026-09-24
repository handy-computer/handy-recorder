# macOS audio diagnostics

Small Swift tools for questions the Rust probe cannot answer through the
library. Build with `swiftc -O <file>.swift -o <name>` (Xcode command line
tools).

- `mic_users.swift`: which processes are using audio input right now
  (CoreAudio's per-process `IsRunningInput`, what drives the orange mic
  indicator), and each input device's alive/running state.
- `handoff_monitor.swift [bluetooth-address]`: polls every 100 ms and logs
  every change to input devices (alive, running, rate, streams, mute), the
  default input, input users, and a Bluetooth device's connection state.
  Run it in the background during a probe, then line its log up with the
  probe's results file.

- `handoff_listener.swift [--device <UID or name>] [--bt <address>] [--no-tap]
  [--voice-processing] [--play silence|tone] [--play-after <s>] [--play-for <s>]
  [--output <UID or name>]`:
  records from one input device (default: the default input) through an
  AUHAL input unit, as CPAL does, and logs with UTC timestamps in the
  probe's format:
  - every CoreAudio property notification, from wildcard listeners on the
    system object, every device (output-only too), everything each device
    owns (streams, volume, mute, data-source controls), boxes, clocks,
    plug-ins, and process objects, re-registered as objects appear;
  - the input unit's own property changes (running, format, render errors);
  - when the recorded audio turns to exact zeros and back, and callback
    gaps over 100 ms, with a heartbeat every 5 s;
  - the Bluetooth connection state (address taken from an AirPods UID, or
    `--bt`).

  It records because some notifications reach only a process doing I/O on
  the device (the per-process client object's `pdv#` and `goin`, for
  example). Ctrl-C stops it. Checked on 2026-09-24:
  - Recording the built-in microphone, an input volume change on the
    AirPods (the default input) was logged as `volm`, `vold`, and `mute`
    notifications, so the listeners also catch other devices.
  - Recording the AirPods (24 kHz, call mode), opening logs about 80
    notifications on the AirPods' input and output devices (among them
    `avcp` = `tsco` and `nsrt` = 24000 on the output device). The first
    320 ms of audio were exact zeros, then real audio.

Result so far (2026-09-24): during an AirPods handoff to a phone, none of
the properties `handoff_monitor` polls changed (see TODO.md, "Digital
silence"). Next: run `handoff_listener` during a handoff:

```sh
swiftc -O handoff_listener.swift -o /tmp/handoff_listener
/tmp/handoff_listener --device AirPods | tee ../results/handoff-listener.log
# move the AirPods to the phone, wait ~20 s, move them back, Ctrl-C
```

Look for notifications near the `tap: audio is EXACT ZEROS` line, and
near the line where real audio returns. Opening the device logs a burst of
its own; ignore the first second.

### Pulling the headset back from a phone

Opening the AirPods' input while they play music on a phone records exact
zeros; a call, or playback, on the Mac moves them back. These runs find out
whether something a library can do moves them. Start each one with the
AirPods playing music on the phone, and note whether and when the phone's
music stops and the log shows `tap: audio is real`:

```sh
swiftc -O handoff_listener.swift -o /tmp/handoff_listener
R=../results
# 1. Baseline: input only (as the library does today). Watch `avcp` at open.
/tmp/handoff_listener --device AirPods | tee $R/pull-1-baseline.log
# 2. The voice-processing unit (what calling apps use).
/tmp/handoff_listener --device AirPods --voice-processing | tee $R/pull-2-voice.log
# 3a. Silent output on the AirPods, 5 s in, kept running.
/tmp/handoff_listener --device AirPods --play silence | tee $R/pull-3a-silence.log
# 3b. The same, stopped after 2 s: do the AirPods stay?
/tmp/handoff_listener --device AirPods --play silence --play-for 2 | tee $R/pull-3b-silence-brief.log
# 3c. Only if silence does nothing: a quiet 440 Hz tone.
/tmp/handoff_listener --device AirPods --play tone --play-for 2 | tee $R/pull-3c-tone.log
```

Ctrl-C each after about 20 s. The output device is the one sharing the
AirPods' Bluetooth address; `--output` picks another.
