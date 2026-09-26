//! What the CPAL PulseAudio host leaves out: a fragment size (the server's
//! default delivers audio in fragments of seconds) and noticing that the
//! opened source was removed (the server moves the stream to another
//! source, and CPAL ignores the notice).

use std::{
    ffi::CString,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use futures::executor::block_on;
use pulseaudio::{ClientError, protocol::PulseError};

use crate::backend::{BackendError, BackendErrorKind};

/// How often the watch looks the source up. With `Timeouts::watchdog_tick`,
/// bounds how long a stream moved to another source records from it before
/// the recorder fails.
const LOOKUP_INTERVAL: Duration = Duration::from_secs(1);

/// Frames per fragment to request: the power of two at or above 20 ms.
///
/// Without a request, the server delivers audio in fragments of seconds,
/// and CPAL's `play` waits for the first one: every open took 2 s on
/// PipeWire 1.4, and up to 3.8 s on PulseAudio 15.99, where a recording
/// also lost its last fragment's audio at `stop`. PipeWire runs its whole
/// graph at the smallest quantum any stream asks for, and its default is
/// 1024 frames at 48 kHz: asking for 960 frames there dropped the graph to
/// 512, and 480 to 256 (`pw-top`), which costs every application power for
/// as long as the recorder stays open. 1024 left it at 1024. The library
/// frames chunks itself, so the fragment size only sets how often audio
/// arrives.
///
/// Needs the patched CPAL (`Cargo.toml`): CPAL 0.18.2 caps a
/// fixed-size stream's buffer at one fragment, and PulseAudio then
/// delivers one callback and nothing more.
pub(super) fn fragment_frames(sample_rate: u32) -> u32 {
    (sample_rate / 50).max(1).next_power_of_two()
}

/// Checks that one source still exists, from a thread of its own with a
/// connection of its own, so a hung server never blocks the device thread:
/// that thread also runs the watchdog, which reports a hung server as a
/// stall.
pub(super) struct SourceWatch {
    /// The source was looked up and is gone. Until a lookup says so, the
    /// source counts as present.
    removed: Arc<AtomicBool>,
    name: String,
    /// The connection's socket. Shutting it down fails a lookup in flight
    /// and ends the connection's reactor thread, which otherwise notices a
    /// dropped client only when it next wakes: an idle one never does, and
    /// every recorder left a thread and three descriptors behind (measured
    /// on PipeWire 1.4).
    socket: UnixStream,
    /// Dropped to stop the watch thread.
    _stop: mpsc::Sender<()>,
}

impl SourceWatch {
    /// Starts watching. `None`, logged, if it cannot: the recorder then
    /// works as before, without the check.
    pub(super) fn start(source: &str) -> Option<Self> {
        let Ok(name) = CString::new(source) else {
            log::warn!("cannot watch PulseAudio source {source:?}: the name contains NUL");
            return None;
        };
        let socket = match connect_socket().and_then(|s| Ok((s.try_clone()?, s))) {
            Ok(socket) => socket,
            Err(e) => {
                log::warn!("cannot watch PulseAudio source {source}: {e}");
                return None;
            }
        };
        let removed = Arc::new(AtomicBool::new(false));
        let (stop, stopped) = mpsc::channel();
        let spawned = thread::Builder::new()
            .name("handy-recorder-pulse-watch".into())
            .spawn({
                let removed = Arc::clone(&removed);
                move || watch(socket.0, name, &removed, &stopped)
            });
        if let Err(e) = spawned {
            log::warn!("cannot watch PulseAudio source {source}: {e}");
            return None;
        }
        Some(Self {
            removed,
            name: source.to_owned(),
            socket: socket.1,
            _stop: stop,
        })
    }

    /// `Err` when the source no longer exists. Never blocks.
    pub(super) fn check(&self) -> Result<(), BackendError> {
        if !self.removed.load(Ordering::Relaxed) {
            return Ok(());
        }
        Err(BackendError::new(
            BackendErrorKind::DeviceNotAvailable,
            format!(
                "the PulseAudio source {} was removed (the sound server moves its streams to another source)",
                self.name
            ),
        ))
    }
}

impl Drop for SourceWatch {
    /// Ends the watch thread wherever it is: waiting for the next lookup
    /// (`_stop`), or blocked on the server (the socket). Not joined: it
    /// exits on its own, and nothing here may block the device thread.
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

/// The watch thread: connects, then looks the source up every
/// `LOOKUP_INTERVAL` until stopped or the source is gone.
fn watch(socket: UnixStream, name: CString, removed: &AtomicBool, stopped: &mpsc::Receiver<()>) {
    let source = name.to_string_lossy().into_owned();
    let started = Instant::now();
    // No timeout: a hung handshake blocks only this thread, and `Drop`
    // ends it.
    let cookie = pulseaudio::cookie_path_from_env().and_then(|path| std::fs::read(path).ok());
    let client = match pulseaudio::Client::new_unix(c"handy-recorder-watch", socket, cookie) {
        Ok(client) => client,
        Err(e) => {
            log::warn!("cannot watch PulseAudio source {source}: {e}");
            return;
        }
    };
    log::debug!(
        "watching PulseAudio source {source} (connected in {:?})",
        started.elapsed()
    );
    let mut unsure_logged = false;
    let mut first_index = None;
    loop {
        match block_on(client.lookup_source_by_name(name.clone())) {
            Ok(index) if same_source(&mut first_index, index) => {}
            Ok(index) => {
                log::debug!(
                    "PulseAudio source {source} was removed and added again (index {first_index:?}, now {index})"
                );
                removed.store(true, Ordering::Relaxed);
                return;
            }
            Err(ClientError::ServerError(PulseError::NoEntity)) => {
                removed.store(true, Ordering::Relaxed);
                return;
            }
            // Not evidence the source is gone. The server going away fails
            // the stream itself; a watch stopped by `Drop` ends here too.
            Err(e) => {
                if !unsure_logged {
                    unsure_logged = true;
                    log::debug!("cannot check PulseAudio source {source}: {e}");
                }
            }
        }
        if stopped.recv_timeout(LOOKUP_INTERVAL) != Err(mpsc::RecvTimeoutError::Timeout) {
            return;
        }
    }
}

/// Whether a lookup found the source the first lookup found. A source
/// removed and added again, a replug between two lookups, comes back under
/// a new index: PulseAudio and pipewire-pulse (which uses PipeWire's
/// `object.serial`) never reuse one (measured on PipeWire 1.4). The stream
/// stays on whichever source the server moved it to.
fn same_source(first: &mut Option<u32>, index: u32) -> bool {
    *first.get_or_insert(index) == index
}

fn connect_socket() -> Result<UnixStream, ClientError> {
    let path = pulseaudio::socket_path_from_env().ok_or(ClientError::ServerUnavailable)?;
    Ok(UnixStream::connect(path)?)
}
