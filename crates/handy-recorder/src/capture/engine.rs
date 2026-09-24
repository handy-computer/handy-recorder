//! The recorder engine: opens the device on its own thread, starts the
//! delivery thread, supervises both, and implements start, stop, and close.
//!
//! Threads per open recorder:
//! - device thread: creates, owns, and drops the platform stream; runs the
//!   watchdog; turns stream errors into failures. Never runs application
//!   code, so it can always close the stream.
//! - delivery thread (`delivery.rs`): drains the ring and runs the sink.
//! - notification thread: short-lived, calls the failure handler once.

use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use rtrb::RingBuffer;

use super::FrameResampler;
use super::delivery::{self, DeliveryCmd, DeliveryPipeline};
use super::transport::{
    CaptureTransportState, Routing, is_microphone_access_denied, is_no_input_device_error,
    write_input_to_ring,
};
use crate::backend::{Backend, BackendError, BackendErrorKind, InputData, InputStream};
use crate::{
    Channels, Error, ErrorKind, Format, InputDevice, Permission, RecorderConfig, RecorderInfo,
    Sink, StartError, Stopped,
};

// Two seconds of ring capacity absorbs consumer stalls without adding latency
// during normal 10 ms drains.
const AUDIO_RING_SECONDS: usize = 2;

/// Output rates outside this range are `UnsupportedFormat`.
// TODO(review): see TODO.md, "Format limits".
const MIN_OUTPUT_RATE: u32 = 1_000;
const MAX_OUTPUT_RATE: u32 = 768_000;
/// Chunks longer than this are `UnsupportedFormat`.
const MAX_CHUNK_SECONDS: usize = 10;

/// Every internal bound. Not configurable by applications; tests shorten
/// them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timeouts {
    /// `open` waits this long for the platform to build and start the stream.
    pub open: Duration,
    /// `close` waits this long for the platform to tear the stream down.
    pub close: Duration,
    /// `close` waits this long for the delivery thread to leave the sink.
    pub delivery_exit: Duration,
    /// A device that delivers no audio for this long after `open` fails with
    /// `NoAudio`. Generous: Bluetooth devices can take seconds.
    pub no_audio: Duration,
    /// A device that delivered audio and then made no callback for this long
    /// fails with `Stalled`.
    pub stall: Duration,
    /// A delivery thread (usually a sink) that makes no progress for this
    /// long fails the recorder with `SinkStalled`.
    pub heartbeat: Duration,
    /// `stop` waits this long for the callback to acknowledge the pause
    /// (Handy's value); otherwise the stream stalled.
    pub pause_ack: Duration,
    /// `stop`'s overall deadline. Must exceed `pause_ack`, so a stopped
    /// callback is classified as `Stalled` before it expires.
    pub stop: Duration,
    /// How often the device thread's watchdog checks progress.
    pub watchdog_tick: Duration,
}

// REVIEW(timeouts): generous placeholders, to be set from the tier-3 hardware
// probes (DESIGN.md, "Before the first release").
impl Default for Timeouts {
    fn default() -> Self {
        Self {
            open: Duration::from_secs(10),
            close: Duration::from_secs(5),
            delivery_exit: Duration::from_secs(1),
            no_audio: Duration::from_secs(10),
            stall: Duration::from_secs(5),
            heartbeat: Duration::from_secs(10),
            pause_ack: Duration::from_secs(2),
            stop: Duration::from_secs(5),
            watchdog_tick: Duration::from_millis(50),
        }
    }
}

impl Timeouts {
    fn validate(&self) {
        assert!(
            self.stop > self.pause_ack,
            "stop's deadline must exceed the pause-acknowledgement bound"
        );
    }
}

/// State shared by the caller, the callback, and both threads.
pub(crate) struct Shared {
    pub transport: CaptureTransportState,
    /// The recorder's failure. First writer wins; never cleared.
    failure: OnceLock<Error>,
    /// The opened device, for error context.
    device: OnceLock<InputDevice>,
    /// When the stream started, for error context.
    started_at: OnceLock<Instant>,
    /// Advanced by the delivery thread on every loop and drain.
    pub heartbeat: AtomicU64,
    /// Platform errors the stream survived (xruns, real-time denied).
    pub survived_errors: AtomicU64,
    device_tx: mpsc::Sender<DeviceMsg>,
}

impl Shared {
    fn new(device_tx: mpsc::Sender<DeviceMsg>) -> Self {
        Self {
            transport: CaptureTransportState::default(),
            failure: OnceLock::new(),
            device: OnceLock::new(),
            started_at: OnceLock::new(),
            heartbeat: AtomicU64::new(0),
            survived_errors: AtomicU64::new(0),
            device_tx,
        }
    }

    pub fn failure(&self) -> Option<Error> {
        self.failure.get().cloned()
    }

    /// An error of `kind` carrying the device and the time into the stream.
    pub fn error(&self, kind: ErrorKind) -> Error {
        let mut error = Error::new(kind);
        if let Some(device) = self.device.get() {
            error = error.with_device(device.clone());
        }
        if let Some(started) = self.started_at.get() {
            error = error.with_elapsed(started.elapsed());
        }
        error
    }

    /// Fails the recorder, unless it already failed. Returns the recorder's
    /// failure: `error`, or the one that came first.
    pub fn fail(&self, error: Error) -> Error {
        if self.failure.set(error).is_ok() {
            let _ = self.device_tx.send(DeviceMsg::Failed);
        }
        self.failure.get().cloned().expect("failure was just set")
    }
}

enum DeviceMsg {
    /// A platform error from the stream's error callback.
    StreamError(BackendError),
    /// The recorder failed; tear down.
    Failed,
    /// Close: tear down and exit.
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenState {
    Pending,
    Done,
    /// `open` gave up waiting; a late stream must release itself.
    Cancelled,
}

type Handler = Box<dyn FnOnce(Error) + Send + 'static>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Idle,
    Recording,
    /// A `stop` is collecting the recording.
    Stopping,
}

struct Threads {
    device: JoinHandle<()>,
    /// Disconnects when the device thread exits.
    device_exit: mpsc::Receiver<()>,
    delivery: JoinHandle<()>,
    /// Disconnects when the delivery thread exits.
    delivery_exit: mpsc::Receiver<()>,
    device_tx: mpsc::Sender<DeviceMsg>,
}

pub(crate) struct Engine<S> {
    pub(crate) shared: Arc<Shared>,
    backend: Arc<dyn Backend>,
    info: RecorderInfo,
    delivery_tx: mpsc::Sender<DeliveryCmd<S>>,
    slot: Mutex<Slot>,
    threads: Mutex<Option<Threads>>,
    timeouts: Timeouts,
}

/// What the device thread hands back from a successful open.
struct Opened {
    info: RecorderInfo,
    pipeline: DeliveryPipeline,
}

impl<S: Sink> Engine<S> {
    pub fn open(
        backend: Arc<dyn Backend>,
        config: RecorderConfig,
        handler: Option<Handler>,
        timeouts: Timeouts,
    ) -> Result<Self, Error> {
        timeouts.validate();
        validate_request(&config)?;

        let (device_tx, device_rx) = mpsc::channel();
        let shared = Arc::new(Shared::new(device_tx.clone()));
        let (init_tx, init_rx) = mpsc::sync_channel::<Result<Opened, Error>>(1);
        let open_state = Arc::new(Mutex::new(OpenState::Pending));
        let (device_exit_tx, device_exit) = mpsc::channel::<()>();

        let engine_backend = Arc::clone(&backend);
        let device = {
            let shared = Arc::clone(&shared);
            let open_state = Arc::clone(&open_state);
            let error_tx = device_tx.clone();
            thread::Builder::new()
                .name("handy-recorder-device".into())
                .spawn(move || {
                    let _exit = device_exit_tx;
                    let opened = open_stream(&*backend, &config, &shared, error_tx, &timeouts);
                    let mut state = open_state.lock().unwrap();
                    match opened {
                        Ok((stream, opened)) => {
                            if *state == OpenState::Cancelled {
                                drop(state);
                                log::warn!(
                                    "the platform finished opening a stream after open timed out; releasing it"
                                );
                                drop(stream);
                                log::info!("released the late-opened stream");
                                return;
                            }
                            *state = OpenState::Done;
                            let _ = init_tx.send(Ok(opened));
                            drop(state);
                            run_device(stream, shared, device_rx, timeouts, handler);
                        }
                        Err(error) => {
                            *state = OpenState::Done;
                            let _ = init_tx.send(Err(error));
                        }
                    }
                })
                .map_err(|e| {
                    Error::new(ErrorKind::Backend).with_detail(format!("cannot spawn a thread: {e}"))
                })?
        };

        let result = match init_rx.recv_timeout(timeouts.open) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let mut state = open_state.lock().unwrap();
                if *state == OpenState::Done {
                    // Finished just as the wait expired.
                    drop(state);
                    init_rx.recv().expect("the device thread sent its result")
                } else {
                    *state = OpenState::Cancelled;
                    drop(state);
                    let error = shared.error(ErrorKind::OpenTimedOut).with_detail(format!(
                        "no result within {:.1} s",
                        timeouts.open.as_secs_f64()
                    ));
                    log::warn!(
                        "{error}; the opening thread is detached and will release the stream if the platform call ever completes"
                    );
                    return Err(error);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::new(ErrorKind::Processing)
                .with_detail("the device thread exited during open")),
        };
        let Opened { info, pipeline } = match result {
            Ok(opened) => opened,
            Err(error) => {
                let _ = device.join();
                log::warn!("open failed: {error}");
                return Err(error);
            }
        };

        let (delivery_tx, delivery_rx) = mpsc::channel();
        let (delivery_exit_tx, delivery_exit) = mpsc::channel::<()>();
        let delivery = {
            let shared = Arc::clone(&shared);
            thread::Builder::new()
                .name("handy-recorder-delivery".into())
                .spawn(move || {
                    let _exit = delivery_exit_tx;
                    delivery::run(pipeline, shared, delivery_rx);
                })
        };
        let delivery = match delivery {
            Ok(delivery) => delivery,
            Err(e) => {
                let _ = device_tx.send(DeviceMsg::Close);
                return Err(shared
                    .error(ErrorKind::Backend)
                    .with_detail(format!("cannot spawn a thread: {e}")));
            }
        };

        log::info!(
            "opened {} ({}) at {} Hz, {} ch; delivering {} Hz, {} ch, {}-frame chunks",
            info.device.name,
            info.device.backend,
            info.device_format.sample_rate,
            info.device_format.channels,
            info.format.sample_rate,
            info.format.channels,
            info.frames_per_chunk
        );

        Ok(Self {
            shared,
            backend: engine_backend,
            info,
            delivery_tx,
            slot: Mutex::new(Slot::Idle),
            threads: Mutex::new(Some(Threads {
                device,
                device_exit,
                delivery,
                delivery_exit,
                device_tx,
            })),
            timeouts,
        })
    }

    pub fn info(&self) -> &RecorderInfo {
        &self.info
    }

    pub fn start(&self, sink: S) -> Result<(), StartError<S>> {
        let mut slot = self.slot.lock().unwrap();
        if *slot != Slot::Idle {
            return Err(StartError {
                error: Error::new(ErrorKind::AlreadyRecording),
                sink,
            });
        }
        if let Some(error) = self.shared.failure() {
            return Err(StartError { error, sink });
        }
        // Access revoked while the recorder was open: macOS keeps the stream
        // running and delivers exact zeros, so the recorder fails instead.
        if self.backend.permission_status() == Permission::Denied {
            let error = self.shared.fail(permission_denied(
                self.shared.error(ErrorKind::PermissionDenied),
            ));
            return Err(StartError { error, sink });
        }
        if let Err(mpsc::SendError(cmd)) = self
            .delivery_tx
            .send(DeliveryCmd::Start(sink, Instant::now()))
        {
            let DeliveryCmd::Start(sink, _) = cmd else {
                unreachable!("the unsent command is the start just sent")
            };
            let error = self.shared.fail(
                self.shared
                    .error(ErrorKind::Processing)
                    .with_detail("the delivery thread exited"),
            );
            return Err(StartError { error, sink });
        }
        *slot = Slot::Recording;
        Ok(())
    }

    pub fn stop(&self) -> Result<Stopped<S>, Error> {
        {
            let mut slot = self.slot.lock().unwrap();
            if *slot != Slot::Recording {
                return Err(Error::new(ErrorKind::NotRecording));
            }
            *slot = Slot::Stopping;
        }
        let result = self.collect();
        *self.slot.lock().unwrap() = Slot::Idle;
        result
    }

    fn collect(&self) -> Result<Stopped<S>, Error> {
        // A stalled sink holds the recording on an unresponsive thread.
        if let Some(error) = self.shared.failure()
            && error.kind() == ErrorKind::SinkStalled
        {
            return Err(error);
        }
        let (reply_tx, reply_rx) = mpsc::channel();
        if self.delivery_tx.send(DeliveryCmd::Stop(reply_tx)).is_err() {
            return Err(self.shared.fail(
                self.shared
                    .error(ErrorKind::Processing)
                    .with_detail("the delivery thread exited"),
            ));
        }
        match reply_rx.recv_timeout(self.timeouts.stop) {
            Ok(stopped) => Ok(stopped),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let error =
                    self.shared
                        .fail(
                            self.shared
                                .error(ErrorKind::SinkStalled)
                                .with_detail(format!(
                                    "stop did not complete within {:.1} s",
                                    self.timeouts.stop.as_secs_f64()
                                )),
                        );
                log::error!("recording lost: {error}");
                Err(error)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // The delivery thread already failed (a stalled sink the
                // heartbeat caught) or exited.
                Err(self.shared.failure().unwrap_or_else(|| {
                    self.shared.fail(
                        self.shared
                            .error(ErrorKind::Processing)
                            .with_detail("the delivery thread exited"),
                    )
                }))
            }
        }
    }
}

impl<S> Engine<S> {
    /// Close: discard any recording, tear the stream down, and wait (bounded)
    /// for both threads. Idempotent.
    pub fn shutdown(&self) -> Result<(), Error> {
        let Some(threads) = self.threads.lock().unwrap().take() else {
            return Ok(());
        };
        let _ = self.delivery_tx.send(DeliveryCmd::Shutdown);
        let _ = threads.device_tx.send(DeviceMsg::Close);
        let current = thread::current().id();

        let result = match threads.device_exit.recv_timeout(self.timeouts.close) {
            Err(mpsc::RecvTimeoutError::Disconnected) | Ok(()) => {
                let _ = threads.device.join();
                Ok(())
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let error = self
                    .shared
                    .error(ErrorKind::CloseTimedOut)
                    .with_detail(format!(
                        "the platform did not release the stream within {:.1} s",
                        self.timeouts.close.as_secs_f64()
                    ));
                log::warn!(
                    "{error}; the device thread is detached and the stream may stay open until the process exits"
                );
                Err(error)
            }
        };

        // Dropping the last handle from inside the sink runs this on the
        // delivery thread itself, which must not wait for itself.
        if threads.delivery.thread().id() == current {
            log::debug!("closed from inside the sink; the delivery thread exits after it returns");
        } else {
            match threads
                .delivery_exit
                .recv_timeout(self.timeouts.delivery_exit)
            {
                Err(mpsc::RecvTimeoutError::Disconnected) | Ok(()) => {
                    let _ = threads.delivery.join();
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    log::warn!("the delivery thread is still inside the sink; it is detached")
                }
            }
        }
        if result.is_ok() {
            log::debug!("recorder closed");
        }
        result
    }
}

/// Checks what can be checked before knowing the device.
fn validate_request(config: &RecorderConfig) -> Result<(), Error> {
    if let Some(rate) = config.sample_rate
        && !(MIN_OUTPUT_RATE..=MAX_OUTPUT_RATE).contains(&rate)
    {
        return Err(
            Error::new(ErrorKind::UnsupportedFormat).with_detail(format!(
                "sample_rate {rate} Hz is outside {MIN_OUTPUT_RATE}..={MAX_OUTPUT_RATE} Hz"
            )),
        );
    }
    if config.frames_per_chunk == Some(0) {
        return Err(Error::new(ErrorKind::UnsupportedFormat)
            .with_detail("frames_per_chunk must be at least 1"));
    }
    Ok(())
}

/// The default chunk: about 10 ms.
fn default_frames_per_chunk(sample_rate: u32) -> usize {
    ((sample_rate as usize + 50) / 100).max(1)
}

fn permission_denied(error: Error) -> Error {
    error.with_detail(
        "microphone access is denied for this app (macOS: System Settings > Privacy & Security > Microphone)",
    )
}

/// Maps a platform error during open.
fn open_error(error: BackendError, device: Option<&InputDevice>) -> Error {
    let message = error.message.to_string();
    let kind = if error.kind == BackendErrorKind::PermissionDenied
        || is_microphone_access_denied(&message)
    {
        ErrorKind::PermissionDenied
    } else if error.kind == BackendErrorKind::DeviceNotAvailable
        || is_no_input_device_error(&message)
    {
        ErrorKind::DeviceUnavailable
    } else if error.kind == BackendErrorKind::DeviceBusy {
        ErrorKind::DeviceBusy
    } else {
        ErrorKind::Backend
    };
    let error = Error::new(kind).with_detail(message);
    match device {
        Some(device) => error.with_device(device.clone()),
        None => error,
    }
}

/// Maps a platform error reported while the stream runs.
fn runtime_error(shared: &Shared, error: BackendError) -> Error {
    let kind = match error.kind {
        BackendErrorKind::DeviceNotAvailable => ErrorKind::DeviceLost,
        BackendErrorKind::StreamInvalidated | BackendErrorKind::DeviceChanged => {
            ErrorKind::StreamInvalidated
        }
        BackendErrorKind::PermissionDenied => ErrorKind::PermissionDenied,
        _ => ErrorKind::Backend,
    };
    shared.error(kind).with_detail(error.message.into_owned())
}

/// Runs on the device thread: resolves the device, validates and builds the
/// whole pipeline, then builds and starts the stream.
fn open_stream(
    backend: &dyn Backend,
    config: &RecorderConfig,
    shared: &Arc<Shared>,
    error_tx: mpsc::Sender<DeviceMsg>,
    timeouts: &Timeouts,
) -> Result<(Box<dyn InputStream>, Opened), Error> {
    let device = backend
        .open_device(config.device.as_deref())
        .map_err(|e| open_error(e, None))?;
    let info = device.info().clone();
    let _ = shared.device.set(info.clone());
    // macOS opens a denied microphone and delivers exact zeros; say so
    // instead of recording silence.
    if backend.permission_status() == Permission::Denied {
        return Err(permission_denied(shared.error(ErrorKind::PermissionDenied)));
    }
    let device_format = device.format();
    let channels = device_format.channels as usize;
    let in_rate = device_format.sample_rate;
    if channels == 0 || in_rate == 0 {
        return Err(shared.error(ErrorKind::Backend).with_detail(format!(
            "the device reports {channels} channels at {in_rate} Hz"
        )));
    }

    let routing = match config.channels {
        Channels::All => Routing::All,
        Channels::MixToMono => Routing::MixToMono,
        Channels::Only(n) if (n as usize) < channels => Routing::Only(n as usize),
        Channels::Only(n) => {
            return Err(shared.error(ErrorKind::InvalidChannel).with_detail(format!(
                "channel {n} requested; the device has {channels} (numbered from 0)"
            )));
        }
    };
    let k = routing.output_channels(channels);
    let out_rate = config.sample_rate.unwrap_or(in_rate);
    let frames_per_chunk = config
        .frames_per_chunk
        .unwrap_or_else(|| default_frames_per_chunk(out_rate));
    if frames_per_chunk > out_rate as usize * MAX_CHUNK_SECONDS {
        return Err(shared.error(ErrorKind::UnsupportedFormat).with_detail(format!(
            "frames_per_chunk {frames_per_chunk} is more than {MAX_CHUNK_SECONDS} s at {out_rate} Hz"
        )));
    }
    let resampler = FrameResampler::new(in_rate as usize, out_rate as usize, frames_per_chunk, k)
        .map_err(|e| {
        shared
            .error(ErrorKind::UnsupportedFormat)
            .with_detail(format!(
                "cannot resample {in_rate} Hz to {out_rate} Hz: {e}"
            ))
    })?;

    // Capacity is a whole number of K-sample frames, so reads and writes
    // stay frame-aligned across wraparound.
    let ring_capacity = in_rate as usize * AUDIO_RING_SECONDS * k;
    let (mut producer, mut consumer) = RingBuffer::new(ring_capacity);

    // Touch rtrb's uninitialized pages before the stream starts to reduce
    // callback page faults. This does not pin them.
    {
        let chunk = producer
            .write_chunk(ring_capacity)
            .expect("new audio ring has its full capacity available");
        chunk.commit_all();
    }
    {
        let chunk = consumer
            .read_chunk(ring_capacity)
            .expect("pre-filled audio ring is readable");
        chunk.commit_all();
    }

    let callback_shared = Arc::clone(shared);
    let data = move |data: InputData<'_>| {
        let producer = &mut producer;
        let transport = &callback_shared.transport;
        match data {
            InputData::U8(d) => write_input_to_ring(d, channels, routing, producer, transport),
            InputData::I8(d) => write_input_to_ring(d, channels, routing, producer, transport),
            InputData::I16(d) => write_input_to_ring(d, channels, routing, producer, transport),
            InputData::I32(d) => write_input_to_ring(d, channels, routing, producer, transport),
            InputData::F32(d) => write_input_to_ring(d, channels, routing, producer, transport),
        }
    };
    let error_shared = Arc::clone(shared);
    let error = move |error: BackendError| {
        // May run on the platform audio thread. Survivable errors are only
        // counted; others go to the device thread, which fails the recorder.
        if error.kind.stream_survives() {
            error_shared.survived_errors.fetch_add(1, Ordering::Relaxed);
        } else {
            let _ = error_tx.send(DeviceMsg::StreamError(error));
        }
    };

    let started = Instant::now();
    let stream = device
        .start(Box::new(data), Box::new(error))
        .map_err(|e| open_error(e, Some(&info)))?;
    let _ = shared.started_at.set(Instant::now());
    log::debug!("stream built and started in {:?}", started.elapsed());

    let format = Format {
        sample_rate: out_rate,
        channels: k as u16,
    };
    Ok((
        stream,
        Opened {
            info: RecorderInfo {
                device: info,
                device_format: Format {
                    sample_rate: in_rate,
                    channels: device_format.channels,
                },
                format,
                frames_per_chunk,
            },
            pipeline: DeliveryPipeline {
                consumer,
                resampler,
                in_sample_rate: in_rate,
                channels: k,
                out_sample_rate: out_rate,
                pause_ack_timeout: timeouts.pause_ack,
            },
        },
    ))
}

/// The device thread after a successful open: holds the stream, watches for
/// failures, tears down, and notifies.
fn run_device(
    stream: Box<dyn InputStream>,
    shared: Arc<Shared>,
    device_rx: mpsc::Receiver<DeviceMsg>,
    timeouts: Timeouts,
    mut handler: Option<Handler>,
) {
    let mut stream = Some(stream);
    let mut watchdog = Watchdog::new(Instant::now());
    loop {
        match device_rx.recv_timeout(timeouts.watchdog_tick) {
            Ok(DeviceMsg::StreamError(error)) => {
                if stream.is_some() {
                    let error = runtime_error(&shared, error);
                    shared.fail(error);
                }
            }
            Ok(DeviceMsg::Failed) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(DeviceMsg::Close) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                teardown(&mut stream);
                return;
            }
        }

        if stream.is_some() && shared.failure.get().is_none() {
            watchdog.check(&shared, &timeouts);
        }

        if let Some(error) = shared.failure()
            && stream.is_some()
        {
            log::error!("recorder failed: {error}");
            teardown(&mut stream);
            if let Some(handler) = handler.take() {
                notify(handler, error);
            }
        }
    }
}

fn teardown(stream: &mut Option<Box<dyn InputStream>>) {
    if let Some(stream) = stream.take() {
        let started = Instant::now();
        drop(stream);
        log::debug!("stream released in {:?}", started.elapsed());
    }
}

/// Calls the failure handler on its own short-lived thread, so a slow or
/// blocking handler delays nothing but itself.
fn notify(handler: Handler, error: Error) {
    let spawned = thread::Builder::new()
        .name("handy-recorder-notify".into())
        .spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || handler(error)));
            if result.is_err() {
                log::error!("the failure handler panicked");
            }
        });
    if let Err(e) = spawned {
        log::error!("cannot spawn the failure notification thread: {e}");
    }
}

/// Detects the failures nothing reports: no audio after open, callbacks
/// stopping, and a delivery thread that stopped making progress.
struct Watchdog {
    opened_at: Instant,
    callbacks: u64,
    last_callback_at: Instant,
    heartbeat: u64,
    last_heartbeat_at: Instant,
}

impl Watchdog {
    fn new(now: Instant) -> Self {
        Self {
            opened_at: now,
            callbacks: 0,
            last_callback_at: now,
            heartbeat: 0,
            last_heartbeat_at: now,
        }
    }

    // TODO(review): see TODO.md, "Watchdog false positives".
    fn check(&mut self, shared: &Shared, timeouts: &Timeouts) {
        let now = Instant::now();
        let callbacks = shared.transport.callbacks.load(Ordering::Relaxed);
        if callbacks != self.callbacks {
            self.callbacks = callbacks;
            self.last_callback_at = now;
        }
        let heartbeat = shared.heartbeat.load(Ordering::Relaxed);
        if heartbeat != self.heartbeat {
            self.heartbeat = heartbeat;
            self.last_heartbeat_at = now;
        }

        let error = if callbacks == 0 {
            let waited = now - self.opened_at;
            (waited >= timeouts.no_audio).then(|| {
                shared.error(ErrorKind::NoAudio).with_detail(format!(
                    "no audio {:.1} s after the stream started (bound {:.1} s)",
                    waited.as_secs_f64(),
                    timeouts.no_audio.as_secs_f64()
                ))
            })
        } else {
            let silent = now - self.last_callback_at;
            (silent >= timeouts.stall).then(|| {
                shared.error(ErrorKind::Stalled).with_detail(format!(
                    "no audio callback for {:.1} s after {callbacks} callbacks (bound {:.1} s)",
                    silent.as_secs_f64(),
                    timeouts.stall.as_secs_f64()
                ))
            })
        };
        let error = error.or_else(|| {
            let stuck = now - self.last_heartbeat_at;
            (stuck >= timeouts.heartbeat).then(|| {
                shared.error(ErrorKind::SinkStalled).with_detail(format!(
                    "the delivery thread made no progress for {:.1} s (bound {:.1} s)",
                    stuck.as_secs_f64(),
                    timeouts.heartbeat.as_secs_f64()
                ))
            })
        });
        if let Some(error) = error {
            log::warn!("watchdog tripped: {error}");
            shared.fail(error);
        }
    }
}
