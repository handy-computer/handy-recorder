use crate::{Error, InputDevice};

/// How to open a recorder. Every field is optional; `RecorderConfig::default()`
/// opens the system default device and delivers its audio unchanged.
///
/// The device always runs at the format the OS has it set to. These fields
/// describe what the sink receives; the library converts (channel routing,
/// resampling, chunking) and never changes anything about the device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecorderConfig {
    /// An `InputDevice::id` from `list_input_devices`. `None` opens the
    /// system default device.
    pub device: Option<String>,
    /// The rate the sink receives. `None` keeps the device's rate, with no
    /// resampling.
    pub sample_rate: Option<u32>,
    /// Which channels the sink receives. Default: all of them.
    pub channels: Channels,
    /// How many frames each `process_chunk` call receives. A frame is one
    /// sample per channel, so at 16 kHz mono, 480 frames is 30 ms. `None`
    /// delivers about 10 ms per call. Set it only if the sink needs a fixed
    /// size, such as a VAD model that takes exactly 512 samples.
    pub frames_per_chunk: Option<usize>,
}

impl RecorderConfig {
    /// Speech capture: 16 kHz, mixed to mono, 480-frame (30 ms) chunks, from
    /// the system default device.
    pub fn speech() -> Self {
        todo!()
    }
}

/// Which channels the sink receives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Channels {
    /// All device channels, interleaved.
    #[default]
    All,
    /// The arithmetic average of all device channels. Can attenuate devices
    /// with unused channels or cancel opposite-polarity channels.
    MixToMono,
    /// One zero-based device channel. Out of range is
    /// `ErrorKind::InvalidChannel` from `open`.
    Only(u16),
}

/// A sample rate and channel count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
}

/// What a recorder opened. Useful for display and logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderInfo {
    /// The device that is recording, including when `RecorderConfig::device`
    /// was `None`.
    pub device: InputDevice,
    /// What the device runs at: whatever the OS has it set to.
    pub device_format: Format,
    /// What the sink receives.
    pub format: Format,
    /// Frames in every chunk handed to the sink: the requested
    /// `frames_per_chunk`, or the resolved ~10 ms default.
    pub frames_per_chunk: usize,
}

/// What ended a recording. The first failure wins.
#[derive(Debug, Clone)]
pub enum EndReason {
    /// The application called `stop`; nothing ended the recording earlier.
    /// Says nothing about dropped audio: `Stopped::is_complete` checks both.
    StopCalled,
    /// The recorder failed during the recording. Carries the same error the
    /// failure handler received and `start` returns from then on. The audio up
    /// to the failure was delivered.
    RecorderFailed(Error),
    /// The sink panicked; carries the panic message. The recorder is fine.
    SinkPanicked(String),
}
