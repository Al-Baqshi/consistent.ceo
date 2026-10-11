//! Tonic-based streaming client for the Audio2Face gRPC service.

#[allow(unused_imports)]
use anyhow::Context;
use anyhow::Result;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;
use tonic::transport::Channel;
use tracing::{debug, warn};

use crate::proto::audio_to_face_client::AudioToFaceClient;
use crate::proto::{
    AudioChunk, ClientMessage, SessionEnd, SessionStart, client_message::Payload as CPayload,
    server_message::Payload as SPayload,
};

/// Parameters for one Audio2Face session, sent as the stream's
/// `SessionStart` message.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub session_id: String,
    pub sample_rate: u32,
    pub voice_actor: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            session_id: uuid_like_id(),
            sample_rate: 16_000,
            voice_actor: "mark".to_string(),
        }
    }
}

/// Cheap unique-enough session id — a nanosecond timestamp prefixed for
/// readability, not a real UUID.
fn uuid_like_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("a2f-{ts}")
}

/// One blendshape frame exposed to the caller.
#[derive(Debug, Clone)]
pub struct BlendshapeFrame {
    pub pts_ms: u64,
    pub weights: Vec<f32>,
    pub names: Vec<String>,
}

/// Errors surfaced from connecting to or streaming with the Audio2Face
/// service.
#[derive(Debug, thiserror::Error)]
pub enum A2fStreamError {
    #[error("transport: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("rpc: {0}")]
    Rpc(#[from] Status),
    #[error("other: {0}")]
    Other(String),
}

/// Thin wrapper around the tonic-generated client.
pub struct A2fClient {
    inner: AudioToFaceClient<Channel>,
}

impl A2fClient {
    /// Connects to the Audio2Face gRPC endpoint.
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, A2fStreamError> {
        let endpoint = endpoint.into();
        let channel = Channel::from_shared(endpoint)
            .map_err(|e| A2fStreamError::Other(format!("invalid endpoint: {e}")))?
            .connect()
            .await?;
        Ok(Self {
            inner: AudioToFaceClient::new(channel),
        })
    }

    /// Open a bidi stream. Returns a sender for audio chunks and a receiver
    /// for blendshape frames. Drop the sender to end the session cleanly.
    pub async fn open_stream(
        &mut self,
        cfg: SessionConfig,
    ) -> Result<(mpsc::Sender<Vec<u8>>, mpsc::Receiver<BlendshapeFrame>), A2fStreamError> {
        let (audio_tx, audio_rx) = mpsc::channel::<Vec<u8>>(64);
        let (frame_tx, frame_rx) = mpsc::channel::<BlendshapeFrame>(64);

        let (client_tx, client_rx) = mpsc::channel::<ClientMessage>(64);
        client_tx
            .send(ClientMessage {
                payload: Some(CPayload::SessionStart(SessionStart {
                    session_id: cfg.session_id.clone(),
                    sample_rate: cfg.sample_rate,
                    voice_actor: cfg.voice_actor.clone(),
                    emotion_neutral: 1.0,
                    ..Default::default()
                })),
            })
            .await
            .map_err(|e| A2fStreamError::Other(format!("send session_start: {e}")))?;

        let outbound = ReceiverStream::new(client_rx);
        let resp = self.inner.stream(outbound).await?;
        let mut inbound = resp.into_inner();

        // Forward outbound audio chunks to the server, tracking a running
        // presentation timestamp (PCM16 mono: 2 bytes/sample) so the service
        // knows each chunk's position in the stream.
        let client_tx_audio = client_tx.clone();
        tokio::spawn(async move {
            let mut audio_rx = audio_rx;
            let mut pts_ms: u64 = 0;
            while let Some(chunk) = audio_rx.recv().await {
                let chunk_ms = (chunk.len() as u64 / 2) * 1000 / cfg.sample_rate as u64;
                let msg = ClientMessage {
                    payload: Some(CPayload::AudioChunk(AudioChunk {
                        pcm16_le: chunk,
                        pts_ms,
                    })),
                };
                if client_tx_audio.send(msg).await.is_err() {
                    break;
                }
                pts_ms = pts_ms.saturating_add(chunk_ms.max(1));
            }
            // Audio channel closed — tell the server the session is over.
            let _ = client_tx_audio
                .send(ClientMessage {
                    payload: Some(CPayload::SessionEnd(SessionEnd {
                        reason: "client closed".to_string(),
                    })),
                })
                .await;
        });

        // Forward inbound blendshape frames to the caller; log session/status
        // messages instead of surfacing them as frames.
        tokio::spawn(async move {
            while let Some(msg) = inbound.next().await {
                match msg {
                    Ok(server_msg) => match server_msg.payload {
                        Some(SPayload::BlendshapeFrame(frame)) => {
                            let out = BlendshapeFrame {
                                pts_ms: frame.pts_ms,
                                weights: frame.weights,
                                names: frame.names,
                            };
                            if frame_tx.send(out).await.is_err() {
                                break;
                            }
                        }
                        Some(SPayload::SessionReady(r)) => {
                            debug!(
                                "a2f session ready: id={} fps={}",
                                r.session_id, r.frame_rate_hz
                            );
                        }
                        Some(SPayload::Status(s)) => {
                            debug!("a2f status: {} {}", s.code, s.message);
                        }
                        None => {}
                    },
                    Err(e) => {
                        warn!("a2f rpc error: {e}");
                        break;
                    }
                }
            }
        });

        Ok((audio_tx, frame_rx))
    }
}
