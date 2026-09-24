//! Recorder lifecycle over the fake backend: exact boundaries, the recording
//! slot, repeated recordings, close and drop.

use std::{sync::Arc, thread};

use super::support::{self, Chunks, fake, open, push_idle, ramp, start, stop_with_boundary};
use crate::{Channels, CollectingSink, EndReason, ErrorKind, Recorder, RecorderConfig};

/// 16 kHz mono in, 16 kHz mono out: no resampling, so samples pass through
/// unchanged and positions can be checked exactly.
fn passthrough(frames_per_chunk: usize) -> RecorderConfig {
    RecorderConfig {
        frames_per_chunk: Some(frames_per_chunk),
        ..RecorderConfig::default()
    }
}

#[test]
fn a_recording_delivers_exactly_the_audio_between_start_and_the_boundary() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<Chunks> = open(&fake, passthrough(160));
    push_idle(&recorder, &fake, &ramp(0, 500));

    start(&recorder, Chunks::default());
    let first = ramp(1000, 1234);
    assert!(fake.push(&first));
    let boundary = ramp(5000, 77);
    let stopped = stop_with_boundary(&recorder, &fake, &boundary).unwrap();

    assert!(stopped.is_complete());
    let chunks = stopped.sink;
    let expected: Vec<f32> = first.iter().chain(&boundary).copied().collect();
    assert_eq!(chunks.real(), expected);
    assert_eq!(chunks.valid_frames(), 1234 + 77);
    // Every chunk is full size; only the last is padded, with zeros.
    assert!(chunks.chunks.iter().all(|(s, _)| s.len() == 160));
    let (last, rest) = chunks.chunks.split_last().unwrap();
    assert!(rest.iter().all(|(_, v)| *v == 160));
    assert_eq!(last.1, (1234 + 77) % 160);
    assert!(last.0[last.1..].iter().all(|&s| s == 0.0));
    assert_eq!((chunks.sample_rate, chunks.channels), (16_000, 1));
}

/// Handy's `idle_chunks_are_discarded_without_reaching_the_recording` and
/// `repeated_start_stop_cycles_resume_capture_without_leaking_samples`,
/// through the recorder.
#[test]
fn repeated_recordings_on_a_warm_recorder_neither_lose_nor_leak_audio() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<Chunks> = open(&fake, passthrough(480));
    let mut expected_all = Vec::new();
    for cycle in 0..5 {
        push_idle(&recorder, &fake, &ramp(9000 + cycle, 300));
        start(&recorder, Chunks::default());
        let audio = ramp(cycle * 10, 200 + cycle * 7);
        assert!(fake.push(&audio));
        let boundary = [0.75f32 + cycle as f32];
        let stopped = stop_with_boundary(&recorder, &fake, &boundary).unwrap();
        let mut expected = audio.clone();
        expected.extend_from_slice(&boundary);
        assert_eq!(stopped.sink.real(), expected, "cycle {cycle}");
        // The pause is cleared before stop returns.
        assert!(
            !support::transport(&recorder)
                .pause_requested
                .load(std::sync::atomic::Ordering::Acquire)
        );
        expected_all.push(expected);
    }
    // A start immediately after stop loses nothing: the next block is the
    // first of the recording.
    start(&recorder, Chunks::default());
    assert!(fake.push(&[0.5f32, 0.25]));
    let stopped = stop_with_boundary(&recorder, &fake, &[0.125f32]).unwrap();
    assert_eq!(stopped.sink.real(), vec![0.5, 0.25, 0.125]);
}

#[test]
fn start_while_recording_fails_with_already_recording_and_returns_the_sink() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<CollectingSink> = open(&fake, passthrough(160));
    start(&recorder, CollectingSink::new());

    let error = recorder.start(CollectingSink::new()).unwrap_err();
    assert_eq!(error.error.kind(), ErrorKind::AlreadyRecording);
    let _sink_came_back: CollectingSink = error.sink;

    stop_with_boundary(&recorder, &fake, &[0.0f32]).unwrap();
    // After stop, the slot is free again.
    start(&recorder, CollectingSink::new());
}

#[test]
fn stop_without_a_recording_is_not_recording() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<CollectingSink> = open(&fake, passthrough(160));
    assert_eq!(recorder.stop().unwrap_err().kind(), ErrorKind::NotRecording);

    start(&recorder, CollectingSink::new());
    stop_with_boundary(&recorder, &fake, &[0.0f32]).unwrap();
    assert_eq!(recorder.stop().unwrap_err().kind(), ErrorKind::NotRecording);
}

#[test]
fn concurrent_stops_give_the_recording_to_exactly_one() {
    let fake = fake(16_000, 1);
    let recorder: Arc<Recorder<CollectingSink>> = Arc::new(open(&fake, passthrough(160)));
    for _ in 0..10 {
        start(&recorder, CollectingSink::new());
        let results: Vec<_> = thread::scope(|scope| {
            let stops: Vec<_> = (0..4).map(|_| scope.spawn(|| recorder.stop())).collect();
            // Keep callbacks coming so the pause handshake completes.
            let mut results = Vec::new();
            for stop in stops {
                while !stop.is_finished() {
                    fake.push(&[0.0f32; 16]);
                    thread::yield_now();
                }
                results.push(stop.join().unwrap());
            }
            results
        });
        let won = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(won, 1);
        assert!(
            results
                .iter()
                .filter_map(|r| r.as_ref().err())
                .all(|e| e.kind() == ErrorKind::NotRecording)
        );
    }
}

#[test]
fn close_during_a_recording_discards_it_and_releases_the_stream() {
    let fake = fake(16_000, 1);
    let recorder: Recorder<CollectingSink> = open(&fake, passthrough(160));
    start(&recorder, CollectingSink::new());
    assert!(fake.push(&ramp(0, 1000)));

    recorder.close().expect("close");
    assert!(!fake.is_streaming());
}

#[test]
fn dropping_the_recorder_closes_it() {
    let fake = fake(16_000, 1);
    {
        let recorder: Recorder<CollectingSink> = open(&fake, passthrough(160));
        start(&recorder, CollectingSink::new());
        assert!(fake.is_streaming());
    }
    assert!(!fake.is_streaming());
}

/// Handy's `shutdown_is_processed_without_audio_samples`.
#[test]
fn close_without_any_audio_is_prompt() {
    let fake = fake(48_000, 2);
    let recorder: Recorder<CollectingSink> = open(&fake, RecorderConfig::speech());
    let started = std::time::Instant::now();
    recorder.close().expect("close");
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[test]
fn closing_from_inside_the_sink_does_not_deadlock() {
    use std::sync::Mutex;

    /// Holds the only handle to its own recorder and drops it mid-chunk.
    struct Closer(Arc<Mutex<Option<Recorder<Closer>>>>);
    impl crate::Sink for Closer {
        fn process_chunk(&mut self, _: crate::AudioChunk<'_>) {
            let recorder = self.0.lock().unwrap().take();
            drop(recorder);
        }
    }

    let fake = fake(16_000, 1);
    let slot = Arc::new(Mutex::new(None));
    let recorder: Recorder<Closer> = open(&fake, passthrough(160));
    start(&recorder, Closer(Arc::clone(&slot)));
    *slot.lock().unwrap() = Some(recorder);

    // The first full chunk runs the sink, which closes the recorder from
    // the delivery thread.
    assert!(fake.push(&ramp(0, 320)));
    support::wait_until("the sink closed the recorder", || !fake.is_streaming());
}

#[test]
fn the_sink_first_call_marks_audio_flowing() {
    use std::sync::mpsc;

    struct Ready(Option<mpsc::Sender<()>>);
    impl crate::Sink for Ready {
        fn process_chunk(&mut self, _: crate::AudioChunk<'_>) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    let fake = fake(48_000, 1);
    let recorder: Recorder<Ready> = open(&fake, RecorderConfig::speech());
    let (tx, rx) = mpsc::channel();
    start(&recorder, Ready(Some(tx)));
    assert!(rx.try_recv().is_err(), "no audio yet");
    // Silence counts: flowing means the device delivers samples.
    assert!(fake.push(&[0.0f32; 4800]));
    rx.recv_timeout(support::WAIT).expect("first chunk");
    let stopped = stop_with_boundary(&recorder, &fake, &[0.0f32]).unwrap();
    assert!(matches!(stopped.end_reason, EndReason::StopCalled));
}

#[test]
fn collecting_sink_keeps_only_real_audio() {
    let fake = fake(16_000, 2);
    let recorder: Recorder<CollectingSink> = open(
        &fake,
        RecorderConfig {
            channels: Channels::All,
            frames_per_chunk: Some(100),
            ..RecorderConfig::default()
        },
    );
    start(&recorder, CollectingSink::new());
    let audio: Vec<f32> = ramp(0, 2 * 130);
    assert!(fake.push(&audio));
    let stopped = stop_with_boundary(&recorder, &fake, &[0.5f32, -0.5]).unwrap();
    let mut expected = audio;
    expected.extend_from_slice(&[0.5, -0.5]);
    assert_eq!(stopped.sink.into_samples(), expected);
}
