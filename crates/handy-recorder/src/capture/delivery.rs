//! The delivery thread: drains the ring, resamples, cuts exact chunks, and
//! runs the application's sink.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, atomic::Ordering, mpsc},
    time::{Duration, Instant},
};

use rtrb::Consumer;

use super::FrameResampler;
use super::engine::Shared;
use super::transport::drain_available_samples;
use crate::{AudioChunk, EndReason, Error, ErrorKind, Sink, Stopped};

const CONSUMER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_DRAIN_CHUNK: Duration = Duration::from_millis(50);

pub(crate) enum DeliveryCmd<S> {
    /// Begin a recording into this sink. Carries the send time for logging.
    Start(S, Instant),
    /// End the recording and send it back.
    Stop(mpsc::Sender<Stopped<S>>),
    /// Discard any recording and exit.
    Shutdown,
}

/// What to do with a chunk drained from the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkDisposition {
    /// Process as active recording audio, including during the final stop drain.
    Capture,
    /// Consume idle audio without processing it.
    Discard,
}

/// Everything the delivery thread needs, built and validated at open.
pub(crate) struct DeliveryPipeline {
    pub consumer: Consumer<f32>,
    pub resampler: FrameResampler,
    /// Device rate: the ring's rate.
    pub in_sample_rate: u32,
    /// K: samples per frame in the ring and in every chunk.
    pub channels: usize,
    pub out_sample_rate: u32,
    pub pause_ack_timeout: Duration,
}

pub(crate) fn run<S: Sink>(
    pipeline: DeliveryPipeline,
    shared: Arc<Shared>,
    cmd_rx: mpsc::Receiver<DeliveryCmd<S>>,
) {
    let DeliveryPipeline {
        mut consumer,
        resampler,
        in_sample_rate,
        channels,
        out_sample_rate,
        pause_ack_timeout,
    } = pipeline;
    let max_drain_samples =
        ((in_sample_rate as u128 * MAX_DRAIN_CHUNK.as_millis()) / 1_000).max(1) as usize * channels;
    let mut processor = Processor {
        shared,
        resampler,
        channels,
        in_sample_rate,
        out_sample_rate,
        max_drain_samples,
        pause_ack_timeout,
        stream_running_at: Instant::now(),
        first_audio_logged: false,
        active: None,
    };

    loop {
        processor.beat();
        // Avoid sleeping with queued audio; check commands before each bounded
        // drain so Stop cannot sit behind a multi-second backlog.
        let mut command = if consumer.slots() > 0 {
            match cmd_rx.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        } else {
            match cmd_rx.recv_timeout(CONSUMER_POLL_INTERVAL) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        };

        loop {
            if let Some(cmd) = command.take() {
                match cmd {
                    DeliveryCmd::Start(sink, sent_at) => {
                        processor.begin(sink, sent_at, consumer.slots() > 0)
                    }
                    DeliveryCmd::Stop(reply_tx) => {
                        if let Some(stopped) = processor.stop(&mut consumer) {
                            let _ = reply_tx.send(stopped);
                        }
                    }
                    DeliveryCmd::Shutdown => {
                        processor
                            .shared
                            .transport
                            .pause_requested
                            .store(true, Ordering::Release);
                        if processor.active.take().is_some() {
                            log::debug!("recorder closed during a recording; discarded it");
                        }
                        return;
                    }
                }
            }

            command = match cmd_rx.try_recv() {
                Ok(command) => Some(command),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            };
        }

        // A failure ends the active recording at the failure point: deliver
        // what was captured before it, flush the tail, and go back to
        // discarding while the recording waits for its `stop`.
        if let Some(error) = processor.shared.failure()
            && processor.capturing()
        {
            processor.end_on_failure(&mut consumer, error);
        }

        let disposition = if processor.capturing() {
            ChunkDisposition::Capture
        } else {
            ChunkDisposition::Discard
        };
        let _drained = processor.drain(&mut consumer, disposition);
        #[cfg(test)]
        processor
            .shared
            .transport
            .frames_drained
            .fetch_add(_drained / channels, Ordering::Release);

        let overrun_frames = processor
            .shared
            .transport
            .overrun_frames
            .swap(0, Ordering::AcqRel);
        if processor.capturing() {
            processor.observe_overrun(overrun_frames);
        }
    }
}

/// One recording in progress: the lent sink and its statistics.
struct Active<S> {
    sink: S,
    /// Set when the recording ended before `stop`: a failure or a sink panic.
    ended: Option<EndReason>,
    dropped_frames: u64,
    overrun_episodes: u32,
    started: Instant,
    first_chunk_after: Option<Duration>,
    /// Frames drained from the ring into this recording.
    input_frames: u64,
    /// Real frames delivered to the sink (the sum of `valid_frames`).
    output_frames: u64,
    /// Exact digital silence: a real microphone always has a noise floor,
    /// so long runs of exact zeros mean denied access, a muted device, or
    /// a stale stream. Reported, never acted on.
    zero_run: u64,
    longest_zero_run: u64,
    heard_nonzero: bool,
}

impl<S> Active<S> {
    fn observe_silence(&mut self, raw: &[f32], channels: usize) {
        for frame in raw.chunks_exact(channels) {
            if frame.iter().all(|&s| s == 0.0) {
                self.zero_run += 1;
            } else {
                self.longest_zero_run = self.longest_zero_run.max(self.zero_run);
                self.zero_run = 0;
                self.heard_nonzero = true;
            }
        }
    }
}

struct Processor<S> {
    shared: Arc<Shared>,
    resampler: FrameResampler,
    channels: usize,
    in_sample_rate: u32,
    out_sample_rate: u32,
    max_drain_samples: usize,
    pause_ack_timeout: Duration,
    stream_running_at: Instant,
    first_audio_logged: bool,
    active: Option<Active<S>>,
}

impl<S: Sink> Processor<S> {
    fn beat(&self) {
        self.shared.heartbeat.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether drained audio belongs to a recording.
    fn capturing(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.ended.is_none())
    }

    fn begin(&mut self, sink: S, sent_at: Instant, audio_queued: bool) {
        log::debug!(
            "start processed {:?} after it was sent; the recording begins with {} audio",
            sent_at.elapsed(),
            if audio_queued {
                "the queued"
            } else {
                "the next"
            }
        );
        // Ignore overruns accumulated while the stream was idle; only
        // active-recording loss is relevant.
        self.shared
            .transport
            .overrun_frames
            .store(0, Ordering::Release);
        self.shared.xruns.store(0, Ordering::Relaxed);
        self.resampler.reset();
        self.active = Some(Active {
            sink,
            // A failure that raced `start` ends the recording at once.
            ended: self.shared.failure().map(EndReason::RecorderFailed),
            dropped_frames: 0,
            overrun_episodes: 0,
            started: Instant::now(),
            first_chunk_after: None,
            input_frames: 0,
            output_frames: 0,
            zero_run: 0,
            longest_zero_run: 0,
            heard_nonzero: false,
        });
        #[cfg(test)]
        self.shared
            .transport
            .starts_applied
            .fetch_add(1, Ordering::Release);
    }

    /// Drain up to one bounded chunk from the ring. Returns the number of
    /// samples consumed so callers can tell an empty ring from a busy one.
    fn drain(&mut self, consumer: &mut Consumer<f32>, disposition: ChunkDisposition) -> usize {
        self.drain_at_most(consumer, disposition, self.max_drain_samples)
    }

    fn drain_at_most(
        &mut self,
        consumer: &mut Consumer<f32>,
        disposition: ChunkDisposition,
        max_samples: usize,
    ) -> usize {
        self.beat();
        let max_samples = max_samples.min(self.max_drain_samples);
        let channels = self.channels;
        drain_available_samples(consumer, max_samples, channels, |raw| {
            self.process_raw_chunk(raw, disposition)
        })
    }

    fn process_raw_chunk(&mut self, raw: &[f32], disposition: ChunkDisposition) {
        if !self.first_audio_logged {
            self.first_audio_logged = true;
            log::debug!(
                "first audio arrived {:?} after the stream started",
                self.stream_running_at.elapsed()
            );
        }

        if disposition == ChunkDisposition::Discard {
            return;
        }
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.input_frames += (raw.len() / self.channels) as u64;
        active.observe_silence(raw, self.channels);
        let format = (self.out_sample_rate, self.channels as u16);
        let result = self.resampler.push(raw, |samples, valid| {
            deliver(active, samples, valid, format)
        });
        if let Err(e) = result {
            self.fail_processing(e.to_string());
        }
    }

    /// Account for frames the callback could not fit into the ring during
    /// the active recording. Warns once per recording.
    fn observe_overrun(&mut self, frames: u64) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if frames == 0 {
            return;
        }
        active.dropped_frames = active.dropped_frames.saturating_add(frames);
        active.overrun_episodes += 1;
        if active.overrun_episodes == 1 {
            log::warn!(
                "microphone capture ring dropped {frames} frames; continuing the active recording"
            );
        }
    }

    /// A resampling failure is a library bug: the pipeline can no longer be
    /// trusted, so the recorder fails and the recording ends.
    fn fail_processing(&mut self, detail: String) {
        let error = self
            .shared
            .fail(self.shared.error(ErrorKind::Processing).with_detail(detail));
        if let Some(active) = self.active.as_mut() {
            active.ended.get_or_insert(EndReason::RecorderFailed(error));
        }
    }

    /// Flush the resampler tail into the sink.
    fn finish_tail(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let format = (self.out_sample_rate, self.channels as u16);
        let result = self
            .resampler
            .finish(|samples, valid| deliver(active, samples, valid, format));
        if let Err(e) = result {
            self.fail_processing(e.to_string());
        }
    }

    /// Drains the audio in the ring now, not audio written while draining:
    /// a failed stream that is not yet torn down can keep writing faster
    /// than a slow sink consumes, and the drain must still end.
    fn drain_all(&mut self, consumer: &mut Consumer<f32>) {
        let mut remaining = consumer.slots();
        while remaining > 0 {
            let drained = self.drain_at_most(consumer, ChunkDisposition::Capture, remaining);
            if drained == 0 {
                break;
            }
            remaining -= drained;
        }
        let overrun = self
            .shared
            .transport
            .overrun_frames
            .swap(0, Ordering::AcqRel);
        self.observe_overrun(overrun);
    }

    fn end_on_failure(&mut self, consumer: &mut Consumer<f32>, error: Error) {
        self.drain_all(consumer);
        self.finish_tail();
        if let Some(active) = self.active.as_mut() {
            active.ended.get_or_insert(EndReason::RecorderFailed(error));
        }
    }

    /// Ends the recording and hands it back. `None` if there is none.
    fn stop(&mut self, consumer: &mut Consumer<f32>) -> Option<Stopped<S>> {
        self.active.as_ref()?;
        if self.capturing() {
            let overrun = self
                .shared
                .transport
                .overrun_frames
                .swap(0, Ordering::AcqRel);
            self.observe_overrun(overrun);

            // Request a pause that forwards one boundary block, then drain
            // all audio committed before the acknowledgement. A failed
            // recorder's stream is gone, so no acknowledgement will come.
            let transport = &self.shared.transport;
            let mut acknowledged = false;
            if self.shared.failure().is_none() {
                transport.pause_acknowledged.store(false, Ordering::Relaxed);
                transport.pause_requested.store(true, Ordering::Release);
                let pause_started = Instant::now();
                loop {
                    if self
                        .shared
                        .transport
                        .pause_acknowledged
                        .load(Ordering::Acquire)
                    {
                        acknowledged = true;
                        break;
                    }
                    if self.shared.failure().is_some() {
                        break;
                    }
                    if pause_started.elapsed() >= self.pause_ack_timeout {
                        // Callbacks have stopped: the stream stalled.
                        let error = self.shared.error(ErrorKind::Stalled).with_detail(format!(
                            "the audio callback did not run for {:.1} s while stopping",
                            self.pause_ack_timeout.as_secs_f64()
                        ));
                        log::warn!("{error}");
                        self.shared.fail(error);
                        break;
                    }
                    let drained = self.drain(consumer, ChunkDisposition::Capture);
                    if drained == 0 {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }

            // Everything still in the ring, including the boundary block,
            // belongs to this recording.
            self.drain_all(consumer);
            self.finish_tail();

            if acknowledged {
                // Resume before stop() returns so an immediate recording
                // cannot lose its first callback to this pause request.
                let transport = &self.shared.transport;
                transport.pause_acknowledged.store(false, Ordering::Relaxed);
                transport.pause_requested.store(false, Ordering::Release);
            }
        }

        let failure = self.shared.failure();
        let active = self.active.take()?;
        let longest_zero_run = active.longest_zero_run.max(active.zero_run);
        let end_reason = active.ended.unwrap_or(match failure {
            Some(error) => EndReason::RecorderFailed(error),
            None => EndReason::StopCalled,
        });
        let stopped = Stopped {
            sink: active.sink,
            end_reason,
            dropped_frames: active.dropped_frames,
        };
        log::debug!(
            "recording stopped ({:?}) after {:?}: {} frames drained at {} Hz, {} frames delivered at {} Hz, \
             {} dropped in {} overrun episodes, first chunk after {:?}",
            stopped.end_reason,
            active.started.elapsed(),
            active.input_frames,
            self.in_sample_rate,
            active.output_frames,
            self.out_sample_rate,
            stopped.dropped_frames,
            active.overrun_episodes,
            active.first_chunk_after,
        );
        let rate = self.in_sample_rate as f64;
        if active.input_frames > 0 && !active.heard_nonzero {
            log::warn!(
                "the whole recording ({:.1} s) was exact digital silence: microphone access denied, a muted device, or a stale stream",
                active.input_frames as f64 / rate
            );
        } else if longest_zero_run >= self.in_sample_rate as u64 {
            log::warn!(
                "the recording contained {:.1} s of exact digital silence: a muted device or a stalled stream",
                longest_zero_run as f64 / rate
            );
        }
        // REVIEW(xruns): reported in the log only; `is_complete` does not
        // see them yet.
        let xruns = self.shared.xruns.swap(0, Ordering::Relaxed);
        if xruns > 0 {
            log::warn!(
                "the platform reported {xruns} xruns during the recording: audio may be missing \
                 before the library received it, not counted in dropped_frames"
            );
        }
        if !stopped.is_complete() {
            log::warn!(
                "incomplete recording: {:?}, {} frames dropped",
                stopped.end_reason,
                stopped.dropped_frames
            );
        }
        Some(stopped)
    }
}

/// Hands one chunk to the sink, unless the recording has ended. A panic is
/// caught: the sink is not called again and the recording ends.
fn deliver<S: Sink>(active: &mut Active<S>, samples: &[f32], valid: usize, format: (u32, u16)) {
    if active.ended.is_some() {
        return;
    }
    let chunk = AudioChunk {
        samples,
        sample_rate: format.0,
        channels: format.1,
        valid_frames: valid,
    };
    let sink = &mut active.sink;
    match catch_unwind(AssertUnwindSafe(|| sink.process_chunk(chunk))) {
        Ok(()) => {
            if active.first_chunk_after.is_none() {
                active.first_chunk_after = Some(active.started.elapsed());
            }
            active.output_frames += valid as u64;
        }
        Err(payload) => {
            let message = panic_message(payload.as_ref());
            log::error!("the sink panicked; ending the recording: {message}");
            active.ended = Some(EndReason::SinkPanicked(message));
        }
    }
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::Active;
    use std::time::Instant;

    fn active() -> Active<()> {
        Active {
            sink: (),
            ended: None,
            dropped_frames: 0,
            overrun_episodes: 0,
            started: Instant::now(),
            first_chunk_after: None,
            input_frames: 0,
            output_frames: 0,
            zero_run: 0,
            longest_zero_run: 0,
            heard_nonzero: false,
        }
    }

    #[test]
    fn silence_tracking_counts_runs_of_all_zero_frames() {
        let mut a = active();
        a.observe_silence(&[0.0; 8], 2);
        assert!(!a.heard_nonzero);
        assert_eq!(a.zero_run, 4);

        // A frame with any nonzero channel ends the run.
        a.observe_silence(&[0.0, 1e-9, 0.0, 0.0], 2);
        assert!(a.heard_nonzero);
        assert_eq!(a.longest_zero_run, 4);
        assert_eq!(a.zero_run, 1);

        // Runs continue across drained chunks.
        a.observe_silence(&[0.0; 20], 2);
        assert_eq!(a.zero_run, 11);
    }
}
