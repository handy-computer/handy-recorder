use std::fmt;
use std::time::Duration;

use crate::InputDevice;

/// Follows `std::io::Error`: match on [`Error::kind`] to decide what to do,
/// and log the whole value (`Display`) to explain it, e.g.
/// "AirPods Pro (CoreAudio) disconnected 12.4 s into the stream:
/// kAudioHardwareBadDeviceError".
#[derive(Debug, Clone)]
pub struct Error(Box<Inner>);

#[derive(Debug, Clone)]
struct Inner {
    kind: ErrorKind,
    device: Option<InputDevice>,
    elapsed: Option<Duration>,
    detail: Option<String>,
}

impl Error {
    /// What happened. The part applications match on.
    pub fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    /// The device involved, if one had been resolved.
    pub fn device(&self) -> Option<&InputDevice> {
        self.0.device.as_ref()
    }

    /// How long the stream had been running when this happened. `None` for
    /// failures before the stream started (during `open`) and for errors
    /// that are not about the stream (`AlreadyRecording`, `NotRecording`).
    pub fn elapsed(&self) -> Option<Duration> {
        self.0.elapsed
    }

    /// The platform's own message, verbatim (a CoreAudio status, a WASAPI
    /// HRESULT, an ALSA error), or which requested setting was unsupported.
    pub fn detail(&self) -> Option<&str> {
        self.0.detail.as_deref()
    }

    pub(crate) fn new(kind: ErrorKind) -> Self {
        Self(Box::new(Inner {
            kind,
            device: None,
            elapsed: None,
            detail: None,
        }))
    }

    pub(crate) fn with_device(mut self, device: InputDevice) -> Self {
        self.0.device = Some(device);
        self
    }

    pub(crate) fn with_elapsed(mut self, elapsed: Duration) -> Self {
        self.0.elapsed = Some(elapsed);
        self
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.0.detail = Some(detail.into());
        self
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Name the device first when the error is about it. Sink, library,
        // and misuse errors are not, so they read without it.
        let device = self
            .0
            .device
            .as_ref()
            .filter(|_| self.0.kind.is_about_device());
        if let Some(device) = device {
            write!(f, "{} ({}) ", device.name, device.backend)?;
        }
        f.write_str(self.0.kind.describe(device.is_some()))?;
        if let Some(elapsed) = self.0.elapsed {
            write!(f, " {:.1} s into the stream", elapsed.as_secs_f64())?;
        }
        if let Some(detail) = &self.0.detail {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

/// The types of errors the library will deliver
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ErrorKind {
    // From `open`. 
    /// No such device, or no input device at all.
    DeviceUnavailable,
    /// Another application has exclusive use of the device.
    DeviceBusy,
    /// Permission denied to record audio devices, given by the OS
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

impl ErrorKind {
    fn is_about_device(self) -> bool {
        !matches!(
            self,
            Self::SinkStalled | Self::Processing | Self::AlreadyRecording | Self::NotRecording
        )
    }

    /// The phrase `Display` uses, written to follow the device's name when
    /// there is one ("AirPods Pro (CoreAudio) disconnected").
    fn describe(self, after_device: bool) -> &'static str {
        match (self, after_device) {
            (Self::DeviceUnavailable, true) => "is not available",
            (Self::DeviceUnavailable, false) => "no input device is available",
            (Self::DeviceBusy, true) => "is in use by another application",
            (Self::DeviceBusy, false) => "the input device is in use by another application",
            (Self::PermissionDenied, true) => "access was denied",
            (Self::PermissionDenied, false) => "microphone access was denied",
            (Self::UnsupportedFormat, true) => "cannot deliver the requested format",
            (Self::UnsupportedFormat, false) => "the requested format is not supported",
            (Self::InvalidChannel, true) => "has no such channel",
            (Self::InvalidChannel, false) => "the requested channel does not exist",
            (Self::OpenTimedOut, true) => "did not finish opening in time",
            (Self::OpenTimedOut, false) => "the input device did not finish opening in time",
            (Self::NoAudio, true) => "never delivered audio",
            (Self::NoAudio, false) => "the input device never delivered audio",
            (Self::DeviceLost, true) => "disconnected",
            (Self::DeviceLost, false) => "the input device disconnected",
            (Self::StreamInvalidated, true) => "stream was invalidated",
            (Self::StreamInvalidated, false) => "the input stream was invalidated",
            (Self::Stalled, true) => "stopped delivering audio",
            (Self::Stalled, false) => "the input device stopped delivering audio",
            (Self::SinkStalled, _) => "the sink stopped returning",
            (Self::Backend, true) => "reported an error",
            (Self::Backend, false) => "the audio backend reported an error",
            (Self::Processing, _) => "audio processing failed (a bug in handy-recorder)",
            (Self::AlreadyRecording, _) => "a recording is already active",
            (Self::NotRecording, _) => "there is no recording to stop",
            (Self::CloseTimedOut, true) => "did not finish closing in time",
            (Self::CloseTimedOut, false) => "the input device did not finish closing in time",
        }
    }
}
