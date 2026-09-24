//! The recorder over the backend seam, with the fake backend: device
//! resolution, open failures, stream errors, and stream ownership.

use std::sync::Arc;

use super::AudioRecorder;
use crate::backend::{
    BackendError, BackendErrorKind, DeviceFormat, SampleFormat, fake::FakeBackend,
};

fn fake() -> FakeBackend {
    FakeBackend::new(DeviceFormat {
        sample_rate: 48_000,
        channels: 2,
        sample_format: SampleFormat::I16,
    })
}

fn recorder(fake: &FakeBackend) -> AudioRecorder {
    AudioRecorder::with_backend(Arc::new(fake.clone()))
}

#[test]
fn open_starts_one_stream_and_close_releases_it() {
    let fake = fake();
    let mut recorder = recorder(&fake);

    recorder.open(None).expect("open default");
    assert!(fake.is_streaming());
    assert_eq!(recorder.device.as_ref().unwrap().name, "Fake Mic");

    // Opening an open, healthy recorder is a no-op, as in Handy.
    recorder.open(None).expect("reopen");
    assert_eq!(fake.streams_started(), 1);

    recorder.close().expect("close");
    assert!(!fake.is_streaming());
    assert!(!fake.push(&[0i16, 0]), "no stream after close");
}

#[test]
fn open_by_id_and_unknown_id() {
    let fake = fake();
    let mut recorder = recorder(&fake);
    recorder.open(Some("fake:mic")).expect("open by id");
    recorder.close().unwrap();

    let error = recorder.open(Some("fake:nope")).unwrap_err();
    assert!(error.to_string().contains("fake:nope"), "{error}");
    assert!(!fake.is_streaming());
}

#[test]
fn open_failure_keeps_the_platform_message_and_permission_kind() {
    let fake = fake();
    let mut recorder = recorder(&fake);
    fake.fail_next_open(BackendError::new(
        BackendErrorKind::PermissionDenied,
        "WASAPI error: 0x80070005",
    ));

    let error = recorder.open(None).unwrap_err();
    let io = error.downcast_ref::<std::io::Error>().expect("io::Error");
    assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
    assert_eq!(io.to_string(), "WASAPI error: 0x80070005");
    assert!(!fake.is_streaming());

    // The failure does not stick.
    recorder.open(None).expect("open after a failed open");
}

#[test]
fn permission_denied_is_recognized_by_kind_as_well_as_message() {
    let fake = fake();
    let mut recorder = recorder(&fake);
    // CoreAudio's message for a denied microphone matches none of Handy's
    // patterns.
    fake.fail_next_open(BackendError::new(
        BackendErrorKind::PermissionDenied,
        "Failed to build input stream: Unauthorized",
    ));

    let error = recorder.open(None).unwrap_err();
    let io = error.downcast_ref::<std::io::Error>().expect("io::Error");
    assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn stream_error_marks_the_stream_for_rebuild_and_reopen_replaces_it() {
    let fake = fake();
    let mut recorder = recorder(&fake);
    recorder.open(None).unwrap();
    assert!(!recorder.needs_reopen());

    assert!(fake.report_error(BackendError::new(
        BackendErrorKind::DeviceNotAvailable,
        "device unplugged",
    )));
    assert!(recorder.needs_reopen());

    // Handy's recovery model: the next open tears down and rebuilds.
    recorder.open(None).expect("rebuild");
    assert_eq!(fake.streams_started(), 2);
    assert!(fake.is_streaming());
    assert!(!recorder.needs_reopen());
    recorder.close().unwrap();
}
