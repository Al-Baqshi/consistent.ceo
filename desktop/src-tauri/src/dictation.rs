//! On-device dictation via the `consistent-dictate` Swift sidecar.
//!
//! The sidecar wraps Apple's SpeechAnalyzer (macOS 26+) and speaks NDJSON:
//! we write `{"cmd":"start"|"stop"|"quit"}` to its stdin and forward every
//! stdout line to the frontend as a `dictation-event` (parsed JSON payload).
//!
//! The frontend is responsible for what to do with the transcript — show it,
//! and (Phase 2) inject `final` segments at the OS cursor. This module is just
//! the process broker, mirroring `voice.rs`.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// Holds the running sidecar child so `dictate_stop` can reach it. `None` when
/// no dictation session is active.
#[derive(Default)]
pub struct DictationProcess(pub Arc<Mutex<Option<CommandChild>>>);

/// What a committed transcript segment should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Type the text at the OS cursor (the Wispr-Flow behaviour).
    Dictate,
    /// Send the text to the gateway's tool-calling agent to act on.
    Command,
}

// Encodes Mode as a bare u8 for atomic storage: 0 = Dictate, 1 = Command.
static MODE: AtomicU8 = AtomicU8::new(0);

/// The dictation mode currently in effect (last value set by `set_mode`).
pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::Command,
        _ => Mode::Dictate,
    }
}

/// Record which mode a committed `final` transcript segment should be
/// handled as.
pub fn set_mode(mode: Mode) {
    MODE.store(
        match mode {
            Mode::Dictate => 0,
            Mode::Command => 1,
        },
        Ordering::Relaxed,
    );
}

/// Which dictation sidecar to spawn on this machine.
///
/// Apple's on-device SpeechAnalyzer (`consistent-dictate`) needs Apple Silicon's
/// Neural Engine and macOS 26+. Everywhere else — Intel Macs, older macOS,
/// Windows, Linux — the cross-platform Whisper engine (`consistent-whisper`)
/// runs the model on the CPU. Both speak the same NDJSON protocol, so the rest
/// of this module is engine-agnostic.
///
/// `CONSISTENT_DICTATION_ENGINE=apple|whisper` forces one, for testing.
fn dictation_engine() -> &'static str {
    match std::env::var("CONSISTENT_DICTATION_ENGINE").as_deref() {
        Ok("apple") => return "consistent-dictate",
        Ok("whisper") => return "consistent-whisper",
        _ => {}
    }
    #[cfg(target_os = "macos")]
    {
        if std::env::consts::ARCH == "aarch64" && macos_major() >= 26 {
            return "consistent-dictate";
        }
    }
    "consistent-whisper"
}

/// Major component of the macOS product version (e.g. 26 from "26.5.2"), or 0 if
/// it can't be read.
#[cfg(target_os = "macos")]
fn macos_major() -> u32 {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.trim()
                .split('.')
                .next()
                .and_then(|m| m.parse::<u32>().ok())
        })
        .unwrap_or(0)
}

/// True while a dictation sidecar is running.
fn is_running(app: &AppHandle) -> bool {
    app.state::<DictationProcess>()
        .0
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

/// Start or stop dictation in `mode`. Driven by the global hotkeys, so the
/// feature works end-to-end without any frontend involvement.
pub fn toggle(app: &AppHandle, mode: Mode) {
    if is_running(app) {
        stop(app);
    } else {
        start(app, mode);
    }
}

/// Force-start in `mode` — the press half of push-to-talk. Idempotent while
/// already running (a held key repeats its press event).
pub fn start(app: &AppHandle, mode: Mode) {
    set_mode(mode);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = dictate_start(app.clone()).await {
            eprintln!("[dictate] start failed: {e}");
            let _ = app.emit(
                "dictation-event",
                serde_json::json!({
                    "type": "error",
                    "message": e,
                }),
            );
        }
    });
}

/// Stop listening — the release half of push-to-talk, and the second press of
/// a hands-free toggle.
pub fn stop(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = dictate_stop(app.clone()).await {
            eprintln!("[dictate] stop failed: {e}");
        }
    });
}

/// Spawn the dictation sidecar (if not already running) and tell it to start
/// listening. Transcript events arrive on the frontend as `dictation-event`.
#[tauri::command]
pub async fn dictate_start(app: AppHandle) -> Result<(), String> {
    let state = app.state::<DictationProcess>();
    {
        // Already running: just (re)send start so a held hotkey is idempotent.
        let mut guard = state.0.lock().map_err(|e| e.to_string())?;
        if let Some(child) = guard.as_mut() {
            return write_cmd(child, "start");
        }
    }

    let engine = dictation_engine();
    // The Apple engine (`consistent-dictate`) is not declared in
    // `bundle.externalBin` — it can't be, because Tauri requires every
    // declared sidecar to exist on every platform and this one only builds on
    // macOS 26+. A shipped app on Apple Silicon therefore may not carry it:
    // fall back to the cross-platform Whisper engine instead of hard-failing.
    let sidecar = match app.shell().sidecar(engine) {
        Ok(cmd) => cmd,
        Err(e) if engine == "consistent-dictate" => {
            eprintln!("[dictate] apple engine unavailable ({e}); falling back to whisper");
            app.shell().sidecar("consistent-whisper").map_err(|e| {
                format!("dictation sidecar 'consistent-whisper' not found (run build:whisper): {e}")
            })?
        }
        Err(e) => {
            return Err(format!(
                "dictation sidecar '{engine}' not found (run build:dictation / build:whisper): {e}"
            ));
        }
    };

    let (mut rx, mut child) = sidecar.spawn().map_err(|e| e.to_string())?;

    // Forward sidecar stdout → frontend. Each stdout chunk may contain one or
    // more NDJSON lines; split and emit the parsed JSON so the UI gets objects.
    let app_for_reader = app.clone();
    tauri::async_runtime::spawn(async move {
        let mut buf = String::new();
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(bytes) => {
                    buf.push_str(&String::from_utf8_lossy(&bytes));
                    emit_complete_lines(&app_for_reader, &mut buf);
                }
                CommandEvent::Stderr(bytes) => {
                    // Debug logs only — surface at trace level via the console.
                    eprintln!("[dictate] {}", String::from_utf8_lossy(&bytes).trim_end());
                }
                CommandEvent::Terminated(payload) => {
                    let _ = app_for_reader.emit(
                        "dictation-event",
                        serde_json::json!({
                            "type": "terminated",
                            "code": payload.code,
                        }),
                    );
                    break;
                }
                CommandEvent::Error(e) => {
                    let _ = app_for_reader.emit(
                        "dictation-event",
                        serde_json::json!({
                            "type": "error",
                            "message": e,
                        }),
                    );
                    break;
                }
                _ => {}
            }
        }
        // Reader loop ended: clear the stored child so the next start respawns.
        if let Some(state) = app_for_reader.try_state::<DictationProcess>() {
            if let Ok(mut guard) = state.0.lock() {
                let _ = guard.take();
            }
        }
    });

    write_cmd(&mut child, "start")?;
    *state.0.lock().map_err(|e| e.to_string())? = Some(child);
    Ok(())
}

/// Tell the sidecar to stop listening and quit; drop our handle to it.
#[tauri::command]
pub async fn dictate_stop(app: AppHandle) -> Result<(), String> {
    let state = app.state::<DictationProcess>();
    let child = state.0.lock().map_err(|e| e.to_string())?.take();
    if let Some(mut child) = child {
        // Ask for a clean shutdown; if the pipe is already gone, kill.
        if write_cmd(&mut child, "quit").is_err() {
            let _ = child.kill();
        }
    }
    Ok(())
}

/// Write one NDJSON control command to the sidecar's stdin.
fn write_cmd(child: &mut CommandChild, cmd: &str) -> Result<(), String> {
    let line = format!("{{\"cmd\":\"{cmd}\"}}\n");
    // CommandChild::write takes &self on a cloneable writer handle.
    child
        .write(line.as_bytes())
        .map_err(|e| format!("failed to write '{cmd}' to dictation sidecar: {e}"))
}

/// Pull complete `\n`-terminated lines out of `buf`, parse each as JSON, and
/// emit it. Any trailing partial line stays in `buf` for the next chunk.
///
/// Committed (`final`) segments are also typed at the OS cursor when auto-inject
/// is on — that's what closes the dictation loop without any frontend work.
fn emit_complete_lines(app: &AppHandle, buf: &mut String) {
    while let Some(idx) = buf.find('\n') {
        let line: String = buf.drain(..=idx).collect();
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(payload) => {
                let kind = payload.get("type").and_then(|t| t.as_str());
                if kind == Some("final") {
                    if let Some(text) = payload.get("text").and_then(|t| t.as_str()) {
                        match mode() {
                            Mode::Dictate if crate::inject::auto_inject_enabled() => {
                                crate::inject::inject_text_detached(text.to_string());
                            }
                            Mode::Command => {
                                crate::agent::send_to_agent_detached(app.clone(), text.to_string());
                            }
                            Mode::Dictate => {}
                        }
                    }
                }
                // First-run feedback: the Whisper engine downloads a ~148 MB
                // model on first use, and dictation errors (mic denied, model
                // fetch failed) are otherwise silent. A system notification is
                // the only surface the user sees — dictation has no window.
                match kind {
                    Some("model")
                        if payload.get("status").and_then(|s| s.as_str())
                            == Some("downloading") =>
                    {
                        notify(
                            app,
                            "Preparing on-device dictation",
                            "Downloading the speech model — one time, then it's instant.",
                        );
                    }
                    Some("error") => {
                        let msg = payload
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("Dictation could not start.");
                        notify(app, "Dictation error", msg);
                    }
                    _ => {}
                }
                let _ = app.emit("dictation-event", payload);
            }
            Err(e) => eprintln!("[dictate] non-JSON stdout line ignored: {e}: {line}"),
        }
    }
}

/// Best-effort system notification — the only feedback surface dictation has,
/// since it runs from a hotkey with no window. Failures are swallowed: a missing
/// notification must never break dictation itself.
fn notify(app: &AppHandle, title: &str, body: &str) {
    let _ = app.notification().builder().title(title).body(body).show();
}
