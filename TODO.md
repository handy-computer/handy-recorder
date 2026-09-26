# Review list

Open work, open questions, and decisions kept for reference. Code
locations carry a `TODO(review)` marker naming the entry here.

Two pre-release items live in DESIGN.md, "Before the first release": the
Rubato 5.x upgrade and the review of every internal timeout (marked
`REVIEW(timeouts)` in the code). The current values look reasonable;
drop the "placeholder" wording once Windows and Linux do not contradict
them.

## Where we are (for a fresh session)

Branch `extract-from-handy`. Phases 1 and 2 of DESIGN.md's plan are done
except the Handy branch. Hardware-tested on macOS (every probe), Windows
(every probe but Bluetooth and exclusive mode), and Linux (PipeWire,
every probe but Bluetooth; plain PulseAudio in a container and in CI; raw
ALSA); see `tools/probe/README.md`. CPAL is a git dependency: v0.18.2 plus
one PulseAudio fix, commit `053b6f6` of branch
`pulseaudio-record-max-length` of github.com/handy-computer/cpal
(`crates/handy-recorder/Cargo.toml`). DESIGN.md is not committed.

## Work

4. A review of the engine code (`capture/engine.rs`,
   `capture/delivery.rs`): the concurrency there has only its own tests
   and one hardware session behind it.
6. A README, including the macOS < 14.2 weak-link flag
   (`-C link-arg=-Wl,-weak_framework,CoreAudio`) DESIGN.md asks for.
8. Run `format-change` on macOS and Windows (written; Linux does not
   apply). CPAL reports a CoreAudio rate change as `StreamInvalidated`, so
   macOS should PASS with no library change. Windows likely fails the
   stream with the unplug code, which the library reports as `DeviceLost`;
   if so, report `StreamInvalidated` when the device still exists.

### Device enumeration on every open

`open` lists input devices to find a device by ID, and, for the default
device, to report which device it opened. Measured on macOS: about 1.4
ms warm, 99 ms for the first enumeration in a process (CoreAudio warming
up). Windows: 2.5-7 ms, the first open included, with two devices (the
`resolve_device=` debug line). Linux (Fedora, PipeWire 1.4): about 12 ms
(6-20) on PulseAudio with 6 inputs, connecting to the server included,
of a 43 ms open; about 17 ms (11-26) on ALSA with 13 inputs, of a 33 ms
open. `list_input_devices` takes about 10 ms on either. Slower than
macOS and Windows, but once per open the cost does not matter. There is
no constant-time lookup through CPAL 0.18: `device_by_id` enumerates on
every host. The platforms can look up directly (CoreAudio translates a
UID to a device; WASAPI `IMMDeviceEnumerator::GetDevice`; PulseAudio
source info by name), so an upstream CPAL change could add it. IDs
synthesized as `name#occurrence` (the platform gave none) can only be
found by enumerating.

Location: `backend/cpal/mod.rs`, `open_device`.
