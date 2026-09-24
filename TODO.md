# Review list

Decisions taken during implementation that need a closer look, and work
deliberately left for later. Code locations carry a `TODO(review)` marker
naming the entry here.

Two pre-release items live in DESIGN.md, "Before the first release": the
Rubato 5.x upgrade and the review of every internal timeout (marked
`REVIEW(timeouts)` in the code).

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
