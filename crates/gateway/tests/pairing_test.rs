//! End-to-end HTTP tests for pairing a phone by presence (`pairing.rs`).
//!
//! The unit tests prove the state machine. These prove the contract through
//! the real router: a phone that scanned the computer's code and was allowed
//! on the computer gets a session, signs back in later with only its key, and
//! is locked out the moment the computer removes it.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use http_body_util::BodyExt;
use ring::signature::{Ed25519KeyPair, KeyPair};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::util::ServiceExt;

use consistent_world_gateway::auth_store::AuthStore;
use consistent_world_gateway::pairing::{
    collect_message, confirmation_code, enroll_message, login_message,
};
use consistent_world_gateway::server;
use consistent_world_gateway::state::ServerState;
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};

const STATIC_TOKEN: &str = "static-token";

async fn build_app(bind: &str) -> axum::Router {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let db = surrealdb::engine::any::connect("memory").await.unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    let auth = AuthStore::new(Arc::new(db));
    auth.init_schema().await.unwrap();
    let config = Arc::new(consistent_world_gateway::config::AppConfig {
        gateway_bind: bind.to_string(),
        ..Default::default()
    });
    server::build_router(
        ServerState::new("test".into(), Some(STATIC_TOKEN.into()), memory)
            .with_auth_store(auth)
            .with_app_config(config),
    )
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = token {
        request = request.header("authorization", format!("Bearer {t}"));
    }
    let request = match body {
        Some(b) => request.body(Body::from(b.to_string())).unwrap(),
        None => request.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

struct Phone(Ed25519KeyPair);

impl Phone {
    fn new() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Phone(Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap())
    }
    fn public_key(&self) -> String {
        B64.encode(self.0.public_key().as_ref())
    }
    fn sign(&self, msg: &str) -> String {
        B64.encode(self.0.sign(msg.as_bytes()).as_ref())
    }
}

/// Scan, enroll, allow on the computer, collect. Returns (session, device id).
async fn pair(app: &axum::Router, computer: &str, phone: &Phone) -> (String, String) {
    let (status, offer) = call(app, "POST", "/api/pair/offer", Some(computer), None).await;
    assert_eq!(status, StatusCode::OK, "offer: {offer}");
    let nonce = offer["nonce"].as_str().unwrap().to_string();
    assert_eq!(offer["payload"]["nonce"], nonce.as_str());
    assert_eq!(offer["payload"]["v"], 1);

    let pk = phone.public_key();
    let (status, enrolled) = call(
        app,
        "POST",
        "/api/pair/enroll",
        None,
        Some(json!({
            "nonce": nonce,
            "publicKey": pk,
            "signature": phone.sign(&enroll_message(&nonce, &pk)),
            "label": "Test iPhone",
            "platform": "ios",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "enroll: {enrolled}");
    let request_id = enrolled["requestId"].as_str().unwrap().to_string();
    let code = enrolled["code"].as_str().unwrap().to_string();
    assert_eq!(code, confirmation_code(&nonce, &pk));

    // The computer sees the same six digits.
    let (status, pending) = call(app, "GET", "/api/pair/requests", Some(computer), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pending[0]["code"], code.as_str());
    assert_eq!(pending[0]["label"], "Test iPhone");

    let collect = json!({ "signature": phone.sign(&collect_message(&request_id)) });
    let uri = format!("/api/pair/requests/{request_id}/collect");
    let (status, waiting) = call(app, "POST", &uri, None, Some(collect.clone())).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(waiting["status"], "pending");

    let (status, approved) = call(
        app,
        "POST",
        &format!("/api/pair/requests/{request_id}/approve"),
        Some(computer),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "approve: {approved}");

    let (status, got) = call(app, "POST", &uri, None, Some(collect)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got["status"], "approved");
    (
        got["token"].as_str().unwrap().to_string(),
        got["deviceId"].as_str().unwrap().to_string(),
    )
}

async fn device_login(app: &axum::Router, phone: &Phone, device_id: &str) -> (StatusCode, Value) {
    let (status, ch) = call(
        app,
        "POST",
        "/api/auth/device/challenge",
        None,
        Some(json!({ "deviceId": device_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = ch["challengeId"].as_str().unwrap();
    let challenge = ch["challenge"].as_str().unwrap();
    call(
        app,
        "POST",
        "/api/auth/device/login",
        None,
        Some(json!({
            "challengeId": id,
            "signature": phone.sign(&login_message(id, challenge)),
        })),
    )
    .await
}

#[tokio::test]
async fn the_machine_owner_pairs_a_phone_and_can_remove_it() {
    let app = build_app("127.0.0.1").await;
    let phone = Phone::new();

    // The desktop's machine token pairs to the local owner account, which
    // has no password: exactly the install that could not pair before.
    let (session, device_id) = pair(&app, STATIC_TOKEN, &phone).await;
    let (status, me) = call(&app, "GET", "/api/auth/me", Some(&session), None).await;
    assert_eq!(status, StatusCode::OK, "me: {me}");

    // Later, from anywhere: the key alone signs the phone back in.
    let (status, again) = device_login(&app, &phone, &device_id).await;
    assert_eq!(status, StatusCode::OK, "device login: {again}");
    let second = again["token"].as_str().unwrap().to_string();
    assert_eq!(again["userId"], me["userId"]);

    // Another phone's key cannot borrow this device id.
    let (status, _) = device_login(&app, &Phone::new(), &device_id).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The computer lists and removes it; every session it held dies at once.
    let (status, devices) = call(&app, "GET", "/api/auth/devices", Some(&session), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(devices[0]["paired"], true);
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/api/auth/devices/{device_id}"),
        Some(STATIC_TOKEN),
        None,
    )
    .await;
    // The machine token speaks for the local owner, so it can remove the phone.
    assert_eq!(status, StatusCode::NO_CONTENT);
    for token in [&session, &second] {
        let (status, _) = call(&app, "GET", "/api/auth/me", Some(token), None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "a removed phone kept a session"
        );
    }
    let (status, _) = device_login(&app, &phone, &device_id).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_signed_in_account_pairs_to_itself_and_codes_are_single_use() {
    let app = build_app("127.0.0.1").await;
    let (status, reg) = call(
        &app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({ "username": "pairer", "password": "correct horse battery" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let token = reg["token"].as_str().unwrap().to_string();

    let phone = Phone::new();
    let (session, _) = pair(&app, &token, &phone).await;
    let (_, me) = call(&app, "GET", "/api/auth/me", Some(&session), None).await;
    assert_eq!(me["username"], "pairer");

    // An offer cannot be replayed by a second phone.
    let (_, offer) = call(&app, "POST", "/api/pair/offer", Some(&token), None).await;
    let nonce = offer["nonce"].as_str().unwrap().to_string();
    for (i, p) in [Phone::new(), Phone::new()].iter().enumerate() {
        let pk = p.public_key();
        let (status, _) = call(
            &app,
            "POST",
            "/api/pair/enroll",
            None,
            Some(json!({
                "nonce": nonce,
                "publicKey": pk,
                "signature": p.sign(&enroll_message(&nonce, &pk)),
            })),
        )
        .await;
        let want = if i == 0 {
            StatusCode::OK
        } else {
            StatusCode::GONE
        };
        assert_eq!(status, want);
    }
}

#[tokio::test]
async fn the_machine_token_cannot_pair_on_a_network_bound_gateway() {
    let app = build_app("0.0.0.0").await;
    let (status, _) = call(&app, "POST", "/api/pair/offer", Some(STATIC_TOKEN), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, "POST", "/api/pair/offer", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
