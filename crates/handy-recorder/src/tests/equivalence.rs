//! Equivalence with Handy: the same synthetic input through the frozen Handy
//! reference (`capture::handy_reference`) and through the extracted pipeline
//! must give bit-identical real audio, resampler delay included, and the
//! number of real frames must follow the frame-count formula.
//!
//! The extracted side runs end to end through the fake backend: the test
//! thread delivers callback blocks while the recorder's consumer thread
//! handles start and stop, with the pause handshake at each stop. One stream
//! carries several recordings with idle audio between them, so resampler
//! reset and tail flushing are covered.
//!
//! When the pipeline's shape changes (fake backend, sinks and chunks), only
//! the extracted side of this file changes; the reference never does.

use std::{sync::atomic::Ordering, time::Duration};

use dasp_sample::{FromSample, Sample};
use rtrb::RingBuffer;

use super::support::{self, Chunks};
use crate::backend::InputSample;
use crate::capture::handy_reference as handy;
use crate::{Channels, Recorder, RecorderConfig};

/// Handy's output rate.
const OUTPUT_SAMPLE_RATE: u32 = 16_000;

/// Frames per callback block, cycled. Includes single-frame blocks and sizes
/// on both sides of the resampler's 1024-sample chunk.
const BLOCK_FRAMES: &[usize] = &[1, 7, 480, 1024, 3, 2048, 441, 1, 1, 1500];

const RATES: &[u32] = &[8_000, 16_000, 44_100, 48_000, 96_000];

/// (device channels, selected channel). `None` averages all channels.
/// Handy's fallback for an out-of-range channel is gone: it is
/// `InvalidChannel` from `open` (see `formats.rs`).
const ROUTINGS: &[(usize, Option<usize>)] = &[(1, None), (2, None), (2, Some(1)), (4, Some(3))];

/// One interleaved callback block.
type Block<T> = Vec<T>;

/// A stream: idle audio, then a recording, repeated.
struct Stream<T> {
    /// Per recording: the idle blocks written before its Start.
    idle: Vec<Vec<Block<T>>>,
    /// Per recording: its blocks. The last is the stop's boundary block.
    recordings: Vec<Vec<Block<T>>>,
}

/// Deterministic test signal: a distinct tone per channel plus hashed noise,
/// with full-scale samples at regular frames so integer extremes are hit.
fn sample_at(frame: usize, channel: usize, rate: u32) -> f32 {
    if frame.is_multiple_of(997) {
        return if channel.is_multiple_of(2) { 1.0 } else { -1.0 };
    }
    let t = frame as f64 / rate as f64;
    let tone = (2.0 * std::f64::consts::PI * (220.0 + 170.0 * channel as f64) * t).sin() * 0.6;
    let mut x = (frame as u64 * 8 + channel as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    let noise = ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 0.4;
    (tone + noise) as f32
}

fn make_stream<T>(rate: u32, channels: usize) -> Stream<T>
where
    T: Sample + FromSample<f32>,
{
    let rate_frames = rate as usize;
    // Recording lengths in frames: ends mid-chunk, tiny (less than one
    // resampler chunk), short, and long.
    let recording_frames = [rate_frames * 37 / 100, 10, 300, rate_frames / 2 + 13];
    let idle_frames = rate_frames / 10 + 5;

    let mut frame = 0;
    let mut block_index = 0;
    let mut blocks = |frames: usize| {
        let mut out = Vec::new();
        let mut remaining = frames;
        while remaining > 0 {
            let len = BLOCK_FRAMES[block_index % BLOCK_FRAMES.len()].min(remaining);
            block_index += 1;
            let block = (frame..frame + len)
                .flat_map(|f| (0..channels).map(move |c| sample_at(f, c, rate).to_sample::<T>()))
                .collect();
            out.push(block);
            frame += len;
            remaining -= len;
        }
        out
    };

    let mut stream = Stream {
        idle: Vec::new(),
        recordings: Vec::new(),
    };
    for frames in recording_frames {
        stream.idle.push(blocks(idle_frames));
        stream.recordings.push(blocks(frames));
    }
    stream
}

fn frame_duration(frame_samples: usize) -> Duration {
    Duration::from_secs_f64(frame_samples as f64 / OUTPUT_SAMPLE_RATE as f64)
}

/// Handy as shipped: each recording's input goes through Handy's callback and
/// ring into one `FrameResampler`, reset at each start and finished at stop.
fn run_reference<T>(
    stream: &Stream<T>,
    rate: u32,
    channels: usize,
    use_channel: Option<usize>,
    frame_samples: usize,
) -> Vec<Vec<f32>>
where
    T: Sample,
    f32: FromSample<T>,
{
    let mut resampler = handy::FrameResampler::new(
        rate as usize,
        OUTPUT_SAMPLE_RATE as usize,
        frame_duration(frame_samples),
    );
    stream
        .recordings
        .iter()
        .map(|blocks| {
            resampler.reset();
            let (mut producer, mut consumer) = RingBuffer::<f32>::new(rate as usize * 2);
            let transport = handy::CaptureTransportState::default();
            let mut out = Vec::new();
            for block in blocks {
                handy::write_input_to_ring(block, channels, use_channel, &mut producer, &transport);
                let chunk = consumer
                    .read_chunk(consumer.slots())
                    .expect("reference ring is readable");
                let (first, second) = chunk.as_slices();
                resampler.push(first, |f| out.extend_from_slice(f));
                resampler.push(second, |f| out.extend_from_slice(f));
                chunk.commit_all();
            }
            assert_eq!(transport.overrun_samples.load(Ordering::Relaxed), 0);
            resampler.finish(|f| out.extend_from_slice(f));
            out
        })
        .collect()
}

/// The library, opened on the fake backend through the public API and
/// driven the way a device drives it.
fn run_extracted<T>(
    stream: &Stream<T>,
    rate: u32,
    channels: usize,
    use_channel: Option<usize>,
    frame_samples: usize,
) -> Vec<Vec<f32>>
where
    T: InputSample,
{
    let fake = support::fake_with(rate, channels as u16, T::FORMAT);
    let config = RecorderConfig {
        device: None,
        sample_rate: Some(OUTPUT_SAMPLE_RATE),
        channels: match use_channel {
            Some(channel) => Channels::Only(channel as u16),
            None => Channels::MixToMono,
        },
        frames_per_chunk: Some(frame_samples),
    };
    let recorder: Recorder<Chunks> = support::open(&fake, config);

    let mut outputs = Vec::new();
    for (idle, blocks) in stream.idle.iter().zip(&stream.recordings) {
        // Idle audio is discarded. Wait until it is drained, so none of it is
        // still in the ring when the recording starts.
        for block in idle {
            support::push_idle(&recorder, &fake, block);
        }
        support::start(&recorder, Chunks::default());

        let (boundary, before_stop) = blocks.split_last().expect("recording has blocks");
        for block in before_stop {
            assert!(fake.push(block));
        }
        // The first callback after the stop request is the boundary block;
        // it belongs to the recording.
        let stopped = support::stop_with_boundary(&recorder, &fake, boundary).expect("stop");
        assert!(stopped.is_complete(), "{:?}", stopped.end_reason);
        outputs.push(stopped.sink.all());
    }

    recorder.close().expect("close");
    assert!(!fake.is_streaming(), "close released the stream");
    outputs
}

/// Frames of real audio in a recording's output: the frame-count formula,
/// including the resampler delay Handy emits (trimming it is deferred).
fn real_output_frames(rate: u32, input_frames: usize, frame_samples: usize) -> usize {
    if rate == OUTPUT_SAMPLE_RATE {
        return input_frames;
    }
    let delay = crate::capture::FrameResampler::new(
        rate as usize,
        OUTPUT_SAMPLE_RATE as usize,
        frame_samples,
        1,
    )
    .unwrap()
    .output_delay();
    input_frames * OUTPUT_SAMPLE_RATE as usize / rate as usize + delay
}

/// The real audio must match Handy bit for bit. After it, the library emits
/// zero padding up to a whole chunk; Handy may instead emit resampled
/// padding from its last partial resampler block (an intentional change:
/// see TODO.md, "Final chunk: synthetic resampler output").
fn assert_equivalent(
    expected: &[f32],
    actual: &[f32],
    real: usize,
    frame_samples: usize,
    context: &str,
) {
    assert!(
        expected.len() >= real,
        "{context}: Handy emitted less than the real audio"
    );
    assert_eq!(
        actual.len(),
        real.div_ceil(frame_samples) * frame_samples,
        "{context}: output length"
    );
    if let Some(i) = (0..real).find(|&i| expected[i].to_bits() != actual[i].to_bits()) {
        panic!(
            "{context}: first difference at sample {i}: Handy {} vs extracted {}",
            expected[i], actual[i]
        );
    }
    assert!(
        actual[real..].iter().all(|&s| s == 0.0),
        "{context}: padding is not zeros"
    );
}

fn check_format<T>(format: &str)
where
    T: InputSample + Sample + FromSample<f32>,
    f32: FromSample<T>,
{
    for (case, (&rate, &(channels, use_channel))) in RATES
        .iter()
        .flat_map(|rate| ROUTINGS.iter().map(move |routing| (rate, routing)))
        .enumerate()
    {
        // Handy's default 30 ms frames, and a VAD-style 512-sample frame.
        let frame_samples = if case % 2 == 0 { 480 } else { 512 };
        let stream = make_stream::<T>(rate, channels);

        let expected = run_reference(&stream, rate, channels, use_channel, frame_samples);
        let actual = run_extracted(&stream, rate, channels, use_channel, frame_samples);

        assert_eq!(expected.len(), actual.len());
        for (recording, (expected, actual)) in expected.iter().zip(&actual).enumerate() {
            let context = format!(
                "{format} {rate} Hz, {channels} ch, channel {use_channel:?}, \
                 {frame_samples}-sample frames, recording {recording}"
            );
            assert!(
                expected.iter().any(|&s| s != 0.0),
                "{context}: reference output is silent"
            );
            let input_frames: usize = stream.recordings[recording]
                .iter()
                .map(|block| block.len() / channels)
                .sum();
            let real = real_output_frames(rate, input_frames, frame_samples);
            assert_equivalent(expected, actual, real, frame_samples, &context);
        }
    }
}

#[test]
fn f32_input_matches_handy() {
    check_format::<f32>("f32");
}

#[test]
fn i16_input_matches_handy() {
    check_format::<i16>("i16");
}

#[test]
fn i32_input_matches_handy() {
    check_format::<i32>("i32");
}

#[test]
fn u8_input_matches_handy() {
    check_format::<u8>("u8");
}

#[test]
fn i8_input_matches_handy() {
    check_format::<i8>("i8");
}
