//! Callback and ring tests, against the callback as a free function,
//! including the K-channel ring tests. Consumer-loop scenarios are tested
//! through `Recorder` in `crate::tests`.

use std::sync::atomic::Ordering;

use rtrb::RingBuffer;

use super::{
    CaptureTransportState, Routing, drain_available_samples, is_microphone_access_denied,
    is_no_input_device_error, write_input_to_ring,
};

#[test]
fn callback_writes_mono_samples() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(8);
    let transport = CaptureTransportState::default();

    write_input_to_ring(
        &[0.25f32, -0.5, 1.0],
        1,
        Routing::MixToMono,
        &mut producer,
        &transport,
    );

    let mut output = [0.0; 3];
    consumer.pop_entire_slice(&mut output).expect("samples");
    assert_eq!(output, [0.25, -0.5, 1.0]);
}

#[test]
fn callback_downmixes_or_selects_multichannel_input() {
    let transport = CaptureTransportState::default();
    let (mut average_tx, mut average_rx) = RingBuffer::<f32>::new(4);
    write_input_to_ring(
        &[1.0f32, 3.0, -1.0, 1.0],
        2,
        Routing::MixToMono,
        &mut average_tx,
        &transport,
    );
    let mut averaged = [0.0; 2];
    average_rx
        .pop_entire_slice(&mut averaged)
        .expect("averaged samples");
    assert_eq!(averaged, [2.0, 0.0]);

    let (mut selected_tx, mut selected_rx) = RingBuffer::<f32>::new(4);
    write_input_to_ring(
        &[1.0f32, 3.0, -1.0, 1.0],
        2,
        Routing::Only(1),
        &mut selected_tx,
        &transport,
    );
    let mut selected = [0.0; 2];
    selected_rx
        .pop_entire_slice(&mut selected)
        .expect("selected samples");
    assert_eq!(selected, [3.0, 1.0]);
}

#[test]
fn callback_forwards_boundary_block_then_stays_silent_until_resumed() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(8);
    let transport = CaptureTransportState::default();

    // The block in hand when a pause is first observed was captured before
    // the stop, so it is forwarded and only then acknowledged.
    transport.pause_requested.store(true, Ordering::Release);
    write_input_to_ring(
        &[1.0f32, 2.0],
        1,
        Routing::MixToMono,
        &mut producer,
        &transport,
    );
    assert!(transport.pause_acknowledged.load(Ordering::Acquire));
    assert_eq!(consumer.slots(), 2);

    // Later blocks while paused are dropped and are not counted as overruns.
    write_input_to_ring(&[3.0f32], 1, Routing::MixToMono, &mut producer, &transport);
    assert_eq!(consumer.slots(), 2);
    assert_eq!(transport.overrun_frames.load(Ordering::Relaxed), 0);

    // Clearing the pause, as the consumer does before stop() returns, resumes capture.
    transport.pause_acknowledged.store(false, Ordering::Relaxed);
    transport.pause_requested.store(false, Ordering::Release);
    write_input_to_ring(&[4.0f32], 1, Routing::MixToMono, &mut producer, &transport);
    let mut output = [0.0; 3];
    consumer.pop_entire_slice(&mut output).expect("samples");
    assert_eq!(output, [1.0, 2.0, 4.0]);
    assert!(!transport.pause_acknowledged.load(Ordering::Acquire));
}

#[test]
fn callback_partially_fills_ring_and_counts_dropped_audio() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(2);
    let transport = CaptureTransportState::default();

    write_input_to_ring(
        &[1.0f32, 2.0, 3.0],
        1,
        Routing::MixToMono,
        &mut producer,
        &transport,
    );

    let mut captured = [0.0; 2];
    consumer
        .pop_entire_slice(&mut captured)
        .expect("partial callback audio");
    assert_eq!(captured, [1.0, 2.0]);
    assert_eq!(transport.overrun_frames.load(Ordering::Relaxed), 1);
}

#[test]
fn bounded_drain_leaves_remaining_samples_for_the_next_command_cycle() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(8);
    producer
        .push_entire_slice(&[1.0, 2.0, 3.0, 4.0, 5.0])
        .expect("samples");
    let mut drained = Vec::new();

    let count =
        drain_available_samples(&mut consumer, 3, 1, |part| drained.extend_from_slice(part));

    assert_eq!(count, 3);
    assert_eq!(drained, [1.0, 2.0, 3.0]);
    assert_eq!(consumer.slots(), 2);
}

#[test]
fn ring_wraparound_preserves_both_read_slices_in_order() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(5);
    let transport = CaptureTransportState::default();
    producer
        .push_entire_slice(&[1.0, 2.0, 3.0, 4.0])
        .expect("initial samples");
    let mut discarded = [0.0; 3];
    consumer
        .pop_entire_slice(&mut discarded)
        .expect("advance ring head");

    write_input_to_ring(
        &[5.0f32, 6.0, 7.0, 8.0],
        1,
        Routing::MixToMono,
        &mut producer,
        &transport,
    );

    let chunk = consumer.read_chunk(5).expect("wrapped samples");
    let (first, second) = chunk.as_slices();
    assert!(!first.is_empty());
    assert!(!second.is_empty());
    let ordered = first
        .iter()
        .chain(second.iter())
        .copied()
        .collect::<Vec<_>>();
    assert_eq!(ordered, [4.0, 5.0, 6.0, 7.0, 8.0]);
}

#[test]
fn detects_access_is_denied() {
    assert!(is_microphone_access_denied("Access is denied"));
}

#[test]
fn detects_permission_denied() {
    assert!(is_microphone_access_denied("permission denied"));
}

#[test]
fn detects_windows_error_code() {
    assert!(is_microphone_access_denied("WASAPI error: 0x80070005"));
}

#[test]
fn does_not_match_unrelated_errors() {
    assert!(!is_microphone_access_denied("device not found"));
}

#[test]
fn detects_no_input_device() {
    assert!(is_no_input_device_error("No input device found"));
}

#[test]
fn detects_coreaudio_config_error() {
    assert!(is_no_input_device_error(
        "Failed to fetch preferred config: A backend-specific error has occurred: An unknown error unknown to the coreaudio-rs API occurred"
    ));
}

#[test]
fn does_not_match_other_errors_for_no_device() {
    assert!(!is_no_input_device_error("permission denied"));
    assert!(!is_no_input_device_error("device not found"));
}

#[test]
fn callback_writes_all_channels_interleaved() {
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(8);
    let transport = CaptureTransportState::default();

    write_input_to_ring(
        &[1.0f32, -1.0, 2.0, -2.0, 3.0, -3.0],
        2,
        Routing::All,
        &mut producer,
        &transport,
    );

    let mut output = [0.0; 6];
    consumer.pop_entire_slice(&mut output).expect("samples");
    assert_eq!(output, [1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
}

#[test]
fn overrun_drops_whole_frames_and_counts_frames() {
    // Five free slots hold two stereo frames; the fifth slot stays empty
    // rather than taking half of the third frame.
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(5);
    let transport = CaptureTransportState::default();

    write_input_to_ring(
        &[1.0f32, -1.0, 2.0, -2.0, 3.0, -3.0],
        2,
        Routing::All,
        &mut producer,
        &transport,
    );

    assert_eq!(consumer.slots(), 4);
    let mut output = [0.0; 4];
    consumer.pop_entire_slice(&mut output).expect("samples");
    assert_eq!(output, [1.0, -1.0, 2.0, -2.0]);
    assert_eq!(transport.overrun_frames.load(Ordering::Relaxed), 1);
}

#[test]
fn stereo_frames_stay_aligned_across_ring_wraparound() {
    // Capacity is a multiple of K (as the engine allocates it), so a frame
    // never straddles the wrap point unevenly.
    let (mut producer, mut consumer) = RingBuffer::<f32>::new(6);
    let transport = CaptureTransportState::default();
    let mut left = Vec::new();
    let mut right = Vec::new();

    for block in 0..10 {
        let base = block as f32 * 10.0;
        write_input_to_ring(
            &[base + 1.0, -(base + 1.0), base + 2.0, -(base + 2.0)],
            2,
            Routing::All,
            &mut producer,
            &transport,
        );
        let chunk = consumer.read_chunk(consumer.slots()).unwrap();
        let (first, second) = chunk.as_slices();
        let samples: Vec<f32> = first.iter().chain(second).copied().collect();
        chunk.commit_all();
        for frame in samples.chunks_exact(2) {
            left.push(frame[0]);
            right.push(frame[1]);
        }
    }

    assert_eq!(transport.overrun_frames.load(Ordering::Relaxed), 0);
    assert!(
        left.iter().all(|&s| s > 0.0),
        "left channel rotated: {left:?}"
    );
    assert_eq!(left, right.iter().map(|s| -s).collect::<Vec<_>>());
    assert_eq!(left.len(), 20);
}
