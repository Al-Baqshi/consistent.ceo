//! The capture → resample → VAD → whisper loop.
//!
//! whisper.cpp is not a streaming recogniser: it transcribes a whole audio
//! buffer at once. So we segment speech ourselves with a simple energy gate and
//! transcribe one *utterance* (speech bounded by silence) at a time:
//!
//!   * while you speak, the growing buffer is transcribed on a throttle and sent
//!     as a `volatile` (rewrite-in-place) hypothesis;
//!   * when you pause (silence past a threshold), the buffer is transcribed once
//!     more and sent as `final` — the committed text the shell types at the
//!     cursor — then the buffer resets for the next utterance.
//!
//! Audio maths (downmix, resample to 16 kHz) is reused from
//! `consistent_world_audio`, the same code the Gemini Live voice path uses.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use consistent_world_audio::capture::{downmix_to_mono, start_default_input_threaded};
use consistent_world_audio::resample::Resampler;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

use crate::emit;

/// whisper.cpp's fixed input rate.
const TARGET_RATE: u32 = 16_000;

/// RMS above this counts as speech. Mic input in [-1,1]; quiet rooms sit well
/// below 0.01, conversational speech above it.
const SPEECH_RMS: f32 = 0.012;
/// Silence this long *after* speech ends an utterance and commits a `final`.
const SILENCE_TO_COMMIT: Duration = Duration::from_millis(700);
/// Don't transcribe buffers shorter than this — whisper pads to 1 s and short
/// blips just produce noise.
const MIN_UTTERANCE: Duration = Duration::from_millis(400);
/// Force a commit past this length so a long monologue still lands as text and
/// each transcription stays fast.
const MAX_UTTERANCE: Duration = Duration::from_secs(18);
/// Don't recompute the volatile hypothesis more often than this — one whisper
/// pass over a few seconds of audio is not free on a CPU.
const VOLATILE_EVERY: Duration = Duration::from_millis(900);

fn samples_for(d: Duration) -> usize {
    (TARGET_RATE as f64 * d.as_secs_f64()) as usize
}

/// Load the model and run the dictation loop until `running` goes false
/// (a `stop`/`quit` command). Emits protocol events via [`crate::emit`].
pub fn listen(running: Arc<AtomicBool>, model_path: &str) -> Result<()> {
    let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
        .context("load whisper model")?;
    let mut state = ctx.create_state().context("create whisper state")?;

    // Keep `mic` whole for the whole loop: it owns a Drop that stops capture and
    // releases the device, so `rx` is borrowed in place rather than moved out.
    let mut mic = start_default_input_threaded(16).context("open microphone")?;
    let native_rate = mic.sample_rate;
    let channels = mic.channels;

    // 10 ms resampler blocks; `push` buffers anything that doesn't fill a block.
    let chunk = (native_rate / 100).max(1) as usize;
    let mut resampler = Resampler::new(native_rate, TARGET_RATE, chunk)
        .context("build 16 kHz resampler")?;

    emit(serde_json::json!({ "type": "state", "value": "listening" }));

    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4);

    let mut utterance: Vec<f32> = Vec::new();
    let mut silence_run = 0usize; // consecutive silent 16 kHz samples
    let mut had_speech = false;
    let mut last_volatile = Instant::now();
    let mut last_emitted_volatile = String::new();

    while running.load(Ordering::Relaxed) {
        // Pull one native-rate chunk; time out so `stop` is honoured even if the
        // mic goes quiet and stops delivering buffers.
        let native = match mic.rx.blocking_recv() {
            Some(c) => c,
            None => break, // mic thread gone
        };

        let mono = downmix_to_mono(&native, channels);
        let mut got = resampler.push(&mono).context("resample")?;
        if got.is_empty() {
            continue;
        }

        // Track silence on the incoming block before it's appended.
        let rms = rms(&got);
        if rms >= SPEECH_RMS {
            had_speech = true;
            silence_run = 0;
        } else {
            silence_run += got.len();
        }
        utterance.append(&mut got);

        let dur = utterance.len();
        let silence = Duration::from_secs_f64(silence_run as f64 / TARGET_RATE as f64);

        // Commit when speech is followed by enough silence, or the buffer is long.
        let long_enough = dur >= samples_for(MIN_UTTERANCE);
        let ended = had_speech && long_enough && silence >= SILENCE_TO_COMMIT;
        let too_long = dur >= samples_for(MAX_UTTERANCE);

        if ended || too_long {
            if let Some(text) = transcribe(&mut state, &utterance, threads)? {
                emit(serde_json::json!({ "type": "final", "text": text }));
            }
            utterance.clear();
            silence_run = 0;
            had_speech = false;
            last_emitted_volatile.clear();
            continue;
        }

        // Live hypothesis while speaking, throttled.
        if had_speech && long_enough && last_volatile.elapsed() >= VOLATILE_EVERY {
            last_volatile = Instant::now();
            if let Some(text) = transcribe(&mut state, &utterance, threads)? {
                if text != last_emitted_volatile {
                    last_emitted_volatile = text.clone();
                    emit(serde_json::json!({ "type": "volatile", "text": text }));
                }
            }
        }
    }

    // Flush a trailing utterance so the last words aren't lost on stop.
    if had_speech && utterance.len() >= samples_for(MIN_UTTERANCE) {
        if let Some(text) = transcribe(&mut state, &utterance, threads)? {
            emit(serde_json::json!({ "type": "final", "text": text }));
        }
    }

    emit(serde_json::json!({ "type": "state", "value": "stopped" }));
    Ok(())
}

/// One-shot transcription of a 16 kHz mono buffer through a freshly loaded
/// model. Used by the `--file` self-test; the live loop reuses a single state.
pub fn transcribe_once(model_path: &str, audio_16k: &[f32]) -> Result<Option<String>> {
    let ctx = WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
        .context("load whisper model")?;
    let mut state = ctx.create_state().context("create whisper state")?;
    let threads = std::thread::available_parallelism()
        .map(|n| n.get() as i32)
        .unwrap_or(4);
    transcribe(&mut state, audio_16k, threads)
}

/// Root-mean-square amplitude of a block — our voice-activity signal.
fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
    (sum_sq / samples.len() as f32).sqrt()
}

/// Transcribe 16 kHz mono audio, returning trimmed text (or `None` if empty).
fn transcribe(
    state: &mut whisper_rs::WhisperState,
    audio: &[f32],
    threads: i32,
) -> Result<Option<String>> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some("en"));
    params.set_n_threads(threads);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    // Suppress whisper's non-speech tokens like "[BLANK_AUDIO]".
    params.set_suppress_blank(true);

    state.full(params, audio).context("whisper full")?;

    let n = state.full_n_segments().context("segment count")?;
    let mut text = String::new();
    for i in 0..n {
        text.push_str(&state.full_get_segment_text(i).context("segment text")?);
    }
    let text = text.trim().to_string();
    if text.is_empty() || is_noise_token(&text) {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

/// whisper occasionally emits bracketed non-speech markers for near-silence.
/// Treat those as nothing said.
fn is_noise_token(text: &str) -> bool {
    let t = text.trim();
    (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_is_zero() {
        assert_eq!(rms(&[]), 0.0);
        assert_eq!(rms(&[0.0; 100]), 0.0);
    }

    #[test]
    fn rms_of_constant_is_its_magnitude() {
        // RMS of a DC level equals its absolute value.
        assert!((rms(&[0.5; 64]) - 0.5).abs() < 1e-6);
        assert!((rms(&[-0.3; 64]) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn speech_reads_louder_than_a_quiet_room() {
        // A tone near typical speech level clears the gate; near-silence doesn't.
        let speech: Vec<f32> = (0..1600)
            .map(|i| 0.2 * (2.0 * std::f32::consts::PI * 200.0 * i as f32 / 16000.0).sin())
            .collect();
        let quiet: Vec<f32> = (0..1600).map(|i| 0.001 * (i as f32).sin()).collect();
        assert!(rms(&speech) >= SPEECH_RMS);
        assert!(rms(&quiet) < SPEECH_RMS);
    }

    #[test]
    fn bracketed_markers_are_treated_as_noise() {
        assert!(is_noise_token("[BLANK_AUDIO]"));
        assert!(is_noise_token("(silence)"));
        assert!(is_noise_token("  [music]  "));
        assert!(!is_noise_token("hello there"));
        assert!(!is_noise_token("[unterminated"));
        assert!(!is_noise_token("the array[0] value"));
    }

    #[test]
    fn samples_for_matches_the_16khz_rate() {
        assert_eq!(samples_for(std::time::Duration::from_secs(1)), 16_000);
        assert_eq!(samples_for(std::time::Duration::from_millis(500)), 8_000);
    }
}
