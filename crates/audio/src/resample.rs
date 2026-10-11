//! Streaming resampler using rubato's SincFixedIn for arbitrary source -> target rates.
//!
//! Input: mono f32 samples at `source_rate`.
//! Output: mono f32 samples at `target_rate`, suitable for PCM16 encoding.

use anyhow::{Context, Result};
use rubato::{
    Resampler as RubatoResampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// Stateful wrapper around rubato's `SincFixedIn`, buffering partial input
/// chunks between calls to `push`.
pub struct Resampler {
    inner: SincFixedIn<f32>,
    pub source_rate: u32,
    pub target_rate: u32,
    pub chunk_size: usize,
    /// Carry-over input samples that didn't fill a full chunk last call.
    leftover: Vec<f32>,
}

impl Resampler {
    /// Build a new resampler. `chunk_size` is the number of input samples per
    /// call to `process_chunk`; the output length depends on the ratio.
    pub fn new(source_rate: u32, target_rate: u32, chunk_size: usize) -> Result<Self> {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };
        let inner = SincFixedIn::<f32>::new(
            target_rate as f64 / source_rate as f64,
            1.0,
            params,
            chunk_size,
            1,
        )
        .context("failed to construct SincFixedIn resampler")?;
        Ok(Self {
            inner,
            source_rate,
            target_rate,
            chunk_size,
            leftover: Vec::new(),
        })
    }

    /// Feed an arbitrary-length chunk of mono f32 samples. Returns resampled
    /// mono f32 samples. Internally buffers partial chunks.
    pub fn push(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        self.leftover.extend_from_slice(input);
        let mut out: Vec<f32> = Vec::new();
        // Only resample once a full chunk is available; a short remainder
        // stays buffered for the next call.
        while self.leftover.len() >= self.chunk_size {
            let block: Vec<f32> = self.leftover.drain(..self.chunk_size).collect();
            let channels = vec![block];
            let resampled = self
                .inner
                .process(&channels, None)
                .context("resampler process failed")?;
            out.extend(resampled.into_iter().next().unwrap_or_default());
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downsamples_48k_to_16k_preserves_amplitude_roughly() {
        let src = 48_000u32;
        let tgt = 16_000u32;
        let chunk = 480;
        let mut r = Resampler::new(src, tgt, chunk).unwrap();

        let tone: Vec<f32> = (0..src)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / src as f32).sin())
            .collect();

        let mut out: Vec<f32> = Vec::new();
        for block in tone.chunks(chunk) {
            if block.len() == chunk {
                out.extend(r.push(block).unwrap());
            }
        }

        assert!(!out.is_empty());
        let peak = out.iter().map(|v| v.abs()).fold(0.0_f32, f32::max);
        assert!(peak > 0.5 && peak < 1.1, "unexpected peak: {peak}");
    }

    #[test]
    fn partial_chunk_is_buffered_not_dropped() {
        let mut r = Resampler::new(44_100, 16_000, 441).unwrap();
        let half: Vec<f32> = vec![0.1; 100];
        let out = r.push(&half).unwrap();
        assert!(out.is_empty(), "partial chunk must not emit yet");
        assert_eq!(r.leftover.len(), 100);
    }
}
