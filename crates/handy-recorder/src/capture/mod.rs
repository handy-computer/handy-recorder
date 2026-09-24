//! The capture engine. Its real-time path, pause handshake, consumer loop,
//! and resampler come from Handy (`src-tauri/src/audio_toolkit/audio/` at
//! commit 8f9cf53c); `handy_reference.rs` is a frozen copy the equivalence
//! test holds them to. What changed and why is in DESIGN.md ("Extraction
//! from Handy", "Regression strategy") and TODO.md.
//!
//! - `transport`: the real-time callback and the atomics it shares.
//! - `delivery`: the delivery thread (Handy's consumer loop) and the sink.
//! - `engine`: the device thread, watchdog, and start/stop/close.
//! - `resampler`: K-channel resampling and exact chunking.

pub(crate) mod delivery;
pub(crate) mod engine;
// Frozen: never edited, never reformatted.
#[cfg(test)]
#[rustfmt::skip]
pub(crate) mod handy_reference;
mod resampler;
pub(crate) mod transport;

pub(crate) use resampler::FrameResampler;

#[cfg(test)]
mod smoke {
    use std::time::Duration;

    use crate::{CollectingSink, Recorder, RecorderConfig};

    /// Real hardware: records one second of speech-format audio from the
    /// default microphone. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_one_second_from_the_default_microphone() {
        let recorder: Recorder<CollectingSink> =
            Recorder::open(RecorderConfig::speech()).expect("open default microphone");
        eprintln!("recording from {:?}", recorder.info());
        recorder.start(CollectingSink::new()).expect("start");
        std::thread::sleep(Duration::from_secs(1));
        let stopped = recorder.stop().expect("stop");
        recorder.close().expect("close");

        assert!(stopped.is_complete(), "{:?}", stopped.end_reason);
        let samples = stopped.sink.into_samples();
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        eprintln!("captured {} samples at 16 kHz, peak {peak}", samples.len());
        // About one second; the start edge includes up to one poll interval
        // of audio from before start, and a Bluetooth device may start late.
        assert!(samples.len() >= 12_000, "only {} samples", samples.len());
    }
}
