//! The capture engine module hub. The engine itself handles the
//! real-time OS callback and safely gets the data onto an ordinary
//! thread so applications can do whatever processing they need.
//!
//! The engine will resample into the format specified and deliver
//! fixed size AudioChunks
//!
//! It also watches for errors and issues on the capture side and
//! reports them appropriately.

pub(crate) mod delivery;
pub(crate) mod engine;
mod resampler;
pub(crate) mod transport;
mod watchdog;

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
