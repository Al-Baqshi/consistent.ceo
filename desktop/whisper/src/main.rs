//! On-device dictation sidecar — Whisper (whisper.cpp) engine.
//!
//! The cross-platform counterpart to `consistent-dictate` (Apple SpeechAnalyzer,
//! Apple-Silicon-only). It speaks the **identical** NDJSON protocol, so the
//! Tauri shell in `desktop/src-tauri/src/dictation.rs` drives either engine with
//! the same code and picks one at runtime.
//!
//! Protocol (one JSON object per line):
//!   stdin  (client → sidecar): {"cmd":"start"} | {"cmd":"stop"} | {"cmd":"quit"}
//!   stdout (sidecar → client): {"type":"ready"}
//!                              {"type":"model","status":"downloading"|"ready"}
//!                              {"type":"state","value":"listening"|"stopped"}
//!                              {"type":"volatile","text":"…"}   (in-progress guess)
//!                              {"type":"final","text":"…"}      (committed segment)
//!                              {"type":"error","message":"…"}
//!   stderr: freeform debug logs (never parsed by the client)

mod engine;
mod model;

use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;

/// Serialize one JSON object as a single stdout line and flush. A global lock
/// keeps whole lines intact when the engine thread and main thread both emit.
pub fn emit(value: serde_json::Value) {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn log(msg: &str) {
    eprintln!("[whisper] {msg}");
}

/// A running dictation session: the engine thread and its stop flag.
struct Session {
    running: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

/// Start listening if not already. Ensures the model is present first (a
/// one-time download), then spawns the capture/transcribe loop on its own thread
/// so the stdin command loop stays responsive.
fn start(session: &mut Option<Session>) {
    if session.is_some() {
        return; // idempotent: a held hotkey re-sends `start`
    }

    let model_path = match model::ensure_model(|| {
        emit(serde_json::json!({ "type": "model", "status": "downloading" }));
    }) {
        Ok(p) => {
            emit(serde_json::json!({ "type": "model", "status": "ready" }));
            p
        }
        Err(e) => {
            emit(serde_json::json!({ "type": "error", "message": format!("model unavailable: {e}") }));
            return;
        }
    };

    let running = Arc::new(AtomicBool::new(true));
    let engine_running = running.clone();
    let model_str = model_path.to_string_lossy().to_string();

    let handle = std::thread::Builder::new()
        .name("whisper-engine".into())
        .spawn(move || {
            if let Err(e) = engine::listen(engine_running.clone(), &model_str) {
                emit(serde_json::json!({ "type": "error", "message": format!("dictation failed: {e}") }));
                emit(serde_json::json!({ "type": "state", "value": "stopped" }));
            }
            engine_running.store(false, Ordering::Relaxed);
        })
        .expect("spawn engine thread");

    *session = Some(Session { running, handle });
}

/// Stop the current session (if any) and wait for the engine to flush + release
/// the microphone.
fn stop(session: &mut Option<Session>) {
    if let Some(s) = session.take() {
        s.running.store(false, Ordering::Relaxed);
        let _ = s.handle.join();
    }
}

/// Offline self-test: transcribe a WAV file and print the text. Proves the model
/// loads and runs on this host without needing a live mic or an interactive
/// session — the only way to verify transcription on headless CI or an Intel box.
/// `consistent-whisper --file path/to/audio.wav`
fn run_file_test(path: &str) -> i32 {
    let audio = match load_wav_16k_mono(path) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[whisper] could not read {path}: {e}");
            return 2;
        }
    };
    let model = match model::ensure_model(|| eprintln!("[whisper] downloading model…")) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[whisper] model unavailable: {e}");
            return 2;
        }
    };
    match engine::transcribe_once(&model.to_string_lossy(), &audio) {
        Ok(Some(text)) => {
            println!("{text}");
            0
        }
        Ok(None) => {
            eprintln!("[whisper] (no speech detected)");
            1
        }
        Err(e) => {
            eprintln!("[whisper] transcription failed: {e}");
            2
        }
    }
}

/// Load a WAV as 16 kHz mono f32, downmixing and resampling as needed.
fn load_wav_16k_mono(path: &str) -> anyhow::Result<Vec<f32>> {
    use anyhow::Context;
    let mut reader = hound::WavReader::open(path).context("open wav")?;
    let spec = reader.spec();
    let channels = spec.channels;

    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().filter_map(Result::ok).collect(),
        hound::SampleFormat::Int => {
            let max = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .filter_map(Result::ok)
                .map(|s| s as f32 / max)
                .collect()
        }
    };

    let mono = consistent_world_audio::capture::downmix_to_mono(&interleaved, channels);
    if spec.sample_rate == 16_000 {
        return Ok(mono);
    }
    let chunk = (spec.sample_rate / 100).max(1) as usize;
    let mut rs = consistent_world_audio::resample::Resampler::new(spec.sample_rate, 16_000, chunk)
        .context("build resampler")?;
    rs.push(&mono).context("resample wav")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Self-test mode: transcribe a file and exit (no protocol, no mic).
    if let Some(i) = args.iter().position(|a| a == "--file") {
        let path = args.get(i + 1).cloned().unwrap_or_default();
        if path.is_empty() {
            eprintln!("[whisper] --file needs a path");
            std::process::exit(2);
        }
        std::process::exit(run_file_test(&path));
    }

    emit(serde_json::json!({ "type": "ready" }));

    let mut session: Option<Session> = None;

    // `--once` starts listening immediately for a manual terminal smoke test.
    if args.iter().any(|a| a == "--once") {
        start(&mut session);
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cmd = serde_json::from_str::<serde_json::Value>(trimmed)
            .ok()
            .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_string));

        match cmd.as_deref() {
            Some("start") => start(&mut session),
            Some("stop") => stop(&mut session),
            Some("quit") => {
                stop(&mut session);
                return;
            }
            _ => log(&format!("ignoring unparseable stdin line: {trimmed}")),
        }
    }

    // stdin closed: shut down cleanly so the mic is released.
    stop(&mut session);
}
