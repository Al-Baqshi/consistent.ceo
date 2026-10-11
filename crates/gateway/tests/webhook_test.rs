//! Smoke tests for gateway route auth gating and response shapes.
//!
//! Exercises `/api/sessions` specifically: unauthenticated calls are
//! rejected, and authenticated calls return the paginated response shape.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::util::ServiceExt;

use std::sync::Arc;

use consistent_world_gateway::state::ServerState;
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};

#[tokio::test]
async fn test_webhook_requires_auth() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());

    let app = consistent_world_gateway::server::build_router(ServerState::new(
        "test".to_string(),
        Some("secret".to_string()),
        memory,
    ));

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_webhook_returns_reply() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());

    let app = consistent_world_gateway::server::build_router(ServerState::new(
        "test".to_string(),
        Some("secret".to_string()),
        memory,
    ));

    let res = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/sessions")
                .header("authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    // `/api/sessions` returns the paginated `PaginatedSessionResponse` object
    // (`items` array + paging metadata), not a bare array. Test was lagging
    // the server-side change to pagination.
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["items"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(false),
        "expected empty items array, got: {json}"
    );
}
