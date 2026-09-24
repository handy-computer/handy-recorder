//! Cross-platform microphone capture.
//!
//! Handy Recorder owns the real-time audio path.
//! It gives applications audio in the
//! format they asked for, on an ordinary thread, reporting when audio was lost
//! or the stream broke.
//!
//! # Concepts
//!
//! - A [`Recorder`] is "the microphone is on." It can stay open (warm) across
//!   many recordings.
//! - A recording is "the audio between [`Recorder::start`] and
//!   [`Recorder::stop`]." One at a time; while none is active, captured audio
//!   is discarded.
//! - A [`Sink`] is application code that receives the recording's audio as
//!   fixed-size [`AudioChunk`]s. It is lent to the library by `start` and
//!   handed back by `stop`.
//! - `stop` returns a [`Stopped`]: the sink, how the recording ended, and
//!   whether any audio was lost. Partial success is normal; `stop` returns
//!   `Err` only when the sink cannot be given back.
//!
//! # Failures
//!
//! A failure ends what it broke. Device-side failures (unplugged, stalled,
//! invalidated) end the recorder and its active recording; a sink panic ends
//! only the recording. A failed recorder stays failed: `start` returns its
//! error, and the application opens a new recorder to continue. Every
//! [`Error`] says what happened ([`Error::kind`]), on which device, when, and
//! the platform's own message.
//!
//! Open with [`Recorder::open_with_failure_handler`] to learn of a failure
//! the moment it happens. Without it, a failure surfaces only at the next
//! `start` or `stop`, which in an application where the user or the sink
//! decides when to stop can be never.
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
use std::sync::Arc;

use backend::{Backend, cpal::CpalBackend};
use capture::engine::{Engine, Timeouts};

// ---------------------------------------------------------------------------
// Recorder
// ---------------------------------------------------------------------------

/// An open microphone, recording into sinks of type `S`. Owned, `Send`,
/// `Sync`, lifetime-free: store it in a struct, or share it with an `Arc`.
///
/// Every recording on a recorder uses the same sink type. An application
/// that needs several kinds uses an enum or `Box<dyn Sink>`.
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
        Self::open_with(CpalBackend::shared(), config, None, Timeouts::default())
    }

    /// `open`, plus a function called the moment this recorder fails. The
    /// recommended way to open a recorder.
    pub fn open_with_failure_handler(
        config: RecorderConfig,
        handler: impl FnOnce(Error) + Send + 'static,
    ) -> Result<Self, Error> {
        Self::open_with(
            CpalBackend::shared(),
            config,
            Some(Box::new(handler)),
            Timeouts::default(),
        )
    }

    /// `open` with a chosen backend and internal bounds (the fake backend
    /// and short timeouts in tests).
    pub(crate) fn open_with(
        backend: Arc<dyn Backend>,
        config: RecorderConfig,
        handler: Option<Box<dyn FnOnce(Error) + Send + 'static>>,
        timeouts: Timeouts,
    ) -> Result<Self, Error> {
        Ok(Self {
            engine: Engine::open(backend, config, handler, timeouts)?,
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
    /// Briefly pauses the callback after the block it is writing, delivers
    /// all audio up to that block, flushes the resampler tail, and resumes
    /// capture before returning, so the recorder keeps running for the next
    /// `start`. If the recorder has already failed, the recording ended at
    /// the failure point and `stop` returns without waiting for a callback.
    ///
    /// A recording that ended on its own (the recorder failed, the sink
    /// panicked) still waits here for its `stop`, and a new `start` fails
    /// with `AlreadyRecording` until then.
    ///
    /// Returns `Ok` whenever the sink can be given back, including after the
    /// recorder failed or the sink panicked; `end_reason` says what ended it.
    /// Returns `Err` with `NotRecording` if there is no recording, or
    /// `SinkStalled` if the sink never returned and is lost; after
    /// `SinkStalled` the slot is released and the recorder has failed. To
    /// cancel a recording, stop it and drop the result.
    ///
    /// An incomplete recording (see [`Stopped::is_complete`]) is also logged
    /// at `warn` level with its end reason and dropped-frame count, so it is
    /// never silent even if the application does not check. Every recording's
    /// diagnostics (overrun episodes, time to first audio, frame counts) are
    /// logged at `debug`.
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
    /// because the ring was full, usually because the sink was slow. Only the
    /// library can see these: the sink receives the audio on either side of
    /// the gap with nothing to mark it. Approximate at the recording's edges:
    /// a drop racing `start` or `stop` may be counted on either side.
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
