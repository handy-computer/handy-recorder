# Review list

Open work and open questions. Code locations carry a `TODO(review)`
marker naming the entry here.

Two pre-release items live in DESIGN.md, "Before the first release": the
Rubato 5.x upgrade and the review of every internal timeout (marked
`REVIEW(timeouts)` in the code). The current values look reasonable;
drop the "placeholder" wording once Windows and Linux do not contradict
them.

## Where we are (for a fresh session)

Branch `extract-from-handy`. Phases 1 and 2 of DESIGN.md's plan are done
except the Handy branch. Hardware-tested on macOS (every probe) and
Windows (every probe but Bluetooth and exclusive mode); see
`tools/probe/README.md`. DESIGN.md is not committed.

## Work

1. A Linux session with the probe checklist (`tools/probe/README.md`),
   plus the tier-2 virtual-device tests (PulseAudio and pipewire-pulse
   null sources; need Linux or CI). `audio-service-restart` is expected
   to fail its reopen until "PulseAudio server restarts" is fixed; watch
   `sleep-wake --recording` (below, "Sleep on Linux"). Windows still
   lacks Bluetooth.
2. Fix "PulseAudio server restarts" (below).
3. A review of the engine code (`capture/engine.rs`,
   `capture/delivery.rs`): the concurrency there has only its own tests
   and one hardware session behind it.
4. The phase-2 exit criterion: a Handy branch using this crate as a path
   dependency, with Handy's sink (VAD, collection, streaming forward)
   passing Handy's audio tests. It is in another repository.
5. A README, including the macOS < 14.2 weak-link flag
   (`-C link-arg=-Wl,-weak_framework,CoreAudio`) DESIGN.md asks for.
6. `list_input_devices` filters out PulseAudio "Monitor of ..." sources
   (decided; not implemented).

## Open questions

### Default device changing mid-recording

What happens to a recorder opened on the default device when the user
switches the default input:

- macOS (CoreAudio): the recorder stays on its device, no error
  (confirmed on hardware). CPAL reports `DeviceChanged` for default
  output only.
- Windows (WASAPI): the recorder stays on its device, no error
  (confirmed on hardware). CPAL fails a stream opened from
  `default_input_device()` with `StreamInvalidated` when the default
  changes (WASAPI cannot reroute a stream; confirmed on hardware), so
  `open` resolves the default and opens that device instead.
- Linux, PulseAudio: expected to stay on its device (CPAL resolves the
  default to a named source when it opens). Not tested.
- Linux, ALSA: stays on the PCM opened. Not tested.
- iOS and PipeWire (not enabled) emit `DeviceChanged`, which the library
  treats as a recorder failure.

Whether a reroute should ever be survivable only matters if PipeWire or
iOS is enabled.

Location: `backend/mod.rs`, `BackendErrorKind::DeviceChanged`;
`backend/cpal/mod.rs`, `open_device`.

### Device enumeration on every open

`open` lists input devices to find a device by ID, and, for the default
device, to report which device it opened. Measured on macOS: about
1.4 ms warm, 99 ms for the first enumeration in a process (CoreAudio
warming up). Windows: 2.5-7 ms, the first open included, with two
devices (the `resolve_device=` debug line). Not measured on Linux; unless
it is much slower there, the cost does not matter. There is no constant-time
lookup through CPAL 0.18: `device_by_id` enumerates on every host. The
platforms can look up directly (CoreAudio translates a UID to a device;
WASAPI `IMMDeviceEnumerator::GetDevice`; PulseAudio source info by
name), so an upstream CPAL change could add it. IDs synthesized as
`name#occurrence` (the platform gave none) can only be found by
enumerating.

Location: `backend/cpal/mod.rs`, `open_device`.

### PulseAudio server restarts

The CPAL backend keeps one `cpal::Host` for the process (the PulseAudio
host holds a server connection; DESIGN.md asks for one host per process).
If the sound server restarts, that connection is dead and every later
`list_input_devices` and `open` fails until the process restarts. The
backend should recreate the host when an operation fails with a
host-unavailable error.

Location: `backend/cpal/mod.rs`, `CpalBackend::shared`.

### Sleep on Linux

A recording across sleep must fail (`Stalled`), not carry on with a gap.
macOS stops callbacks seconds before it suspends the process, so the
stall check sees it. Windows (Modern Standby) suspends the process and
its callbacks at once, so the watchdog notices its own suspension
instead: a gap between its checks, which `Instant` (QPC) measures because
it counts sleep. On Linux `Instant` is `CLOCK_MONOTONIC`, which stops
during suspend, so if the process is frozen as on Windows the gap is
invisible to both checks. `sleep-wake --recording` compares with the wall
clock and will FAIL if so; the fix would be `CLOCK_BOOTTIME` for the
suspension check.

Location: `capture/engine.rs`, `suspension`.
