# Review list

Decisions taken during implementation that need a closer look, and work
deliberately left for later. Code locations carry a `TODO(review)` marker
naming the entry here.

Two pre-release items live in DESIGN.md, "Before the first release": the
Rubato 5.x upgrade and the review of every internal timeout (marked
`REVIEW(timeouts)` in the code).

Current timeout placeholders (`capture/engine.rs`, `Timeouts::default`):
open 10 s, close 5 s, delivery-thread exit at close 1 s, no audio after
open 10 s, stall 5 s, sink heartbeat 10 s, stop's pause acknowledgement
2 s (Handy's value), stop's overall deadline 5 s, watchdog tick 50 ms.

## Decisions to review

### Device enumeration on every open

The CPAL backend enumerates input devices on every open to report which
device it opened (`InputDevice`: ID, name, occurrence, default flag).
Handy never did this. Measured on macOS with three input devices: about
1.4 ms warm, but about 99 ms for the first enumeration in a process while
CoreAudio warms up. Enumeration during open skips the per-device channel
query (below). Not yet measured on Windows or Linux.

Location: `backend/cpal.rs`, `open_device`.

### `InputDevice::channels`

Added as `Option<u16>` so Handy's channel picker can offer `Channels::Only`
without opening the device (replaces Handy's
`preferred_input_channel_count`). It is the channel count of the format the
device would be opened at. `None` on ALSA, where reading configs opens the
PCM (which cpal avoids during enumeration: failed opens can leak
descriptors and poison ALSA until the process exits). On CoreAudio and
WASAPI it costs one config query per device during `list_input_devices`:
about 2.4 ms warm for three devices on macOS versus 1.4 ms without. The
opened device's `info().device.channels` is always set, from its format.

Location: `device.rs`, `backend/cpal.rs` (`channels_without_opening`).

### Default device changing mid-recording

CPAL 0.18 can report `DeviceChanged`: a stream on the default device was
rerouted to the new default and keeps running. The library treats it as a
recorder failure for now: `info().device` would otherwise name the wrong
microphone, the new device may run at a different rate than the ring and
resampler were built for, and a device change stays visible to the
application ("a failed recorder stays failed"). Of the hosts enabled today
only iOS emits it for input; CoreAudio emits it for default output only,
and WASAPI reports a default change on a capture stream as
`StreamInvalidated`. PipeWire (not enabled) emits it. Review whether a
reroute should instead be survivable when the format is unchanged.

Location: `backend/mod.rs`, `BackendErrorKind::DeviceChanged`.

### Final chunk: synthetic resampler output

When a recording ends partway through a 1024-frame resampler block, Handy
processes the block zero-padded and emits all of its output. The part
past the real audio is resampled padding: near-silence with ringing from
the last real samples, and sometimes more than a whole extra chunk (1
frame at 8 kHz gives 2048 output frames where 1026 are real). The library
keeps only the real frames (the frame-count formula, resampler delay
included) and zero-pads the final chunk, so `valid_frames` is exact. Real
audio stays bit-identical to Handy; the equivalence test checks exactly
that, plus zero padding. Review that this intentional change (the
design's "final-chunk zero padding") is acceptable for Handy.

Location: `capture/resampler.rs`, `drain_tail`.

### Format limits

`open` rejects an output `sample_rate` outside 1 kHz..=768 kHz and a
`frames_per_chunk` of 0 or longer than 10 s, as `UnsupportedFormat`. Any
rate pair Rubato's FFT resampler accepts is otherwise allowed; a pair with
a small common divisor (48000 -> 44101 Hz) makes Rubato use very large
FFTs. A device whose native sample format is not u8/i8/i16/i32/f32 (for
example 24-bit packed) fails `open` with `Backend`, as in Handy.

Location: `capture/engine.rs` (`MIN_OUTPUT_RATE`, `MAX_OUTPUT_RATE`,
`MAX_CHUNK_SECONDS`).

### Watchdog false positives

The watchdog fails the recorder with `NoAudio` (no callback within 10 s of
open), `Stalled` (no callback for 5 s after audio started), or
`SinkStalled` (delivery thread made no progress for 10 s). The known
risks from the design are untested on hardware: system sleep and wake,
a process paused in a debugger, a Bluetooth device slower than 10 s to
start, and a sink whose first call is slow (a VAD model warming up counts
against the 10 s heartbeat). Every trip is logged at `warn` with what it
measured. Values are in the `REVIEW(timeouts)` list.

Location: `capture/engine.rs`, `Watchdog`.

### Allocation in the error callback

For errors that end the stream, the CPAL adapter formats the platform
message into a `String` and sends it over a channel from the error
callback, which may run on the audio thread. This happens at most a few
times per recorder (the first fatal error fails it), and survivable
errors (xruns) never allocate. Review whether fatal errors need a
preallocated slot instead.

Location: `backend/cpal.rs` (`map_error`), `capture/engine.rs` (`open_stream`).

### Untested paths

- `Processing` failures: the fake backend cannot make Rubato fail, so the
  path from a resampler error to a failed recorder is covered only by
  reading. A test hook that injects a resampler error would cover it.
- Real hardware: device loss, stream invalidation, default-device changes,
  sleep/wake, and slow Bluetooth start are probed on macOS only. The
  tier-3 probes are in `tools/probe/` (see its README for the
  per-platform checklist).
  macOS results (MacBook Pro, AirPods Pro 3, USB-C EarPods): device loss
  reported as `DeviceLost` in about 2 s (USB) with the audio before it
  kept, while recording and idle; reopen after reconnect works; AirPods
  first audio 220 ms after open; sharing the microphone with another
  process or app works in both orders; a default-input change leaves the
  recorder on its device, with no failure and no silent switch; a real
  `SinkStalled` trip. Open issues from the session: sleep (above) and
  digital silence (above).
- Windows and Linux compile (Linux type-checked with a stub `alsa.pc`)
  but have not run. Tier-2 virtual-device tests (PulseAudio and
  pipewire-pulse null sources) are not written; they need a Linux machine
  or CI.

### PulseAudio server restarts

The CPAL backend keeps one `cpal::Host` for the process (the PulseAudio
host holds a server connection; DESIGN.md asks for one host per process).
If the sound server restarts, that connection is dead and every later
`list_input_devices` and `open` fails until the process restarts. The
backend should recreate the host when an operation fails with a
host-unavailable error.

Location: `backend/cpal.rs`, `CpalBackend::shared`.

### Default device without an ID

If the platform returns a default device with no ID, `open` reports it
with an empty `id` (and `id_is_stable: false`). It still opens; the ID
cannot be used to reopen it. Not seen on macOS.

Location: `backend/cpal.rs`, `open_device`.

### Permission denied on macOS

macOS opens a microphone the app may not use and delivers exact zeros,
with no error (confirmed on hardware). `open` now fails with
`PermissionDenied` when `permission_status()` is `Denied` (which includes
macOS's `Restricted`), and `start` fails the recorder the same way if
access was revoked while it was open. `NotDetermined` is left alone: macOS
shows its prompt then, and what a recording does while the prompt is up
belongs to the permission-request prototype.

Confirmed on hardware: with access denied, `open` fails with
`PermissionDenied` naming the device and the settings pane.

Location: `capture/engine.rs` (`open_stream`, `Engine::start`).

### Digital silence

Exact zeros from a real microphone mean denied access, a muted device, or
a stale stream (the `pvrecorder` symptom): real microphones have a noise
floor. Seen on hardware twice: with access denied, and once after
replugging USB EarPods (2 s of zeros in both disconnect probes; not
reproduced by the `replug` probe, where the cached config and a fresh
process both recorded real audio). The watchdog deliberately ignores
amplitude; the delivery thread now logs at `warn` when a whole recording,
or a run of 1 s or more, is exact digital silence. Review whether this
should also be a statistic on `Stopped` (a public API addition).

**Bluetooth handoff is this bug, reproduced (2026-09-24, macOS, AirPods
Pro 3 shared with a phone):** moving the AirPods to the phone mid-recording
left the Mac's AirPods device present, with callbacks on schedule and no
platform error, delivering exact zeros (24.5 s of the 30 s recording). The
recorder reported nothing; only the stop-time `warn` noticed. Decision
needed: see the options in the session notes (a platform signal for the
handoff mapped to `DeviceLost`; a sustained-digital-silence failure; or
silence surfaced on `Stopped`).

Location: `capture/delivery.rs` (`observe_silence`, `stop`).

### Sleep and wake

Measured on a MacBook (built-in microphone, `sleep-wake` and `sleep-raw`
probes): macOS stops input callbacks when the sleep sequence starts, about
5 s before the system sleeps, and resumes them after wake on the same
stream, with no platform error. The gap was 32.6 s of process uptime
(203.8 s wall): uptime excludes true sleep, but maintenance wakes run
processes without audio. No usable stall bound survives that, and a sleep
notification would race the stop.

Decision taken: a stall is a failure only during a recording. While idle it
is logged at `info` and forgiven when callbacks resume. During a recording
the silence counts from the later of the last callback and the recording's
start, so a recording started just after wake gets the full bound. To
review:

- Sleeping during a recording still fails the recorder with `Stalled`
  (the recording would otherwise stitch audio across the sleep), although
  the stream itself would resume. Ending only the recording would need a
  new `EndReason`.
- A stream that dies silently while idle is now noticed only once a
  recording starts, after up to the stall bound (5 s) of it. `NoAudio`
  (no callback ever) still fails while idle.
- AirPods disconnect when the Mac sleeps; that is reported as `DeviceLost`.

Confirmed on hardware after the change: an idle recorder survives sleep
(callbacks resumed 20.7 s after the idle stall was logged) and records
normally after wake.

Location: `capture/engine.rs`, `Watchdog::check`.

## Changes from Handy to confirm

- Handy's two `needs_reopen` tests were removed with the rebuild-on-error
  model they tested. Their replacement is the failure tests
  (`tests/failures.rs`): a failed recorder stays failed.
- Handy's consumer-loop tests were migrated to the recorder: idle audio
  discarded, repeated start/stop without leaks, shutdown without audio,
  chunk size, and the missing-callback-at-stop test (now `Stalled`). See
  `tests/lifecycle.rs`, `tests/failures.rs`, `tests/formats.rs`.
- Handy's own start/stop test raced Start against the consumer's drain
  (it could discard the first block as idle audio). The library's tests
  wait until a start is applied instead. The start edge itself is still
  inexact by design (DESIGN.md, "Recording boundaries").
- An out-of-range `Channels::Only` is `InvalidChannel` from `open`; Handy
  fell back to averaging. Handy's adapter implements its own fallback,
  using `InputDevice::channels`.
- `examples/push_to_talk.rs` did not exit on Ctrl-D (its own event sender
  kept the channel open); it now quits on end of input.

## Not started

- The phase-2 exit criterion: a Handy branch using this crate as a path
  dependency, with Handy's sink (VAD, collection, streaming forward)
  passing Handy's audio tests. It is in another repository.
- The permission-request prototype (provisional in DESIGN.md): needs a
  bundled macOS app, a terminal-hosted Node process, and an unbundled CLI.
- `list_input_devices` does not filter PulseAudio "Monitor of ..." sources
  (an open decision in DESIGN.md).
- A README, including the macOS < 14.2 weak-link flag
  (`-C link-arg=-Wl,-weak_framework,CoreAudio`) DESIGN.md asks for.
