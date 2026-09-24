use std::fmt;
use std::time::Duration;

use crate::InputDevice;

/// Everything that can go wrong, with enough context for a log line or a bug
/// report to say exactly what happened.
///
/// Follows `std::io::Error`: match on [`Error::kind`] to decide what to do,
/// and log the whole value (`Display`) to explain it, e.g.
/// "AirPods Pro (CoreAudio) disconnected 12.4 s into the stream:
/// kAudioHardwareBadDeviceError".
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    device: Option<InputDevice>,
    elapsed: Option<Duration>,
    detail: Option<String>,
}

impl Error {
    /// What happened. The part applications match on.
    pub fn kind(&self) -> ErrorKind {
        todo!()
    }

    /// The device involved, if one had been resolved.
    pub fn device(&self) -> Option<&InputDevice> {
        todo!()
    }

    /// How long the stream had been running when this happened. `None` for
    /// failures before the stream started (during `open`) and for errors
    /// that are not about the stream (`AlreadyRecording`, `NotRecording`).
    pub fn elapsed(&self) -> Option<Duration> {
        todo!()
    }

    /// The platform's own message, verbatim (a CoreAudio status, a WASAPI
    /// HRESULT, an ALSA error), or which requested setting was unsupported.
    pub fn detail(&self) -> Option<&str> {
        todo!()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        todo!()
    }
}

impl std::error::Error for Error {}

/// What happened, for application logic.
///
/// Expected failures (a device disappearing, permission denied) and library
/// or application bugs (`Processing`, `SinkStalled`) are separate kinds, so
/// one is never mistaken for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    // From `open`.
    /// No such device, or no input device at all.
    DeviceUnavailable,
    /// Another application has exclusive use of the device.
    DeviceBusy,
    PermissionDenied,
    /// A `sample_rate` or `frames_per_chunk` the library cannot produce.
    UnsupportedFormat,
    /// `Channels::Only(n)` with `n` out of range for the device.
    InvalidChannel,
    /// The platform did not finish opening the device in time.
    OpenTimedOut,

    // A recorder failing. The recorder stays failed; open a new one.
    /// The device opened but never delivered audio.
    NoAudio,
    /// The device disappeared: unplugged, Bluetooth dropped.
    DeviceLost,
    /// The stream can no longer run as built even though the device may
    /// still exist: its configuration changed, or the sound server restarted.
    StreamInvalidated,
    /// The device was delivering audio, then stopped without reporting an
    /// error.
    Stalled,
    /// The sink stopped returning. A bug in the application's sink; the sink
    /// is lost.
    SinkStalled,
    /// Any other platform error; `detail` has the platform's message.
    Backend,
    /// A resampling or framing failure. A bug in this library.
    Processing,

    // Misuse and lifecycle.
    /// `start` before the previous recording was stopped, including one that
    /// ended on its own and is waiting for its `stop`.
    AlreadyRecording,
    /// `stop` with no recording to stop.
    NotRecording,
    /// The platform did not finish tearing the stream down in time.
    CloseTimedOut,
}
