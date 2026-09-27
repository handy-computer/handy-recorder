//! `NativeRecorder`: one open microphone and its event channel.

use std::sync::{Arc, Mutex};
use std::thread;

use handy_recorder::{AudioChunk, EndReason, Error, Recorder, RecorderConfig, RecorderInfo, Sink};
use napi::bindgen_prelude::{Float32Array, Function, Unknown};
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi::{Error as NapiError, Result, Status};
use napi_derive::napi;

use crate::convert::{JsConfig, JsErrorInfo, JsRecorderInfo};

/// Delivers events to the wrapper's dispatcher. Weak: it never keeps the
/// event loop alive (the wrapper does that while an operation is pending),
/// so an idle open recorder doesn't stop the process from exiting. Released
/// when the last clone drops.
pub(crate) type Events =
    Arc<ThreadsafeFunction<Event, Unknown<'static>, JsEvent, Status, false, true>>;

type Core = Recorder<JsSink>;

/// What the recorder reports, in order, on the event channel.
pub(crate) enum Event {
    Opened(RecorderInfo),
    OpenFailed(Error),
    /// One chunk's real audio (padding excluded).
    Chunk(Vec<f32>),
    /// The failure handler: the recorder failed, now or mid-recording.
    Failed(Error),
    Stopped {
        samples: Vec<f32>,
        end_reason: EndReason,
        dropped_frames: u64,
    },
    StopFailed(Error),
    Closed(Option<Error>),
}

/// An event as the dispatcher receives it. `kind` says which fields are set.
#[napi(object, object_from_js = false)]
pub struct JsEvent {
    /// `opened`, `openFailed`, `chunk`, `failed`, `stopped`, `stopFailed`,
    /// or `closed`.
    pub kind: String,
    pub info: Option<JsRecorderInfo>,
    pub error: Option<JsErrorInfo>,
    pub samples: Option<Float32Array>,
    /// `stopCalled`, `recorderFailed`, or `sinkPanicked`.
    pub end_reason: Option<String>,
    pub panic_message: Option<String>,
    pub dropped_frames: Option<f64>,
}

impl From<Event> for JsEvent {
    fn from(event: Event) -> Self {
        let mut js = JsEvent {
            kind: String::new(),
            info: None,
            error: None,
            samples: None,
            end_reason: None,
            panic_message: None,
            dropped_frames: None,
        };
        let kind = match event {
            Event::Opened(info) => {
                js.info = Some(JsRecorderInfo::from(&info));
                "opened"
            }
            Event::OpenFailed(error) => {
                js.error = Some(JsErrorInfo::from(&error));
                "openFailed"
            }
            Event::Chunk(samples) => {
                js.samples = Some(Float32Array::new(samples));
                "chunk"
            }
            Event::Failed(error) => {
                js.error = Some(JsErrorInfo::from(&error));
                "failed"
            }
            Event::Stopped {
                samples,
                end_reason,
                dropped_frames,
            } => {
                js.samples = Some(Float32Array::new(samples));
                js.dropped_frames = Some(dropped_frames as f64);
                js.end_reason = Some(
                    match end_reason {
                        EndReason::StopCalled => "stopCalled",
                        EndReason::RecorderFailed(error) => {
                            js.error = Some(JsErrorInfo::from(&error));
                            "recorderFailed"
                        }
                        EndReason::SinkPanicked(message) => {
                            js.panic_message = Some(message);
                            "sinkPanicked"
                        }
                    }
                    .into(),
                );
                "stopped"
            }
            Event::StopFailed(error) => {
                js.error = Some(JsErrorInfo::from(&error));
                "stopFailed"
            }
            Event::Closed(error) => {
                js.error = error.as_ref().map(JsErrorInfo::from);
                "closed"
            }
        };
        js.kind = kind.into();
        js
    }
}

pub(crate) fn post(events: &Events, event: Event) {
    // Unbounded and non-blocking: never waits for the event loop. Fails only
    // once the environment is closing, when nobody is listening.
    let _ = events.call(event, ThreadsafeFunctionCallMode::NonBlocking);
}

/// Receives the recording on the library's delivery thread. Never waits for
/// JavaScript: chunks are queued, and the whole recording is kept here for
/// `stop`, so a busy event loop costs memory, not audio.
pub(crate) struct JsSink {
    samples: Vec<f32>,
    collect: bool,
    /// Set when the wrapper wants each chunk.
    events: Option<Events>,
}

impl Sink for JsSink {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        let real = &chunk.samples[..chunk.valid_frames * usize::from(chunk.channels)];
        if self.collect {
            self.samples.extend_from_slice(real);
        }
        if let Some(events) = &self.events
            && !real.is_empty()
        {
            post(events, Event::Chunk(real.to_vec()));
        }
    }
}

struct State {
    /// Set once open succeeds; taken by close.
    recorder: Option<Arc<Core>>,
    /// Taken by close, so the dispatcher (and the wrapper it holds) can be
    /// collected.
    events: Option<Events>,
    /// The JavaScript object was collected, or its environment torn down.
    dropped: bool,
}

/// One microphone. Created opening; `opened` or `openFailed` follows.
#[napi]
pub struct NativeRecorder {
    state: Arc<Mutex<State>>,
    collect: bool,
    chunks: bool,
}

/// How to open: the real backend, or a test's fake one.
pub(crate) type Opener = Box<
    dyn FnOnce(
            RecorderConfig,
            Box<dyn FnOnce(Error) + Send + 'static>,
        ) -> std::result::Result<Core, Error>
        + Send,
>;

/// Starts opening a recorder on the real backend.
#[napi(js_name = "openRecorder")]
pub fn open_recorder(
    config: JsConfig,
    dispatch: Function<'_, JsEvent, Unknown<'static>>,
) -> Result<NativeRecorder> {
    open_with(
        config,
        dispatch,
        Box::new(Recorder::open_with_failure_handler),
    )
}

pub(crate) fn open_with(
    config: JsConfig,
    dispatch: Function<'_, JsEvent, Unknown<'static>>,
    opener: Opener,
) -> Result<NativeRecorder> {
    let recorder_config = config.recorder_config()?;
    let events: Events = Arc::new(
        dispatch
            .build_threadsafe_function::<Event>()
            .weak::<true>()
            .callee_handled::<false>()
            .build_callback(|ctx| Ok(JsEvent::from(ctx.value)))?,
    );

    let state = Arc::new(Mutex::new(State {
        recorder: None,
        events: Some(Arc::clone(&events)),
        dropped: false,
    }));
    let opening = Arc::clone(&state);
    thread::Builder::new()
        .name("handy-recorder-node-open".into())
        .spawn(move || {
            let failures = Arc::clone(&events);
            let handler = Box::new(move |error: Error| post(&failures, Event::Failed(error)));
            match opener(recorder_config, handler) {
                Ok(recorder) => {
                    let mut state = opening.lock().unwrap();
                    if state.dropped {
                        drop(state);
                        // Nobody will close it: close it here.
                        let _ = recorder.close();
                        return;
                    }
                    let info = recorder.info().clone();
                    state.recorder = Some(Arc::new(recorder));
                    post(&events, Event::Opened(info));
                }
                Err(error) => {
                    post(&events, Event::OpenFailed(error));
                    opening.lock().unwrap().events = None;
                }
            }
        })
        .map_err(|e| NapiError::from_reason(format!("cannot spawn a thread: {e}")))?;

    Ok(NativeRecorder {
        state,
        collect: config.collect.unwrap_or(true),
        chunks: config.chunks.unwrap_or(false),
    })
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> Result<()> {
    thread::Builder::new()
        .name(name.into())
        .spawn(f)
        .map(drop)
        .map_err(|e| NapiError::from_reason(format!("cannot spawn a thread: {e}")))
}

fn not_open() -> NapiError {
    NapiError::new(Status::GenericFailure, "the recorder is not open")
}

#[napi]
impl NativeRecorder {
    /// Starts a recording. Never waits. Returns the error, if any.
    #[napi]
    pub fn start(&self) -> Result<Option<JsErrorInfo>> {
        let (recorder, events) = {
            let state = self.state.lock().unwrap();
            match (&state.recorder, &state.events) {
                (Some(recorder), Some(events)) => (Arc::clone(recorder), Arc::clone(events)),
                _ => return Err(not_open()),
            }
        };
        let sink = JsSink {
            samples: Vec::new(),
            collect: self.collect,
            events: self.chunks.then_some(events),
        };
        Ok(recorder
            .start(sink)
            .err()
            .map(|e| JsErrorInfo::from(&e.error)))
    }

    /// Stops the recording; `stopped` or `stopFailed` follows, after every
    /// chunk of the recording.
    #[napi]
    pub fn stop(&self) -> Result<()> {
        let (recorder, events) = {
            let state = self.state.lock().unwrap();
            match (&state.recorder, &state.events) {
                (Some(recorder), Some(events)) => (Arc::clone(recorder), Arc::clone(events)),
                _ => return Err(not_open()),
            }
        };
        spawn("handy-recorder-node-stop", move || {
            let event = match recorder.stop() {
                Ok(stopped) => Event::Stopped {
                    samples: stopped.sink.samples,
                    end_reason: stopped.end_reason,
                    dropped_frames: stopped.dropped_frames,
                },
                Err(error) => Event::StopFailed(error),
            };
            post(&events, event);
        })
    }

    /// Closes the microphone; `closed` follows. Returns false if there was
    /// nothing to close (never opened, or already closed).
    #[napi]
    pub fn close(&self) -> Result<bool> {
        let (recorder, events) = {
            let mut state = self.state.lock().unwrap();
            match (state.recorder.take(), state.events.take()) {
                (Some(recorder), Some(events)) => (recorder, events),
                _ => return Ok(false),
            }
        };
        spawn("handy-recorder-node-close", move || {
            let result = match Arc::try_unwrap(recorder) {
                Ok(recorder) => recorder.close(),
                // A stop thread still holds it; the last holder's drop closes
                // it. The wrapper waits for stops, so this doesn't happen.
                Err(shared) => {
                    drop(shared);
                    Ok(())
                }
            };
            post(&events, Event::Closed(result.err()));
        })?;
        Ok(true)
    }
}

impl Drop for NativeRecorder {
    /// Collected or torn down without `close`: close off this thread, which
    /// is JavaScript's, since closing can take seconds.
    fn drop(&mut self) {
        let recorder = {
            let mut state = self.state.lock().unwrap();
            state.dropped = true;
            state.events = None;
            state.recorder.take()
        };
        if let Some(recorder) = recorder {
            let _ = spawn("handy-recorder-node-drop", move || drop(recorder));
        }
    }
}
