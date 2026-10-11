//! The gateway's half of the relay link, exercised for real.
//!
//! This exists because a unit test cannot catch the failure that actually
//! happened: the forwarding HTTP client was built with a zero timeout, which
//! reqwest reads as "expire immediately", so every forwarded request failed
//! the instant it was sent. Frames encoded fine, the link established fine,
//! and only a request that travelled the whole path revealed it.

use std::net::SocketAddr;
use std::time::Duration;

use consistent_world_gateway::relay_client::{RelayClientConfig, run_loop};
use consistent_world_relay::{RelayConfig, RelayState, router};

const SECRET: &str = "link-test-secret";

/// Stands in for the gateway's own HTTP server on loopback.
async fn spawn_loopback_server() -> SocketAddr {
    use axum::Router;
    use axum::routing::get;
    let app = Router::new()
        .route("/health", get(|| async { "{\"status\":\"ok\"}" }))
        .route(
            "/slow",
            get(|| async {
                // Long enough that a mistakenly-short client timeout fails here.
                tokio::time::sleep(Duration::from_secs(3)).await;
                "finished"
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

async fn link(relay: SocketAddr, local: SocketAddr, gateway_id: &str) {
    let config = RelayClientConfig {
        url: format!("ws://{relay}"),
        gateway_id: gateway_id.to_string(),
        secret: SECRET.to_string(),
        local_port: local.port(),
    };
    tokio::spawn(async move { run_loop(config).await });
    tokio::time::sleep(Duration::from_millis(400)).await;
}

#[tokio::test]
async fn a_request_through_the_relay_reaches_the_local_gateway() {
    let local = spawn_loopback_server().await;
    let relay = spawn_relay().await;
    link(relay, local, "gw-link-1").await;

    let resp = reqwest::Client::new()
        .get(format!("http://{relay}/g/gw-link-1/health"))
        .header("authorization", "Bearer 9635febc172e40a1b4dd5c6625a431fd915e9b4fb19b4721885d2bfc28902396")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("the relay answers");
    assert_eq!(resp.status(), 200);
    assert!(resp.text().await.unwrap().contains("ok"));
}

#[tokio::test]
async fn a_slow_response_is_not_cut_off_by_the_forwarding_client() {
    // The regression: a zero/short client timeout kills long turns. A streamed
    // chat turn or an agent run legitimately holds a response open far longer
    // than this.
    let local = spawn_loopback_server().await;
    let relay = spawn_relay().await;
    link(relay, local, "gw-link-2").await;

    let resp = reqwest::Client::new()
        .get(format!("http://{relay}/g/gw-link-2/slow"))
        .header("authorization", "Bearer 9635febc172e40a1b4dd5c6625a431fd915e9b4fb19b4721885d2bfc28902396")
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .expect("the relay answers");
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "finished");
}
