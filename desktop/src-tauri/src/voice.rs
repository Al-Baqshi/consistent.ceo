//! Native desktop voice session: cpal mic → local resample to 16kHz PCM16 → gateway WS proxy.
//!
//! Tauri exposes `voice_start` and `voice_stop` commands. The audio pipeline
//! runs in a background tokio task; the frontend uses browser `Audio()` APIs
//! to play back the 24kHz PCM16 frames the gateway relays back.

use std::sync::{Arc, Mutex};

use consistent_world_audio::capture::{
    downmix_to_mono, f32_to_pcm16_le, start_default_input_threaded,
};
use consistent_world_audio::resample::Resampler;
use futures::{SinkExt, StreamExt};
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, http::header::AUTHORIZATION, protocol::Message},
};

const TARGET_RATE: u32 = 16_000;
const RESAMPLE_CHUNK: usize = 480; // 30ms @ 16kHz

/// Handle to the running voice session's stop signal; `None` while idle.
#[derive(Default)]
pub struct VoiceProcess(pub Arc<Mutex<Option<oneshot::Sender<()>>>>);

/// Arguments the frontend passes to start a voice session.
#[derive(serde::Deserialize)]
pub struct VoiceStartArgs {
    pub gateway_ws_url: String,
    pub auth_token: Option<String>,
    pub system_instruction: Option<String>,
}

/// Start a voice session: capture the mic, stream 16kHz PCM16 to the gateway
/// over WebSocket, and relay its replies back to the frontend as events.
#[tauri::command]
pub async fn voice_start(
    app: AppHandle,
    state: tauri::State<'_, VoiceProcess>,
    args: VoiceStartArgs,
) -> Result<(), String> {
    {
        let mut guard = state.0.lock().map_err(|e| e.to_string())?;
        if guard.is_some() {
            return Err("voice session already running".to_string());
        }
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        *guard = Some(stop_tx);
        drop(guard);

        let app_handle = app.clone();
        tokio::spawn(async move {
            if let Err(e) = run_session(app_handle.clone(), args, stop_rx).await {
                let _ = app_handle.emit("voice-error", e.to_string());
            }
            let _ = app_handle.emit("voice-stopped", ());
        });
    }
    Ok(())
}

/// Signal the running voice session (if any) to stop.
#[tauri::command]
pub async fn voice_stop(state: tauri::State<'_, VoiceProcess>) -> Result<(), String> {
    let mut guard = state.0.lock().map_err(|e| e.to_string())?;
    if let Some(tx) = guard.take() {
        let _ = tx.send(());
    }
    Ok(())
}

// Drives one voice session end to end: opens the mic, connects to the
// gateway's WS endpoint, then loops relaying mic audio out and server
// audio/control frames back in, until stopped or the socket closes.
async fn run_session(
    app: AppHandle,
    args: VoiceStartArgs,
    mut stop_rx: oneshot::Receiver<()>,
) -> Result<(), String> {
    let mut mic = start_default_input_threaded(16).map_err(|e| e.to_string())?;
    let source_rate = mic.sample_rate;
    let channels = mic.channels;
    let mut resampler =
        Resampler::new(source_rate, TARGET_RATE, RESAMPLE_CHUNK).map_err(|e| e.to_string())?;

    // Native client, so the bearer goes in a header rather than the query
    // string — the gateway accepts either, but a URL-borne token ends up in
    // access logs and Referer headers.
    let mut request = args
        .gateway_ws_url
        .as_str()
        .into_client_request()
        .map_err(|e| e.to_string())?;
    if let Some(tok) = args.auth_token.as_deref() {
        request.headers_mut().insert(
            AUTHORIZATION,
            format!("Bearer {tok}")
                .parse()
                .map_err(|_| "bad auth token")?,
        );
    }

    let (ws_stream, _) = connect_async(request).await.map_err(|e| e.to_string())?;
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    if let Some(sys) = args.system_instruction.as_deref() {
        let cfg = serde_json::json!({
            "type": "config",
            "systemInstruction": sys,
        });
        let _ = ws_tx
            .send(Message::Text(cfg.to_string()))
            .await
            .map_err(|e| e.to_string());
    }

    loop {
        tokio::select! {
            // Caller asked us to stop: close the socket and exit the loop.
            _ = &mut stop_rx => {
                let _ = ws_tx.send(Message::Close(None)).await;
                break;
            }
            // A mic frame arrived: downmix, resample to 16kHz, encode PCM16, send.
            frame = mic.rx.recv() => {
                let Some(chunk) = frame else { break };
                let mono = downmix_to_mono(&chunk, channels);
                let resampled = resampler.push(&mono).map_err(|e| e.to_string())?;
                if resampled.is_empty() { continue }
                let pcm = f32_to_pcm16_le(&resampled);
                if ws_tx.send(Message::Binary(pcm)).await.is_err() {
                    break;
                }
            }
            // The gateway sent audio to play back or a control message: relay
            // both to the frontend as events.
            srv = ws_rx.next() => {
                let Some(msg) = srv else { break };
                let msg = msg.map_err(|e| e.to_string())?;
                match msg {
                    Message::Binary(bytes) => {
                        let _ = app.emit("voice-audio", bytes.to_vec());
                    }
                    Message::Text(t) => {
                        let _ = app.emit("voice-control", t.to_string());
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
