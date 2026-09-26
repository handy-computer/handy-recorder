use crate::Error;
use crate::backend::{Backend, cpal::CpalBackend};
use crate::capture::engine::open_error;

/// Lists input devices.
///
/// Callable at any time, from any thread, with or without a recorder open.
pub fn list_input_devices() -> Result<Vec<InputDevice>, Error> {
    CpalBackend::shared()
        .list_input_devices()
        .map_err(|e| open_error(e, None))
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct InputDevice {
    /// Pass as `RecorderConfig::device` to open this device. Always present:
    /// the backend's own ID where it has one, otherwise one built from the
    /// name and occurrence.
    pub id: String,
    pub name: String,
    /// Distinguishes devices with the same name: 0 for the first, 1 for the
    /// second, and so on.
    pub occurrence: u32,
    pub backend: String,
    pub is_default: bool,
    /// Whether `id` still names this device after a restart or replug.
    /// False where the backend has no stable IDs and `id` is built from the
    /// name and occurrence, which is best effort.
    pub id_is_stable: bool,
    /// Channels the device captures at the format the OS has it set to, for
    /// a channel picker (`Channels::Only`). `None` where reading it would
    /// open the device (ALSA) or the platform did not say.
    pub channels: Option<u16>,
    /// Records what another device plays rather than a microphone: a
    /// PulseAudio (or pipewire-pulse) monitor source, "Monitor of ...". For
    /// an application to hide, or to offer as system audio. False on macOS
    /// and Windows, where loopback devices (BlackHole, Stereo Mix) look like
    /// any other input.
    pub is_monitor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Permission {
    Granted,
    Denied,
    NotDetermined,
    /// The platform has no meaningful proactive answer (Linux, Windows when
    /// its privacy settings cannot be read). Denial surfaces as
    /// `Error::PermissionDenied` from `open`.
    Unknown,
}

/// Microphone permission status. A synchronous read; never prompts.
pub fn permission_status() -> Permission {
    crate::backend::permission::permission_status()
}
