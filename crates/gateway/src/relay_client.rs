//! Dial out to a relay so the phone can reach this gateway from anywhere.
//!
//! The gateway keeps binding loopback. This module opens one outbound
//! WebSocket to the operator's chosen relay, receives forwarded requests, and
//! replays each one against `127.0.0.1:<port>` — the same address the desktop
//! app uses. Nothing new listens, no port is forwarded, and the gateway's
//! auth guards are untouched because every replayed request still carries the
//! caller's own bearer token.
//!
//! Reconnects with backoff forever: the whole point is a link that survives a
//! closed laptop lid, a moved phone, and a relay restart.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use consistent_world_relay::protocol::{Frame, Register};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Backoff bounds for reconnecting to the relay.
const RECONNECT_MIN_SECS: u64 = 2;
const RECONNECT_MAX_SECS: u64 = 60;
/// Keepalive cadence; also how quickly a dead link is noticed.
const PING_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct RelayClientConfig {
    /// e.g. `wss://relay.example.com` (the `/link` path is appended).
    pub url: String,
    pub gateway_id: String,
    pub secret: String,
    /// Loopback port the gateway itself serves on.
    pub local_port: u16,
}

impl RelayClientConfig {
    /// Read the relay settings from the environment. `None` means the operator
    /// has not opted in, which is the default.
    pub fn from_env(local_port: u16) -> Option<Self> {
        let url = non_empty("CONSISTENT_WORLD_RELAY_URL")?;
        let secret = non_empty("CONSISTENT_WORLD_RELAY_SECRET")?;
        let gateway_id = non_empty("CONSISTENT_WORLD_RELAY_GATEWAY_ID")?;
        Some(Self {
            url,
            gateway_id,
            secret,
            local_port,
        })
    }

    /// The WebSocket endpoint, tolerating a URL given with or without `/link`
    /// and with an http(s) scheme.
    pub fn link_url(&self) -> String {
        let base = self.url.trim().trim_end_matches('/');
        let base = base
            .strip_prefix("https://")
            .map(|rest| format!("wss://{rest}"))
            .or_else(|| {
                base.strip_prefix("http://")
                    .map(|rest| format!("ws://{rest}"))
            })
            .unwrap_or_else(|| base.to_string());
        if base.ends_with("/link") {
            base
        } else {
            format!("{base}/link")
        }
    }
}

fn non_empty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Replay one forwarded request against the local gateway and stream the
/// response back as frames.
/// One request the relay forwarded, as it arrives off the wire.
struct ForwardedRequest {
    id: u64,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body_b64: String,
}

async fn replay_locally(
    client: &reqwest::Client,
    local_port: u16,
    req: ForwardedRequest,
    out: mpsc::Sender<Frame>,
) {
    let ForwardedRequest {
        id,
        method,
        path,
        headers,
        body_b64,
    } = req;
    let url = format!("http://127.0.0.1:{local_port}{path}");
    let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else {
        let _ = out
            .send(Frame::Failed {
                id,
                message: "bad method".into(),
            })
            .await;
        return;
    };
    let mut request = client.request(method, &url);
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if !body_b64.is_empty()
        && let Ok(bytes) = B64.decode(body_b64.as_bytes())
    {
        request = request.body(bytes);
    }

    let response = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            let _ = out
                .send(Frame::Failed {
                    id,
                    message: e.to_string(),
                })
                .await;
            return;
        }
    };

    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(n, v)| {
            v.to_str()
                .ok()
                .map(|v| (n.as_str().to_string(), v.to_string()))
        })
        .collect();
    if out
        .send(Frame::Response {
            id,
            status,
            headers,
        })
        .await
        .is_err()
    {
        return;
    }

    // Stream rather than buffer: a streamed chat turn must reach the phone
    // token by token, not in one lump when the turn ends.
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => {
                if out
                    .send(Frame::Chunk {
                        id,
                        data: B64.encode(&bytes),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(e) => {
                let _ = out
                    .send(Frame::Failed {
                        id,
                        message: e.to_string(),
                    })
                    .await;
                return;
            }
        }
    }
    let _ = out.send(Frame::End { id }).await;
}

/// One connection attempt. Returns when the link drops.
async fn run_once(config: &RelayClientConfig, client: &reqwest::Client) -> anyhow::Result<()> {
    let (socket, _) = tokio_tungstenite::connect_async(config.link_url()).await?;
    let (mut ws_tx, mut ws_rx) = socket.split();

    let register = Frame::Register(Register {
        gateway_id: config.gateway_id.clone(),
        secret: config.secret.clone(),
    });
    ws_tx.send(Message::Text(register.encode()?)).await?;

    // One writer owns the socket; request handlers queue frames.
    let (out_tx, mut out_rx) = mpsc::channel::<Frame>(256);
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            let Ok(text) = frame.encode() else { continue };
            if ws_tx.send(Message::Text(text)).await.is_err() {
                break;
            }
        }
    });

    let ping_tx = out_tx.clone();
    let pinger = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(PING_SECS));
        loop {
            interval.tick().await;
            if ping_tx.send(Frame::Ping).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg) = ws_rx.next().await {
        let Ok(Message::Text(raw)) = msg else {
            continue;
        };
        let Ok(frame) = Frame::decode(&raw) else {
            continue;
        };
        match frame {
            Frame::Registered { gateway_id } => {
                tracing::info!(gateway_id = %gateway_id, "relay link established");
            }
            Frame::Request {
                id,
                method,
                path,
                headers,
                body,
            } => {
                // Each request runs concurrently: a long streamed turn must
                // not block every other call from the phone.
                let client = client.clone();
                let out = out_tx.clone();
                let port = config.local_port;
                tokio::spawn(async move {
                    replay_locally(
                        &client,
                        port,
                        ForwardedRequest {
                            id,
                            method,
                            path,
                            headers,
                            body_b64: body,
                        },
                        out,
                    )
                    .await;
                });
            }
            Frame::Ping => {
                let _ = out_tx.send(Frame::Pong).await;
            }
            _ => {}
        }
    }

    writer.abort();
    pinger.abort();
    Ok(())
}

/// Keep a relay link up for the life of the process.
pub async fn run_loop(config: RelayClientConfig) {
    // No overall timeout: a streamed chat turn or a long agent run legitimately
    // holds a response open for minutes. Note this must be *omitted*, not set
    // to zero — reqwest reads a zero timeout as "expire immediately", which
    // fails every forwarded request the instant it is sent.
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut backoff = RECONNECT_MIN_SECS;
    loop {
        match run_once(&config, &client).await {
            Ok(()) => tracing::warn!("relay link closed; reconnecting"),
            Err(e) => tracing::warn!(error = %e, "relay link failed; reconnecting"),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(RECONNECT_MAX_SECS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> RelayClientConfig {
        RelayClientConfig {
            url: url.to_string(),
            gateway_id: "gw".into(),
            secret: "s".into(),
            local_port: 18800,
        }
    }

    #[test]
    fn link_url_upgrades_the_scheme_and_appends_the_path() {
        assert_eq!(
            cfg("https://relay.example.com").link_url(),
            "wss://relay.example.com/link"
        );
        assert_eq!(
            cfg("http://localhost:8787").link_url(),
            "ws://localhost:8787/link"
        );
        // Trailing slash, and an explicit /link, are both tolerated.
        assert_eq!(
            cfg("https://relay.example.com/").link_url(),
            "wss://relay.example.com/link"
        );
        assert_eq!(
            cfg("wss://relay.example.com/link").link_url(),
            "wss://relay.example.com/link"
        );
    }

    #[test]
    fn relay_is_opt_in() {
        // With nothing configured there is no relay: the gateway stays purely
        // local until the operator says otherwise.
        unsafe {
            std::env::remove_var("CONSISTENT_WORLD_RELAY_URL");
        }
        assert!(RelayClientConfig::from_env(18800).is_none());
    }
}
