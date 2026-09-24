use std::{
    io::Error,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

use rtrb::{Consumer, Producer, RingBuffer};

use super::FrameResampler;
use crate::backend::{
    cpal::CpalBackend, Backend, BackendError, BackendErrorKind, InputData, InputSample, InputStream, OpenDevice,
};
use crate::InputDevice;

/// Handy's `constants::WHISPER_SAMPLE_RATE`: the rate every frame is resampled to.
const OUTPUT_SAMPLE_RATE: u32 = 16000;

enum Cmd {
    /// Begin capturing. Carries the send timestamp so the consumer can log how
    /// long the command sat in the channel, plus a one-shot first-sample acknowledgement.
    Start(Instant, mpsc::Sender<()>),
    Stop(mpsc::Sender<Vec<f32>>),
    Shutdown,
}

// Two seconds of ring capacity absorbs consumer stalls without adding latency
// during normal 10 ms drains.
const AUDIO_RING_SECONDS: usize = 2;
const CONSUMER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_DRAIN_CHUNK: Duration = Duration::from_millis(50);
const PAUSE_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// Atomics shared by the callback and consumer; audio uses a wait-free SPSC ring.
/// The callback must remain allocation-, lock-, logging-, and blocking-free.
#[derive(Default)]
struct CaptureTransportState {
    pause_requested: AtomicBool,
    /// Set after forwarding a pause's boundary block; subsequent callbacks
    /// remain silent until the consumer clears the request.
    pause_acknowledged: AtomicBool,
    overrun_samples: AtomicU64,
    /// Test-only: Start commands the consumer has applied. Lets a test write
    /// a recording's first block only once it cannot be discarded as idle
    /// audio (the consumer may drain between its command check and a Start
    /// sent just after it).
    #[cfg(test)]
    starts_applied: std::sync::atomic::AtomicUsize,
    /// Test-only: samples drained outside a stop (a stop always empties the
    /// ring), so a test can wait until idle audio has been discarded.
    #[cfg(test)]
    frames_drained: std::sync::atomic::AtomicUsize,
}

/// Callback invoked with each 16 kHz mono frame while recording. Used to feed
/// a live streaming transcription as audio arrives.
pub type AudioFrameCallback = Arc<dyn Fn(&[f32]) + Send + Sync + 'static>;

/// What the worker builds before running the consumer: the stream, the
/// device it opened, the device rate, and the ring's read side.
type OpenedStream = (Box<dyn InputStream>, InputDevice, u32, Consumer<f32>);

/// Handy's frame size when no VAD is attached: 30 ms at 16 kHz.
const DEFAULT_FRAME_SAMPLES: usize = (OUTPUT_SAMPLE_RATE * 30 / 1000) as usize;

pub struct AudioRecorder {
    backend: Arc<dyn Backend>,
    device: Option<InputDevice>,
    cmd_tx: Option<mpsc::Sender<Cmd>>,
    worker_handle: Option<std::thread::JoinHandle<()>>,
    /// Samples per emitted 16 kHz frame. Handy sizes frames for its VAD
    /// backend; here the caller chooses.
    frame_samples: usize,
    audio_cb: Option<AudioFrameCallback>,
    /// Which input channel to use. None = average all (original behavior).
    selected_channel: Option<usize>,
    /// Set by the backend when the active input stream can no longer capture.
    stream_error: Arc<AtomicBool>,
    /// Test-only: the open stream's transport, so tests can follow the
    /// consumer through Start and the stop handshake.
    #[cfg(test)]
    transport: Option<Arc<CaptureTransportState>>,
}

impl AudioRecorder {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        Ok(Self::with_backend(CpalBackend::shared()))
    }

    pub(crate) fn with_backend(backend: Arc<dyn Backend>) -> Self {
        AudioRecorder {
            backend,
            device: None,
            cmd_tx: None,
            worker_handle: None,
            frame_samples: DEFAULT_FRAME_SAMPLES,
            audio_cb: None,
            selected_channel: None,
            stream_error: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            transport: None,
        }
    }

    /// Size of each emitted 16 kHz frame, in samples. Replaces Handy's
    /// `with_vad`, which sized frames for the VAD backend.
    pub fn with_frame_samples(mut self, frame_samples: usize) -> Self {
        assert!(frame_samples > 0, "frame size must be non-zero");
        self.frame_samples = frame_samples;
        self
    }

    /// Register a callback that receives real-time 16 kHz frames.
    /// Frames arrive in real time, in order, on the
    /// recorder's consumer thread — keep the callback cheap (e.g. forward to a
    /// channel) so it never stalls capture.
    pub fn with_audio_callback<F>(mut self, cb: F) -> Self
    where
        F: Fn(&[f32]) + Send + Sync + 'static,
    {
        self.audio_cb = Some(Arc::new(cb));
        self
    }

    pub fn with_selected_channel(mut self, channel: Option<u16>) -> Self {
        self.set_selected_channel(channel);
        self
    }

    pub fn set_selected_channel(&mut self, channel: Option<u16>) {
        self.selected_channel = channel.map(usize::from);
    }

    /// Opens `device` (an `InputDevice::id`), or the system default for
    /// `None`. Handy takes a `cpal::Device` here.
    pub fn open(&mut self, device: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
        if self.worker_handle.is_some() {
            if !self.needs_reopen() {
                return Ok(()); // already open
            }
            log::warn!("Capture stream failed; rebuilding microphone stream");
            self.close()?;
        }

        self.stream_error.store(false, Ordering::Relaxed);

        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<InputDevice, BackendError>>(1);

        let backend = Arc::clone(&self.backend);
        let device_id = device.map(str::to_owned);
        let frame_samples = self.frame_samples;
        // Move the optional real-time audio frame callback into the worker thread
        let audio_cb = self.audio_cb.clone();
        let selected_channel = self.selected_channel;
        let stream_error = Arc::clone(&self.stream_error);
        let transport = Arc::new(CaptureTransportState::default());
        let worker_transport = Arc::clone(&transport);

        let worker = std::thread::spawn(move || {
            let transport = worker_transport;
            let init_result = (|| -> Result<OpenedStream, BackendError> {
                let device = backend.open_device(device_id.as_deref())?;
                let info = device.info().clone();
                let format = device.format();
                let sample_rate = format.sample_rate;
                let channels = format.channels as usize;

                log::info!(
                    "Using device: {:?}\nSample rate: {}\nChannels: {}\nFormat: {:?}",
                    info.name,
                    sample_rate,
                    channels,
                    format.sample_format
                );

                if let Some(channel) = selected_channel {
                    if channel < channels {
                        log::info!("Using selected input channel: {}", channel + 1);
                    } else {
                        log::warn!(
                            "Selected input channel {} is out of range for a {}-channel device; averaging all channels instead",
                            channel + 1,
                            channels
                        );
                    }
                } else {
                    log::info!("Averaging all {} input channels", channels);
                }

                let (stream, sample_consumer) = AudioRecorder::build_stream(
                    device,
                    channels,
                    selected_channel,
                    Arc::clone(&transport),
                    Arc::clone(&stream_error),
                )?;

                Ok((stream, info, sample_rate, sample_consumer))
            })();

            match init_result {
                Ok((stream, info, sample_rate, sample_consumer)) => {
                    let _ = init_tx.send(Ok(info));
                    // Timestamp for the play()-returned -> first-samples gap the
                    // init handshake can't see (hardware dependent).
                    let stream_running_at = Instant::now();
                    let processor = CaptureProcessor::new(
                        sample_rate,
                        frame_samples,
                        audio_cb,
                        stream_running_at,
                    );
                    run_consumer(
                        processor,
                        sample_consumer,
                        cmd_rx,
                        transport,
                        Arc::clone(&stream_error),
                    );
                    drop(stream);
                }
                Err(error) => {
                    log::error!("{error}");
                    let _ = init_tx.send(Err(error));
                }
            }
        });

        match init_rx.recv() {
            Ok(Ok(device)) => {
                self.device = Some(device);
                self.cmd_tx = Some(cmd_tx);
                self.worker_handle = Some(worker);
                #[cfg(test)]
                {
                    self.transport = Some(transport);
                }
                Ok(())
            }
            Ok(Err(error)) => {
                let _ = worker.join();
                // Handy matches the message; the backend's classification also
                // catches platforms whose message says neither (CoreAudio's
                // is "Unauthorized").
                let error_message = error.to_string();
                let kind = if error.kind == BackendErrorKind::PermissionDenied
                    || is_microphone_access_denied(&error_message)
                {
                    std::io::ErrorKind::PermissionDenied
                } else {
                    std::io::ErrorKind::Other
                };
                Err(Box::new(Error::new(kind, error_message)))
            }
            Err(recv_error) => {
                let _ = worker.join();
                Err(Box::new(Error::other(format!(
                    "Failed to initialize microphone worker: {recv_error}"
                ))))
            }
        }
    }

    /// Queue a recording start and return a one-shot receiver that resolves only
    /// after the first real microphone sample chunk has entered the capture path.
    /// `Stream::play()` returning is not sufficient: some Bluetooth and USB
    /// devices take much longer to begin delivering callbacks.
    pub fn start(&self) -> Result<mpsc::Receiver<()>, Box<dyn std::error::Error>> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| Error::other("Recorder is not open"))?;
        let (ready_tx, ready_rx) = mpsc::channel();
        tx.send(Cmd::Start(Instant::now(), ready_tx))?;
        Ok(ready_rx)
    }

    pub fn stop(&self) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
        let tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| Error::other("Recorder is not open"))?;
        let (resp_tx, resp_rx) = mpsc::channel();
        tx.send(Cmd::Stop(resp_tx))?;
        Ok(resp_rx.recv()?)
    }

    /// True when the active capture stream must be rebuilt.
    ///
    /// cpal may report a device disconnect asynchronously without closing its
    /// callback channel, so also honor the error callback's explicit flag.
    pub fn needs_reopen(&self) -> bool {
        self.stream_error.load(Ordering::Relaxed)
            || self
                .worker_handle
                .as_ref()
                .is_some_and(|handle| handle.is_finished())
    }

    pub fn close(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(Cmd::Shutdown);
        }
        if let Some(handle) = self.worker_handle.take() {
            let _ = handle.join();
        }
        self.device = None;
        Ok(())
    }

    fn build_stream(
        device: Box<dyn OpenDevice>,
        channels: usize,
        selected_channel: Option<usize>,
        transport: Arc<CaptureTransportState>,
        stream_error: Arc<AtomicBool>,
    ) -> Result<(Box<dyn InputStream>, Consumer<f32>), BackendError> {
        let ring_capacity = device.format().sample_rate as usize * AUDIO_RING_SECONDS;
        let (mut sample_producer, mut sample_consumer) = RingBuffer::new(ring_capacity);

        // Touch rtrb's uninitialized pages before the stream starts to reduce
        // callback page faults. This does not pin them.
        {
            let chunk = sample_producer
                .write_chunk(ring_capacity)
                .expect("new audio ring has its full capacity available");
            chunk.commit_all();
        }
        {
            let chunk = sample_consumer
                .read_chunk(ring_capacity)
                .expect("pre-filled audio ring is readable");
            chunk.commit_all();
        }

        // Resolve the effective channel to use. If the selected channel is
        // out of range for this device, fall back to averaging all channels.
        let use_channel = selected_channel.filter(|&channel| channel < channels);
        let callback_transport = Arc::clone(&transport);
        let write = move |data: InputData<'_>| {
            let producer = &mut sample_producer;
            let transport = &callback_transport;
            match data {
                InputData::U8(d) => Self::write_input_to_ring(d, channels, use_channel, producer, transport),
                InputData::I8(d) => Self::write_input_to_ring(d, channels, use_channel, producer, transport),
                InputData::I16(d) => Self::write_input_to_ring(d, channels, use_channel, producer, transport),
                InputData::I32(d) => Self::write_input_to_ring(d, channels, use_channel, producer, transport),
                InputData::F32(d) => Self::write_input_to_ring(d, channels, use_channel, producer, transport),
            }
        };

        let stream = device.start(
            Box::new(write),
            Box::new(move |_err: BackendError| {
                // Error callbacks may share the platform audio thread. Defer
                // logging and recovery to the consumer/manager path.
                stream_error.store(true, Ordering::Release);
            }),
        )?;
        Ok((stream, sample_consumer))
    }

    /// Real-time callback body. Keep this allocation-free, wait-free, and free
    /// of locks, logging, clocks, and system calls.
    fn write_input_to_ring<T>(
        data: &[T],
        channels: usize,
        use_channel: Option<usize>,
        producer: &mut Producer<f32>,
        transport: &CaptureTransportState,
    ) where
        T: InputSample,
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
                        .map(|&sample| sample.to_f32()),
                )
            } else if let Some(channel) = use_channel {
                chunk.fill_from_iter(
                    data.chunks_exact(channels)
                        .take(writable_frames)
                        .map(|frame| frame[channel].to_f32()),
                )
            } else {
                chunk.fill_from_iter(data.chunks_exact(channels).take(writable_frames).map(
                    |frame| {
                        frame
                            .iter()
                            .map(|&sample| sample.to_f32())
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
}

fn acknowledge_pause_after_write(transport: &CaptureTransportState) {
    if transport.pause_requested.load(Ordering::Acquire) {
        transport.pause_acknowledged.store(true, Ordering::Release);
    }
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

/// Route one 16 kHz frame to recording and live outputs. Handy runs VAD here;
/// VAD stays in Handy, so every frame is emitted (Handy's no-VAD path).
/// Kept free-standing to permit disjoint borrows around resampler callbacks.
fn handle_frame(samples: &[f32], audio_cb: &Option<AudioFrameCallback>, out_buf: &mut Vec<f32>) {
    out_buf.extend_from_slice(samples);
    if let Some(cb) = audio_cb {
        cb(samples);
    }
}

fn drain_available_samples(
    consumer: &mut Consumer<f32>,
    max_samples: usize,
    mut process: impl FnMut(&[f32]),
) -> usize {
    let available = consumer.slots().min(max_samples);
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

/// What to do with a chunk drained from the ring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChunkDisposition {
    /// Process as active recording audio, including during the final stop drain.
    Capture,
    /// Consume idle audio without processing it.
    Discard,
}

/// Converts raw ring samples into 16 kHz frames across recording sessions.
/// Ring transport stays outside to avoid conflicting borrows during drains.
struct CaptureProcessor {
    // ---- stream-scoped: fixed for the life of the input stream ---------- //
    in_sample_rate: u32,
    audio_cb: Option<AudioFrameCallback>,
    stream_running_at: Instant,
    frame_resampler: FrameResampler,
    max_drain_samples: usize,
    first_chunk_logged: bool,

    // ---- recording-scoped: reset by `begin_recording` ------------------- //
    processed_samples: Vec<f32>,
    awaiting_first_captured_chunk: Option<Instant>,
    capture_ready_tx: Option<mpsc::Sender<()>>,
    total_dropped_samples: u64,
    overrun_warning_logged: bool,
}

impl CaptureProcessor {
    fn new(
        in_sample_rate: u32,
        frame_samples: usize,
        audio_cb: Option<AudioFrameCallback>,
        stream_running_at: Instant,
    ) -> Self {
        // Resample into fixed-size frames so a fixed-size consumer (a VAD)
        // never sees a partial frame.
        let frame_duration =
            Duration::from_secs_f64(frame_samples as f64 / OUTPUT_SAMPLE_RATE as f64);
        let frame_resampler = FrameResampler::new(
            in_sample_rate as usize,
            OUTPUT_SAMPLE_RATE as usize,
            frame_duration,
        );

        let max_drain_samples =
            ((in_sample_rate as u128 * MAX_DRAIN_CHUNK.as_millis()) / 1_000).max(1) as usize;

        Self {
            in_sample_rate,
            audio_cb,
            stream_running_at,
            frame_resampler,
            max_drain_samples,
            first_chunk_logged: false,
            processed_samples: Vec::new(),
            awaiting_first_captured_chunk: None,
            capture_ready_tx: None,
            total_dropped_samples: 0,
            overrun_warning_logged: false,
        }
    }

    /// Reset per-recording state and arm the first-sample acknowledgement.
    fn begin_recording(&mut self, ready_tx: mpsc::Sender<()>) {
        self.awaiting_first_captured_chunk = Some(Instant::now());
        self.capture_ready_tx = Some(ready_tx);
        self.total_dropped_samples = 0;
        self.overrun_warning_logged = false;
        self.processed_samples.clear();
        self.frame_resampler.reset();
    }

    /// Drop a pending first-sample acknowledgement. If Stop was queued before
    /// the first chunk, this prevents a stale ready UI event or start chime.
    fn cancel_ready_signal(&mut self) {
        self.capture_ready_tx = None;
        self.awaiting_first_captured_chunk = None;
    }

    /// Drain up to one bounded chunk from the ring. Returns the number of
    /// samples consumed so callers can tell an empty ring from a busy one.
    fn drain(&mut self, consumer: &mut Consumer<f32>, disposition: ChunkDisposition) -> usize {
        let max_samples = self.max_drain_samples;
        drain_available_samples(consumer, max_samples, |raw| {
            self.process_raw_chunk(raw, disposition)
        })
    }

    fn process_raw_chunk(&mut self, raw: &[f32], disposition: ChunkDisposition) {
        let chunk_ms = raw.len() as f64 * 1000.0 / self.in_sample_rate as f64;
        if !self.first_chunk_logged {
            self.first_chunk_logged = true;
            log::debug!(
                "first audio samples arrived {:?} after stream start ({:.1}ms drained)",
                self.stream_running_at.elapsed(),
                chunk_ms
            );
        }

        if disposition == ChunkDisposition::Discard {
            return;
        }

        self.frame_resampler.push(raw, |frame: &[f32]| {
            handle_frame(frame, &self.audio_cb, &mut self.processed_samples)
        });

        if let Some(started) = self.awaiting_first_captured_chunk.take() {
            log::debug!(
                "first captured samples ({:.1}ms) processed {:?} after Cmd::Start",
                chunk_ms,
                started.elapsed()
            );
        }
        if let Some(ready_tx) = self.capture_ready_tx.take() {
            // Silence still counts: readiness means the host is delivering samples,
            // not that VAD has detected speech.
            let _ = ready_tx.send(());
        }
    }

    /// Account for samples the callback could not fit into the ring during
    /// the active recording. Warns once per recording.
    fn observe_overrun(&mut self, samples: u64) {
        if samples == 0 {
            return;
        }

        self.total_dropped_samples = self.total_dropped_samples.saturating_add(samples);
        if !self.overrun_warning_logged {
            self.overrun_warning_logged = true;
            log::warn!(
                "Microphone capture ring dropped {samples} samples; continuing the active recording"
            );
        }
    }

    /// Flush the resampler tail and hand back the finished recording.
    fn finish_recording(&mut self) -> Vec<f32> {
        self.frame_resampler.finish(|frame: &[f32]| {
            handle_frame(frame, &self.audio_cb, &mut self.processed_samples)
        });

        if self.total_dropped_samples > 0 {
            log::warn!(
                "Active recording completed after dropping {} microphone samples",
                self.total_dropped_samples
            );
        }
        std::mem::take(&mut self.processed_samples)
    }
}

fn run_consumer(
    mut processor: CaptureProcessor,
    mut sample_consumer: Consumer<f32>,
    cmd_rx: mpsc::Receiver<Cmd>,
    transport: Arc<CaptureTransportState>,
    stream_error: Arc<AtomicBool>,
) {
    let mut recording = false;
    let mut stream_error_logged = false;

    loop {
        // Avoid sleeping with queued audio; check commands before each bounded
        // drain so Stop cannot sit behind a multi-second backlog.
        let mut command = if sample_consumer.slots() > 0 {
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
                    Cmd::Start(sent_at, ready_tx) => {
                        log::debug!(
                            "Cmd::Start processed {:?} after send; capture begins with {} samples",
                            sent_at.elapsed(),
                            if sample_consumer.slots() > 0 {
                                "the in-flight"
                            } else {
                                "the next available"
                            }
                        );
                        // Ignore overruns accumulated while the always-on stream
                        // was idle; only active-capture loss is relevant.
                        transport.overrun_samples.store(0, Ordering::Release);
                        processor.begin_recording(ready_tx);
                        recording = true;
                        #[cfg(test)]
                        transport.starts_applied.fetch_add(1, Ordering::Release);
                    }
                    Cmd::Stop(reply_tx) => {
                        processor
                            .observe_overrun(transport.overrun_samples.swap(0, Ordering::AcqRel));
                        recording = false;
                        processor.cancel_ready_signal();

                        // Request a pause that forwards one boundary block, then drain
                        // all audio committed before the acknowledgement.
                        transport.pause_acknowledged.store(false, Ordering::Relaxed);
                        transport.pause_requested.store(true, Ordering::Release);
                        let pause_started = Instant::now();
                        while !transport.pause_acknowledged.load(Ordering::Acquire)
                            && pause_started.elapsed() < PAUSE_ACK_TIMEOUT
                        {
                            let drained =
                                processor.drain(&mut sample_consumer, ChunkDisposition::Capture);
                            if drained == 0 {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                        }

                        let pause_timed_out = !transport.pause_acknowledged.load(Ordering::Acquire);
                        if pause_timed_out {
                            log::warn!("Timed out waiting for the microphone callback to pause");
                            // Preserve the existing recovery model: finish this
                            // stop, then rebuild the stream on the next start.
                            stream_error.store(true, Ordering::Release);
                        }

                        // Everything still in the ring, including the boundary
                        // block, belongs to this recording.
                        while processor.drain(&mut sample_consumer, ChunkDisposition::Capture) > 0 {
                        }

                        // Include drops that raced with the pause request.
                        processor
                            .observe_overrun(transport.overrun_samples.swap(0, Ordering::AcqRel));
                        let samples = processor.finish_recording();
                        if !pause_timed_out {
                            // Resume before stop() returns so an immediate recording
                            // cannot lose its first callback to this pause request.
                            transport.pause_acknowledged.store(false, Ordering::Relaxed);
                            transport.pause_requested.store(false, Ordering::Release);
                        }
                        let _ = reply_tx.send(samples);

                        if pause_timed_out {
                            return;
                        }
                    }
                    Cmd::Shutdown => {
                        transport.pause_requested.store(true, Ordering::Release);
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

        let disposition = if recording {
            ChunkDisposition::Capture
        } else {
            ChunkDisposition::Discard
        };
        let _drained = processor.drain(&mut sample_consumer, disposition);
        #[cfg(test)]
        transport.frames_drained.fetch_add(_drained, Ordering::Release);

        let overrun_samples = transport.overrun_samples.swap(0, Ordering::AcqRel);
        if recording {
            processor.observe_overrun(overrun_samples);
        }

        // The CPAL error callback only sets an atomic; log here and rebuild the
        // stream on the next start.
        if stream_error.load(Ordering::Acquire) && !stream_error_logged {
            log::error!("Microphone backend reported a stream error; it will be rebuilt");
            stream_error_logged = true;
        }
    }
}

#[cfg(test)]
mod equivalence;
#[cfg(test)]
mod seam_tests;
#[cfg(test)]
mod tests;
