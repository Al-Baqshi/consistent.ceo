//! `consistent-relay` — run a rendezvous point for your own gateways.
//!
//! ```text
//! CONSISTENT_RELAY_SECRET=<shared secret> consistent-relay
//! ```
//!
//! Put it behind TLS (a reverse proxy, or a platform that terminates for you).
//! It stores nothing, so a restart costs only the reconnect.

use std::net::SocketAddr;

use consistent_world_relay::{RelayConfig, RelayState, router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // No default secret, ever: an open relay would let anyone claim any
    // gateway id and receive traffic meant for someone's machine.
    let registration_secret = std::env::var("CONSISTENT_RELAY_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "CONSISTENT_RELAY_SECRET is required — generate one with `openssl rand -hex 32`"
            )
        })?;

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8787);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let app = router(RelayState::new(RelayConfig {
        registration_secret,
    }));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "consistent-relay listening");
    axum::serve(listener, app).await?;
    Ok(())
}
