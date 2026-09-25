//! Cross-platform microphone capture.
//!
//! Handy Recorder owns the real-time audio path.
//! It gives applications audio in the
//! format they asked for, on an ordinary thread, reporting when audio was lost
//! or the stream broke.
//!
//! # Concepts
//!
//! - A [`Recorder`] when opened warms the selected microphone. When start
//!   is called, the application will begin recieving AudioChunks from the Sink
//! - A [`Sink`] is application code that receives the recording's audio as
//!   fixed-size [`AudioChunk`]s. It is lent to the library by `start` and
//!   handed back by `stop`.
//!
//! # Example
//!
//! ```no_run
//! use handy_recorder::{CollectingSink, Recorder, RecorderConfig};
//! use std::time::Duration;
//!
//! # fn main() -> Result<(), handy_recorder::Error> {
//! let recorder = Recorder::open_with_failure_handler(RecorderConfig::speech(), |error| {
//!     // Runs on a library thread. `stop` still returns the audio captured so far.
//!     eprintln!("microphone failed: {error}");
//! })?;
//!
//! recorder.start(CollectingSink::new())?;
//! std::thread::sleep(Duration::from_secs(5));
//!
//! let stopped = recorder.stop()?;
//! if !stopped.is_complete() {
//!     // The mic failed or audio was dropped. What was captured is still here.
//!     eprintln!("incomplete recording: {:?}", stopped.end_reason);
//! }
//! let samples = stopped.sink.into_samples();
//! # Ok(())
//! # }
//! ```

mod backend;
mod capture;
mod device;
mod error;
mod sink;
mod types;

#[cfg(test)]
mod tests;

pub use device::{InputDevice, Permission, list_input_devices, permission_status};
pub use error::{Error, ErrorKind};
pub use sink::{AudioChunk, CollectingSink, Sink};
pub use types::{Channels, EndReason, Format, RecorderConfig, RecorderInfo};

use std::fmt;

use backend::cpal::CpalBackend;
use capture::engine::{Engine, Timeouts};

// ---------------------------------------------------------------------------
// Recorder
// ---------------------------------------------------------------------------

/// An open microphone, recording into sinks of type `S`. Owned, `Send`,
/// `Sync`, lifetime-free: store it in a struct, or share it with an `Arc`.
///
/// Every recording on a recorder uses the same sink type. An application
/// that needs several kinds uses an enum, which `stop` hands back to match
/// on, or `Box<dyn Sink>` when its sinks forward audio elsewhere and nothing
/// needs to be read back out of them.
///
/// Dropping it is `close`.
pub struct Recorder<S> {
    pub(crate) engine: Engine<S>,
}

impl<S: Sink> Recorder<S> {
    /// Opens the device at the format the OS has it set to and builds the
    /// whole output pipeline (ring, channel routing, resampler, framer).
    ///
    /// Returns once the OS stream is started. Audio
    /// may not be flowing yet: Bluetooth devices can take seconds to deliver
    /// their first samples. A sink's first `process_chunk` call is the signal
    /// that audio is flowing. If the device never delivers audio, the
    /// recorder fails with `ErrorKind::NoAudio`.
    ///
    /// Without a failure handler, a failure surfaces only at the next `start`
    /// or `stop`. Use the following
    /// [`open_with_failure_handler`](Self::open_with_failure_handler)
    /// to capture failures
    pub fn open(config: RecorderConfig) -> Result<Self, Error> {
        Ok(Self {
            engine: Engine::open(CpalBackend::shared(), config, None, Timeouts::default())?,
        })
    }

    /// `open`, plus a function called the moment this recorder fails. The
    /// recommended way to open a recorder.
    pub fn open_with_failure_handler(
        config: RecorderConfig,
        handler: impl FnOnce(Error) + Send + 'static,
    ) -> Result<Self, Error> {
        Ok(Self {
            engine: Engine::open(
                CpalBackend::shared(),
                config,
                Some(Box::new(handler)),
                Timeouts::default(),
            )?,
        })
    }

    /// What was opened: the device, the format it runs at, and the format
    /// the sink receives. Can be called once `open` returns.
    pub fn info(&self) -> &RecorderInfo {
        self.engine.info()
    }

    /// Starts a recording into `sink`
    ///
    /// Cheap and prompt: it never waits for audio or touches the device.
    /// Fails only with `AlreadyRecording` or the error the recorder failed
    /// with; the sink always comes back in the error.
    pub fn start(&self, sink: S) -> Result<(), StartError<S>> {
        self.engine.start(sink)
    }

    /// Ends the recording and hands back the sink.
    ///
    /// A recording that ended on its own (the recorder failed, the sink
    /// panicked) still waits here for its `stop`, and a new `start` fails
    /// with `AlreadyRecording` until then.
    ///
    /// Returns `Ok` whenever the sink can be given back, including after the
    /// recorder failed or the sink panicked; `end_reason` says what ended it.
    /// Returns `Err` with `NotRecording` if there is no recording, or
    /// `SinkStalled` if the sink never returned and is lost; after
    /// `SinkStalled` the slot is released and the recorder has failed.
    /// Called from inside the sink, it returns `StopFromSink` at once and the
    /// recording continues. To cancel a recording, stop it and drop the
    /// result.
    pub fn stop(&self) -> Result<Stopped<S>, Error> {
        self.engine.stop()
    }

    /// Closes the microphone. An active recording is discarded; call `stop`
    /// first to keep it.
    pub fn close(self) -> Result<(), Error> {
        // `drop` then finds the recorder already closed.
        self.engine.shutdown()
    }
}

impl<S> Drop for Recorder<S> {
    fn drop(&mut self) {
        if let Err(error) = self.engine.shutdown() {
            log::warn!("closing the recorder on drop failed: {error}");
        }
    }
}

/// `start` failed. The sink is handed back.
pub struct StartError<S> {
    /// `AlreadyRecording`, or the error the recorder failed with.
    pub error: Error,
    pub sink: S,
}

/// For callers that do not need the sink back: `?` drops it.
impl<S> From<StartError<S>> for Error {
    fn from(e: StartError<S>) -> Error {
        e.error
    }
}

impl<S> fmt::Debug for StartError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartError")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<S> fmt::Display for StartError<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "cannot start recording: {}", self.error)
    }
}

impl<S> std::error::Error for StartError<S> {}

/// What `stop` returns: the sink, what ended the recording, and what was lost.
#[derive(Debug)]
pub struct Stopped<S> {
    pub sink: S,
    /// What ended the recording: the application's `stop`, or a failure
    /// before it.
    pub end_reason: EndReason,
    /// Frames the device produced during the recording that were lost
    /// because the ring was full, usually because the sink was slow.
    pub dropped_frames: u64,
}

impl<S> Stopped<S> {
    /// `StopCalled` and no dropped frames. The library never presents an
    /// incomplete recording as complete.
    pub fn is_complete(&self) -> bool {
        matches!(self.end_reason, EndReason::StopCalled) && self.dropped_frames == 0
    }
}

// Public handles are `Send`, and the recorder is also `Sync`, so they can be
// stored anywhere, shared between threads, and held by the Node binding.
const _: () = {
    const fn assert_send<T: Send>() {}
    const fn assert_sync<T: Sync>() {}
    assert_send::<Recorder<CollectingSink>>();
    assert_sync::<Recorder<CollectingSink>>();
    assert_send::<Stopped<CollectingSink>>();
    assert_send::<StartError<CollectingSink>>();
    assert_send::<Error>();
};
