//! `FakeMic`: the core's fake microphone, for this package's tests only
//! (`test-backend` feature, never published).
//!
//! A feeder thread plays the platform's audio thread, so audio keeps coming
//! while a test blocks the event loop. It sends a ramp: frame `n` of the
//! stream has every channel at `(n % RAMP) / RAMP`, which survives mixing to
//! mono exactly, so a test can check a recording is complete and in order.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use handy_recorder::Permission;
use handy_recorder::testing::{FakeError, FakeMic as CoreFakeMic, Held};
use napi::bindgen_prelude::{Float32Array, Function, Unknown};
use napi::{Error as NapiError, Result, Status};
use napi_derive::napi;

use crate::convert::JsConfig;
use crate::recorder::{JsEvent, NativeRecorder, open_with};

const RAMP: u64 = 1 << 16;

#[napi]
pub struct FakeMic {
    mic: CoreFakeMic,
    channels: u16,
    open_timeout: Duration,
    start_hold: Mutex<Option<Held>>,
    teardown_hold: Mutex<Option<Held>>,
    feeder: Mutex<Option<Feeder>>,
    /// Frames delivered to a running stream, across feeders.
    frames_fed: Arc<AtomicU64>,
}

struct Feeder {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

fn fake_error(kind: &str) -> Result<FakeError> {
    Ok(match kind {
        "DeviceNotAvailable" => FakeError::DeviceNotAvailable,
        "DeviceBusy" => FakeError::DeviceBusy,
        "PermissionDenied" => FakeError::PermissionDenied,
        "StreamInvalidated" => FakeError::StreamInvalidated,
        "Other" => FakeError::Other,
        _ => {
            return Err(NapiError::new(
                Status::InvalidArg,
                format!("unknown fake error {kind:?}"),
            ));
        }
    })
}

#[napi]
impl FakeMic {
    #[napi(constructor)]
    pub fn new(sample_rate: u32, channels: u32, open_timeout_ms: Option<u32>) -> Self {
        let channels = channels as u16;
        Self {
            mic: CoreFakeMic::new(sample_rate, channels),
            channels,
            open_timeout: Duration::from_millis(u64::from(open_timeout_ms.unwrap_or(2000))),
            start_hold: Mutex::new(None),
            teardown_hold: Mutex::new(None),
            feeder: Mutex::new(None),
            frames_fed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Opens a recorder on this device, as `openRecorder` does on a real one.
    #[napi]
    pub fn open(
        &self,
        config: JsConfig,
        dispatch: Function<'_, JsEvent, Unknown<'static>>,
    ) -> Result<NativeRecorder> {
        let mic = self.mic.clone();
        let open_timeout = self.open_timeout;
        open_with(
            config,
            dispatch,
            Box::new(move |config, handler| mic.open(config, Some(handler), open_timeout)),
        )
    }

    /// Delivers one block of interleaved samples. False if no stream runs.
    #[napi]
    pub fn push(&self, samples: Float32Array) -> bool {
        self.mic.push(&samples)
    }

    /// Starts feeding the ramp in blocks of `block_frames`, one every
    /// `interval_ms` on average, from a thread of its own.
    #[napi]
    pub fn start_feeding(&self, block_frames: u32, interval_ms: u32) -> Result<()> {
        self.stop_feeding();
        let stop = Arc::new(AtomicBool::new(false));
        let mic = self.mic.clone();
        let channels = usize::from(self.channels);
        let fed = Arc::clone(&self.frames_fed);
        let stopping = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("fake-mic-feeder".into())
            .spawn(move || {
                let frames = block_frames as usize;
                let interval = Duration::from_millis(u64::from(interval_ms));
                let mut block = vec![0.0f32; frames * channels];
                // Paced by the clock, not by sleeps, which overshoot on a
                // loaded machine: a late wake delivers the blocks it owes, as
                // a real device's callback catches up.
                let started = Instant::now();
                let mut blocks: u32 = 0;
                while !stopping.load(Ordering::Relaxed) {
                    let due =
                        (started.elapsed().as_nanos() / interval.as_nanos().max(1)) as u32 + 1;
                    while blocks < due {
                        let first = fed.load(Ordering::Relaxed);
                        for (i, frame) in block.chunks_mut(channels).enumerate() {
                            frame.fill(((first + i as u64) % RAMP) as f32 / RAMP as f32);
                        }
                        if mic.push(&block) {
                            fed.fetch_add(frames as u64, Ordering::Relaxed);
                        }
                        blocks += 1;
                    }
                    thread::sleep(
                        (started + interval * blocks).saturating_duration_since(Instant::now()),
                    );
                }
            })
            .map_err(|e| NapiError::from_reason(e.to_string()))?;
        *self.feeder.lock().unwrap() = Some(Feeder { stop, thread });
        Ok(())
    }

    /// Stops the feeder, waiting for its last block.
    #[napi]
    pub fn stop_feeding(&self) {
        if let Some(feeder) = self.feeder.lock().unwrap().take() {
            feeder.stop.store(true, Ordering::Relaxed);
            let _ = feeder.thread.join();
        }
    }

    /// Frames the feeder has delivered.
    #[napi(getter)]
    pub fn frames_fed(&self) -> f64 {
        self.frames_fed.load(Ordering::Relaxed) as f64
    }

    /// The ramp's period, in frames.
    #[napi(getter)]
    pub fn ramp(&self) -> u32 {
        RAMP as u32
    }

    /// Reports a platform error to the running stream.
    #[napi]
    pub fn report_error(&self, kind: String) -> Result<bool> {
        Ok(self.mic.report_error(fake_error(&kind)?))
    }

    /// Makes the next open fail with this platform error.
    #[napi]
    pub fn fail_next_open(&self, kind: String) -> Result<()> {
        self.mic.fail_next_open(fake_error(&kind)?);
        Ok(())
    }

    /// Makes the next stream start hang until `release_start`.
    #[napi]
    pub fn hang_next_start(&self) {
        *self.start_hold.lock().unwrap() = Some(self.mic.hang_next_start());
    }

    #[napi]
    pub fn release_start(&self) {
        if let Some(held) = self.start_hold.lock().unwrap().take() {
            held.release();
        }
    }

    /// Makes stream teardown hang until `release_teardown`.
    #[napi]
    pub fn hang_teardown(&self) {
        *self.teardown_hold.lock().unwrap() = Some(self.mic.hang_teardown());
    }

    #[napi]
    pub fn release_teardown(&self) {
        if let Some(held) = self.teardown_hold.lock().unwrap().take() {
            held.release();
        }
    }

    /// `"granted"`, `"denied"`, `"not-determined"`, or `"unknown"`.
    #[napi]
    pub fn set_permission(&self, permission: String) -> Result<()> {
        self.mic.set_permission(match permission.as_str() {
            "granted" => Permission::Granted,
            "denied" => Permission::Denied,
            "not-determined" => Permission::NotDetermined,
            "unknown" => Permission::Unknown,
            _ => {
                return Err(NapiError::new(
                    Status::InvalidArg,
                    format!("unknown permission {permission:?}"),
                ));
            }
        });
        Ok(())
    }

    #[napi(getter)]
    pub fn is_streaming(&self) -> bool {
        self.mic.is_streaming()
    }

    /// Streams started on this device, ever.
    #[napi(getter)]
    pub fn streams_started(&self) -> u32 {
        self.mic.streams_started() as u32
    }
}

impl Drop for FakeMic {
    fn drop(&mut self) {
        self.stop_feeding();
        self.release_start();
        self.release_teardown();
    }
}
