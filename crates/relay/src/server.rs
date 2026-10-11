//! The relay's HTTP + WebSocket surface.
//!
//! Two routes matter:
//!
//! * `GET /link` — the gateway's outbound WebSocket. One per gateway id.
//! * `/g/{gateway_id}/*` — everything the phone calls, forwarded down that
//!   socket and streamed back.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc};

use crate::protocol::{
    Frame, RELAY_MARKER_HEADER, is_stripped_request_header, strip_client_relay_marker,
};

/// How long a forwarded request waits for its response head before giving up.
const REQUEST_TIMEOUT_SECS: u64 = 120;
/// Bound on a single request body the relay will buffer. Uploads beyond this
/// belong on the local network, not through a rendezvous hop.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// Shared secret a gateway must present to claim any id. A single-tenant
    /// relay (the common case: one operator, one machine) needs nothing more.
    pub registration_secret: String,
}

/// One connected gateway.
struct Connection {
    /// Frames to write down the socket.
    tx: mpsc::Sender<Frame>,
    /// Response channels awaiting a reply, keyed by request id.
    pending: Mutex<HashMap<u64, mpsc::Sender<Frame>>>,
}

impl Connection {
    /// Is the map's entry for this gateway id still *this* link?
    ///
    /// A gateway that reconnects registers a new connection under the same id
    /// while the old link's reader may still be unwinding. Without this check
    /// the dying link's cleanup deletes the live replacement, and the relay
    /// reports the gateway offline while the gateway believes it is connected
    /// — a silent stranding that only another reconnect can clear.
    fn is_same(self: &Arc<Self>, other: &Arc<Connection>) -> bool {
        Arc::ptr_eq(self, other)
    }
}

#[derive(Clone)]
pub struct RelayState {
    config: Arc<RelayConfig>,
    connections: Arc<Mutex<HashMap<String, Arc<Connection>>>>,
    next_request_id: Arc<AtomicU64>,
}

impl RelayState {
    pub fn new(config: RelayConfig) -> Self {
        Self {
            config: Arc::new(config),
            connections: Arc::new(Mutex::new(HashMap::new())),
            next_request_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Is this gateway currently connected? Drives both routing and the
    /// "your Mac is unreachable" answer.
    pub async fn is_online(&self, gateway_id: &str) -> bool {
        self.connections.lock().await.contains_key(gateway_id)
    }
}

pub fn router(state: RelayState) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/link", get(link_handler))
        .route("/g/{gateway_id}/{*rest}", any(proxy_handler))
        .with_state(state)
}

async fn link_handler(State(state): State<RelayState>, ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(move |socket| gateway_link(state, socket))
}

/// Serve one gateway's outbound connection for its lifetime.
async fn gateway_link(state: RelayState, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // First frame must be a valid registration; anything else is dropped
    // without explanation — an unauthenticated peer learns nothing.
    let gateway_id = match ws_rx.next().await {
        Some(Ok(Message::Text(raw))) => match Frame::decode(&raw) {
            Ok(Frame::Register(reg)) if reg.secret == state.config.registration_secret => {
                let ack = Frame::Registered {
                    gateway_id: reg.gateway_id.clone(),
                };
                if let Ok(text) = ack.encode()
                    && ws_tx.send(Message::Text(text.into())).await.is_err()
                {
                    return;
                }
                reg.gateway_id
            }
            _ => {
                tracing::warn!("relay: rejected a link that did not register correctly");
                return;
            }
        },
        _ => return,
    };

    let (tx, mut rx) = mpsc::channel::<Frame>(256);
    let connection = Arc::new(Connection {
        tx,
        pending: Mutex::new(HashMap::new()),
    });
    state
        .connections
        .lock()
        .await
        .insert(gateway_id.clone(), connection.clone());
    tracing::info!(gateway_id = %gateway_id, "relay: gateway online");

    // Writer: everything queued for this gateway.
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let Ok(text) = frame.encode() else { continue };
            if ws_tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Reader: route responses back to whoever is waiting.
    while let Some(Ok(msg)) = ws_rx.next().await {
        let Message::Text(raw) = msg else {
            continue;
        };
        let Ok(frame) = Frame::decode(&raw) else {
            continue;
        };
        match &frame {
            Frame::Ping => {
                let _ = connection.tx.send(Frame::Pong).await;
            }
            Frame::Response { id, .. } | Frame::Chunk { id, .. } | Frame::Failed { id, .. } => {
                let waiter = connection.pending.lock().await.get(id).cloned();
                if let Some(w) = waiter {
                    let _ = w.send(frame).await;
                }
            }
            Frame::End { id } => {
                let waiter = connection.pending.lock().await.remove(id);
                if let Some(w) = waiter {
                    let _ = w.send(frame).await;
                }
            }
            _ => {}
        }
    }

    {
        let mut connections = state.connections.lock().await;
        match connections.get(&gateway_id) {
            // Still ours: this gateway really did go away.
            Some(current) if connection.is_same(current) => {
                connections.remove(&gateway_id);
                tracing::info!(gateway_id = %gateway_id, "relay: gateway offline");
            }
            // A newer link already took over — leave it alone.
            _ => tracing::info!(
                gateway_id = %gateway_id,
                "relay: stale link closed; a newer link is serving this gateway"
            ),
        }
    }
    writer.abort();
}

/// Forward one phone request to its gateway and stream the answer back.
async fn proxy_handler(
    State(state): State<RelayState>,
    Path((gateway_id, rest)): Path<(String, String)>,
    request: Request,
) -> Response {
    let Some(connection) = state.connections.lock().await.get(&gateway_id).cloned() else {
        // The distinctive status the app turns into "your Mac is offline".
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "that gateway is not connected right now",
        )
            .into_response();
    };

    let method = request.method().to_string();
    let query = request
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let path = format!("/{rest}{query}");
    let mut headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .filter(|(name, _)| !is_stripped_request_header(name.as_str()))
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|v| (name.as_str().to_string(), v.to_string()))
        })
        .collect();
    // Tell the gateway, truthfully, that this arrived over the tunnel — it is
    // the only party that can tell a real session from a deploy token, so it
    // makes that call. Strip first: the marker must describe reality, not
    // whatever the caller claimed.
    strip_client_relay_marker(&mut headers);
    headers.push((RELAY_MARKER_HEADER.to_string(), "1".to_string()));

    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response(),
    };

    let id = state.next_request_id.fetch_add(1, Ordering::Relaxed);
    let (reply_tx, mut reply_rx) = mpsc::channel::<Frame>(64);
    connection.pending.lock().await.insert(id, reply_tx);

    let sent = connection
        .tx
        .send(Frame::Request {
            id,
            method,
            path,
            headers,
            body: B64.encode(&body),
        })
        .await;
    if sent.is_err() {
        connection.pending.lock().await.remove(&id);
        return (StatusCode::BAD_GATEWAY, "gateway link closed").into_response();
    }

    // Wait for the head, then stream the body as chunks arrive — this is what
    // keeps streamed chat working through the relay.
    let head = tokio::time::timeout(
        std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS),
        reply_rx.recv(),
    )
    .await;
    let (status, headers) = match head {
        Ok(Some(Frame::Response {
            status, headers, ..
        })) => (status, headers),
        Ok(Some(Frame::Failed { message, .. })) => {
            connection.pending.lock().await.remove(&id);
            return (StatusCode::BAD_GATEWAY, message).into_response();
        }
        _ => {
            connection.pending.lock().await.remove(&id);
            return (StatusCode::GATEWAY_TIMEOUT, "gateway did not answer").into_response();
        }
    };

    let stream = async_stream::stream! {
        while let Some(frame) = reply_rx.recv().await {
            match frame {
                Frame::Chunk { data, .. } => {
                    if let Ok(bytes) = B64.decode(data.as_bytes()) {
                        yield Ok::<_, std::io::Error>(bytes);
                    }
                }
                Frame::End { .. } | Frame::Failed { .. } => break,
                _ => {}
            }
        }
    };

    let mut response =
        Response::builder().status(StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY));
    for (name, value) in headers {
        if is_stripped_request_header(&name) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(&value),
        ) {
            response = response.header(n, v);
        }
    }
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> RelayState {
        RelayState::new(RelayConfig {
            registration_secret: "secret".into(),
        })
    }

    #[tokio::test]
    async fn an_unknown_gateway_is_reported_offline_not_found() {
        let s = state();
        assert!(!s.is_online("gw-1").await);
    }

    #[tokio::test]
    async fn a_dying_link_never_evicts_the_reconnected_one() {
        let s = state();
        let mk = || {
            Arc::new(Connection {
                tx: mpsc::channel(1).0,
                pending: Mutex::new(HashMap::new()),
            })
        };
        let old_link = mk();
        let new_link = mk();
        // The gateway reconnected: the map now holds the NEW link.
        s.connections
            .lock()
            .await
            .insert("gw".to_string(), new_link.clone());
        // The old link's cleanup runs late and must not touch it.
        assert!(!old_link.is_same(&new_link));
        {
            let mut c = s.connections.lock().await;
            if let Some(cur) = c.get("gw")
                && old_link.is_same(cur)
            {
                c.remove("gw");
            }
        }
        assert!(s.is_online("gw").await, "the live link must survive");
    }

    #[tokio::test]
    async fn request_ids_are_unique_per_relay() {
        let s = state();
        let a = s.next_request_id.fetch_add(1, Ordering::Relaxed);
        let b = s.next_request_id.fetch_add(1, Ordering::Relaxed);
        assert_ne!(a, b);
    }
}
