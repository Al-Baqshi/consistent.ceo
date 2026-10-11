//! WS7 — the connectivity surface, end to end.
//!
//! Inbound: an API key minted through `/api/auth/api-keys` must authenticate
//! ordinary `/api/*` calls exactly like a session token, because it *is* a
//! session row. Outbound: webhook subscriptions are workspace-scoped CRUD.
//!
//! These tests are the source of truth for `docs/integrations.md` — the request
//! and response shapes documented there are the ones asserted here.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::util::ServiceExt;

use consistent_world_gateway::{
    auth_store::AuthStore,
    server,
    state::{ControlState, Entity, ServerState},
    webhooks::SurrealWebhookStore,
};
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};

/// The assembled router plus its `ControlState`, for asserting on state after HTTP calls.
struct Harness {
    router: axum::Router,
    control: Arc<ControlState>,
}

/// Builds a `Harness`: router wired with an auth store, webhook store, and control state.
async fn harness() -> Harness {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let db = surrealdb::engine::any::connect("memory").await.unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    let db = Arc::new(db);

    let auth = AuthStore::new(db.clone());
    auth.init_schema().await.unwrap();
    let webhooks = SurrealWebhookStore::new(db.clone());
    webhooks.init_schema().await.unwrap();

    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    // A static token must be configured, or the bearer layer falls through to
    // the open `anonymous/default` context and nothing is actually gated.
    let state = ServerState::new("test".into(), Some("static-secret".into()), memory)
        .with_control_state(control.clone())
        .with_auth_store(auth);
    Harness {
        router: server::build_router_with_webhooks(state, Some(webhooks)),
        control,
    }
}

/// Sends an authenticated (if `token` is given) JSON request and returns `(status, parsed body)`.
async fn json_request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&b).unwrap()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let res = router.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

/// Register a user and return `(session_token, workspace_id)`.
async fn register(router: &axum::Router, username: &str) -> (String, String) {
    let (status, body) = json_request(
        router,
        "POST",
        "/api/auth/register",
        None,
        Some(serde_json::json!({
            "username": username,
            "password": "correct horse battery",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register: {body}");
    (
        body["token"].as_str().unwrap().to_string(),
        body["workspaceId"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn api_key_authenticates_the_inbound_surface() {
    let h = harness().await;
    let (session, workspace_id) = register(&h.router, "integrator").await;

    // Mint a key with the session token.
    let (status, key) = json_request(
        &h.router,
        "POST",
        "/api/auth/api-keys",
        Some(&session),
        Some(serde_json::json!({ "name": "Zapier" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create key: {key}");
    let token = key["token"].as_str().unwrap().to_string();
    let key_id = key["id"].as_str().unwrap().to_string();
    assert_eq!(key["name"], "Zapier");

    // Listing shows the key but never the token.
    let (status, list) =
        json_request(&h.router, "GET", "/api/auth/api-keys", Some(&session), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert!(!list.to_string().contains(&token));

    // The documented firehose: POST /api/captures with the key.
    let (status, capture) = json_request(
        &h.router,
        "POST",
        "/api/captures",
        Some(&token),
        Some(serde_json::json!({
            "raw_text": "Investor asked for the Q3 deck by Friday",
            "source": "zapier",
            "source_ref": "msg_123",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "capture: {capture}");
    assert_eq!(capture["source"], "zapier");
    assert_eq!(capture["workspace_id"], workspace_id);
    assert_eq!(capture["status"], "raw");

    // And the documented goal surface: POST /api/missions.
    h.control.entities.write().await.push(Entity {
        id: "e_ops".into(),
        name: "Operations".into(),
        owner: Some(workspace_id.clone()),
        status: "Active".into(),
        dept: "General".into(),
        visibility: consistent_world_gateway::state::EntityVisibility::Workspace,
        ..Default::default()
    });
    let (status, mission) = json_request(
        &h.router,
        "POST",
        "/api/missions",
        Some(&token),
        Some(serde_json::json!({
            "entity_id": "e_ops",
            "title": "Ship the Q3 deck",
            "goal": "Investor-ready deck reviewed and sent by Friday",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mission: {mission}");
    assert_eq!(mission["title"], "Ship the Q3 deck");

    // Revoked keys stop working immediately.
    let (status, _) = json_request(
        &h.router,
        "DELETE",
        &format!("/api/auth/api-keys/{key_id}"),
        Some(&session),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = json_request(&h.router, "GET", "/api/captures", Some(&token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_crud_is_workspace_scoped() {
    let h = harness().await;
    let (alice, _) = register(&h.router, "wh_alice").await;
    let (bob, _) = register(&h.router, "wh_bob").await;

    let (status, sub) = json_request(
        &h.router,
        "POST",
        "/api/webhooks",
        Some(&alice),
        Some(serde_json::json!({
            "url": "https://example.com/hook",
            "kinds": ["task_overdue", "approval_request_created"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create webhook: {sub}");
    let id = sub["id"].as_str().unwrap().to_string();
    assert_eq!(sub["url"], "https://example.com/hook");

    // A relative URL is a typo, not a subscription.
    let (status, _) = json_request(
        &h.router,
        "POST",
        "/api/webhooks",
        Some(&alice),
        Some(serde_json::json!({ "url": "example.com/hook" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, list) = json_request(&h.router, "GET", "/api/webhooks", Some(&alice), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);

    // Bob sees none of Alice's, and can't delete one by id.
    let (status, list) = json_request(&h.router, "GET", "/api/webhooks", Some(&bob), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(list.as_array().unwrap().is_empty());
    let (status, _) = json_request(
        &h.router,
        "DELETE",
        &format!("/api/webhooks/{id}"),
        Some(&bob),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = json_request(
        &h.router,
        "DELETE",
        &format!("/api/webhooks/{id}"),
        Some(&alice),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, list) = json_request(&h.router, "GET", "/api/webhooks", Some(&alice), None).await;
    assert!(list.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn webhook_routes_require_auth() {
    let h = harness().await;
    let res = h
        .router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/webhooks")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
