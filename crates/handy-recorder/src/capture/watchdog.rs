//! The device thread's watchdog: detects the failures nothing reports (no
//! audio after open, callbacks stopping, a delivery thread that stopped
//! making progress, the process suspended during a recording).

use std::{
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

use super::engine::{Shared, Timeouts};
use crate::{Error, ErrorKind};

pub(super) struct Watchdog {
    opened_at: Instant,
    callbacks: u64,
    last_callback_at: Instant,
    heartbeat: u64,
    last_heartbeat_at: Instant,
    /// An idle stall was logged and not yet resolved.
    idle_stall_logged: bool,
}

impl Watchdog {
    pub(super) fn new(shared: &Shared) -> Self {
        let now = Instant::now();
        // Opening took a while before the watchdog existed; that is not a
        // suspension.
        *shared.watchdog_checked_at.lock().unwrap() = now;
        Self {
            opened_at: now,
            callbacks: 0,
            last_callback_at: now,
            heartbeat: 0,
            last_heartbeat_at: now,
            idle_stall_logged: false,
        }
    }

    pub(super) fn check(&mut self, shared: &Shared, timeouts: &Timeouts) {
        let now = Instant::now();
        let recording_since = shared.recording_since();
        let suspended = {
            let mut checked_at = shared.watchdog_checked_at.lock().unwrap();
            let suspended = suspension(*checked_at, now, recording_since, timeouts.stall);
            *checked_at = now;
            // Failed under the lock: `stop` reads the same timestamp.
            if let Suspension::DuringRecording(gap) = suspended {
                let error = suspended_during_recording(shared, gap, timeouts.stall);
                log::warn!("watchdog tripped: {error}");
                shared.fail(error);
            }
            suspended
        };
        if let Suspension::WhileIdle(gap) = suspended {
            // Every thread of the process was frozen, so the gap is evidence
            // of nothing: it must not count toward `NoAudio`, `SinkStalled`,
            // or an idle stall.
            log::info!(
                "the process was suspended for {:.1} s while idle (system sleep?)",
                gap.as_secs_f64()
            );
            self.opened_at += gap;
            self.last_callback_at += gap;
            self.last_heartbeat_at += gap;
        }
        let callbacks = shared.transport.callbacks.load(Ordering::Relaxed);
        if callbacks != self.callbacks {
            if self.idle_stall_logged {
                self.idle_stall_logged = false;
                log::info!(
                    "audio callbacks resumed after {:.1} s",
                    (now - self.last_callback_at).as_secs_f64()
                );
            }
            self.callbacks = callbacks;
            self.last_callback_at = now;
        }
        let heartbeat = shared.heartbeat.load(Ordering::Relaxed);
        if heartbeat != self.heartbeat {
            self.heartbeat = heartbeat;
            self.last_heartbeat_at = now;
        }

        if matches!(suspended, Suspension::DuringRecording(_)) {
            return;
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
            // macOS stops callbacks for tens of seconds of awake time around
            // system sleep and resumes them after wake, so a stall while idle
            // is not a failure. During a recording it is: the audio has a gap
            // of unknown length. Silence counts from the later of the last
            // callback and the recording's start, so a recording started
            // just after wake gets the full bound for audio to resume.
            match recording_since {
                Some(since) => {
                    let silent = now - self.last_callback_at.max(since);
                    (silent >= timeouts.stall).then(|| {
                        shared.error(ErrorKind::Stalled).with_detail(format!(
                            "no audio callback for {:.1} s during a recording, after {callbacks} callbacks (bound {:.1} s)",
                            silent.as_secs_f64(),
                            timeouts.stall.as_secs_f64()
                        ))
                    })
                }
                None => {
                    let silent = now - self.last_callback_at;
                    if silent >= timeouts.stall && !self.idle_stall_logged {
                        self.idle_stall_logged = true;
                        log::info!(
                            "no audio callbacks for {:.1} s while idle (system sleep?); \
                             not a failure unless it lasts into a recording",
                            silent.as_secs_f64()
                        );
                    }
                    None
                }
            }
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

/// `stop`'s half of the suspension check. On wake, callbacks can resume
/// before the watchdog's next check; without this, a stop in that window
/// would pass the handshake and return the recording as complete, gap and
/// all. Skipped once the recorder has failed: the watchdog stops checking
/// then, so the gap since its last check is not a suspension.
pub(super) fn fail_if_suspended(shared: &Shared, timeouts: &Timeouts) {
    let recording_since = shared.recording_since();
    if shared.failure().is_some() {
        return;
    }
    // Held while failing, as in `Watchdog::check`.
    let checked_at = shared.watchdog_checked_at.lock().unwrap();
    if let Suspension::DuringRecording(gap) =
        suspension(*checked_at, Instant::now(), recording_since, timeouts.stall)
    {
        let error = suspended_during_recording(shared, gap, timeouts.stall);
        log::warn!("{error}");
        shared.fail(error);
    }
}

fn suspended_during_recording(shared: &Shared, gap: Duration, stall: Duration) -> Error {
    shared.error(ErrorKind::Stalled).with_detail(format!(
        "the process was suspended for {:.1} s during a recording (system sleep?), \
         so the audio has a gap (bound {:.1} s)",
        gap.as_secs_f64(),
        stall.as_secs_f64()
    ))
}

#[derive(Debug, PartialEq, Eq)]
enum Suspension {
    None,
    WhileIdle(Duration),
    DuringRecording(Duration),
}

/// Whether the watchdog's thread was frozen since its last check, and so the
/// whole process with it. Windows (Modern Standby) suspends the process
/// during sleep: callbacks and the watchdog stop together and resume
/// together, so on wake a callback can arrive before the next check and the
/// stall check never sees the gap. `Instant` counts sleep there, so a gap
/// between checks of at least the stall bound, during a recording that began
/// before it, is the same gap in the audio. macOS stops callbacks before it
/// suspends the process, and its `Instant` excludes sleep; the stall check
/// catches it instead.
// TODO(review): see TODO.md, "Sleep on Linux".
fn suspension(
    last_check_at: Instant,
    now: Instant,
    recording_since: Option<Instant>,
    stall: Duration,
) -> Suspension {
    let gap = now.saturating_duration_since(last_check_at);
    if gap < stall {
        Suspension::None
    } else if recording_since.is_some_and(|since| since <= last_check_at) {
        Suspension::DuringRecording(gap)
    } else {
        // Idle, or a recording started after wake, before this check.
        Suspension::WhileIdle(gap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_suspension_fails_only_a_recording_that_spans_it() {
        let stall = Duration::from_secs(5);
        let before = Instant::now();
        let last_check = before + Duration::from_secs(1);
        let woke = last_check + Duration::from_secs(100);
        let gap = Duration::from_secs(100);

        assert_eq!(
            suspension(last_check, woke, Some(before), stall),
            Suspension::DuringRecording(gap)
        );
        assert_eq!(
            suspension(last_check, woke, None, stall),
            Suspension::WhileIdle(gap)
        );
        // Started after wake, before this check ran.
        assert_eq!(
            suspension(last_check, woke, Some(woke), stall),
            Suspension::WhileIdle(gap)
        );
        // An ordinary tick.
        assert_eq!(
            suspension(
                last_check,
                last_check + Duration::from_millis(50),
                Some(before),
                stall
            ),
            Suspension::None
        );
    }
}
