//! The relay's one claim: a request made to it reaches a gateway that is only
//! listening on loopback, and the answer streams back.
//!
//! This test stands up all three participants for real — a loopback "gateway"
//! HTTP server, the relay, and a gateway-side link that replays requests
//! against that server — because the failure this guards against (frames that
//! encode fine but never correlate) is invisible to unit tests.

use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use consistent_world_relay::protocol::{Frame, Register};
use consistent_world_relay::{RelayConfig, RelayState, router};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

const SECRET: &str = "test-secret";
/// Shaped exactly like `AuthStore::gen_token()` — two uuid simple-forms
/// concatenated. Earlier versions of these tests used an invented
/// `cws_workspace.principal.random` shape that nothing in the product mints,
/// which is how a relay guard that rejected 100% of real sessions passed its
/// own end-to-end tests.
const REAL_TOKEN: &str = "9635febc172e40a1b4dd5c6625a431fd915e9b4fb19b4721885d2bfc28902396";

/// A stand-in for the real gateway: answers on loopback only.
async fn spawn_local_gateway() -> SocketAddr {
    use axum::Router;
    use axum::routing::get;
    let app = Router::new()
        .route("/api/hello", get(|| async { "hello from the mac" }))
        .route(
            "/api/stream",
            get(|| async {
                // Two chunks, so a buffered implementation would still pass
                // while a broken streaming one would not.
                let s = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>("first\n".as_bytes().to_vec()),
                    Ok::<_, std::io::Error>("second\n".as_bytes().to_vec()),
                ]);
                axum::body::Body::from_stream(s)
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// Echoes back whatever relay marker the gateway side received.
async fn spawn_marker_echo_server() -> SocketAddr {
    use axum::Router;
    use axum::routing::get;
    let app = Router::new().route(
        "/echo",
        get(|headers: axum::http::HeaderMap| async move {
            headers
                .get("x-consistent-relay")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("absent")
                .to_string()
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

async fn spawn_relay() -> SocketAddr {
    let app = router(RelayState::new(RelayConfig {
        registration_secret: SECRET.to_string(),
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// The gateway side of the link, condensed: register, then replay each
/// forwarded request against the local server and stream the answer back.
async fn spawn_gateway_link(relay: SocketAddr, local: SocketAddr, gateway_id: &str) {
    let url = format!("ws://{relay}/link");
    let gateway_id = gateway_id.to_string();
    tokio::spawn(async move {
        let (socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut tx, mut rx) = socket.split();
        let register = Frame::Register(Register {
            gateway_id,
            secret: SECRET.to_string(),
        })
        .encode()
        .unwrap();
        tx.send(Message::Text(register)).await.unwrap();

        let client = reqwest::Client::new();
        while let Some(Ok(Message::Text(raw))) = rx.next().await {
            let Ok(frame) = Frame::decode(&raw) else {
                continue;
            };
            if let Frame::Request {
                id,
                method,
                path,
                headers,
                ..
            } = frame
            {
                let url = format!("http://{local}{path}");
                let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
                // Forward the headers the relay sent, exactly as the real
                // gateway-side client does — otherwise this stub silently
                // drops the very thing some tests assert on.
                let mut req = client.request(method, &url);
                for (name, value) in headers {
                    req = req.header(name, value);
                }
                let resp = req.send().await.unwrap();
                let status = resp.status().as_u16();
                let head = Frame::Response {
                    id,
                    status,
                    headers: vec![],
                }
                .encode()
                .unwrap();
                tx.send(Message::Text(head)).await.unwrap();
                let mut stream = resp.bytes_stream();
                while let Some(Ok(bytes)) = stream.next().await {
                    let chunk = Frame::Chunk {
                        id,
                        data: B64.encode(&bytes),
                    }
                    .encode()
                    .unwrap();
                    tx.send(Message::Text(chunk)).await.unwrap();
                }
                let end = Frame::End { id }.encode().unwrap();
                tx.send(Message::Text(end)).await.unwrap();
            }
        }
    });
    // Let the registration land before the first request.
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn a_request_reaches_a_loopback_only_gateway_through_the_relay() {
    let local = spawn_local_gateway().await;
    let relay = spawn_relay().await;
    spawn_gateway_link(relay, local, "gw-1").await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{relay}/g/gw-1/api/hello"))
        // A session-shaped bearer: the relay refuses static tokens.
        .header(
            "authorization",
            "Bearer 9635febc172e40a1b4dd5c6625a431fd915e9b4fb19b4721885d2bfc28902396",
        )
        .send()
        .await
        .expect("request reaches the relay");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "hello from the mac");
}

#[tokio::test]
async fn a_streamed_response_arrives_in_pieces_not_one_lump() {
    let local = spawn_local_gateway().await;
    let relay = spawn_relay().await;
    spawn_gateway_link(relay, local, "gw-2").await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{relay}/g/gw-2/api/stream"))
        .header(
            "authorization",
            "Bearer 9635febc172e40a1b4dd5c6625a431fd915e9b4fb19b4721885d2bfc28902396",
        )
        .send()
        .await
        .expect("request reaches the relay");
    assert_eq!(resp.status(), 200);

    let mut chunks = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(Ok(bytes)) = stream.next().await {
        chunks.push(String::from_utf8_lossy(&bytes).to_string());
    }
    let joined = chunks.concat();
    assert!(joined.contains("first"), "got: {joined}");
    assert!(joined.contains("second"), "got: {joined}");
    assert!(
        chunks.len() >= 2,
        "a streamed body must arrive in pieces, got {chunks:?}"
    );
}

#[tokio::test]
async fn an_offline_gateway_answers_service_unavailable() {
    let relay = spawn_relay().await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{relay}/g/nobody/api/hello"))
        .header(
            "authorization",
            "Bearer 9635febc172e40a1b4dd5c6625a431fd915e9b4fb19b4721885d2bfc28902396",
        )
        .send()
        .await
        .unwrap();
    // The distinctive status the app turns into "your Mac is offline".
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn every_forwarded_request_carries_a_truthful_relay_marker() {
    // The relay cannot tell a session token from a deploy token — both are 64
    // hex characters from the same generator. So it does not guess: it marks
    // the request as tunnel-origin and lets the gateway decide. The marker must
    // be present, and must survive no matter what the caller sent.
    let local = spawn_marker_echo_server().await;
    let relay = spawn_relay().await;
    spawn_gateway_link(relay, local, "gw-marker").await;

    let body = reqwest::Client::new()
        .get(format!("http://{relay}/g/gw-marker/echo"))
        .header("authorization", format!("Bearer {REAL_TOKEN}"))
        // A caller trying to look like it did NOT come from the relay.
        .header("x-consistent-relay", "0")
        .send()
        .await
        .expect("the relay answers")
        .text()
        .await
        .unwrap();
    assert_eq!(
        body, "1",
        "the relay's own marker must win over the caller's"
    );
}

#[tokio::test]
async fn a_link_with_the_wrong_secret_never_becomes_reachable() {
    let relay = spawn_relay().await;
    let url = format!("ws://{relay}/link");
    tokio::spawn(async move {
        let (socket, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (mut tx, _rx) = socket.split();
        let register = Frame::Register(Register {
            gateway_id: "impostor".into(),
            secret: "wrong".into(),
        })
        .encode()
        .unwrap();
        let _ = tx.send(Message::Text(register)).await;
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = reqwest::Client::new();
    let resp = client
        .get(format!("http://{relay}/g/impostor/api/hello"))
        .header(
            "authorization",
            "Bearer 9635febc172e40a1b4dd5c6625a431fd915e9b4fb19b4721885d2bfc28902396",
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503, "a rejected link must not be routable");
}
