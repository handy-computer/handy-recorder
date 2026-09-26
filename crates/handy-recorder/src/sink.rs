/// Application code that receives audio from the library.
///
/// Called on the delivery thread, never on the real-time audio thread. Rules:
///
/// - **Return promptly.** A slow sink causes dropped audio, which is counted
///   in `Stopped::dropped_frames`.
///   A sink that never returns fails the recorder with `SinkStalled`, and the
///   sink is lost.
/// - **The sink comes back.** `Recorder::stop` returns it, even after a panic.
/// - **Errors are the application's.** `process_chunk` returns nothing; a
///   sink records its own errors and the application decides what they mean.
///   A panic is caught and ends the recording with
///   `EndReason::SinkPanicked`; the recorder is unaffected. Requires
///   `panic = "unwind"`.
///
/// The first `process_chunk` call of a recording means audio is flowing. An
/// application that shows a "connecting" state (Bluetooth devices can take
/// seconds) has its sink signal that moment.
pub trait Sink: Send + 'static {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>);
}

/// Lets a recorder take `Box<dyn Sink>`.
impl<S: Sink + ?Sized> Sink for Box<S> {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        (**self).process_chunk(chunk);
    }
}

/// Exactly `frames_per_chunk` frames of interleaved audio.
#[derive(Debug, Clone, Copy)]
pub struct AudioChunk<'a> {
    /// Interleaved samples, `frames_per_chunk * channels` long.
    pub samples: &'a [f32],
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames of real audio. Equal to `frames_per_chunk` except in the final
    /// chunk, whose remainder is zero padding.
    pub valid_frames: usize,
}

impl AudioChunk<'_> {
    /// True when every real sample in this chunk is exactly zero (the final
    /// chunk's zero padding is ignored).
    ///
    /// A real microphone always has a noise floor, so a run of these usually
    /// means a muted device, a Bluetooth headset connected to another device
    /// (it can stay listed here and deliver zeros), or denied access. A
    /// device's first few hundred milliseconds can be zeros too. The library
    /// only logs digital silence; what counts as too long, and what to tell
    /// the user, is the application's decision.
    pub fn is_digital_silence(&self) -> bool {
        let real = self.valid_frames * self.channels as usize;
        self.samples[..real].iter().all(|&s| s == 0.0)
    }
}

/// Collects a recording's real audio (padding excluded) in memory.
#[derive(Debug, Default)]
pub struct CollectingSink {
    samples: Vec<f32>,
}

impl CollectingSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// The collected interleaved samples.
    pub fn into_samples(self) -> Vec<f32> {
        self.samples
    }
}

impl Sink for CollectingSink {
    fn process_chunk(&mut self, chunk: AudioChunk<'_>) {
        let real = chunk.valid_frames * chunk.channels as usize;
        self.samples.extend_from_slice(&chunk.samples[..real]);
    }
}
