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
cargo run -p handy-recorder-probe -- <probe> [--device <id>] [--secs <n>] [--recording] [--take-headset]
```

`cargo run -p handy-recorder-probe` with no probe name lists them. The
first build takes a minute; later runs start at once. `--take-headset` opens
every recorder with `RecorderConfig::take_headset` (macOS, Bluetooth
headsets).

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
| 2 | `auto` | nothing (about 45 s) | all PASS on the built-in or USB microphone (`virtual-disconnect` runs on Linux only) |
| 3 | `disconnect-recording` | pick the USB mic; unplug when told; replug | PASS: `DeviceLost` (or `StreamInvalidated`) within a few seconds, audio before it kept, a new recorder works after replug |
| 4 | `disconnect-idle` | the same, while idle | PASS |
| 5 | `disconnect-recording` | pick the Bluetooth headset; turn it off or case it | PASS |
| 5b | `bluetooth-handoff` | a headset shared with a phone (AirPods): move it to the phone mid-recording, then back | a device loss, or real audio; FAIL means a stale stream (digital silence, no failure). Coming back works |
| 6 | `slow-start` | Bluetooth headset worn, idle | PASS: first audio well under 10 s (the `NoAudio` bound) |
| 7 | `external-app` | a meeting app's test call, both orders | PASS: neither side disrupted |
| 7b | `meeting-app` | a real call (Google Meet, Zoom, Teams): start it, stay in it, leave it, when told; again with `--recording`; on the built-in mic and on a Bluetooth headset | PASS: a new process started during the call records real audio (the most important check); every recording real and complete, before, during, and after the call, on the recorder kept open throughout and on new ones; the call app unaffected |
| 8 | `default-change` | switch the default input mid-recording | INFO: the recorder stays on its device, no silent switch |
| 9 | `sleep-wake` | sleep 30 s+, wake, Enter | PASS: an idle recorder survives sleep |
| 10 | `sleep-wake --recording` | the same, while recording | INFO: `Stalled` (a sleep ends the recording by design) or a device loss |
| 11 | `sleep-raw --device <built-in id>` | the same | INFO: callbacks RESUMED; note the gap lengths |
| 12 | `audio-service-restart` | restart the audio service when told | PASS: reported (or real audio throughout), and a new recorder in the same process works |
| 13 | `permission` | revoke microphone access first | INFO: `open` fails with `PermissionDenied` |
| 14 | `format-change` | macOS and Windows: change the device's sample rate when told; set it back afterwards | PASS: reported (expect `StreamInvalidated`), or the platform converts and a recording afterwards has the right length; a new recorder opens at the new rate |
| 15 | `soak` | nothing (30 min; `--secs` to change) | PASS: every recording complete and the right length, no failure, memory flat |

`all` runs the whole list, asking before each interactive probe.

### macOS

Done on a MacBook Pro, macOS 27, 2026-09-24 (see "Results so far"). Still to
do: `audio-service-restart` (`sudo killall coreaudiod`), a desktop Mac, an
Intel Mac, and macOS older than 14.2 (the CoreAudio weak-link check in
DESIGN.md). AirPods open at 24 kHz in call mode; that is expected.

### Windows

Done on a ThinkPad L14 Gen 2, Windows 11, 2026-09-25 (see "Results so
far"). Still to do: Bluetooth (5, 5b, 6), a desktop PC (S3 sleep instead
of Modern Standby), and exclusive mode. Notes:

- **Permission (13):** Settings > Privacy & security > Microphone has
  three switches, and turning off any one makes WASAPI refuse a desktop
  app's stream: "Microphone access" (registry: HKLM `ConsentStore\microphone`),
  "Let apps access your microphone" (HKCU, same key; despite the name it
  covers desktop apps), and "Let desktop apps access your microphone"
  (HKCU `...\microphone\NonPackaged`). `permission_status` reads all three.
  Try each one alone.
- **Disabling the device** (Settings > System > Sound > the device >
  Don't allow; Allow again under "All sound devices") behaves like
  unplugging: `DeviceLost`, AUDCLNT_E_DEVICE_INVALIDATED.
- **Disconnect (3-5):** wait for the device to be back before pressing
  Enter at "Reconnect"; the probe retries the reopen for a while, but a
  replugged USB device can take seconds to reappear.
- **Bluetooth (5, 6):** the headset switches to its hands-free profile when
  the microphone opens; note the rate `list`/`slow-start` report.
- **Sleep (9-11):** Modern Standby suspends the whole process, callbacks and
  watchdog alike. `sleep-raw` reports the gap.
- **Service restart (12):** an administrator PowerShell,
  `Restart-Service audiosrv -Force` (a normal one fails with "Cannot open
  audiosrv service").
- Optional: another app with exclusive control of the device (Sound Control
  Panel > Recording > device > Properties > Advanced) should make `open`
  fail with `DeviceBusy`. Needs an app that captures in exclusive mode; the
  probe has none yet.

### Linux

Done on a ThinkPad T14 (AMD), Fedora 43, PipeWire 1.4.11, 2026-09-26,
and on plain PulseAudio and pipewire-pulse in containers and CI (see
"Results so far").
Still to do: Bluetooth, and plain PulseAudio on a machine that runs it
(sound card, unplug, sleep). The results header says which sound server
ran ("sound server").

- **Host:** `list` should show devices with backend `PulseAudio`. `ALSA`
  means the PulseAudio host was unavailable and the library fell back;
  record that and stop, since sharing and naming then behave differently.
- **Monitor sources:** `list` will include "Monitor of ..." sources (system
  output); note them (an open decision in DESIGN.md).
- **Sharing (2: `second-process`, 7):** the most important Linux checks.
  Through the sound server both should PASS; raw ALSA devices are exclusive.
- **Disconnects (3, 4):** the sound server moves a stream whose source
  went away to another source, with no error; the library checks once a
  second that its source still exists. `virtual-disconnect` (in `auto`)
  checks the same with a virtual source and no hardware.
- **Service restart (12):** PipeWire:
  `systemctl --user restart pipewire pipewire-pulse wireplumber`;
  PulseAudio: `pulseaudio -k`.
- **Sleep (9-11):** `systemctl suspend`.
- **Bluetooth (5, 6):** switching to the headset's microphone may need its
  profile set to headset (HSP/HFP) in the sound settings.
- **Permission (13):** not applicable outside sandboxes; skip.
- **CI:** `tools/probe/ci/pulseaudio.sh` (plain PulseAudio) and
  `tools/probe/ci/pipewire.sh` (pipewire-pulse) run `auto` on Ubuntu
  24.04, the input a null sink's monitor playing noise;
  `virtual-disconnect` must PASS, which it only does on the PulseAudio
  host. Both run locally in an `ubuntu:24.04` container (podman or
  docker) as a non-root user, with the packages the workflow installs.
- **Finding the server:** CPAL's PulseAudio client looks only at
  `XDG_RUNTIME_DIR`, `PULSE_RUNTIME_PATH` and `PULSE_SERVER`; libpulse
  (and so `pactl`) also looks elsewhere. With none of them set (a bare
  container, `sudo`), `pactl` works but the library falls back to ALSA
  through the server's ALSA plugin, and every probe but
  `virtual-disconnect` still passes.
- **ALSA (no sound server):** the fallback. Stop the sound server first
  (PipeWire: `systemctl --user stop pipewire.socket pipewire-pulse.socket
  pipewire pipewire-pulse wireplumber`; `start` them again afterwards);
  `list` then shows backend `ALSA`. Pass `--device alsa:plughw:CARD=<n>,DEV=0`
  (`arecord -l` lists the cards): `default` usually routes to the sound
  server's ALSA plugin, which hangs without it (`OpenTimedOut` after 10
  s). Raw devices are exclusive, so `two-clients` and `second-process`
  fail with `DeviceBusy` by design.

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
| `meeting-app` | during a real call, a new process opens the microphone and records 5 s (an app started mid-call); plus one recorder kept open across the call: recording before it, while it starts (`--recording`) or ends, three push-to-talk recordings during it, and after it; and a new recorder during and after | a call app |
| `slow-start` | time from open to first audio | a Bluetooth headset |
| `disconnect-recording` | failure reported, audio before it kept, `start` returns it, reopen after reconnect works | a removable device |
| `disconnect-idle` | the same while the recorder is open but idle | a removable device |
| `virtual-disconnect` | Linux: a virtual PulseAudio source removed during a recording and while idle: `DeviceLost`, audio before it kept | `pactl` |
| `bluetooth-handoff` | a shared headset moving to the phone mid-recording and back: reported, not silent | a headset paired with a phone |
| `replug` | silence after replugging a USB device: a fresh process vs this process | a USB device |
| `default-change` | changing the system default input mid-recording (informational) | two inputs |
| `sleep-wake` | no watchdog false positive across sleep; `--recording` to sleep mid-recording | |
| `sleep-raw` | a raw CPAL stream (no library, no watchdog) across sleep: callback gaps, errors, whether it resumes | the built-in mic |
| `audio-service-restart` | the OS audio service restarting mid-recording; a new recorder in the same process afterwards | admin rights (Windows, macOS) |
| `permission` | what `open` does with access denied (informational) | access revoked |
| `format-change` | the device's sample rate changed during a recording: reported, or converted; never audio at the wrong speed | macOS or Windows |
| `soak` | one recorder open for `--secs` (default 30 min), a 5 s recording every 30 s: completeness, length drift, memory (Linux), watchdog false positives | time |

`auto` runs every probe that needs no operator action.

## Results so far

macOS 27, MacBook Pro (Mac16,6), AirPods Pro 3, USB-C EarPods, 2026-09-24:

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
- Permission denied: `open` fails with `PermissionDenied`.
- Bluetooth handoff: moving the AirPods to a phone mid-recording keeps the
  Mac's device alive, callbacks on schedule, and no platform error; the
  stream delivers exact zeros until the AirPods come back, then real audio
  resumes on the same stream. No CoreAudio device property changes during
  the handoff.

Windows 11 (build 26200), ThinkPad L14 Gen 2 (i5-1135G7, Modern Standby
only), built-in Intel Smart Sound mic array and USB-C EarPods, both 48 kHz
2 ch, 2026-09-25:

- `auto`: all PASS on the built-in mic. First open in a process 25-400 ms
  (almost all of it building the stream; about 400 ms early in the
  session, 25 ms later), 70-80 ms after that; first audio 15-30 ms after
  the stream starts. Resolving the device (enumeration) costs 2.5-7 ms.
- The built-in mic's DSP gates a quiet room: the floor falls from about
  -55 dBFS to about -100 dBFS after about 2 s, without reaching exact
  zeros. (It made `second-process` report digital silence until the probe
  stopped rounding peaks.)
- Every stream reports one xrun: WASAPI's discontinuity flag on its first
  read, which CPAL's `device_position != 0` guard misses on these devices.
- Disconnects (EarPods, unplugged): `DeviceLost` (AUDCLNT_E_DEVICE_INVALIDATED)
  in about 2 s, recording and idle, audio before it kept; reopen works. A
  recorder opened about 1 s after the replug got 1.9 s of exact zeros
  before real audio (as once on macOS). Disabling the device in Settings
  is the same (`DeviceLost`), and so is re-enabling it (1.0 s of zeros
  once). While the device's page in Sound settings was open, a recording
  on it got 3-6 s of exact zeros before the loss, with no failure; the same
  happened (3.5 s) during `default-change`, not during plain unplugs.
- Sharing: another process, a recording app in both orders, and a Google
  Meet call (both orders, a new process during the call) all work.
- Default-input change: before the fix, `StreamInvalidated`; after opening
  the resolved device, the recorder stays on its device.
- Sleep: Modern Standby suspends the process; callbacks stop at sleep and
  resume on wake (108 s gap), and `Instant` counts the sleep. An idle
  recorder survives. Before the fix a recording across sleep carried on
  with a 114 s gap and no failure; now it ends with `Stalled` ("the process
  was suspended").
- Audio service restart: the stream fails with `DeviceLost`
  (AUDCLNT_E_DEVICE_INVALIDATED) or `StreamInvalidated` (ERROR_NOT_FOUND),
  varying between runs; a new recorder in the same process works at once.
- Permission: turning off any one of the three switches makes `open` fail
  with `PermissionDenied` (E_ACCESSDENIED from WASAPI), "Let apps access
  your microphone" included. `permission_status` reads `Denied` for each
  (`Granted` with all on). Turning off "Microphone access" or "Let apps
  access your microphone" while a recorder is open (`hold --secs 30`)
  fails its stream with AUDCLNT_E_DEVICE_INVALIDATED, the unplug code;
  before the fix it read as `DeviceLost`, now as `PermissionDenied`.

Fedora 43 (KDE), ThinkPad T14 (AMD Ryzen, s2idle), PipeWire 1.4.11 with
pipewire-pulse, built-in digital microphone and USB-C EarPods, 2026-09-26.
Before the fixes (these results led to them):

- Host `PulseAudio`; sharing works: `second-process`, `two-clients`,
  `external-app`, and `meeting-app` (Jitsi in Firefox, both orders) PASS.
- Every open took 2.0-2.1 s, and audio arrived in 8192-frame (170 ms)
  blocks: CPAL requests no fragment size, and the server's default is 2 s,
  which CPAL's `play` waits out. Requesting 1024 frames fixes both (open
  80-140 ms); smaller requests (480, 960) shrank PipeWire's graph quantum
  for every application (`pw-top`). With fixed fragments, a second stream
  on one server connection starved, so each stream now connects on its
  own.
- Disconnects: no failure. Unplugging the EarPods moved the stream to the
  built-in microphone (the server sends `RecordStreamMoved`, which CPAL's
  pulseaudio crate ignores); the recording carried on with the other
  microphone's audio. Reproduced by removing a virtual source
  (`virtual-disconnect`). With the source check, `DeviceLost` about 0.5 s
  after removal. A default-input change leaves the stream on its source.
- The EarPods deliver 0.7 to over 3 s of exact zeros each time their
  source resumes from suspend (raw CPAL too), so `baseline` on them FAILs
  on digital silence.
- The built-in microphone carries a DC offset (channel 1 about +0.29,
  channel 0 about +0.04), so its level reads -16.6 dBFS whatever the room
  does; the probes' levels do not remove it.
- Sleep: an idle recorder survives. A recording across a 110 s sleep
  carried on with no failure: the process is frozen, and `Instant`
  (`CLOCK_MONOTONIC`) stops (`sleep-raw`: 54.4 s of wall time, 1.8 s of
  `Instant`). Fixed with `CLOCK_BOOTTIME`; to be confirmed.
- Audio service restart: `StreamInvalidated` ("PulseAudio disconnected"),
  audio before it kept; reopening in the same process failed for 15 s.
  Fixed by connecting per stream; to be confirmed.
- ALSA fallback (`PULSE_SERVER` pointing nowhere): 14 devices with
  duplicate names, the `null` PCM among them; a `hw:` device PipeWire
  holds fails with `DeviceBusy`; `default` records through PipeWire.

After the fixes, same machine and day:

- Open 80-140 ms; `auto` all PASS on the built-in microphone, with
  `virtual-disconnect` (`DeviceLost` about 0.5 s after removal).
- Unplugging the EarPods: `DeviceLost`, recording and idle, audio before
  it kept; a new recorder after replugging records real audio.
- Sleep across a recording: `Stalled`, "the process was suspended for
  113.2 s during a recording", reported on wake.
- Audio service restart (twice): `StreamInvalidated`, audio before it
  kept; a new recorder in the same process records at once.
- `soak` (30 min, built-in microphone): 60 recordings, all complete;
  audio/wall 1.0044 overall, 1.0015-1.0075 each (the start and stop
  edges, no drift); RSS 13.3 MB -> 14.5 MB, flat after the first minutes;
  no warnings.

Plain PulseAudio 15.99.1, Ubuntu 22.04 container (podman, on the Fedora
machine), `tools/probe/ci/pulseaudio.sh`, 2026-09-26:

- CPAL's fixed-size streams break on PulseAudio itself: CPAL caps the
  server-side buffer at one fragment, and the stream then delivers one
  callback and nothing more (882 to 4410 frames tried). Without the cap
  (a one-line change in CPAL: branch `pulseaudio-record-max-length` of
  github.com/handy-computer/cpal, the library's CPAL dependency),
  fixed fragments work. It happens when the
  source delivers more than a fragment at a time (here, likely because
  `pacat`'s 50 ms latency sets the null sink's; not verified); an idle
  null monitor or a sound card with timer
  scheduling (`tsched=1`) shrinks its blocks and works even with the cap.
  With `tsched=0`, the built-in microphone lost 7-12% of its audio at
  480-frame fragments with the cap and none without it.
- The cap is not what starves a second stream on one connection under
  pipewire-pulse: at 1024-frame fragments that happens with and without
  it, in release builds. One connection per stream avoids it.
- Without fixed fragments, PulseAudio's default is as coarse as
  PipeWire's on a source that honors latency (a null source: `play` took
  3.8 s): opens up to 1.9 s, a 3 s recording returned 2.35 s (the end
  was still in the server's buffer), warm recordings 2.4-25x their hold;
  `two-clients` and `second-process` FAIL.
- With the patched CPAL and 1024-frame fragments: `auto` all PASS, twice;
  open about 11 ms; warm recordings 1.09-1.46x their hold;
  `virtual-disconnect` `DeviceLost` about 0.5 s after removal (the source
  check works on PulseAudio too).
- `module-sine-source` delivers 350 ms blocks whatever is requested; the
  script uses a null sink's monitor playing noise instead.
- The pulseaudio crate's reader zero-fills 1 MB on every socket read. In
  a debug build it fell behind a source sending 400 packets a second (2.5
  ms each) and never recovered: a server reply never arrived, and closing
  the stream hung. Release builds keep up; the script runs the probe in
  release. A sound card at 1024-frame fragments sends about 47 a second,
  so a debug build of an application is likely fine, but not tested
  (TODO.md).

Raw ALSA (PipeWire stopped), same Fedora machine, 2026-09-26:

- The library falls back to ALSA by itself. `default` routes to
  PipeWire's ALSA plugin and hangs without a server (`OpenTimedOut`
  after 10 s).
- Built-in microphone (`plughw` and `hw`): `baseline`, `warm-cycles`,
  `slow-sink` PASS, open about 40 ms. `two-clients` and `second-process`
  fail with `DeviceBusy`: raw devices are exclusive.
- The EarPods deliver exact zeros in a quiet room (8 s, apart from tiny
  blips) on raw ALSA too: the headset gates silence to digital zero; it
  is not the sound server.
- Unplugging the EarPods (`sysdefault:CARD=Earpods`): `DeviceLost` ("The
  requested audio device is not available"), recording and idle, about
  3 s after the prompt; audio before it kept; reopening after the replug
  works. ALSA reports the unplug itself; no source check is involved.
- Sleep across a recording: `Stalled`, "the process was suspended for
  53.3 s", as on PipeWire.

Ubuntu 24.04 containers (podman, on the Fedora machine),
`tools/probe/ci/pulseaudio.sh` and `tools/probe/ci/pipewire.sh`,
2026-09-26:

- Plain PulseAudio 16.1 and PipeWire 1.0.5 (pipewire-pulse, WirePlumber,
  no sound card): `auto` all PASS on both; `virtual-disconnect`
  `DeviceLost` 0.45-0.49 s after removal. Opens about 10 ms on
  PulseAudio, 25-115 ms on PipeWire.
- The first run without `XDG_RUNTIME_DIR` fell back to ALSA through
  PulseAudio's ALSA plugin: every probe passed but `virtual-disconnect`,
  which reported INFO, so the scripts now set it and require that probe
  to PASS.
