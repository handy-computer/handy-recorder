use rubato::{FftFixedIn, ResampleError, Resampler, ResamplerConstructionError};
use std::fmt;

const RESAMPLER_CHUNK_SIZE: usize = 1024;

/// Cap on zero-chunk rounds when draining the tail at `finish()`.
const MAX_TAIL_ROUNDS: usize = 8;

#[derive(Debug)]
pub enum ResamplerError {
    Resample(ResampleError),
    /// The tail drain stopped before all real audio had emerged.
    TailIncomplete {
        expected: usize,
        emitted: usize,
    },
}

impl fmt::Display for ResamplerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Resample(e) => write!(f, "resampler error: {e}"),
            Self::TailIncomplete { expected, emitted } => write!(
                f,
                "resampler tail incomplete: {emitted} of {expected} frames emerged"
            ),
        }
    }
}

impl From<ResampleError> for ResamplerError {
    fn from(e: ResampleError) -> Self {
        Self::Resample(e)
    }
}

/// Resamples interleaved K-channel audio into fixed-size chunks, emitted as
/// `(samples, valid frames)`. Only the final chunk is zero-padded.
pub struct FrameResampler {
    resampler: Option<FftFixedIn<f32>>,
    channels: usize,
    chunk_in: usize,
    /// Input frames waiting for a full resampler chunk, one buffer per channel.
    in_buf: Vec<Vec<f32>>,
    /// Scratch for interleaving multichannel resampler output.
    interleaved: Vec<f32>,
    frames_per_chunk: usize,
    /// Interleaved output frames waiting for a full chunk.
    pending: Vec<f32>,
    in_hz: usize,
    out_hz: usize,
    /// Frames in/out, so `finish()` knows how much its delay line holds.
    in_count: usize,
    out_count: usize,
}

impl FrameResampler {
    pub fn new(
        in_hz: usize,
        out_hz: usize,
        frames_per_chunk: usize,
        channels: usize,
    ) -> Result<Self, ResamplerConstructionError> {
        assert!(frames_per_chunk > 0, "chunk size must be non-zero");
        assert!(channels > 0, "channel count must be non-zero");

        let chunk_in = RESAMPLER_CHUNK_SIZE;

        let resampler = if in_hz != out_hz {
            Some(FftFixedIn::<f32>::new(
                in_hz, out_hz, chunk_in, 1, channels,
            )?)
        } else {
            None
        };

        Ok(Self {
            resampler,
            channels,
            chunk_in,
            in_buf: vec![Vec::with_capacity(chunk_in); channels],
            interleaved: Vec::new(),
            frames_per_chunk,
            pending: Vec::with_capacity(frames_per_chunk * channels),
            in_hz,
            out_hz,
            in_count: 0,
            out_count: 0,
        })
    }

    /// Output delay in output frames (0 when not resampling).
    pub fn output_delay(&self) -> usize {
        self.resampler.as_ref().map_or(0, |r| r.output_delay())
    }

    /// Feeds interleaved whole frames.
    pub fn push(
        &mut self,
        mut src: &[f32],
        mut emit: impl FnMut(&[f32], usize),
    ) -> Result<(), ResamplerError> {
        debug_assert_eq!(src.len() % self.channels, 0, "partial frame");
        if self.resampler.is_none() {
            self.emit_frames(src, &mut emit);
            return Ok(());
        }
        self.in_count += src.len() / self.channels;

        while !src.is_empty() {
            let space = self.chunk_in - self.in_buf[0].len();
            let take = space.min(src.len() / self.channels);
            if self.channels == 1 {
                self.in_buf[0].extend_from_slice(&src[..take]);
            } else {
                for frame in src[..take * self.channels].chunks_exact(self.channels) {
                    for (buf, &sample) in self.in_buf.iter_mut().zip(frame) {
                        buf.push(sample);
                    }
                }
            }
            src = &src[take * self.channels..];

            if self.in_buf[0].len() == self.chunk_in {
                let result = self.resampler.as_mut().unwrap().process(&self.in_buf, None);
                self.clear_input();
                let out = result?;
                let frames = out[0].len();
                self.out_count += frames;
                self.emit_planar(&out, frames, &mut emit);
            }
        }
        Ok(())
    }

    /// Flushes the tail and the final padded chunk, even if the tail fails.
    pub fn finish(&mut self, mut emit: impl FnMut(&[f32], usize)) -> Result<(), ResamplerError> {
        let result = self.drain_tail(&mut emit);

        if !self.pending.is_empty() {
            let valid_frames = self.pending.len() / self.channels;
            self.pending
                .resize(self.frames_per_chunk * self.channels, 0.0);
            emit(&self.pending, valid_frames);
            self.pending.clear();
        }
        result
    }

    fn drain_tail(&mut self, emit: &mut impl FnMut(&[f32], usize)) -> Result<(), ResamplerError> {
        if self.resampler.is_none() || self.in_count == 0 {
            return Ok(());
        }
        // All real audio is out once in*ratio + delay frames are.
        let delay = self.output_delay();
        let expected = self.in_count * self.out_hz / self.in_hz + delay;

        // Keep only real output; the internal padding is synthetic.
        if !self.in_buf[0].is_empty() {
            let result = self
                .resampler
                .as_mut()
                .unwrap()
                .process_partial(Some(&self.in_buf), None);
            self.clear_input();
            let out = result?;
            let take = expected.saturating_sub(self.out_count).min(out[0].len());
            self.out_count += take;
            self.emit_planar(&out, take, emit);
        }

        // Feed zeros until all real audio is out.
        let mut rounds = 0;
        while self.out_count < expected && rounds < MAX_TAIL_ROUNDS {
            rounds += 1;
            let out = self
                .resampler
                .as_mut()
                .unwrap()
                .process_partial::<Vec<f32>>(None, None)?;
            let take = (expected - self.out_count).min(out[0].len());
            self.out_count += take;
            self.emit_planar(&out, take, emit);
        }
        if self.out_count < expected {
            return Err(ResamplerError::TailIncomplete {
                expected,
                emitted: self.out_count,
            });
        }
        Ok(())
    }

    /// Clears all state between recordings, so no audio leaks into the next.
    pub fn reset(&mut self) {
        self.clear_input();
        self.pending.clear();
        self.in_count = 0;
        self.out_count = 0;
        if let Some(ref mut resampler) = self.resampler {
            resampler.reset();
        }
    }

    fn clear_input(&mut self) {
        for buf in &mut self.in_buf {
            buf.clear();
        }
    }

    /// Emits the first `frames` frames of planar resampler output.
    fn emit_planar(
        &mut self,
        out: &[Vec<f32>],
        frames: usize,
        emit: &mut impl FnMut(&[f32], usize),
    ) {
        if self.channels == 1 {
            self.emit_frames(&out[0][..frames], emit);
            return;
        }
        let mut interleaved = std::mem::take(&mut self.interleaved);
        interleaved.clear();
        interleaved.extend((0..frames).flat_map(|i| out.iter().map(move |ch| ch[i])));
        self.emit_frames(&interleaved, emit);
        self.interleaved = interleaved;
    }

    fn emit_frames(&mut self, mut data: &[f32], emit: &mut impl FnMut(&[f32], usize)) {
        let chunk_samples = self.frames_per_chunk * self.channels;
        while !data.is_empty() {
            let space = chunk_samples - self.pending.len();
            let take = space.min(data.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];

            if self.pending.len() == chunk_samples {
                emit(&self.pending, self.frames_per_chunk);
                self.pending.clear();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_output(resampler: &mut FrameResampler, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        resampler
            .push(input, |frame, _| out.extend_from_slice(frame))
            .unwrap();
        out
    }

    #[test]
    fn reset_between_recordings_no_crosstalk() {
        let mut r = FrameResampler::new(48000, 16000, 480, 1).unwrap();

        let ramp: Vec<f32> = (0..48000).map(|i| i as f32 / 48000.0).collect();
        let out1 = collect_output(&mut r, &ramp);
        r.finish(|_, _| {}).unwrap();
        assert!(!out1.is_empty(), "Recording 1 should produce output");

        r.reset();

        let dc = vec![-0.5f32; 48000];
        let out2 = collect_output(&mut r, &dc);

        // Skip the first chunk's transient; the rest must be free of the ramp.
        let tail = &out2[480..];
        for (i, &s) in tail.iter().enumerate() {
            assert!(
                (s - (-0.5)).abs() < 0.05,
                "Recording 2 sample {} = {} (expected ~-0.5); ramp leaked through",
                i + 480,
                s
            );
        }
    }

    /// Push silence ending in a 200-sample 0.5 burst, then assert finish()
    /// recovers the burst and emits floor(input*ratio) + output_delay samples,
    /// padded to whole 480-sample frames.
    fn assert_tail_burst_flushed(in_hz: usize, input_len: usize, expected_out: usize) {
        let mut rs = FrameResampler::new(in_hz, 16000, 480, 1).unwrap();
        let mut input = vec![0.0f32; input_len];
        input[input_len - 200..].fill(0.5);

        let mut out = Vec::new();
        rs.push(&input, |frame, _| out.extend_from_slice(frame))
            .unwrap();
        rs.finish(|frame, _| out.extend_from_slice(frame)).unwrap();

        let max_abs = out.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            max_abs > 0.3,
            "tail burst was lost in the resampler, max_abs={max_abs}"
        );
        assert_eq!(out.len(), expected_out);
    }

    #[test]
    fn finish_flushes_the_tail() {
        // 48 kHz, exact chunks: in_buf stays empty, so the burst survives
        // only via the delay-line drain. 4096 in -> 1365 real + 171 delay.
        assert_tail_burst_flushed(48000, 4 * RESAMPLER_CHUNK_SIZE, 1920);
        // 44.1 kHz: fft_size_in (1323) exceeds the 1024 chunk, so the drain
        // must survive a zero-output round. 4096 in -> 1486 real + 240 delay.
        assert_tail_burst_flushed(44100, 4 * RESAMPLER_CHUNK_SIZE, 1920);
        // Ends mid-chunk: partial-chunk path plus delay drain together.
        // 4396 in -> 1465 real + 171 delay.
        assert_tail_burst_flushed(48000, 4 * RESAMPLER_CHUNK_SIZE + 300, 1920);
    }

    fn signal(rate: usize, channel: usize, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| {
                let t = i as f64 / rate as f64;
                ((2.0 * std::f64::consts::PI * (300.0 + 250.0 * channel as f64) * t).sin() * 0.5)
                    as f32
            })
            .collect()
    }

    /// Resamples `input` (interleaved) in irregular pushes and returns the
    /// emitted chunks as (samples, valid frames).
    fn run(
        in_hz: usize,
        out_hz: usize,
        frames_per_chunk: usize,
        channels: usize,
        input: &[f32],
    ) -> Vec<(Vec<f32>, usize)> {
        let mut r = FrameResampler::new(in_hz, out_hz, frames_per_chunk, channels).unwrap();
        let mut chunks = Vec::new();
        let mut rest = input;
        for (i, size) in [1usize, 333, 1024, 7, 2048].iter().cycle().enumerate() {
            if rest.is_empty() || i > 10_000 {
                break;
            }
            let take = (size * channels).min(rest.len());
            r.push(&rest[..take], |c, v| chunks.push((c.to_vec(), v)))
                .unwrap();
            rest = &rest[take..];
        }
        r.finish(|c, v| chunks.push((c.to_vec(), v))).unwrap();
        chunks
    }

    #[test]
    fn stereo_channels_match_mono_resampling_exactly() {
        for (in_hz, out_hz) in [
            (48_000, 16_000),
            (44_100, 16_000),
            (16_000, 48_000),
            (48_000, 48_000),
        ] {
            let frames = in_hz / 3 + 17;
            let left = signal(in_hz, 0, frames);
            let right = signal(in_hz, 1, frames);
            let stereo: Vec<f32> = left
                .iter()
                .zip(&right)
                .flat_map(|(&l, &r)| [l, r])
                .collect();

            let out = run(in_hz, out_hz, 480, 2, &stereo);
            let out_left = run(in_hz, out_hz, 480, 1, &left);
            let out_right = run(in_hz, out_hz, 480, 1, &right);

            let joined: Vec<f32> = out.iter().flat_map(|(c, _)| c.clone()).collect();
            let got_left: Vec<u32> = joined.iter().step_by(2).map(|s| s.to_bits()).collect();
            let got_right: Vec<u32> = joined
                .iter()
                .skip(1)
                .step_by(2)
                .map(|s| s.to_bits())
                .collect();
            let want = |o: &Vec<(Vec<f32>, usize)>| -> Vec<u32> {
                o.iter()
                    .flat_map(|(c, _)| c.iter().map(|s| s.to_bits()))
                    .collect()
            };
            assert_eq!(got_left, want(&out_left), "{in_hz}->{out_hz} left");
            assert_eq!(got_right, want(&out_right), "{in_hz}->{out_hz} right");
            let valid = |o: &Vec<(Vec<f32>, usize)>| o.iter().map(|(_, v)| *v).collect::<Vec<_>>();
            assert_eq!(valid(&out), valid(&out_left));
        }
    }

    #[test]
    fn output_counts_follow_the_frame_count_formula() {
        // floor(input * out / in) plus the output delay; passthrough is exact.
        for in_hz in [8_000, 16_000, 22_050, 32_000, 44_100, 48_000, 96_000] {
            for frames in [1, 10, 1023, 1024, 4097, in_hz / 3] {
                let input = signal(in_hz, 0, frames);
                let chunks = run(in_hz, 16_000, 480, 1, &input);
                let valid: usize = chunks.iter().map(|(_, v)| *v).sum();
                let delay = FrameResampler::new(in_hz, 16_000, 480, 1)
                    .unwrap()
                    .output_delay();
                let expected = frames * 16_000 / in_hz + delay;
                assert_eq!(valid, expected, "{in_hz} Hz, {frames} frames");
            }
        }
    }
}
