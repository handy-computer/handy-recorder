//! Failures over the fake backend: the design's failure-outcomes table,
//! the watchdog, the failure handler, and bounded waits.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use super::support::{
    self, Chunks, fake, open, open_notified, open_with, push_idle, ramp, start, stop_with_boundary,
    timeouts,
};
use crate::backend::{BackendError, BackendErrorKind, fake::Gate};
use crate::{
    AudioChunk, CollectingSink, EndReason, Error, ErrorKind, Recorder, RecorderConfig, Sink,
};

fn passthrough() -> RecorderConfig {
    RecorderConfig {
        frames_per_chunk: Some(160),
        ..RecorderConfig::default()
    }
}

fn lost() -> BackendError {
    BackendError::new(
        BackendErrorKind::DeviceNotAvailable,
        "kAudioHardwareBadDeviceError",
    )
}

fn recorder_failed(reason: &EndReason) -> &Error {
    match reason {
        EndReason::RecorderFailed(error) => error,
        other => panic!("expected RecorderFailed, got {other:?}"),
    }
}

#[test]
fn device_loss_during_a_recording_keeps_the_audio_and_fails_the_recorder() {
    let fake = fake(16_000, 1);
    let (recorder, failures) = open_notified::<Chunks>(&fake, passthrough(), timeouts());
    start(&recorder, Chunks::default());
    let audio = ramp(0, 1000);
    assert!(fake.push(&audio));
    thread::sleep(Duration::from_millis(20));
    assert!(fake.report_error(lost()));

    // The handler fires at once, with the full context.
    let error = failures
        .recv_timeout(support::WAIT)
        .expect("failure handler");
    assert_eq!(error.kind(), ErrorKind::DeviceLost);
    assert_eq!(error.device().unwrap().name, "Fake Mic");
    assert!(error.elapsed().is_some());
    assert_eq!(error.detail(), Some("kAudioHardwareBadDeviceError"));
    let display = error.to_string();
    assert!(
        display.starts_with("Fake Mic (Fake) disconnected ")
            && display.ends_with(" s into the stream: kAudioHardwareBadDeviceError"),
        "{display}"
    );
    // The stale stream is torn down.
    support::wait_until("stream torn down", || !fake.is_streaming());

    // stop returns at once, with the audio captured before the failure.
    let started = Instant::now();
    let stopped = recorder.stop().expect("stop keeps the audio");
    assert!(started.elapsed() < timeouts().pause_ack);
    assert_eq!(
        recorder_failed(&stopped.end_reason).kind(),
        ErrorKind::DeviceLost
    );
    assert!(!stopped.is_complete());
    assert_eq!(stopped.sink.real(), audio);

    // A failed recorder stays failed, and start returns the sink.
    let again = recorder.start(Chunks::default()).unwrap_err();
    assert_eq!(again.error.kind(), ErrorKind::DeviceLost);
    assert!(failures.try_recv().is_err(), "the handler fires once");
}

#[test]
fn a_failure_while_idle_surfaces_at_the_next_start() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<CollectingSink> = open(&fake, passthrough());
    assert!(fake.report_error(BackendError::new(
        BackendErrorKind::StreamInvalidated,
        "AUDCLNT_E_RESOURCES_INVALIDATED"
    )));
    support::wait_until("stream torn down", || !fake.is_streaming());
    let error = recorder.start(CollectingSink::new()).unwrap_err();
    assert_eq!(error.error.kind(), ErrorKind::StreamInvalidated);
    assert_eq!(recorder.stop().unwrap_err().kind(), ErrorKind::NotRecording);
    recorder
        .close()
        .expect("close is valid on a failed recorder");
}

#[test]
fn a_default_device_reroute_fails_the_recorder() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<CollectingSink> = open(&fake, passthrough());
    assert!(fake.report_error(BackendError::new(
        BackendErrorKind::DeviceChanged,
        "default input device changed"
    )));
    support::wait_until("stream torn down", || !fake.is_streaming());
    let error = recorder.start(CollectingSink::new()).unwrap_err();
    assert_eq!(error.error.kind(), ErrorKind::StreamInvalidated);
}

#[test]
fn errors_the_stream_survives_are_counted_not_fatal() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<Chunks> = open(&fake, passthrough());
    start(&recorder, Chunks::default());
    for _ in 0..3 {
        assert!(fake.report_error(BackendError::new(BackendErrorKind::Xrun, "xrun")));
    }
    assert!(fake.report_error(BackendError::new(BackendErrorKind::RealtimeDenied, "rt")));
    assert!(fake.push(&ramp(0, 100)));
    let stopped = stop_with_boundary(&recorder, &fake, &[0.5f32]).unwrap();
    assert!(stopped.is_complete());
    assert!(fake.is_streaming());
    assert_eq!(
        recorder
            .engine
            .shared
            .survived_errors
            .load(std::sync::atomic::Ordering::Relaxed),
        4
    );
}

#[test]
fn a_device_that_never_delivers_audio_fails_with_no_audio() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.no_audio = Duration::from_millis(150);
    let (recorder, failures) = open_notified::<Chunks>(&fake, passthrough(), t);
    start(&recorder, Chunks::default());

    let error = failures
        .recv_timeout(support::WAIT)
        .expect("failure handler");
    assert_eq!(error.kind(), ErrorKind::NoAudio);
    assert!(!fake.is_streaming());
    let stopped = recorder.stop().expect("stop");
    assert_eq!(
        recorder_failed(&stopped.end_reason).kind(),
        ErrorKind::NoAudio
    );
    assert!(stopped.sink.chunks.is_empty());
}

#[test]
fn a_device_that_stops_calling_back_fails_with_stalled() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.stall = Duration::from_millis(150);
    let (recorder, failures) = open_notified::<Chunks>(&fake, passthrough(), t);
    start(&recorder, Chunks::default());
    assert!(fake.push(&ramp(0, 500)));

    let error = failures
        .recv_timeout(support::WAIT)
        .expect("failure handler");
    assert_eq!(error.kind(), ErrorKind::Stalled);
    let stopped = recorder.stop().expect("stop");
    assert_eq!(
        recorder_failed(&stopped.end_reason).kind(),
        ErrorKind::Stalled
    );
    assert_eq!(stopped.sink.real(), ramp(0, 500));
}

#[test]
fn a_healthy_idle_or_stopping_stream_is_not_mistaken_for_a_stall() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.stall = Duration::from_millis(100);
    t.no_audio = Duration::from_millis(100);
    let (recorder, failures) = open_notified::<Chunks>(&fake, passthrough(), t);
    // Callbacks keep coming, including while the stop's pause holds the
    // callback silent, for several stall bounds.
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut cycles = 0;
    while Instant::now() < deadline {
        start(&recorder, Chunks::default());
        assert!(fake.push(&[0.1f32; 32]));
        thread::scope(|scope| {
            let stopping = scope.spawn(|| recorder.stop());
            while !stopping.is_finished() {
                fake.push(&[0.2f32; 32]);
                thread::sleep(Duration::from_millis(1));
            }
            assert!(stopping.join().unwrap().unwrap().is_complete());
        });
        push_idle(&recorder, &fake, &[0.0f32; 16]);
        cycles += 1;
    }
    assert!(cycles > 3);
    assert!(failures.try_recv().is_err(), "no false positive");
}

#[test]
fn callbacks_stopping_during_stop_fail_with_stalled_and_keep_the_audio() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.pause_ack = Duration::from_millis(150);
    let (recorder, failures) = open_notified::<Chunks>(&fake, passthrough(), t);
    start(&recorder, Chunks::default());
    assert!(fake.push(&ramp(0, 700)));

    // No callback arrives to acknowledge the pause.
    let stopped = recorder.stop().expect("stop keeps the audio");
    assert_eq!(
        recorder_failed(&stopped.end_reason).kind(),
        ErrorKind::Stalled
    );
    assert_eq!(stopped.sink.real(), ramp(0, 700));
    assert_eq!(
        failures.recv_timeout(support::WAIT).unwrap().kind(),
        ErrorKind::Stalled
    );
    support::wait_until("stream torn down", || !fake.is_streaming());
}

#[test]
fn a_device_failure_during_the_stop_handshake_is_not_misreported_as_stalled() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.pause_ack = Duration::from_secs(3);
    t.stop = Duration::from_secs(4);
    let recorder: Recorder<Chunks> = open_with(&fake, passthrough(), t).unwrap();
    start(&recorder, Chunks::default());
    assert!(fake.push(&ramp(0, 300)));
    let started = Instant::now();
    let stopped = thread::scope(|scope| {
        let stopping = scope.spawn(|| recorder.stop());
        support::wait_until("pause requested", || {
            support::transport(&recorder)
                .pause_requested
                .load(std::sync::atomic::Ordering::Acquire)
        });
        assert!(fake.report_error(lost()));
        stopping.join().unwrap()
    })
    .expect("stop");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "did not wait out the pause bound"
    );
    assert_eq!(
        recorder_failed(&stopped.end_reason).kind(),
        ErrorKind::DeviceLost
    );
    assert_eq!(stopped.sink.real(), ramp(0, 300));
}

/// A sink that panics on its `n`th chunk.
#[derive(Debug)]
struct PanicsAt(usize, Chunks);
impl Sink for PanicsAt {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        if self.1.chunks.len() == self.0 {
            panic!("sink bug at chunk {}", self.0);
        }
        self.1.process_chunk(chunk);
    }
}

#[test]
fn a_sink_panic_ends_only_the_recording() {
    let fake = fake(16_000, 1);
    let (recorder, failures) = open_notified::<PanicsAt>(&fake, passthrough(), timeouts());
    start(&recorder, PanicsAt(2, Chunks::default()));
    assert!(fake.push(&ramp(0, 160 * 5)));
    let stopped = stop_with_boundary(&recorder, &fake, &[0.0f32]).unwrap();

    match &stopped.end_reason {
        EndReason::SinkPanicked(message) => assert_eq!(message, "sink bug at chunk 2"),
        other => panic!("expected SinkPanicked, got {other:?}"),
    }
    // The sink comes back with what it collected before the panic.
    assert_eq!(stopped.sink.1.chunks.len(), 2);
    assert!(!stopped.is_complete());

    // The recorder is fine.
    start(&recorder, PanicsAt(usize::MAX, Chunks::default()));
    assert!(fake.push(&ramp(0, 200)));
    let stopped = stop_with_boundary(&recorder, &fake, &[0.5f32]).unwrap();
    assert!(stopped.is_complete());
    assert_eq!(stopped.sink.1.valid_frames(), 201);
    assert!(
        failures.try_recv().is_err(),
        "a sink panic is not a recorder failure"
    );
}

#[test]
fn a_slot_held_by_a_recording_that_ended_on_its_own_waits_for_stop() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<PanicsAt> = open(&fake, passthrough());
    start(&recorder, PanicsAt(0, Chunks::default()));
    assert!(fake.push(&ramp(0, 320)));
    thread::sleep(Duration::from_millis(50));
    // Ended by the panic, but still holding the slot.
    let error = recorder.start(PanicsAt(0, Chunks::default())).unwrap_err();
    assert_eq!(error.error.kind(), ErrorKind::AlreadyRecording);
    let stopped = recorder.stop().expect("stop");
    assert!(matches!(stopped.end_reason, EndReason::SinkPanicked(_)));
}

/// A sink that blocks on its first chunk until the gate opens.
#[derive(Debug)]
struct Blocks(Gate, Arc<Mutex<bool>>);
impl Sink for Blocks {
    fn process_chunk(&mut self, _: AudioChunk<'_>) {
        *self.1.lock().unwrap() = true;
        self.0.wait();
    }
}

#[test]
fn a_sink_that_never_returns_is_caught_by_the_heartbeat() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.heartbeat = Duration::from_millis(200);
    let (recorder, failures) = open_notified::<Blocks>(&fake, passthrough(), t);
    let gate = Gate::default();
    let entered = Arc::new(Mutex::new(false));
    start(&recorder, Blocks(gate.clone(), Arc::clone(&entered)));
    assert!(fake.push(&ramp(0, 320)));

    let error = failures
        .recv_timeout(support::WAIT)
        .expect("failure handler");
    assert_eq!(error.kind(), ErrorKind::SinkStalled);
    // The stream is still torn down: the device thread never runs the sink.
    support::wait_until("stream torn down", || !fake.is_streaming());

    let started = Instant::now();
    let error = recorder.stop().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::SinkStalled);
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "stop returns at once"
    );
    let again = recorder
        .start(Blocks(gate.clone(), Arc::clone(&entered)))
        .unwrap_err();
    assert_eq!(again.error.kind(), ErrorKind::SinkStalled);

    // close is bounded even with the delivery thread stuck in the sink.
    let started = Instant::now();
    recorder.close().expect("close");
    assert!(started.elapsed() < Duration::from_secs(2));
    gate.open();
}

#[test]
fn a_sink_that_never_returns_is_caught_by_the_stop_deadline() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.stop = Duration::from_millis(700);
    let (recorder, failures) = open_notified::<Blocks>(&fake, passthrough(), t);
    let gate = Gate::default();
    let entered = Arc::new(Mutex::new(false));
    start(&recorder, Blocks(gate.clone(), Arc::clone(&entered)));

    // The sink first blocks inside stop's drain.
    let result = thread::scope(|scope| {
        let stopping = scope.spawn(|| recorder.stop());
        support::wait_until("pause requested", || {
            support::transport(&recorder)
                .pause_requested
                .load(std::sync::atomic::Ordering::Acquire)
        });
        assert!(fake.push(&ramp(0, 320)));
        stopping.join().unwrap()
    });
    assert!(*entered.lock().unwrap());
    assert_eq!(result.unwrap_err().kind(), ErrorKind::SinkStalled);
    assert_eq!(
        failures.recv_timeout(support::WAIT).unwrap().kind(),
        ErrorKind::SinkStalled
    );
    support::wait_until("stream torn down", || !fake.is_streaming());
    // The slot was released and the recorder has failed.
    let again = recorder
        .start(Blocks(gate.clone(), Arc::clone(&entered)))
        .unwrap_err();
    assert_eq!(again.error.kind(), ErrorKind::SinkStalled);
    gate.open();
}

#[test]
fn the_failure_handler_is_called_once_even_when_it_panics() {
    let fake = fake(16_000, 1);
    let calls = Arc::new(Mutex::new(0));
    let counted = Arc::clone(&calls);
    let recorder: Recorder<CollectingSink> = Recorder::open_with(
        Arc::new(fake.clone()),
        passthrough(),
        Some(Box::new(move |_| {
            *counted.lock().unwrap() += 1;
            panic!("handler bug");
        })),
        timeouts(),
    )
    .unwrap();
    // A failure right after open, then more errors.
    assert!(fake.report_error(lost()));
    support::wait_until("handler called", || *calls.lock().unwrap() == 1);
    fake.report_error(lost());
    thread::sleep(Duration::from_millis(50));
    assert_eq!(*calls.lock().unwrap(), 1);
    // The recorder is unaffected by the handler's panic.
    let error = recorder.start(CollectingSink::new()).unwrap_err();
    assert_eq!(error.error.kind(), ErrorKind::DeviceLost);
    recorder.close().unwrap();
}

#[test]
fn the_failure_handler_can_stop_and_close_the_recorder() {
    let fake = fake(16_000, 1);
    let slot: Arc<Mutex<Option<Recorder<Chunks>>>> = Arc::new(Mutex::new(None));
    let (done_tx, done_rx) = mpsc::channel();
    let handler_slot = Arc::clone(&slot);
    let recorder = Recorder::open_with(
        Arc::new(fake.clone()),
        passthrough(),
        Some(Box::new(move |_| {
            let recorder = handler_slot.lock().unwrap().take().unwrap();
            let stopped = recorder.stop();
            let closed = recorder.close();
            let _ = done_tx.send((stopped.map(|s| s.sink.real()), closed));
        })),
        timeouts(),
    )
    .unwrap();
    start(&recorder, Chunks::default());
    assert!(fake.push(&ramp(0, 400)));
    *slot.lock().unwrap() = Some(recorder);
    assert!(fake.report_error(lost()));

    let (stopped, closed) = done_rx
        .recv_timeout(support::WAIT)
        .expect("handler finished");
    assert_eq!(stopped.expect("stop from the handler"), ramp(0, 400));
    closed.expect("close from the handler");
    assert!(!fake.is_streaming());
}

#[test]
fn open_failures_map_to_their_kinds_and_leave_no_stream() {
    let cases = [
        (
            BackendErrorKind::PermissionDenied,
            "Failed to build input stream: Unauthorized",
            ErrorKind::PermissionDenied,
        ),
        (
            BackendErrorKind::Other,
            "WASAPI error: 0x80070005",
            ErrorKind::PermissionDenied,
        ),
        (
            BackendErrorKind::DeviceNotAvailable,
            "No input device found",
            ErrorKind::DeviceUnavailable,
        ),
        (
            BackendErrorKind::DeviceBusy,
            "AUDCLNT_E_DEVICE_IN_USE",
            ErrorKind::DeviceBusy,
        ),
        (
            BackendErrorKind::Other,
            "something else",
            ErrorKind::Backend,
        ),
    ];
    for (backend_kind, message, kind) in cases {
        let fake = fake(16_000, 1);
        fake.fail_next_open(BackendError::new(backend_kind, message));
        let error = open_with::<CollectingSink>(&fake, passthrough(), timeouts())
            .err()
            .expect("open fails");
        assert_eq!(error.kind(), kind, "{message}");
        assert_eq!(error.detail(), Some(message));
        assert_eq!(
            error.elapsed(),
            None,
            "open failures happen before the stream"
        );
        assert!(!fake.is_streaming());
    }

    let fake = fake(16_000, 1);
    let error = open_with::<CollectingSink>(
        &fake,
        RecorderConfig {
            device: Some("fake:nope".into()),
            ..passthrough()
        },
        timeouts(),
    )
    .err()
    .unwrap();
    assert_eq!(error.kind(), ErrorKind::DeviceUnavailable);
}

#[test]
fn a_hanging_open_times_out_and_the_late_stream_releases_itself() {
    let fake = fake(16_000, 1);
    let gate = fake.hang_next_start();
    let mut t = timeouts();
    t.open = Duration::from_millis(200);
    let started = Instant::now();
    let error = open_with::<CollectingSink>(&fake, passthrough(), t)
        .err()
        .expect("open times out");
    assert_eq!(error.kind(), ErrorKind::OpenTimedOut);
    assert!(started.elapsed() < Duration::from_secs(1));

    // The platform call completes later; the stream is released at once.
    gate.open();
    support::wait_until("the late stream started", || fake.streams_started() == 1);
    support::wait_until("the late stream released", || !fake.is_streaming());
}

#[test]
fn a_hanging_teardown_makes_close_time_out() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.close = Duration::from_millis(200);
    let recorder: Recorder<CollectingSink> = open_with(&fake, passthrough(), t).unwrap();
    let gate = fake.hang_teardown();
    let started = Instant::now();
    let error = recorder.close().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::CloseTimedOut);
    assert!(started.elapsed() < Duration::from_secs(1));
    gate.open();
    support::wait_until("the stream is finally released", || !fake.is_streaming());
}

#[test]
fn every_start_failure_returns_the_sink() {
    #[derive(Debug, PartialEq)]
    struct Tagged(u32);
    impl Sink for Tagged {
        fn process_chunk(&mut self, _: AudioChunk<'_>) {}
    }
    let fake = fake(16_000, 1);
    let recorder: Recorder<Tagged> = open(&fake, passthrough());
    start(&recorder, Tagged(1));
    assert_eq!(recorder.start(Tagged(2)).unwrap_err().sink, Tagged(2));
    stop_with_boundary(&recorder, &fake, &[0.0f32]).unwrap();
    fake.report_error(lost());
    support::wait_until("failed", || !fake.is_streaming());
    assert_eq!(recorder.start(Tagged(3)).unwrap_err().sink, Tagged(3));
}

#[test]
fn denied_microphone_access_fails_open_instead_of_recording_silence() {
    let fake = fake(16_000, 1);
    fake.set_permission(crate::Permission::Denied);
    let error = open_with::<CollectingSink>(&fake, passthrough(), timeouts())
        .err()
        .expect("open fails");
    assert_eq!(error.kind(), ErrorKind::PermissionDenied);
    assert_eq!(error.device().unwrap().name, "Fake Mic");
    assert!(error.detail().unwrap().contains("Privacy & Security"));
    assert_eq!(fake.streams_started(), 0, "no stream was built");

    // Not yet asked: open proceeds (macOS shows its prompt).
    fake.set_permission(crate::Permission::NotDetermined);
    open_with::<CollectingSink>(&fake, passthrough(), timeouts()).expect("opens");
}

#[test]
fn access_revoked_while_open_fails_the_recorder_at_start() {
    let fake = fake(16_000, 1);
    let (recorder, failures) = open_notified::<CollectingSink>(&fake, passthrough(), timeouts());
    fake.set_permission(crate::Permission::Denied);

    let error = recorder.start(CollectingSink::new()).unwrap_err();
    assert_eq!(error.error.kind(), ErrorKind::PermissionDenied);
    assert_eq!(
        failures.recv_timeout(support::WAIT).unwrap().kind(),
        ErrorKind::PermissionDenied
    );
    support::wait_until("stream torn down", || !fake.is_streaming());
    // A failed recorder stays failed, even if access comes back.
    fake.set_permission(crate::Permission::Granted);
    assert_eq!(
        recorder
            .start(CollectingSink::new())
            .unwrap_err()
            .error
            .kind(),
        ErrorKind::PermissionDenied
    );
}

/// macOS stops callbacks for tens of seconds of awake time around system
/// sleep, then resumes them (measured on hardware with the `sleep-raw`
/// probe).
#[test]
fn a_stall_while_idle_is_not_a_failure() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.stall = Duration::from_millis(100);
    let (recorder, failures) = open_notified::<Chunks>(&fake, passthrough(), t);
    push_idle(&recorder, &fake, &ramp(0, 160));
    // "Asleep": no callbacks for several stall bounds.
    thread::sleep(Duration::from_millis(400));
    assert!(
        failures.try_recv().is_err(),
        "an idle stall failed the recorder"
    );
    assert!(fake.is_streaming());

    // "Awake": callbacks resume and a recording works.
    push_idle(&recorder, &fake, &ramp(0, 160));
    start(&recorder, Chunks::default());
    assert!(fake.push(&ramp(0, 500)));
    let stopped = stop_with_boundary(&recorder, &fake, &[0.5f32]).unwrap();
    assert!(stopped.is_complete());
    assert_eq!(stopped.sink.valid_frames(), 501);
}

#[test]
fn a_recording_started_during_a_stall_gets_the_full_bound_to_resume() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.stall = Duration::from_millis(300);
    let (recorder, failures) = open_notified::<Chunks>(&fake, passthrough(), t);
    push_idle(&recorder, &fake, &ramp(0, 160));
    thread::sleep(Duration::from_millis(600));

    // Started long after the last callback; audio resumes within the bound.
    start(&recorder, Chunks::default());
    thread::sleep(Duration::from_millis(100));
    assert!(fake.push(&ramp(0, 400)));
    let stopped = stop_with_boundary(&recorder, &fake, &[0.5f32]).unwrap();
    assert!(stopped.is_complete(), "{:?}", stopped.end_reason);
    assert!(failures.try_recv().is_err());
}

#[test]
fn a_stall_that_lasts_into_a_recording_fails_with_stalled() {
    let fake = fake(16_000, 1);
    let mut t = timeouts();
    t.stall = Duration::from_millis(300);
    let (recorder, failures) = open_notified::<Chunks>(&fake, passthrough(), t);
    push_idle(&recorder, &fake, &ramp(0, 160));
    thread::sleep(Duration::from_millis(600));

    let started = Instant::now();
    start(&recorder, Chunks::default());
    let error = failures
        .recv_timeout(support::WAIT)
        .expect("failure handler");
    assert_eq!(error.kind(), ErrorKind::Stalled);
    // Counted from the recording's start, not from the last callback.
    assert!(
        started.elapsed() >= Duration::from_millis(280),
        "{:?}",
        started.elapsed()
    );
    let stopped = recorder.stop().expect("stop");
    assert_eq!(
        recorder_failed(&stopped.end_reason).kind(),
        ErrorKind::Stalled
    );
}
