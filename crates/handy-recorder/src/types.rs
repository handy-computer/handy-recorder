use crate::{Error, InputDevice};

/// Config to open a Recroder with.
/// Every field is optional; `RecorderConfig::default()` opens the system
/// default device and delivers its audio unchanged.
///
/// The device always runs at the format the OS has it set to. These fields
/// describe what the sink receives.
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
    /// When set to True the library will attempt to move Bluetooth devices
    /// (AirPods) over to the Recorder. This is MacOS only and a no-op on
    /// other platforms.
    pub take_headset: bool,
}

impl RecorderConfig {
    /// Common ASR speech capture configuration
    pub fn speech() -> Self {
        Self {
            device: None,
            sample_rate: Some(16_000),
            channels: Channels::MixToMono,
            frames_per_chunk: Some(480),
            take_headset: false,
        }
    }
}

/// Which channels the sink receives.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Channels {
    /// All device channels, interleaved.
    #[default]
    All,
    /// Average of all device channels.
    MixToMono,
    /// Integer (0 indexed) of which channel you want.
    Only(u16),
}

/// A sample rate and channel count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
}

/// What a recorder opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecorderInfo {
    /// The device used for recording
    pub device: InputDevice,
    /// What the device runs at
    pub device_format: Format,
    /// What the sink receives.
    pub format: Format,
    /// Frames in every chunk handed to the sink: the requested
    /// `frames_per_chunk`, or the resolved ~10 ms default.
    pub frames_per_chunk: usize,
}

/// What ended a recording.
#[derive(Debug, Clone)]
pub enum EndReason {
    /// The application called `stop`
    StopCalled,
    /// The recorder failed during the recording. Carries the same error the
    /// failure handler received and `start` returns from then on. The audio up
    /// to the failure was delivered.
    RecorderFailed(Error),
    /// The sink panicked; carries the panic message. The recorder is fine.
    SinkPanicked(String),
}
