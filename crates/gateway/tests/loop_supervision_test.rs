//! Integration tests for background-loop supervision.
//!
//! The gateway's nine background loops used to be started with a bare
//! `tokio::spawn` whose `JoinHandle` was dropped. Tokio ends a panicked task
//! quietly, so the first panic in any of them stopped that subsystem for the
//! life of the process — no loop-level log, no operator-visible signal, and
//! `/health` still returning 200. A dead orchestrator looked exactly like an
//! empty task queue.
//!
//! These tests drive the whole chain the fix introduced: `spawn_supervised`
//! restarts the loop, the registry records it, and `/health` reports it — the
//! last step through the real router, because a registry nothing reads would be
//! no better than the silence it replaced.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use consistent_world_gateway::{
    server,
    state::ServerState,
    supervisor::{self, SubsystemRegistry},
};
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};
use http_body_util::BodyExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tower::util::ServiceExt;

/// `/health` predates the camelCase convention most gateway responses follow and
/// already ships `memory_backend` on the wire, so `loop_restarts` matches its
/// siblings rather than the house style. Asserted here so a future rename has to
/// break a test rather than a consumer.
async fn health_json(state: ServerState) -> serde_json::Value {
    let app = server::build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

async fn server_state_with(registry: Arc<SubsystemRegistry>) -> ServerState {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    ServerState::new("test-version".into(), Some("secret".into()), memory).with_subsystems(registry)
}

#[tokio::test]
async fn a_panicking_loop_restarts_and_surfaces_on_health() {
    let registry = SubsystemRegistry::new();
    let ticks = Arc::new(AtomicU32::new(0));

    // A loop that panics on its first two ticks, then runs forever.
    let handle = {
        let ticks = ticks.clone();
        supervisor::spawn_supervised("orchestrator", registry.clone(), None, move || {
            let ticks = ticks.clone();
            async move {
                let n = ticks.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    panic!("tick {n} hit a stale index");
                }
                loop {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        })
    };

    // Wait on the third run actually starting, not on the restart being recorded:
    // the counter increments before the backoff sleep, so `restarts == 2` is true
    // a full backoff before the loop is running again.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while ticks.load(Ordering::SeqCst) < 3 {
        assert!(
            std::time::Instant::now() < deadline,
            "the supervisor should have restarted the loop twice and be running by now \
             (restarts so far: {})",
            registry.total_restarts().await
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        registry.total_restarts().await,
        2,
        "exactly two panics, so exactly two restarts"
    );

    let health = health_json(server_state_with(registry.clone()).await).await;
    assert_eq!(
        health["status"], "degraded",
        "a crashed-and-restarted loop must not read as plain ok: {health}"
    );
    assert_eq!(
        health["loop_restarts"]["orchestrator"], 2,
        "the restart count reaches the health endpoint: {health}"
    );

    // The third run is alive and stays alive: no further restarts accrue.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        registry.total_restarts().await,
        2,
        "the healthy third run must not be restarted"
    );
    assert!(registry.snapshot().await["orchestrator"].running);

    // Panic payloads stay off the public endpoint.
    let body = health.to_string();
    assert!(
        !body.contains("stale index"),
        "/health is unauthenticated; panic detail must not ride on it: {body}"
    );
    // ...but an authenticated caller can still get it from the registry.
    let detail = registry.snapshot().await;
    assert!(
        detail["orchestrator"]
            .last_panic
            .as_deref()
            .is_some_and(|d| d.contains("stale index")),
        "the registry keeps the payload for a privileged reader"
    );

    handle.abort();
}

#[tokio::test]
async fn health_is_ok_while_every_loop_is_healthy() {
    let registry = SubsystemRegistry::new();
    let handle = supervisor::spawn_supervised("scheduler", registry.clone(), None, || async {
        loop {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });

    tokio::time::sleep(Duration::from_millis(60)).await;

    let health = health_json(server_state_with(registry.clone()).await).await;
    assert_eq!(health["status"], "ok");
    assert_eq!(health["loop_restarts"]["scheduler"], 0);

    handle.abort();
}

#[tokio::test]
async fn health_omits_loop_restarts_when_no_registry_is_attached() {
    // A bare ServerState — the shape every other integration test builds — must
    // keep working, and must not grow a misleading empty map.
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let state = ServerState::new("test-version".into(), Some("secret".into()), memory);

    let health = health_json(state).await;
    assert_eq!(health["status"], "ok");
    assert!(
        health.get("loop_restarts").is_none(),
        "absent, not empty: {health}"
    );
}
