//! FROZEN REFERENCE. Do not edit.
//!
//! Handy's capture processing at commit 8f9cf53c
//! (src-tauri/src/audio_toolkit/audio/), the baseline the equivalence test
//! (`recorder/equivalence.rs`) holds the library to. It must keep describing
//! what Handy shipped, not what the library does now, so it is never
//! refactored or "fixed" along with the library.
//!
//! Copied verbatim except:
//! - `write_input_to_ring` is a free function (in Handy it is an associated
//!   function of `AudioRecorder`) and is bounded by `dasp_sample::Sample`,
//!   which is what `cpal::Sample` re-exports (dasp_sample 0.11.0 in Handy's
//!   Cargo.lock), rather than by cpal's traits.
//! - `FrameResampler`'s unit tests are omitted; they are ported in
//!   `resampler.rs`.
//!
//! Versions: rubato 0.16.2 and dasp_sample 0.11.0, as in Handy. These are
//! currently the library's own dependencies. When the library upgrades
//! either, this module must keep the old version through a renamed
//! dev-dependency (for example `rubato_handy = { package = "rubato",
//! version = "=0.16.2" }`).

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use dasp_sample::{FromSample, Sample};
use rtrb::Producer;

// ---- recorder.rs (8f9cf53c) ------------------------------------------------

/// Atomics shared by the callback and consumer; audio uses a wait-free SPSC ring.
/// The callback must remain allocation-, lock-, logging-, and blocking-free.
#[derive(Default)]
pub struct CaptureTransportState {
    pub pause_requested: AtomicBool,
    /// Set after forwarding a pause's boundary block; subsequent callbacks
    /// remain silent until the consumer clears the request.
    pub pause_acknowledged: AtomicBool,
    pub overrun_samples: AtomicU64,
}

/// Real-time callback body. Keep this allocation-free, wait-free, and free
/// of locks, logging, clocks, and system calls.
pub fn write_input_to_ring<T>(
    data: &[T],
    channels: usize,
    use_channel: Option<usize>,
    producer: &mut Producer<f32>,
    transport: &CaptureTransportState,
) where
    T: Sample + Copy,
    f32: FromSample<T>,
{
    // Forward the first block that observes a pause; once acknowledged,
    // remain silent until the consumer resumes capture.
    if transport.pause_requested.load(Ordering::Acquire)
        && transport.pause_acknowledged.load(Ordering::Acquire)
    {
        return;
    }

    let frame_count = data.len() / channels;
    let writable_frames = producer.slots().min(frame_count);
    let written = if writable_frames == 0 {
        0
    } else {
        let chunk = producer
            .write_chunk_uninit(writable_frames)
            .expect("the producer just reported this many writable slots");
        if channels == 1 {
            chunk.fill_from_iter(
                data.iter()
                    .take(writable_frames)
                    .map(|&sample| sample.to_sample::<f32>()),
            )
        } else if let Some(channel) = use_channel {
            chunk.fill_from_iter(
                data.chunks_exact(channels)
                    .take(writable_frames)
                    .map(|frame| frame[channel].to_sample::<f32>()),
            )
        } else {
            chunk.fill_from_iter(data.chunks_exact(channels).take(writable_frames).map(
                |frame| {
                    frame
                        .iter()
                        .map(|&sample| sample.to_sample::<f32>())
                        .sum::<f32>()
                        / channels as f32
                },
            ))
        }
    };
    debug_assert_eq!(written, writable_frames);

    let dropped = frame_count - written;
    if dropped > 0 {
        transport
            .overrun_samples
            .fetch_add(dropped as u64, Ordering::Relaxed);
    }

    // Publish the boundary write before acknowledging, including when the
    // pause request arrives during the write.
    acknowledge_pause_after_write(transport);
}

fn acknowledge_pause_after_write(transport: &CaptureTransportState) {
    if transport.pause_requested.load(Ordering::Acquire) {
        transport.pause_acknowledged.store(true, Ordering::Release);
    }
}

// ---- resampler.rs (8f9cf53c) -----------------------------------------------

use rubato::{FftFixedIn, Resampler};
use std::time::Duration;

// Make this a constant you can tweak
const RESAMPLER_CHUNK_SIZE: usize = 1024;

pub struct FrameResampler {
    resampler: Option<FftFixedIn<f32>>,
    chunk_in: usize,
    in_buf: Vec<f32>,
    frame_samples: usize,
    pending: Vec<f32>,
    in_hz: usize,
    out_hz: usize,
    /// Samples in/out of the inner resampler; `finish()` uses the pair to
    /// know how much real audio (~10-30ms) its delay line still holds.
    in_count: usize,
    out_count: usize,
}

impl FrameResampler {
    pub fn new(in_hz: usize, out_hz: usize, frame_dur: Duration) -> Self {
        let frame_samples = ((out_hz as f64 * frame_dur.as_secs_f64()).round()) as usize;
        assert!(frame_samples > 0, "frame duration too short");

        // Use fixed chunk size instead of GCD-based
        let chunk_in = RESAMPLER_CHUNK_SIZE;

        let resampler = (in_hz != out_hz).then(|| {
            FftFixedIn::<f32>::new(in_hz, out_hz, chunk_in, 1, 1)
                .expect("Failed to create resampler")
        });

        Self {
            resampler,
            chunk_in,
            in_buf: Vec::with_capacity(chunk_in),
            frame_samples,
            pending: Vec::with_capacity(frame_samples),
            in_hz,
            out_hz,
            in_count: 0,
            out_count: 0,
        }
    }

    pub fn push(&mut self, mut src: &[f32], mut emit: impl FnMut(&[f32])) {
        if self.resampler.is_none() {
            self.emit_frames(src, &mut emit);
            return;
        }
        self.in_count += src.len();

        while !src.is_empty() {
            let space = self.chunk_in - self.in_buf.len();
            let take = space.min(src.len());
            self.in_buf.extend_from_slice(&src[..take]);
            src = &src[take..];

            if self.in_buf.len() == self.chunk_in {
                // let start = std::time::Instant::now();
                if let Ok(out) = self
                    .resampler
                    .as_mut()
                    .unwrap()
                    .process(&[&self.in_buf[..]], None)
                {
                    // let duration = start.elapsed();
                    // log::debug!("Resampler took: {:?}", duration);
                    self.out_count += out[0].len();
                    self.emit_frames(&out[0], &mut emit);
                }
                self.in_buf.clear();
            }
        }
    }

    pub fn finish(&mut self, mut emit: impl FnMut(&[f32])) {
        if self.resampler.is_some() {
            // Process any remaining input samples (padded internally).
            if !self.in_buf.is_empty() {
                let result = self
                    .resampler
                    .as_mut()
                    .unwrap()
                    .process_partial(Some(&[&self.in_buf[..]]), None);
                if let Ok(out) = result {
                    self.out_count += out[0].len();
                    self.emit_frames(&out[0], &mut emit);
                }
                // Drop the consumed input: a full in_buf would satisfy the
                // next push()'s chunk check immediately, re-processing this
                // padded tail into the following recording.
                self.in_buf.clear();
            }

            // Output lags input by output_delay() samples, so all real audio
            // has emerged only once in*ratio + delay samples are out. Feed
            // zero chunks until then, trimming the synthetic remainder.
            if self.in_count > 0 {
                let delay = self.resampler.as_ref().unwrap().output_delay();
                let expected = self.in_count * self.out_hz / self.in_hz + delay;
                let mut rounds = 0;
                while self.out_count < expected && rounds < 8 {
                    rounds += 1;
                    let result = self
                        .resampler
                        .as_mut()
                        .unwrap()
                        .process_partial::<&[f32]>(None, None);
                    match result {
                        Ok(out) => {
                            let take = (expected - self.out_count).min(out[0].len());
                            self.out_count += take;
                            self.emit_frames(&out[0][..take], &mut emit);
                        }
                        Err(_) => break,
                    }
                }
            }
        }

        // Emit any remaining pending frame (padded with zeros)
        if !self.pending.is_empty() {
            self.pending.resize(self.frame_samples, 0.0);
            emit(&self.pending);
            self.pending.clear();
        }
    }

    /// Clear all internal buffers so the next `push()` starts from a clean state.
    ///
    /// Call this between recordings to prevent stale audio from the previous
    /// session leaking into the start of the next one via the FFT overlap buffers.
    pub fn reset(&mut self) {
        self.in_buf.clear();
        self.pending.clear();
        self.in_count = 0;
        self.out_count = 0;
        if let Some(ref mut resampler) = self.resampler {
            resampler.reset();
        }
    }

    fn emit_frames(&mut self, mut data: &[f32], emit: &mut impl FnMut(&[f32])) {
        while !data.is_empty() {
            let space = self.frame_samples - self.pending.len();
            let take = space.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];

            if self.pending.len() == self.frame_samples {
                emit(&self.pending);
                self.pending.clear();
            }
        }
    }
}

