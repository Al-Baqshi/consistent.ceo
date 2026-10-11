//! Gemini Live bidirectional audio WebSocket client.
//!
//! Connects to `BidiGenerateContent` and streams PCM16 16kHz audio in,
//! receives PCM16 24kHz audio + text out, per Gemini Live protocol.

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::{CloseFrame, Message},
};
use tracing::{debug, warn};

const BIDI_URL_BASE: &str = "wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent";

/// Sample rate Gemini Live expects for input audio (mic → server).
pub const INPUT_SAMPLE_RATE: u32 = 16_000;
/// Sample rate Gemini Live returns for output audio (server → playback).
pub const OUTPUT_SAMPLE_RATE: u32 = 24_000;

/// Events surfaced to the caller from the server side of the connection.
#[derive(Debug, Clone)]
pub enum LiveEvent {
    /// Server is ready after accepting the setup payload.
    SetupComplete,
    /// PCM16 LE audio at 24kHz.
    Audio(Vec<u8>),
    /// Incremental text output (if the model emits text in addition to audio).
    Text(String),
    /// Incremental transcription of the *user's* input audio.
    InputTranscription(String),
    /// Server signalled end of the current turn.
    TurnComplete,
    /// User speech interrupted the model.
    Interrupted,
    /// The model asked to run one of the declared functions.
    ToolCall {
        /// Server-assigned call id, echoed back in the response.
        id: String,
        name: String,
        args: Value,
    },
    /// Raw unparsed server message (for debugging).
    Raw(Value),
    /// Fatal error from the transport or server.
    Error(String),
}

/// Setup handshake parameters for a [`LiveSession`].
#[derive(Debug, Clone)]
pub struct LiveConfig {
    pub model: String,
    pub system_instruction: Option<String>,
    pub voice: Option<String>,
    pub response_modalities: Vec<String>,
    /// Ask the server to transcribe the user's input audio and stream the
    /// transcript back (`LiveEvent::InputTranscription`).
    pub transcribe_input: bool,
    /// Function declarations the model may call (Gemini `functionDeclarations`
    /// shape). Empty means a voice session that can only talk.
    pub tools: Vec<Value>,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            model: "models/gemini-2.0-flash-live-001".to_string(),
            system_instruction: None,
            voice: None,
            response_modalities: vec!["AUDIO".to_string()],
            transcribe_input: true,
            tools: Vec::new(),
        }
    }
}

/// Handle to a running bidi session. Drop or call `close()` to end it.
pub struct LiveSession {
    tx: mpsc::Sender<ClientCommand>,
    pub events: mpsc::Receiver<LiveEvent>,
}

/// Internal command channel from `LiveSession`'s public methods to the
/// writer task below.
enum ClientCommand {
    Audio(Vec<u8>),
    AudioStreamEnd,
    Text(String),
    /// Result of a function the model asked us to run.
    ToolResponse { id: String, name: String, result: Value },
    Close,
}

impl LiveSession {
    /// Open a new bidi connection and complete the setup handshake.
    pub async fn connect(api_key: &str, config: LiveConfig) -> Result<Self> {
        let url = format!("{BIDI_URL_BASE}?key={api_key}");
        let (ws_stream, _resp) = connect_async(&url)
            .await
            .context("failed to open Gemini Live websocket")?;

        let (mut ws_tx, mut ws_rx) = ws_stream.split();

        let setup = build_setup(&config);
        ws_tx
            .send(Message::Text(
                serde_json::to_string(&setup).context("serialize setup")?,
            ))
            .await
            .context("send setup")?;

        let (evt_tx, evt_rx) = mpsc::channel::<LiveEvent>(64);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientCommand>(64);

        // Reader task: forwards every server frame (text or binary) to
        // `events` until the socket closes.
        let evt_tx_reader = evt_tx.clone();
        tokio::spawn(async move {
            while let Some(frame) = ws_rx.next().await {
                match frame {
                    Ok(Message::Text(t)) => match serde_json::from_str::<Value>(&t) {
                        Ok(v) => forward_server_message(&evt_tx_reader, v).await,
                        Err(e) => {
                            let _ = evt_tx_reader
                                .send(LiveEvent::Error(format!("invalid server json: {e}")))
                                .await;
                        }
                    },
                    Ok(Message::Binary(b)) => match serde_json::from_slice::<Value>(&b) {
                        Ok(v) => forward_server_message(&evt_tx_reader, v).await,
                        Err(_) => {
                            let _ = evt_tx_reader.send(LiveEvent::Audio(b)).await;
                        }
                    },
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(e) => {
                        let _ = evt_tx_reader
                            .send(LiveEvent::Error(format!("ws error: {e}")))
                            .await;
                        break;
                    }
                }
            }
        });

        // Writer task: drains queued client commands (audio/text/close) onto
        // the socket in order.
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                let msg = match cmd {
                    ClientCommand::Audio(bytes) => build_realtime_audio(&bytes),
                    ClientCommand::AudioStreamEnd => build_audio_stream_end(),
                    ClientCommand::Text(t) => build_client_text(&t),
                    ClientCommand::ToolResponse { id, name, result } => serde_json::json!({
                        "toolResponse": {
                            "functionResponses": [{
                                "id": id,
                                "name": name,
                                "response": result,
                            }]
                        }
                    }),
                    ClientCommand::Close => {
                        let _ = ws_tx
                            .send(Message::Close(Some(CloseFrame {
                                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Normal,
                                reason: "".into(),
                            })))
                            .await;
                        break;
                    }
                };
                let payload = match serde_json::to_string(&msg) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("serialize client msg failed: {e}");
                        continue;
                    }
                };
                if let Err(e) = ws_tx.send(Message::Text(payload)).await {
                    warn!("ws send failed: {e}");
                    break;
                }
            }
        });

        Ok(Self {
            tx: cmd_tx,
            events: evt_rx,
        })
    }

    /// Send a chunk of PCM16 LE mono audio at 16kHz.
    pub async fn send_audio(&self, pcm16: Vec<u8>) -> Result<()> {
        self.tx
            .send(ClientCommand::Audio(pcm16))
            .await
            .map_err(|_| anyhow!("send loop closed"))
    }

    /// Signal that the audio stream is paused so the server commits the
    /// buffered audio now instead of waiting for trailing silence (push-to-talk).
    pub async fn send_audio_stream_end(&self) -> Result<()> {
        self.tx
            .send(ClientCommand::AudioStreamEnd)
            .await
            .map_err(|_| anyhow!("send loop closed"))
    }

    /// Send a text prompt as a client turn.
    pub async fn send_text(&self, text: String) -> Result<()> {
        self.tx
            .send(ClientCommand::Text(text))
            .await
            .map_err(|_| anyhow!("send loop closed"))
    }

    /// Answer a function call the model made. The model speaks the result.
    pub async fn send_tool_response(&self, id: String, name: String, result: Value) -> Result<()> {
        self.tx
            .send(ClientCommand::ToolResponse { id, name, result })
            .await
            .map_err(|_| anyhow!("send loop closed"))
    }

    /// Close the session cleanly.
    pub async fn close(&self) -> Result<()> {
        self.tx
            .send(ClientCommand::Close)
            .await
            .map_err(|_| anyhow!("send loop closed"))
    }
}

/// Parses one server-side JSON message and re-emits it as a typed
/// [`LiveEvent`]; unrecognized shapes fall through to `LiveEvent::Raw`.
async fn forward_server_message(tx: &mpsc::Sender<LiveEvent>, v: Value) {
    if v.get("setupComplete").is_some() {
        let _ = tx.send(LiveEvent::SetupComplete).await;
        return;
    }
    if let Some(sc) = v.get("serverContent") {
        if let Some(text) = sc
            .pointer("/inputTranscription/text")
            .and_then(|t| t.as_str())
        {
            let _ = tx
                .send(LiveEvent::InputTranscription(text.to_string()))
                .await;
        }
        if sc
            .get("turnComplete")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            let _ = tx.send(LiveEvent::TurnComplete).await;
        }
        if sc
            .get("interrupted")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            let _ = tx.send(LiveEvent::Interrupted).await;
        }
        if let Some(parts) = sc.pointer("/modelTurn/parts").and_then(|p| p.as_array()) {
            for part in parts {
                if let Some(inline) = part.get("inlineData") {
                    let mime = inline
                        .get("mimeType")
                        .and_then(|m| m.as_str())
                        .unwrap_or("");
                    let data_b64 = inline.get("data").and_then(|d| d.as_str()).unwrap_or("");
                    if mime.starts_with("audio/") {
                        match B64.decode(data_b64) {
                            Ok(bytes) => {
                                let _ = tx.send(LiveEvent::Audio(bytes)).await;
                            }
                            Err(e) => {
                                let _ = tx
                                    .send(LiveEvent::Error(format!("audio b64 decode: {e}")))
                                    .await;
                            }
                        }
                    }
                }
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    let _ = tx.send(LiveEvent::Text(text.to_string())).await;
                }
            }
        }
        return;
    }
    if let Some(calls) = v.pointer("/toolCall/functionCalls").and_then(|c| c.as_array()) {
        for call in calls {
            let Some(name) = call.get("name").and_then(|n| n.as_str()) else {
                continue;
            };
            let _ = tx
                .send(LiveEvent::ToolCall {
                    id: call
                        .get("id")
                        .and_then(|i| i.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    name: name.to_string(),
                    args: call.get("args").cloned().unwrap_or_else(|| serde_json::json!({})),
                })
                .await;
        }
        return;
    }
    debug!("unhandled Live message: {v}");
    let _ = tx.send(LiveEvent::Raw(v)).await;
}

/// Builds the initial `setup` message that opens a Live session.
fn build_setup(cfg: &LiveConfig) -> Value {
    let mut generation_config = serde_json::json!({
        "responseModalities": cfg.response_modalities,
    });
    if let Some(voice) = &cfg.voice {
        generation_config["speechConfig"] = serde_json::json!({
            "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": voice } }
        });
    }

    let mut setup = serde_json::json!({
        "setup": {
            "model": cfg.model,
            "generationConfig": generation_config,
        }
    });

    if let Some(instr) = &cfg.system_instruction {
        setup["setup"]["systemInstruction"] = serde_json::json!({
            "parts": [{ "text": instr }]
        });
    }

    if cfg.transcribe_input {
        setup["setup"]["inputAudioTranscription"] = serde_json::json!({});
    }

    if !cfg.tools.is_empty() {
        setup["setup"]["tools"] = serde_json::json!([
            { "functionDeclarations": cfg.tools }
        ]);
    }

    setup
}

/// Builds the `realtimeInput.audioStreamEnd` message (push-to-talk commit).
fn build_audio_stream_end() -> Value {
    serde_json::json!({
        "realtimeInput": { "audioStreamEnd": true }
    })
}

/// Wraps a PCM16 chunk as a base64 `realtimeInput.mediaChunks` message.
fn build_realtime_audio(pcm16: &[u8]) -> Value {
    let data = B64.encode(pcm16);
    serde_json::json!({
        "realtimeInput": {
            "mediaChunks": [{
                "mimeType": format!("audio/pcm;rate={INPUT_SAMPLE_RATE}"),
                "data": data,
            }]
        }
    })
}

/// Builds a `clientContent` turn carrying a text prompt.
fn build_client_text(text: &str) -> Value {
    serde_json::json!({
        "clientContent": {
            "turns": [{
                "role": "user",
                "parts": [{ "text": text }]
            }],
            "turnComplete": true
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_contains_model_and_modalities() {
        let cfg = LiveConfig {
            model: "models/gemini-2.0-flash-live-001".to_string(),
            system_instruction: Some("be concise".to_string()),
            voice: None,
            response_modalities: vec!["AUDIO".to_string()],
            transcribe_input: false,
            tools: Vec::new(),
        };
        let v = build_setup(&cfg);
        assert_eq!(v["setup"]["model"], "models/gemini-2.0-flash-live-001");
        assert_eq!(
            v["setup"]["generationConfig"]["responseModalities"][0],
            "AUDIO"
        );
        assert_eq!(
            v["setup"]["systemInstruction"]["parts"][0]["text"],
            "be concise"
        );
    }

    #[test]
    fn realtime_audio_wraps_b64_with_correct_mime() {
        let bytes = vec![0u8, 1, 2, 3];
        let v = build_realtime_audio(&bytes);
        let chunk = &v["realtimeInput"]["mediaChunks"][0];
        assert_eq!(chunk["mimeType"], "audio/pcm;rate=16000");
        assert_eq!(chunk["data"], B64.encode(&bytes));
    }

    #[test]
    fn client_text_marks_turn_complete() {
        let v = build_client_text("hello");
        assert_eq!(v["clientContent"]["turnComplete"], true);
        assert_eq!(v["clientContent"]["turns"][0]["parts"][0]["text"], "hello");
    }

    #[test]
    fn audio_stream_end_shape() {
        let v = build_audio_stream_end();
        assert_eq!(v["realtimeInput"]["audioStreamEnd"], true);
    }

    #[test]
    fn setup_includes_input_transcription_when_enabled() {
        let cfg = LiveConfig {
            transcribe_input: true,
            tools: Vec::new(),
            ..LiveConfig::default()
        };
        let v = build_setup(&cfg);
        assert!(v["setup"]["inputAudioTranscription"].is_object());

        let off = LiveConfig {
            transcribe_input: false,
            tools: Vec::new(),
            ..LiveConfig::default()
        };
        let v = build_setup(&off);
        assert!(v["setup"].get("inputAudioTranscription").is_none());
    }

    #[tokio::test]
    async fn input_transcription_forwarded() {
        let (tx, mut rx) = mpsc::channel(4);
        let msg = serde_json::json!({
            "serverContent": { "inputTranscription": { "text": "hello there" } }
        });
        forward_server_message(&tx, msg).await;
        match rx.recv().await {
            Some(LiveEvent::InputTranscription(t)) => assert_eq!(t, "hello there"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
