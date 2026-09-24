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
except the Handy branch. Hardware-tested on macOS only (every macOS probe
has run; see `tools/probe/README.md`). DESIGN.md is not committed.

## Work

1. Windows and Linux sessions with the probe checklist
   (`tools/probe/README.md`), plus the tier-2 virtual-device tests
   (PulseAudio and pipewire-pulse null sources; need Linux or CI). Linux
   `audio-service-restart` is expected to fail its reopen until
   "PulseAudio server restarts" is fixed.
2. Open the default device by its resolved ID (below, "Default device
   changing mid-recording"), and add a default-change check to the
   Windows session.
3. Fix "PulseAudio server restarts" (below).
4. A review of the engine code (`capture/engine.rs`,
   `capture/delivery.rs`): the concurrency there has only its own tests
   and one hardware session behind it.
5. The phase-2 exit criterion: a Handy branch using this crate as a path
   dependency, with Handy's sink (VAD, collection, streaming forward)
   passing Handy's audio tests. It is in another repository.
6. A README, including the macOS < 14.2 weak-link flag
   (`-C link-arg=-Wl,-weak_framework,CoreAudio`) DESIGN.md asks for.
7. The permission-request prototype (provisional in DESIGN.md): a bundled
   macOS app, a terminal-hosted Node process, and an unbundled CLI.
8. `list_input_devices` filters out PulseAudio "Monitor of ..." sources
   (decided; not implemented).
9. Before the first release: DESIGN.md's items, then phase 3 (Node
   binding, `pi-transcribe`).

## Open questions

### Default device changing mid-recording

What happens to a recorder opened on the default device when the user
switches the default input:

- macOS (CoreAudio): the recorder stays on its device, no error
  (confirmed on hardware). CPAL reports `DeviceChanged` for default
  output only.
- Windows (WASAPI): the stream fails with `StreamInvalidated`, so the
  recorder fails. CPAL watches default changes on streams opened from
  `default_input_device()` and reports them this way because WASAPI
  cannot reroute a stream. Not tested on hardware.
- Linux, PulseAudio: expected to stay on its device (CPAL resolves the
  default to a named source when it opens). Not tested.
- Linux, ALSA: stays on the PCM opened. Not tested.
- iOS and PipeWire (not enabled) emit `DeviceChanged`, which the library
  treats as a recorder failure.

Plan: open the default by its resolved ID, so Windows behaves like
macOS. Whether a reroute should ever be survivable only matters if
PipeWire or iOS is enabled.

Location: `backend/mod.rs`, `BackendErrorKind::DeviceChanged`;
`backend/cpal.rs`, `open_device`.

### Device enumeration on every open

`open` lists input devices to find a device by ID, and, for the default
device, to report which device it opened. Measured on macOS: about
1.4 ms warm, 99 ms for the first enumeration in a process (CoreAudio
warming up). Not measured on Windows or Linux. There is no constant-time
lookup through CPAL 0.18: `device_by_id` enumerates on every host. The
platforms can look up directly (CoreAudio translates a UID to a device;
WASAPI `IMMDeviceEnumerator::GetDevice`; PulseAudio source info by
name), so an upstream CPAL change could add it. IDs synthesized as
`name#occurrence` (the platform gave none) can only be found by
enumerating. Decide whether the cost matters after the Windows and Linux
measurements.

Location: `backend/cpal.rs`, `open_device`.

### PulseAudio server restarts

The CPAL backend keeps one `cpal::Host` for the process (the PulseAudio
host holds a server connection; DESIGN.md asks for one host per process).
If the sound server restarts, that connection is dead and every later
`list_input_devices` and `open` fails until the process restarts. The
backend should recreate the host when an operation fails with a
host-unavailable error.

Location: `backend/cpal.rs`, `CpalBackend::shared`.
