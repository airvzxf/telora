//! Streaming mono resampler from the capture device's native rate to
//! the 16 kHz the ASR engines expect.
//!
//! Wraps rubato's synchronous FFT resampler, which handles any rate
//! pair (44.1 kHz, 22.05 kHz, 8 kHz, …) with proper anti-aliasing.
//! Buffers are allocated once in [`MonoResampler::new`], so
//! [`MonoResampler::push`] is safe to call from the realtime audio
//! callback.
//!
//! Samples are processed in fixed chunks (~20 ms), so up to one chunk
//! of trailing audio stays buffered until the next callback fills it.

use anyhow::{Context, Result};
use rubato::{FftFixedInOut, Resampler};

/// Requested input chunk size in frames; rubato may round it.
const CHUNK_FRAMES: usize = 1024;

pub struct MonoResampler {
    /// `None` when input and output rates match (passthrough).
    inner: Option<FftFixedInOut<f32>>,
    input: Vec<f32>,
    output: Vec<f32>,
    chunk_in: usize,
}

impl MonoResampler {
    pub fn new(rate_in: u32, rate_out: u32) -> Result<Self> {
        if rate_in == rate_out {
            return Ok(Self {
                inner: None,
                input: Vec::new(),
                output: Vec::new(),
                chunk_in: 0,
            });
        }
        let inner = FftFixedInOut::<f32>::new(rate_in as usize, rate_out as usize, CHUNK_FRAMES, 1)
            .with_context(|| format!("creating {rate_in} Hz -> {rate_out} Hz resampler"))?;
        let chunk_in = inner.input_frames_next();
        let output = vec![0.0; inner.output_frames_max()];
        Ok(Self {
            inner: Some(inner),
            input: Vec::with_capacity(chunk_in),
            output,
            chunk_in,
        })
    }

    /// Feed one input sample; `emit` is called for every output sample
    /// produced.
    pub fn push(&mut self, sample: f32, mut emit: impl FnMut(f32)) {
        let Some(inner) = self.inner.as_mut() else {
            emit(sample);
            return;
        };
        self.input.push(sample);
        if self.input.len() < self.chunk_in {
            return;
        }
        match inner.process_into_buffer(&[&self.input], &mut [&mut self.output], None) {
            Ok((_, written)) => self.output[..written].iter().copied().for_each(&mut emit),
            Err(e) => log::error!(
                "resampler failed, dropping {} frames: {e}",
                self.input.len()
            ),
        }
        self.input.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: u32 = 16_000;

    fn sine(rate: u32, freq: f32, seconds: f32) -> Vec<f32> {
        let n = (rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    fn resample(rate_in: u32, input: &[f32]) -> Vec<f32> {
        let mut r = MonoResampler::new(rate_in, TARGET).unwrap();
        let mut out = Vec::new();
        for &s in input {
            r.push(s, |o| out.push(o));
        }
        out
    }

    /// Frequency estimated from rising zero crossings, skipping the
    /// first 100 ms (resampler warm-up).
    fn estimated_freq(samples: &[f32], rate: u32) -> f32 {
        let skip = rate as usize / 10;
        let body = &samples[skip..];
        let rising = body
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        rising as f32 * rate as f32 / body.len() as f32
    }

    #[test]
    fn common_capture_rates_keep_pitch_and_duration() {
        // 44.1 kHz and 22.05 kHz are the rates the old integer
        // decimation got wrong (they came out at 22.05 kHz labelled as
        // 16 kHz, i.e. slowed down and pitched down).
        for rate in [48_000, 44_100, 32_000, 22_050, 8_000] {
            let out = resample(rate, &sine(rate, 1000.0, 2.0));

            let expected_len = 2 * TARGET as usize;
            let missing = expected_len.abs_diff(out.len());
            assert!(
                missing <= 2 * CHUNK_FRAMES,
                "{rate} Hz: got {} samples, expected about {expected_len}",
                out.len()
            );

            let f = estimated_freq(&out, TARGET);
            assert!(
                (f - 1000.0).abs() < 10.0,
                "{rate} Hz: 1 kHz tone came out at {f:.1} Hz"
            );
        }
    }

    #[test]
    fn matching_rate_is_passthrough() {
        let input = sine(TARGET, 440.0, 0.1);
        assert_eq!(resample(TARGET, &input), input);
    }

    #[test]
    fn content_above_target_nyquist_is_filtered() {
        // 10 kHz is above 8 kHz (Nyquist at 16 kHz). Plain decimation
        // would alias it down to an audible tone; the resampler must
        // suppress it.
        let out = resample(48_000, &sine(48_000, 10_000.0, 1.0));
        let body = &out[TARGET as usize / 10..];
        let rms = (body.iter().map(|s| s * s).sum::<f32>() / body.len() as f32).sqrt();
        assert!(rms < 0.01, "aliased energy too high: rms {rms}");
    }
}
