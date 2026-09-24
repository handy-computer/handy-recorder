//! Capture code extracted from Handy (`src-tauri/src/audio_toolkit/audio/`,
//! commit 8f9cf53c): the real-time callback, the ring transport, stream
//! construction, the pause handshake, the consumer loop, and the resampler.
//!
//! Kept close to Handy's source so it can be diffed against it. Removed: VAD,
//! the level visualizer, and Tauri wiring, which stay in Handy. Changed only
//! where CPAL 0.18 required it. Handy's behavior is otherwise unchanged,
//! including the parts the design replaces later (see DESIGN.md, "Extraction
//! from Handy").

mod recorder;
mod resampler;

#[allow(unused_imports)]
pub(crate) use recorder::{AudioRecorder, is_microphone_access_denied, is_no_input_device_error};
pub(crate) use resampler::FrameResampler;

/// Handy forces the ALSA host on Linux. The library targets the native
/// PulseAudio host there (with ALSA fallback), which `default_host` selects
/// when the `pulseaudio` feature is enabled and a server is running.
pub(crate) fn get_cpal_host() -> cpal::Host {
    cpal::default_host()
}

#[cfg(test)]
mod smoke {
    use std::time::Duration;

    use super::AudioRecorder;

    /// Real hardware: records one second from the default microphone through
    /// the extracted CPAL path. Run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a microphone"]
    fn records_one_second_from_the_default_microphone() {
        let mut recorder = AudioRecorder::new().expect("recorder");
        recorder.open(None).expect("open default microphone");
        let ready = recorder.start().expect("start");
        ready
            .recv_timeout(Duration::from_secs(5))
            .expect("first audio within 5 s");
        std::thread::sleep(Duration::from_secs(1));
        let samples = recorder.stop().expect("stop");
        recorder.close().expect("close");

        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        eprintln!("captured {} samples at 16 kHz, peak {peak}", samples.len());
        // About one second after first audio, whole 480-sample frames.
        assert!(samples.len() >= 15_000, "only {} samples", samples.len());
        assert_eq!(samples.len() % 480, 0);
    }
}
