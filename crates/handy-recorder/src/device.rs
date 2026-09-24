use crate::Error;

/// Lists input devices.
///
/// Callable at any time, from any thread, with or without a recorder open.
pub fn list_input_devices() -> Result<Vec<InputDevice>, Error> {
    todo!()
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
    todo!()
}
