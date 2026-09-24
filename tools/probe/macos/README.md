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

Result so far (2026-09-24): during an AirPods handoff to a phone, none of
the polled properties changed (see TODO.md, "Digital silence"). Next: a
monitor that registers CoreAudio property listeners with
`kAudioObjectPropertySelectorWildcard` on the device and its streams, to
catch any notification macOS sends at the handoff.
