use crate::backend::{Backend, cpal::CpalBackend};
use crate::{Error, ErrorKind};

/// Lists input devices.
///
/// Callable at any time, from any thread, with or without a recorder open.
pub fn list_input_devices() -> Result<Vec<InputDevice>, Error> {
    CpalBackend::shared()
        .list_input_devices()
        .map_err(|e| Error::new(ErrorKind::Backend).with_detail(e.message.into_owned()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
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
    // TODO(review): see TODO.md, "InputDevice::channels".
    pub channels: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Granted,
    Denied,
    NotDetermined,
    /// The platform has no meaningful proactive answer (Windows, Linux).
    /// Denial surfaces as `Error::PermissionDenied` from `open`.
    Unknown,
}

/// Microphone permission status. A synchronous read; never prompts.
pub fn permission_status() -> Permission {
    platform_permission_status()
}

#[cfg(target_os = "macos")]
pub(crate) fn platform_permission_status() -> Permission {
    use objc2_av_foundation::{AVAuthorizationStatus, AVCaptureDevice, AVMediaTypeAudio};

    // SAFETY: AVMediaTypeAudio is an immutable framework constant.
    let Some(audio) = (unsafe { AVMediaTypeAudio }) else {
        return Permission::Unknown;
    };
    // SAFETY: audio is a valid media type (video or audio are the only ones
    // that do not raise).
    let status = unsafe { AVCaptureDevice::authorizationStatusForMediaType(audio) };
    match status {
        AVAuthorizationStatus::Authorized => Permission::Granted,
        // Restricted: blocked by policy (parental controls, MDM); the user
        // cannot grant it either.
        AVAuthorizationStatus::Denied | AVAuthorizationStatus::Restricted => Permission::Denied,
        AVAuthorizationStatus::NotDetermined => Permission::NotDetermined,
        _ => Permission::Unknown,
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn platform_permission_status() -> Permission {
    Permission::Unknown
}
