//! The real-time half of the capture path: the callback body and the atomics
//! it shares with the delivery thread. From Handy's `recorder.rs`, with
//! K-channel routing and a progress counter for the watchdog.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtrb::{Consumer, Producer};

use crate::backend::InputSample;

/// Atomics shared by the callback, the delivery thread, and the device
/// thread's watchdog; audio uses a wait-free SPSC ring.
/// The callback must remain allocation-, lock-, logging-, and blocking-free.
#[derive(Default)]
pub(crate) struct CaptureTransportState {
    pub pause_requested: AtomicBool,
    /// Set after forwarding a pause's boundary block; subsequent callbacks
    /// remain silent until the consumer clears the request.
    pub pause_acknowledged: AtomicBool,
    /// Frames the callback could not fit into the ring.
    pub overrun_frames: AtomicU64,
    /// Callbacks received with at least one frame, including silent ones
    /// during a pause. The watchdog's measure of progress; it never looks
    /// at amplitude.
    pub callbacks: AtomicU64,
    /// Test-only: Start commands the consumer has applied. Lets a test write
    /// a recording's first block only once it cannot be discarded as idle
    /// audio (the consumer may drain between its command check and a Start
    /// sent just after it).
    #[cfg(test)]
    pub starts_applied: std::sync::atomic::AtomicUsize,
    /// Test-only: frames drained outside a stop (a stop always empties the
    /// ring), so a test can wait until idle audio has been discarded.
    #[cfg(test)]
    pub frames_drained: std::sync::atomic::AtomicUsize,
}

/// Which device channels reach the ring. Resolved and validated at open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Routing {
    /// All device channels, interleaved.
    All,
    /// The average of all device channels.
    MixToMono,
    /// One zero-based device channel, in range.
    Only(usize),
}

impl Routing {
    /// Samples per frame in the ring: K.
    pub(crate) fn output_channels(self, device_channels: usize) -> usize {
        match self {
            Routing::All => device_channels,
            Routing::MixToMono | Routing::Only(_) => 1,
        }
    }
}

/// Real-time callback body. Keep this allocation-free, wait-free, and free
/// of locks, logging, clocks, and system calls.
///
/// Writes whole frames of `routing.output_channels(channels)` samples, so on
/// overrun it drops whole frames and never splits one across the ring.
pub(crate) fn write_input_to_ring<T>(
    data: &[T],
    channels: usize,
    routing: Routing,
    producer: &mut Producer<f32>,
    transport: &CaptureTransportState,
) where
    T: InputSample,
{
    // Only blocks carrying at least one frame count as progress, so a
    // backend that keeps calling back with empty blocks still trips the
    // watchdog.
    let frame_count = data.len() / channels;
    if frame_count > 0 {
        transport.callbacks.fetch_add(1, Ordering::Relaxed);
    }

    // Forward the first block that observes a pause; once acknowledged,
    // remain silent until the consumer resumes capture.
    if transport.pause_requested.load(Ordering::Acquire)
        && transport.pause_acknowledged.load(Ordering::Acquire)
    {
        return;
    }

    let out_channels = routing.output_channels(channels);
    let writable_frames = (producer.slots() / out_channels).min(frame_count);
    let written = if writable_frames == 0 {
        0
    } else {
        let chunk = producer
            .write_chunk_uninit(writable_frames * out_channels)
            .expect("the producer just reported this many writable slots");
        if channels == 1 {
            chunk.fill_from_iter(
                data.iter()
                    .take(writable_frames)
                    .map(|&sample| sample.to_f32()),
            )
        } else {
            match routing {
                Routing::Only(channel) => chunk.fill_from_iter(
                    data.chunks_exact(channels)
                        .take(writable_frames)
                        .map(|frame| frame[channel].to_f32()),
                ),
                Routing::MixToMono => {
                    chunk.fill_from_iter(data.chunks_exact(channels).take(writable_frames).map(
                        |frame| {
                            frame.iter().map(|&sample| sample.to_f32()).sum::<f32>()
                                / channels as f32
                        },
                    ))
                }
                Routing::All => chunk.fill_from_iter(
                    data.iter()
                        .take(writable_frames * channels)
                        .map(|&sample| sample.to_f32()),
                ),
            }
        }
    };
    debug_assert_eq!(written, writable_frames * out_channels);

    let dropped = frame_count - writable_frames;
    if dropped > 0 {
        transport
            .overrun_frames
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

/// Drains up to `max_samples` from the ring, whole frames of `channels`
/// samples only. Returns the number of samples consumed.
pub(crate) fn drain_available_samples(
    consumer: &mut Consumer<f32>,
    max_samples: usize,
    channels: usize,
    mut process: impl FnMut(&[f32]),
) -> usize {
    let available = consumer.slots().min(max_samples);
    let available = available - available % channels;
    if available == 0 {
        return 0;
    }

    let chunk = consumer
        .read_chunk(available)
        .expect("reported audio ring slots must be readable");
    let (first, second) = chunk.as_slices();
    if !first.is_empty() {
        process(first);
    }
    if !second.is_empty() {
        process(second);
    }
    chunk.commit_all();
    available
}

pub fn is_microphone_access_denied(error_message: &str) -> bool {
    let normalized = error_message.to_lowercase();
    normalized.contains("access is denied")
        || normalized.contains("permission denied")
        || normalized.contains("0x80070005")
}

pub fn is_no_input_device_error(error_message: &str) -> bool {
    let normalized = error_message.to_lowercase();
    normalized.contains("no input device found")
        || (normalized.contains("failed to fetch preferred config")
            && normalized.contains("coreaudio"))
}

#[cfg(test)]
mod tests;
