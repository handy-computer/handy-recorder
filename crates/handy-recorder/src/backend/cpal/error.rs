//! Turns cpal's errors into `BackendError`s. The one place that reads
//! platform output: where cpal's kind is missing or too coarse, it is
//! corrected here from the message (WASAPI HRESULTs, CoreAudio and ALSA
//! text). The engine maps kinds only and never reads messages.

use crate::backend::{BackendError, BackendErrorKind};

/// Maps an error from a step of opening the device. The step prefixes the
/// message.
pub(super) fn map_error_during(step: &str, e: cpal::Error) -> BackendError {
    classify(map_kind(e.kind()), format!("{step}: {e}"))
}

/// Maps an error from enumeration or a running stream.
pub(super) fn map_error(e: cpal::Error) -> BackendError {
    let kind = map_kind(e.kind());
    if kind.stream_survives() {
        // May be reported repeatedly on the audio thread (xruns under load),
        // so it carries a fixed description instead of an allocated message.
        let message = match kind {
            BackendErrorKind::Xrun => "A buffer overrun or underrun occurred",
            _ => "Real-time scheduling was refused for the audio thread",
        };
        return BackendError::new(kind, message);
    }
    classify(kind, e.to_string())
}

/// Corrects cpal's kind from the message, then names any WASAPI code in it.
fn classify(kind: BackendErrorKind, message: String) -> BackendError {
    let kind =
        if kind == BackendErrorKind::PermissionDenied || is_microphone_access_denied(&message) {
            BackendErrorKind::PermissionDenied
        } else if is_no_input_device_error(&message) {
            BackendErrorKind::DeviceNotAvailable
        } else if kind == BackendErrorKind::Other
            && matches!(
                hresult(&message),
                Some(ERROR_NOT_FOUND | AUDCLNT_E_SERVICE_NOT_RUNNING)
            )
        {
            // cpal leaves these unclassified. Reported by a running stream, they
            // mean the Windows Audio service went away (measured: restarting it
            // fails the stream with ERROR_NOT_FOUND); the device may still be
            // there. At open the kind makes no difference: the engine reports
            // anything but denial, absence, and busy as `Backend`.
            BackendErrorKind::StreamInvalidated
        } else {
            kind
        };
    BackendError::new(kind, name_hresult(message))
}

/// Denial that cpal leaves unclassified, as WASAPI reports it at open
/// (E_ACCESSDENIED). A running WASAPI stream is failed with the unplug code
/// instead, which the engine tells apart through the privacy settings.
fn is_microphone_access_denied(message: &str) -> bool {
    let normalized = message.to_lowercase();
    normalized.contains("access is denied")
        || normalized.contains("permission denied")
        || normalized.contains("0x80070005")
        // E_ACCESSDENIED as cpal's WASAPI host formats it (io::Error, decimal
        // HRESULT). Unlike the text above, this does not depend on the
        // Windows display language.
        || normalized.contains("os error -2147024891")
}

/// No device to open: none at all, or CoreAudio failing to read the format
/// of a default that has gone.
fn is_no_input_device_error(message: &str) -> bool {
    let normalized = message.to_lowercase();
    normalized.contains("no input device found")
        || (normalized.contains("failed to fetch preferred config")
            && normalized.contains("coreaudio"))
}

const AUDCLNT_E_DEVICE_INVALIDATED: u32 = 0x8889_0004;
const AUDCLNT_E_DEVICE_IN_USE: u32 = 0x8889_000A;
const AUDCLNT_E_SERVICE_NOT_RUNNING: u32 = 0x8889_0010;
const AUDCLNT_E_RESOURCES_INVALIDATED: u32 = 0x8889_0026;
/// HRESULT_FROM_WIN32(ERROR_NOT_FOUND), "Element not found."
const ERROR_NOT_FOUND: u32 = 0x8007_0490;

/// The HRESULT in a WASAPI error message. cpal formats Windows errors as an
/// `io::Error`: the system's (localized) text, then "(os error <decimal>)".
fn hresult(message: &str) -> Option<u32> {
    let (_, rest) = message.rsplit_once("(os error ")?;
    let code: i32 = rest.strip_suffix(')')?.parse().ok()?;
    // HRESULTs are negative as i32 (the failure bit); plain errno values on
    // other platforms are not.
    (code < 0).then_some(code as u32)
}

/// Names the WASAPI codes the library has met, whose system text is missing
/// ("FormatMessageW() returned error 317") or depends on the display
/// language. Any other message is returned unchanged.
fn name_hresult(message: String) -> String {
    let Some(code) = hresult(&message) else {
        return message;
    };
    let (name, meaning) = match code {
        AUDCLNT_E_DEVICE_INVALIDATED => (
            "AUDCLNT_E_DEVICE_INVALIDATED",
            "the device was removed or disabled",
        ),
        AUDCLNT_E_DEVICE_IN_USE => (
            "AUDCLNT_E_DEVICE_IN_USE",
            "another application has exclusive use of the device",
        ),
        AUDCLNT_E_SERVICE_NOT_RUNNING => (
            "AUDCLNT_E_SERVICE_NOT_RUNNING",
            "the Windows Audio service is not running",
        ),
        AUDCLNT_E_RESOURCES_INVALIDATED => (
            "AUDCLNT_E_RESOURCES_INVALIDATED",
            "the stream's resources were invalidated",
        ),
        ERROR_NOT_FOUND => (
            "ERROR_NOT_FOUND",
            "the audio endpoint is gone (the Windows Audio service restarted?)",
        ),
        _ => return message,
    };
    let named = format!("{meaning} ({name}, 0x{code:08X})");
    match message.find("OS Error ") {
        // No system text: the name replaces the placeholder.
        Some(start) if message.contains("FormatMessageW()") => {
            format!("{}{named}", &message[..start])
        }
        _ => format!("{message}: {named}"),
    }
}

fn map_kind(kind: cpal::ErrorKind) -> BackendErrorKind {
    match kind {
        cpal::ErrorKind::DeviceNotAvailable => BackendErrorKind::DeviceNotAvailable,
        cpal::ErrorKind::DeviceBusy => BackendErrorKind::DeviceBusy,
        cpal::ErrorKind::PermissionDenied => BackendErrorKind::PermissionDenied,
        cpal::ErrorKind::UnsupportedConfig => BackendErrorKind::UnsupportedConfig,
        cpal::ErrorKind::StreamInvalidated => BackendErrorKind::StreamInvalidated,
        cpal::ErrorKind::DeviceChanged => BackendErrorKind::DeviceChanged,
        cpal::ErrorKind::Xrun => BackendErrorKind::Xrun,
        cpal::ErrorKind::RealtimeDenied => BackendErrorKind::RealtimeDenied,
        _ => BackendErrorKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::*;

    #[test]
    fn errors_the_stream_survives_are_classified_without_allocating() {
        for (cpal_kind, kind) in [
            (cpal::ErrorKind::Xrun, BackendErrorKind::Xrun),
            (
                cpal::ErrorKind::RealtimeDenied,
                BackendErrorKind::RealtimeDenied,
            ),
        ] {
            let error = map_error(cpal::Error::with_message(cpal_kind, "detail"));
            assert_eq!(error.kind, kind);
            assert!(error.kind.stream_survives());
            assert!(matches!(error.message, Cow::Borrowed(_)));
        }
    }

    /// Messages as cpal's WASAPI host formats them, seen on hardware.
    #[test]
    fn wasapi_codes_are_named_and_a_service_restart_invalidates_the_stream() {
        let unplugged = map_error(cpal::Error::with_message(
            cpal::ErrorKind::DeviceNotAvailable,
            "OS Error -2004287484 (FormatMessageW() returned error 317) (os error -2004287484)",
        ));
        assert_eq!(unplugged.kind, BackendErrorKind::DeviceNotAvailable);
        assert_eq!(
            unplugged.message,
            "the device was removed or disabled (AUDCLNT_E_DEVICE_INVALIDATED, 0x88890004)"
        );

        let restarted = map_error(cpal::Error::with_message(
            cpal::ErrorKind::BackendError,
            "Element not found. (os error -2147023728)",
        ));
        assert_eq!(restarted.kind, BackendErrorKind::StreamInvalidated);
        assert!(
            restarted
                .message
                .starts_with("Element not found. (os error -2147023728): the audio endpoint")
        );
        assert!(restarted.message.ends_with("(ERROR_NOT_FOUND, 0x80070490)"));

        // Unknown codes, and errno values elsewhere, are left alone.
        for message in [
            "Something. (os error -2147467259)",
            "No such file or directory (os error 2)",
        ] {
            let error = map_error(cpal::Error::with_message(
                cpal::ErrorKind::BackendError,
                message,
            ));
            assert_eq!(error.kind, BackendErrorKind::Other);
            assert_eq!(error.message, message);
        }

        // At open, the step prefix stays and denial is still recognized.
        let denied = map_error_during(
            "Failed to build input stream",
            cpal::Error::with_message(
                cpal::ErrorKind::PermissionDenied,
                "Access is denied. (os error -2147024891)",
            ),
        );
        assert_eq!(
            denied.message,
            "Failed to build input stream: Access is denied. (os error -2147024891)"
        );
    }

    #[test]
    fn denial_is_recognized_from_the_message() {
        for message in [
            "Access is denied",
            "permission denied",
            "WASAPI error: 0x80070005",
            "Access is denied. (os error -2147024891)",
        ] {
            let at_open = map_error_during(
                "Failed to build input stream",
                cpal::Error::with_message(cpal::ErrorKind::BackendError, message),
            );
            assert_eq!(
                at_open.kind,
                BackendErrorKind::PermissionDenied,
                "{message}"
            );
            // A running stream is classified the same way. WASAPI does not
            // report denial like this there (see `is_microphone_access_denied`),
            // so in practice this changes nothing.
            let running = map_error(cpal::Error::with_message(
                cpal::ErrorKind::BackendError,
                message,
            ));
            assert_eq!(
                running.kind,
                BackendErrorKind::PermissionDenied,
                "{message}"
            );
        }
        assert!(!is_microphone_access_denied("device not found"));
    }

    #[test]
    fn a_missing_device_is_recognized_from_the_message() {
        assert!(is_no_input_device_error("No input device found"));
        let coreaudio = map_error_during(
            "Failed to fetch preferred config",
            cpal::Error::with_message(
                cpal::ErrorKind::BackendError,
                "A backend-specific error has occurred: An unknown error unknown to the coreaudio-rs API occurred",
            ),
        );
        assert_eq!(coreaudio.kind, BackendErrorKind::DeviceNotAvailable);
        assert!(!is_no_input_device_error("permission denied"));
        assert!(!is_no_input_device_error("device not found"));

        // Denial reported by cpal wins over the message.
        let denied = map_error_during(
            "Failed to build input stream",
            cpal::Error::with_message(cpal::ErrorKind::PermissionDenied, "No input device found"),
        );
        assert_eq!(denied.kind, BackendErrorKind::PermissionDenied);
    }
}
