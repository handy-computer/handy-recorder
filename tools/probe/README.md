# handy-recorder-probe

Scripted hardware probes for handy-recorder (DESIGN.md, testing tier 3). The
library's own tests run against a fake backend; these run the public API
against real devices, ask the operator to act ("disconnect now"), and report
PASS, FAIL, or INFO with what they measured. They are how we learn whether
the fake's assumptions hold on each platform.

The probe is a separate crate in the workspace. The library never depends on
it, and `cargo build` / `cargo test` at the repository root do not touch it.

## Setup

| Platform | Needs |
| --- | --- |
| macOS | Rust (rustup), Xcode command line tools |
| Windows | Rust (rustup, MSVC toolchain), Visual Studio Build Tools with "Desktop development with C++" |
| Linux | Rust (rustup), `build-essential pkg-config libasound2-dev` (Fedora: `gcc pkg-config alsa-lib-devel`), and `pactl` (usually present) |

```sh
git clone <repo> && cd handy-recorder
cargo run -p handy-recorder-probe -- <probe> [--device <id>] [--secs <n>] [--recording]
```

`cargo run -p handy-recorder-probe` with no probe name lists them. The
first build takes a minute; later runs start at once.

## Results

Every run writes `tools/probe/results/<utc time>-<os>-<probe>.txt`
(git-ignored): the machine (OS and version, model or distribution, kernel,
sound server, library commit), the permission status, every input device,
everything the probe printed, each prompt with when the operator answered,
and the library's full debug log. After a session, zip the `results` folder
and send it; nothing else is needed to diagnose a FAIL.

The terminal shows only the probe's output and library warnings. Set
`PROBE_VERBOSE=1` to see the whole debug log live.

## Before a session

- **Bluetooth headsets shared with a phone** (AirPods on the same Apple
  account) can be listed on the computer while the phone holds them; opening
  them then yields exact zeros. Which device holds the headset is up to the
  OS and the headset (macOS: Bluetooth settings > the AirPods > "Connect to
  This Mac"); the library does not and cannot choose. Make sure the computer
  has the headset before Bluetooth probes.
- **Pick the device deliberately.** Probes use the system default input
  unless given `--device <id>` (IDs are in `list`). A Bluetooth headset that
  is connected but not worn delivers exact digital silence, which the probes
  report as a FAIL; wear it, or pass `--device` for the built-in microphone.
- Have to hand: a USB microphone or USB headset, a Bluetooth headset, and a
  meeting app (Zoom, Meet, Teams) or any recording app.
- Close other apps that might hold the microphone, except where a probe asks
  for one.

## The checklist

Run this on every platform, and on each machine that matters (a desktop and
a laptop behave differently around sleep and Bluetooth).

| # | Command | Do | Expect |
| --- | --- | --- | --- |
| 1 | `list` | nothing | every microphone listed, one default, `channels` set (except ALSA) |
| 2 | `auto` | nothing (about 40 s) | all PASS on the built-in or USB microphone |
| 3 | `disconnect-recording` | pick the USB mic; unplug when told; replug | PASS: `DeviceLost` (or `StreamInvalidated`) within a few seconds, audio before it kept, a new recorder works after replug |
| 4 | `disconnect-idle` | the same, while idle | PASS |
| 5 | `disconnect-recording` | pick the Bluetooth headset; turn it off or case it | PASS |
| 5b | `bluetooth-handoff` | a headset shared with a phone (AirPods): move it to the phone mid-recording, then back | a device loss, or real audio; FAIL means a stale stream (digital silence, no failure). Coming back works |
| 6 | `slow-start` | Bluetooth headset worn, idle | PASS: first audio well under 10 s (the `NoAudio` bound) |
| 7 | `external-app` | a meeting app's test call, both orders | PASS: neither side disrupted |
| 8 | `default-change` | switch the default input mid-recording | INFO: the recorder stays on its device, no silent switch |
| 9 | `sleep-wake` | sleep 30 s+, wake, Enter | PASS: an idle recorder survives sleep |
| 10 | `sleep-wake --recording` | the same, while recording | INFO: `Stalled` (a sleep ends the recording by design) or a device loss |
| 11 | `sleep-raw --device <built-in id>` | the same | INFO: callbacks RESUMED; note the gap lengths |
| 12 | `audio-service-restart` | restart the audio service when told | PASS: reported (or real audio throughout), and a new recorder in the same process works |
| 13 | `permission` | revoke microphone access first | INFO: `open` fails with `PermissionDenied` |

`all` runs the whole list, asking before each interactive probe.

### macOS

Done on a MacBook Pro, macOS 27, 2026-09-24 (see "Results so far"). Still to
do: `audio-service-restart` (`sudo killall coreaudiod`), a desktop Mac, an
Intel Mac, and macOS older than 14.2 (the CoreAudio weak-link check in
DESIGN.md). AirPods open at 24 kHz in call mode; that is expected.

### Windows

The first real run of the WASAPI backend. Pay attention to:

- **Permission (13):** turn off Settings > Privacy & security > Microphone >
  "Let desktop apps access your microphone". Expect `PermissionDenied` from
  `open`. The library recognizes it only by the WASAPI message
  (`E_ACCESSDENIED`), so this is the most important Windows check.
- **Disconnect (3-5):** also try disabling the device in Settings > System >
  Sound > the device > Disable, as a variant of unplugging.
- **Default change (8):** CPAL reports a default-device change on a stream
  opened on the default device as `StreamInvalidated`; the library opens the
  resolved device, so expect no failure, but record what happens.
- **Bluetooth (5, 6):** the headset switches to its hands-free profile when
  the microphone opens; note the rate `list`/`slow-start` report.
- **Sleep (9-11):** laptops with Modern Standby may keep audio running;
  record the gaps `sleep-raw` reports.
- **Service restart (12):** administrator PowerShell,
  `Restart-Service audiosrv -Force`.
- Optional: another app with exclusive control of the device (Sound Control
  Panel > Recording > device > Properties > Advanced) should make `open`
  fail with `DeviceBusy`.

### Linux

The first real run of the PulseAudio host. Run the checklist on PipeWire
(with `pipewire-pulse`, most current distributions) and, if possible, on
plain PulseAudio; the results header says which ("sound server").

- **Host:** `list` should show devices with backend `PulseAudio`. `ALSA`
  means the PulseAudio host was unavailable and the library fell back;
  record that and stop, since sharing and naming then behave differently.
- **Monitor sources:** `list` will include "Monitor of ..." sources (system
  output); note them (an open decision in DESIGN.md).
- **Sharing (2: `second-process`, 7):** the most important Linux checks.
  Through the sound server both should PASS; raw ALSA devices are exclusive.
- **Service restart (12):** PipeWire:
  `systemctl --user restart pipewire pipewire-pulse wireplumber`;
  PulseAudio: `pulseaudio -k`. The reopen half is expected to FAIL today:
  the library keeps one server connection per process and does not
  reconnect yet (TODO.md, "PulseAudio server restarts"). The result tells
  us what to fix.
- **Sleep (9-11):** `systemctl suspend`.
- **Bluetooth (5, 6):** switching to the headset's microphone may need its
  profile set to headset (HSP/HFP) in the sound settings.
- **Permission (13):** not applicable outside sandboxes; skip.

## Reading results

- **FAIL "no failure reported ... trailing digital silence"** in a
  disconnect or service-restart probe: a stale stream, the `pvrecorder` bug
  the library exists to prevent. Highest priority.
- **FAIL "exact digital silence"** in `baseline` or `auto`: the device
  delivers zeros: a Bluetooth headset not worn, a muted device, or denied
  access. Check the device before suspecting the library.
- **FAIL "watchdog false positive"**: a `Stalled`, `NoAudio`, or
  `SinkStalled` trip on a healthy device; include the gaps from `sleep-raw`.
- **INFO** results are observations, not verdicts: they record what the
  platform did so we can decide what the library should do.

## Probes

| Probe | What it checks | Needs |
| --- | --- | --- |
| `list` | permission status, devices, IDs, channel counts | |
| `baseline` | a recording is complete, real (not digital silence), and the right length | |
| `warm-cycles` | 30 quick recordings on one open recorder | |
| `two-clients` | two recorders on one device in one process; closing one leaves the other working | |
| `slow-sink` | a sink blocking its first call for `--secs` (3: overrun counted; 12: `SinkStalled`) | |
| `second-process` | another process records 8 s; this one opens 3 s in, records 3 s, closes; both complete, and the other keeps getting audio | |
| `external-app` | record alongside a real app in both orders; neither is disrupted | a recording app |
| `slow-start` | time from open to first audio | a Bluetooth headset |
| `disconnect-recording` | failure reported, audio before it kept, `start` returns it, reopen after reconnect works | a removable device |
| `disconnect-idle` | the same while the recorder is open but idle | a removable device |
| `bluetooth-handoff` | a shared headset moving to the phone mid-recording and back: reported, not silent | a headset paired with a phone |
| `replug` | silence after replugging a USB device: a fresh process vs this process | a USB device |
| `default-change` | changing the system default input mid-recording (informational) | two inputs |
| `sleep-wake` | no watchdog false positive across sleep; `--recording` to sleep mid-recording | |
| `sleep-raw` | a raw CPAL stream (no library, no watchdog) across sleep: callback gaps, errors, whether it resumes | the built-in mic |
| `audio-service-restart` | the OS audio service restarting mid-recording; a new recorder in the same process afterwards | admin rights (Windows, macOS) |
| `permission` | what `open` does with access denied (informational) | access revoked |

`auto` runs every probe that needs no operator action.

## Results so far

macOS 27, MacBook Pro (Mac16,6), AirPods Pro 3, USB-C EarPods, 2026-09-24
(raw logs in `results/2026-09-24-macos-session/` on the machine that ran
them):

- `auto`: all PASS on the built-in microphone.
- Disconnects: `DeviceLost` in about 2 s (USB) with the audio before it
  kept, recording and idle; reopen after reconnect works. Once, a recorder
  opened just after a USB replug got 2 s of exact zeros; not reproduced by
  `replug`.
- AirPods first audio 220 ms after open. Sharing with another process and
  with Voice Memos works in both orders. A default-input change leaves the
  recorder on its device.
- Sleep: callbacks stop about 5 s before the system sleeps and resume after
  wake (32.6 s of process uptime without callbacks). After the watchdog
  change, an idle recorder survives sleep. AirPods disconnect on sleep
  (`DeviceLost`).
- Permission denied: `open` fails with `PermissionDenied` (after the fix;
  before it, recordings were silent).
