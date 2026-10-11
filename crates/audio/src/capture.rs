//! Cross-platform microphone capture using cpal.
//!
//! Emits f32 mono samples at the device's native rate through a tokio mpsc
//! channel. Consumers are responsible for resampling to the target rate.

use anyhow::{Context, Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat, Stream, StreamConfig};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Handle to an active cpal input stream plus the channel receiving
/// captured samples.
pub struct MicStream {
    #[allow(dead_code)]
    stream: Stream,
    pub sample_rate: u32,
    pub channels: u16,
    pub rx: mpsc::Receiver<Vec<f32>>,
}

/// Start capturing from the system default input device.
///
/// Returned `rx` yields chunks of interleaved f32 samples at the device's
/// native rate (not resampled). The underlying cpal stream is retained inside
/// the `MicStream` handle — dropping it stops capture.
pub fn start_default_input(buffer_chunks: usize) -> Result<MicStream> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow!("no default input device available"))?;

    let config = device
        .default_input_config()
        .context("failed to get default input config")?;

    let sample_rate = config.sample_rate().0;
    let channels = config.channels();
    let sample_format = config.sample_format();
    let stream_config: StreamConfig = config.clone().into();

    debug!(
        "starting mic capture: device={:?} rate={} channels={} format={:?}",
        device.name().ok(),
        sample_rate,
        channels,
        sample_format
    );

    let (tx, rx) = mpsc::channel::<Vec<f32>>(buffer_chunks.max(8));

    let err_fn = |err| warn!("cpal stream error: {err}");

    // cpal delivers samples in whatever the default device's native format
    // is; normalize every branch to f32 before handing off to the channel.
    let stream = match sample_format {
        SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _| {
                let _ = tx.try_send(data.to_vec());
            },
            err_fn,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _| {
                let out: Vec<f32> = data.iter().map(|s| s.to_sample::<f32>()).collect();
                let _ = tx.try_send(out);
            },
            err_fn,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _| {
                let out: Vec<f32> = data.iter().map(|s| s.to_sample::<f32>()).collect();
                let _ = tx.try_send(out);
            },
            err_fn,
            None,
        ),
        other => anyhow::bail!("unsupported sample format: {:?}", other),
    }
    .context("failed to build input stream")?;

    stream.play().context("failed to start input stream")?;

    Ok(MicStream {
        stream,
        sample_rate,
        channels,
        rx,
    })
}

/// Send-safe microphone handle.
///
/// cpal's `Stream` is `!Send` on some platforms (notably macOS/CoreAudio), which
/// makes [`MicStream`] impossible to hold across an `.await` in a Tokio task.
/// `ThreadedMic` parks the underlying stream on a dedicated OS thread so the
/// metadata + receiver — which are all `Send` — can be freely moved into async
/// code. Dropping `ThreadedMic` signals the thread to stop and release the
/// device.
pub struct ThreadedMic {
    pub sample_rate: u32,
    pub channels: u16,
    pub rx: mpsc::Receiver<Vec<f32>>,
    shutdown: Option<std::sync::mpsc::Sender<()>>,
}

impl Drop for ThreadedMic {
    fn drop(&mut self) {
        if let Some(s) = self.shutdown.take() {
            let _ = s.send(());
        }
    }
}

/// Spawn [`start_default_input`] on a dedicated OS thread and return a
/// `Send`-safe handle. Blocks until the stream reports its configuration.
pub fn start_default_input_threaded(buffer_chunks: usize) -> Result<ThreadedMic> {
    type MetaResult = Result<(u32, u16, mpsc::Receiver<Vec<f32>>)>;
    let (meta_tx, meta_rx) = std::sync::mpsc::channel::<MetaResult>();
    let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel::<()>();

    std::thread::Builder::new()
        .name("consistent-world-mic".into())
        .spawn(move || {
            let mic = match start_default_input(buffer_chunks) {
                Ok(m) => m,
                Err(e) => {
                    let _ = meta_tx.send(Err(e));
                    return;
                }
            };
            let MicStream {
                stream,
                sample_rate,
                channels,
                rx,
            } = mic;
            if meta_tx.send(Ok((sample_rate, channels, rx))).is_err() {
                drop(stream);
                return;
            }
            // Park here, keeping `stream` (and thus the mic) alive, until
            // ThreadedMic is dropped and signals shutdown.
            let _ = shutdown_rx.recv();
            drop(stream);
        })
        .context("failed to spawn mic thread")?;

    let (sample_rate, channels, rx) = meta_rx
        .recv()
        .map_err(|e| anyhow!("mic thread died before reporting config: {e}"))??;
    Ok(ThreadedMic {
        sample_rate,
        channels,
        rx,
        shutdown: Some(shutdown_tx),
    })
}

/// Downmix interleaved multi-channel f32 samples to mono by averaging channels.
pub fn downmix_to_mono(interleaved: &[f32], channels: u16) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let ch = channels as usize;
    interleaved
        .chunks_exact(ch)
        .map(|frame| frame.iter().sum::<f32>() / ch as f32)
        .collect()
}

/// Convert f32 samples in [-1.0, 1.0] to PCM16 little-endian bytes.
pub fn f32_to_pcm16_le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let v = (clamped * i16::MAX as f32) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// Convert PCM16 LE bytes back to f32 samples (for playback of server audio).
pub fn pcm16_le_to_f32(bytes: &[u8]) -> Vec<f32> {
    // `as_chunks` over `chunks_exact` so the chunk arrives as a `[u8; 2]`
    // rather than a slice the compiler has to bounds-check on every index.
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| i16::from_le_bytes(*b) as f32 / i16::MAX as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_stereo_averages_pairs() {
        let input = vec![1.0, -1.0, 0.5, 0.5];
        let out = downmix_to_mono(&input, 2);
        assert_eq!(out, vec![0.0, 0.5]);
    }

    #[test]
    fn downmix_mono_returns_clone() {
        let input = vec![0.1, 0.2, 0.3];
        assert_eq!(downmix_to_mono(&input, 1), input);
    }

    #[test]
    fn pcm16_round_trip_preserves_shape() {
        let f = vec![0.0, 0.5, -0.5, 1.0, -1.0];
        let bytes = f32_to_pcm16_le(&f);
        assert_eq!(bytes.len(), f.len() * 2);
        let back = pcm16_le_to_f32(&bytes);
        for (a, b) in f.iter().zip(back.iter()) {
            assert!((a - b).abs() < 0.001);
        }
    }
}
