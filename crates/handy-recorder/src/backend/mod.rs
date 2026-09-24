//! The seam between the capture engine and the platform.
//!
//! All platform access goes through [`Backend`]: enumerate devices, open one,
//! build and start its input stream, and report stream errors. No CPAL type
//! crosses it, so the engine and its tests run against [`fake`] exactly as
//! they run against [`cpal`](self::cpal), and a backend can change (another
//! CPAL version on one target, a different macOS backend) in one file.

pub(crate) mod cpal;
#[cfg(test)]
pub(crate) mod fake;
#[cfg(target_os = "macos")]
mod headset_macos;

use std::{borrow::Cow, fmt};

use dasp_sample::{I24, Sample, U24};

use crate::{InputDevice, Permission};

pub(crate) trait Backend: Send + Sync + 'static {
    fn list_input_devices(&self) -> Result<Vec<InputDevice>, BackendError>;

    /// Resolves `id` (an `InputDevice::id`), or the system default for
    /// `None`, and reads the format the OS has it set to. Opens no stream.
    fn open_device(&self, id: Option<&str>) -> Result<Box<dyn OpenDevice>, BackendError>;

    /// Microphone permission, as `crate::permission_status` reports it. A
    /// synchronous read that never prompts.
    fn permission_status(&self) -> Permission;
}

/// A resolved device whose input stream has not been built yet.
pub(crate) trait OpenDevice {
    fn info(&self) -> &InputDevice;

    /// The format the stream will run at: whatever the OS has the device set
    /// to. The library never asks for a different one.
    fn format(&self) -> DeviceFormat;

    /// Builds the input stream and starts it. `data` runs on the platform's
    /// real-time thread; `error` may too. Dropping the returned stream stops
    /// and releases it.
    fn start(
        self: Box<Self>,
        data: DataCallback,
        error: ErrorCallback,
    ) -> Result<Box<dyn InputStream>, BackendError>;
}

/// A running input stream. Dropping it stops the stream and releases the
/// device. Not `Send`: it is created, owned, and dropped on one thread.
pub(crate) trait InputStream {
    /// Starts or stops holding the device's headset (`take_headset`). Best
    /// effort: a backend logs what it could not do. A no-op where it does not
    /// apply.
    fn hold_headset(&mut self, _hold: bool) {}
}

/// Receives each block of interleaved device samples. Called on the
/// real-time thread: must not allocate, lock, log, or block.
pub(crate) type DataCallback = Box<dyn FnMut(InputData<'_>) + Send + 'static>;

/// Receives stream errors. May be called on the real-time thread.
pub(crate) type ErrorCallback = Box<dyn FnMut(BackendError) + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeviceFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: SampleFormat,
}

/// Device sample formats the engine accepts, as Handy does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    U8,
    I8,
    U16,
    I16,
    U24,
    I24,
    U32,
    I32,
    U64,
    I64,
    F32,
    F64,
}

/// One callback's samples, in the device's format.
#[derive(Debug, Clone, Copy)]
pub(crate) enum InputData<'a> {
    U8(&'a [u8]),
    I8(&'a [i8]),
    U16(&'a [u16]),
    I16(&'a [i16]),
    U24(&'a [U24]),
    I24(&'a [I24]),
    U32(&'a [u32]),
    I32(&'a [i32]),
    U64(&'a [u64]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    F64(&'a [f64]),
}

/// A device sample type. The conversion to `f32` is `dasp_sample`'s, which is
/// what Handy used through `cpal::Sample`; the equivalence test holds it to
/// that bit for bit.
pub(crate) trait InputSample: Copy + Send + 'static {
    #[cfg_attr(not(test), allow(dead_code))]
    const FORMAT: SampleFormat;
    fn to_f32(self) -> f32;
    fn wrap(data: &[Self]) -> InputData<'_>;
}

macro_rules! input_sample {
    ($($t:ty => $variant:ident),*) => {$(
        impl InputSample for $t {
            const FORMAT: SampleFormat = SampleFormat::$variant;
            #[inline]
            fn to_f32(self) -> f32 {
                self.to_sample::<f32>()
            }
            fn wrap(data: &[Self]) -> InputData<'_> {
                InputData::$variant(data)
            }
        }
    )*};
}

input_sample!(
    u8 => U8, i8 => I8, u16 => U16, i16 => I16, U24 => U24, I24 => I24, u32 => U32,
    i32 => I32, u64 => U64, i64 => I64,
    f32 => F32, f64 => F64
);

/// A platform error, classified by the backend adapter. The message is the
/// platform's own, kept verbatim, except for errors the stream survives,
/// which carry a fixed description so reporting them never allocates on the
/// audio thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackendError {
    pub kind: BackendErrorKind,
    pub message: Cow<'static, str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendErrorKind {
    /// No such device, no default device, or the device disappeared.
    DeviceNotAvailable,
    DeviceBusy,
    PermissionDenied,
    /// The device's format cannot be captured (for example an unsupported
    /// sample format).
    UnsupportedConfig,
    /// The stream can no longer run as built.
    StreamInvalidated,
    /// The stream was rerouted to a new default device. The stream keeps
    /// running, but the library treats this as the end of the recorder: the
    /// device it reported opening is no longer the one recording, and the
    /// new one may run at a different format.
    // TODO(review): see TODO.md, "Default device changing mid-recording".
    DeviceChanged,
    /// A buffer overrun or underrun; the stream keeps running.
    Xrun,
    /// Real-time scheduling was refused; the stream keeps running.
    RealtimeDenied,
    /// Anything else.
    Other,
}

impl BackendErrorKind {
    /// Whether the recorder carries on after this error: the platform
    /// documents the stream as still running and nothing about it changed.
    pub fn stream_survives(self) -> bool {
        matches!(self, Self::Xrun | Self::RealtimeDenied)
    }
}

impl BackendError {
    pub fn new(kind: BackendErrorKind, message: impl Into<Cow<'static, str>>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BackendError {}
