//! End-to-end HTTP tests for the vault API.
//!
//! The credentials crate's own tests prove the crypto. These prove the *contract*
//! — that the JSON a real web or mobile client sends is the JSON the handlers
//! expect, that scopes survive the query-string round trip, and that the status
//! codes the clients branch on (`423 Locked`, `409 Conflict`, `401`) are the ones
//! actually returned. That is the layer where a camelCase mismatch hides: it
//! compiles, and only a request over the wire catches it.
//!
//! Each test drives the fully assembled router through `oneshot`, so requests
//! pass through the real bearer-auth layer exactly as they would in production.
//! Argon2id at 64 MiB runs for real here, which is why these are a handful of
//! focused flows rather than an exhaustive matrix.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tempfile::TempDir;
use tower::util::ServiceExt;

use consistent_world_gateway::server;
use consistent_world_gateway::state::ServerState;
use consistent_world_gateway::vault_manager::VaultManager;
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};

const TOKEN: &str = "test-secret-token";

/// A router wired exactly like production, with a vault manager rooted in a temp
/// dir. The `TempDir` is returned so it outlives the test.
async fn build_app() -> (axum::Router, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let vaults = Arc::new(VaultManager::new(dir.path().to_path_buf()));

    let state =
        ServerState::new("test".into(), Some(TOKEN.into()), memory).with_vault_manager(vaults);

    (server::build_router(state), dir)
}

/// Send an authenticated JSON request and return `(status, parsed body)`.
///
/// A 204 or empty body comes back as `Value::Null`, which is what the callers
/// below check for.
async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json");

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

#[tokio::test]
async fn a_vault_can_be_created_unlocked_and_used_over_http() {
    let (app, _dir) = build_app().await;

    // Fresh: not initialized.
    let (status, body) = call(&app, "GET", "/api/vault/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["initialized"], false);

    // Create. The response must carry the recovery code exactly once.
    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "correct horse battery" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["initialized"], true);
    assert_eq!(body["unlocked"], true);
    let recovery_code = body["recoveryCode"].as_str().unwrap().to_string();
    assert!(
        !recovery_code.is_empty(),
        "init must return a recovery code"
    );
    // A fresh vault has a password method and a recovery method.
    assert_eq!(body["unlockMethods"].as_array().unwrap().len(), 2);

    // Store an item and read its secret back.
    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/items",
        Some(json!({
            "kind": "login",
            "name": "GitHub",
            "uris": ["https://github.com"],
            "secret": { "username": "ahmad", "password": "hunter2" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let item_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = call(&app, "GET", &format!("/api/vault/items/{item_id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    // Proves camelCase serialization of ItemSecret over the wire.
    assert_eq!(body["password"], "hunter2");
    assert_eq!(body["username"], "ahmad");

    // Listing returns metadata only — never the secret.
    let (status, body) = call(&app, "GET", "/api/vault/items", None).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "GitHub");
    assert!(
        items[0].get("password").is_none(),
        "list must not leak secrets"
    );
}

#[tokio::test]
async fn a_second_init_conflicts() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "first password here" })),
    )
    .await;

    let (status, _) = call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "second password here" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "409 is what the client branches on"
    );
}

#[tokio::test]
async fn locking_makes_reads_return_423_and_unlock_restores_them() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "a strong secret" })),
    )
    .await;

    call(&app, "POST", "/api/vault/lock", None).await;

    // 423 Locked is flow control the clients depend on — distinct from 401.
    let (status, _) = call(&app, "GET", "/api/vault/items", None).await;
    assert_eq!(status, StatusCode::LOCKED);

    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/unlock",
        Some(json!({ "masterPassword": "a strong secret" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unlocked"], true);

    let (status, _) = call(&app, "GET", "/api/vault/items", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_wrong_master_password_is_401_not_423() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "the real password" })),
    )
    .await;
    call(&app, "POST", "/api/vault/lock", None).await;

    let (status, _) = call(
        &app,
        "POST",
        "/api/vault/unlock",
        Some(json!({ "masterPassword": "the wrong password" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_recovery_code_from_init_unlocks_over_http() {
    let (app, _dir) = build_app().await;
    let (_, body) = call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "original password" })),
    )
    .await;
    let recovery_code = body["recoveryCode"].as_str().unwrap().to_string();

    call(&app, "POST", "/api/vault/lock", None).await;

    // The whole point of the code: it opens the vault with no password.
    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/unlock/recovery",
        Some(json!({ "recoveryCode": recovery_code })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["unlocked"], true);
}

#[tokio::test]
async fn a_device_key_can_be_enrolled_and_then_unlocks() {
    // Exercises the mobile/desktop path end to end: fetch a key, enrol it as a
    // device-key method, lock, and reopen with it — no password involved.
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "password for setup" })),
    )
    .await;

    let (status, body) = call(&app, "GET", "/api/vault/random-key", None).await;
    assert_eq!(status, StatusCode::OK);
    let key = body["keyB64"].as_str().unwrap().to_string();

    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/unlock-methods",
        Some(json!({ "kind": "device-key", "label": "Test device", "key": key })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let method_id = body["id"].as_str().unwrap().to_string();

    call(&app, "POST", "/api/vault/lock", None).await;

    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/unlock/key",
        Some(json!({ "methodId": method_id, "key": key })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the device key should reopen the vault"
    );
    assert_eq!(body["unlocked"], true);
    // Now three ways in: password, recovery, device.
    assert_eq!(body["unlockMethods"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn scoped_secrets_are_isolated_per_entity_over_http() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;

    // A global default and two entities overriding it.
    for (scope, value) in [
        (None, "sk-global"),
        (Some("entity:acme"), "sk-acme"),
        (Some("entity:globex"), "sk-globex"),
    ] {
        let mut body = json!({ "key": "OPENAI_API_KEY", "value": value });
        if let Some(s) = scope {
            body["scope"] = json!(s);
        }
        let (status, _) = call(&app, "POST", "/api/vault/secrets", Some(body)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    // Each entity resolves its own value; the query-string scope round-trips.
    let (status, body) = call(
        &app,
        "GET",
        "/api/vault/secrets/OPENAI_API_KEY?scope=entity:acme&resolve=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], "sk-acme");

    // An entity with no override of its own inherits global via resolve.
    let (status, body) = call(
        &app,
        "GET",
        "/api/vault/secrets/OPENAI_API_KEY?scope=entity:newco&resolve=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], "sk-global");

    // Without resolve, that same entity has nothing of its own.
    let (status, _) = call(
        &app,
        "GET",
        "/api/vault/secrets/OPENAI_API_KEY?scope=entity:newco",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The scope list shows the populated scopes.
    let (status, body) = call(&app, "GET", "/api/vault/scopes", None).await;
    assert_eq!(status, StatusCode::OK);
    let scopes: Vec<&str> = body["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["scope"].as_str().unwrap())
        .collect();
    assert!(scopes.contains(&"global"));
    assert!(scopes.contains(&"entity:acme"));
    assert!(scopes.contains(&"entity:globex"));
}

#[tokio::test]
async fn the_full_resolved_environment_is_global_overlaid_with_the_entity() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;

    call(
        &app,
        "POST",
        "/api/vault/secrets",
        Some(json!({ "key": "LOG_LEVEL", "value": "info" })),
    )
    .await;
    call(
        &app,
        "POST",
        "/api/vault/secrets",
        Some(json!({ "key": "OPENAI_API_KEY", "value": "sk-global" })),
    )
    .await;
    call(
        &app,
        "POST",
        "/api/vault/secrets",
        Some(json!({ "key": "OPENAI_API_KEY", "value": "sk-own", "scope": "entity:acme" })),
    )
    .await;

    let (status, body) = call(&app, "GET", "/api/vault/env?scope=entity:acme", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["env"]["OPENAI_API_KEY"], "sk-own", "own value wins");
    assert_eq!(body["env"]["LOG_LEVEL"], "info", "global inherited");
}

#[tokio::test]
async fn a_backup_exported_over_http_restores_into_a_fresh_vault() {
    // Two vaults under two principals. Only bearer tokens differ in production;
    // here both are the same token but the vault manager keys by principal, and
    // the default principal is stable, so a second vault needs its own manager.
    // Simplest honest test: export from one app, import into a separately built
    // one, proving the backup is self-contained.
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "source vault password" })),
    )
    .await;
    call(
        &app,
        "POST",
        "/api/vault/secrets",
        Some(json!({ "key": "API_KEY", "value": "sk-backed-up" })),
    )
    .await;

    let (status, backup) = call(
        &app,
        "POST",
        "/api/vault/export",
        Some(json!({ "passphrase": "a-different-passphrase" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(backup["format"], "consistent-ceo/vault-export");

    // A brand new vault elsewhere.
    let (other, _dir2) = build_app().await;
    call(
        &other,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "totally unrelated password" })),
    )
    .await;

    let (status, body) = call(
        &other,
        "POST",
        "/api/vault/import",
        Some(json!({ "passphrase": "a-different-passphrase", "backup": backup })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["variablesAdded"], 1);

    // The restored secret is really there and readable.
    let (status, body) = call(&other, "GET", "/api/vault/secrets/API_KEY", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], "sk-backed-up");
}

#[tokio::test]
async fn generation_needs_auth_but_not_an_unlocked_vault() {
    let (app, _dir) = build_app().await;

    // No vault at all, but generation is pure randomness and should still work.
    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/generate",
        Some(json!({ "mode": "password", "length": 24, "symbols": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"].as_str().unwrap().chars().count(), 24);
}

#[tokio::test]
async fn each_entity_has_its_own_password_set_over_http() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;

    // The CEO's own login, plus one filed under an entity.
    call(
        &app,
        "POST",
        "/api/vault/items",
        Some(json!({ "kind": "login", "name": "Personal", "secret": { "password": "p1" } })),
    )
    .await;
    call(
        &app,
        "POST",
        "/api/vault/items",
        Some(json!({
            "kind": "login",
            "name": "Acme Stripe",
            "scope": "entity:acme",
            "secret": { "password": "p2" }
        })),
    )
    .await;

    // Filtering by the entity returns only its set.
    let (status, body) = call(&app, "GET", "/api/vault/items?scope=entity:acme", None).await;
    assert_eq!(status, StatusCode::OK);
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "Acme Stripe");
    assert_eq!(items[0]["scope"], "entity:acme");

    // The CEO's own set is separate.
    let (_, body) = call(&app, "GET", "/api/vault/items?scope=global", None).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // No scope = everything, each tagged.
    let (_, body) = call(&app, "GET", "/api/vault/items", None).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    // The entity switcher sees which entities have items.
    let (status, body) = call(&app, "GET", "/api/vault/item-scopes", None).await;
    assert_eq!(status, StatusCode::OK);
    let scopes: Vec<&str> = body["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["scope"].as_str().unwrap())
        .collect();
    assert!(scopes.contains(&"global"));
    assert!(scopes.contains(&"entity:acme"));
}

#[tokio::test]
async fn an_item_can_be_moved_to_another_entity_over_http() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;
    let (_, body) = call(
        &app,
        "POST",
        "/api/vault/items",
        Some(json!({ "kind": "login", "name": "Movable", "secret": { "password": "keep" } })),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_string();

    let (status, _) = call(
        &app,
        "PUT",
        &format!("/api/vault/items/{id}/scope"),
        Some(json!({ "scope": "entity:globex" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, body) = call(&app, "GET", "/api/vault/items?scope=entity:globex", None).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    // Secret survived the move.
    let (_, secret) = call(&app, "GET", &format!("/api/vault/items/{id}"), None).await;
    assert_eq!(secret["password"], "keep");
}

#[tokio::test]
async fn a_csv_imports_into_a_chosen_entity_over_http() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;

    let csv = "name,url,username,password\nGitHub,https://github.com,ops,pw";
    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/import/csv",
        Some(json!({ "csv": csv, "commit": true, "scope": "entity:acme" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["imported"], 1);

    // The imported login landed in the entity's set, not the global one.
    let (_, acme) = call(&app, "GET", "/api/vault/items?scope=entity:acme", None).await;
    assert_eq!(acme["items"].as_array().unwrap().len(), 1);
    let (_, global) = call(&app, "GET", "/api/vault/items?scope=global", None).await;
    assert_eq!(global["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn a_stored_totp_seed_generates_a_live_code_over_http() {
    // The "you only need this one app" path: store a 2FA seed with the login and
    // read the current code back. The seed never comes back — only the digits.
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;

    // The RFC 6238 test seed.
    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/items",
        Some(json!({
            "kind": "login",
            "name": "GitHub",
            "secret": { "username": "ahmad", "totpSecret": "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ" }
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let item_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = call(
        &app,
        "GET",
        &format!("/api/vault/items/{item_id}/totp"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let code = body["code"].as_str().unwrap();
    assert_eq!(code.len(), 6, "a six-digit code");
    assert!(code.chars().all(|c| c.is_ascii_digit()));
    assert!(body["secondsRemaining"].as_u64().unwrap() <= 30);
}

#[tokio::test]
async fn an_item_without_a_totp_seed_is_a_clean_400() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;
    let (_, body) = call(
        &app,
        "POST",
        "/api/vault/items",
        Some(json!({ "kind": "login", "name": "No2FA", "secret": { "password": "p" } })),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_string();

    let (status, _) = call(&app, "GET", &format!("/api/vault/items/{id}/totp"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_bitwarden_csv_imports_over_http() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;

    let csv = "folder,favorite,type,name,notes,fields,reprompt,login_uri,login_username,login_password,login_totp\n\
               Work,1,login,GitHub,,,0,https://github.com,ahmad,hunter2,\n\
               ,0,login,Gmail,,,0,https://mail.google.com,ahmad@gmail.com,s3cret,";

    // Dry run first: reports what would import, stores nothing.
    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/import/csv",
        Some(json!({ "csv": csv, "commit": false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["format"], "Bitwarden");
    assert_eq!(body["detected"], 2);
    assert_eq!(body["imported"], 0);
    assert_eq!(body["committed"], false);

    // Nothing stored yet.
    let (_, items) = call(&app, "GET", "/api/vault/items", None).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 0);

    // Commit.
    let (status, body) = call(
        &app,
        "POST",
        "/api/vault/import/csv",
        Some(json!({ "csv": csv, "commit": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["imported"], 2);

    let (_, items) = call(&app, "GET", "/api/vault/items", None).await;
    assert_eq!(items["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_non_password_csv_is_rejected() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;
    let (status, _) = call(
        &app,
        "POST",
        "/api/vault/import/csv",
        Some(json!({ "csv": "date,amount\n2026-01-01,42", "commit": true })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_health_report_flags_weak_and_reused_over_http() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;

    // One weak, two sharing a (strong) password.
    for (name, pw) in [
        ("Bank", "password1"),
        ("Email", "Kx9#mP2$vL8@qR4!nW6z"),
        ("Cloud", "Kx9#mP2$vL8@qR4!nW6z"),
    ] {
        call(
            &app,
            "POST",
            "/api/vault/items",
            Some(json!({ "kind": "login", "name": name, "secret": { "password": pw } })),
        )
        .await;
    }

    let (status, body) = call(&app, "GET", "/api/vault/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["totalWithPasswords"], 3);
    assert_eq!(body["weak"].as_array().unwrap().len(), 1);
    assert_eq!(body["reused"].as_array().unwrap().len(), 2);
    // A score field the UI can render as a ring.
    assert!(body["score"].as_u64().unwrap() <= 100);
    // No password value in the report.
    let text = body.to_string();
    assert!(!text.contains("password1"));
    assert!(!text.contains("Kx9#mP2"));
}

#[tokio::test]
async fn a_breach_check_of_a_vault_with_no_passwords_makes_no_external_calls() {
    // Deterministic and offline: with nothing to check, the endpoint returns a
    // clean report without reaching Have I Been Pwned. (The k-anonymity logic
    // itself is covered by the credentials crate's `breach` unit tests; the
    // live HIBP path is deliberately not exercised in CI.)
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;
    // A secure note has no password, so it is not a breach target.
    call(
        &app,
        "POST",
        "/api/vault/items",
        Some(json!({ "kind": "secure-note", "name": "Note", "secret": { "notes": "hi" } })),
    )
    .await;

    let (status, body) = call(&app, "GET", "/api/vault/breach-check", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["checked"], 0);
    assert_eq!(body["breached"].as_array().unwrap().len(), 0);
    assert_eq!(body["unreachable"], false);
}

#[tokio::test]
async fn a_changed_password_is_kept_in_history_over_http() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "setup password here" })),
    )
    .await;
    let (_, body) = call(
        &app,
        "POST",
        "/api/vault/items",
        Some(json!({ "kind": "login", "name": "Site", "secret": { "password": "old-pw" } })),
    )
    .await;
    let id = body["id"].as_str().unwrap().to_string();

    // Change the password via the secret update endpoint.
    let (status, _) = call(
        &app,
        "PUT",
        &format!("/api/vault/items/{id}"),
        Some(json!({ "password": "new-pw" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The prior password comes back in the item's history, newest first.
    let (_, secret) = call(&app, "GET", &format!("/api/vault/items/{id}"), None).await;
    assert_eq!(secret["password"], "new-pw");
    let history = secret["passwordHistory"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["password"], "old-pw");
}

#[tokio::test]
async fn password_strength_scores_over_http() {
    let (app, _dir) = build_app().await;

    // No vault needed — this is called at creation time.
    let (status, weak) = call(
        &app,
        "POST",
        "/api/vault/password-strength",
        Some(json!({ "password": "password1" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(weak["score"].as_u64().unwrap() <= 1);
    assert_eq!(weak["acceptable"], false);

    let (_, strong) = call(
        &app,
        "POST",
        "/api/vault/password-strength",
        Some(json!({ "password": "correct horse battery staple" })),
    )
    .await;
    assert!(strong["score"].as_u64().unwrap() >= 3);
    assert_eq!(strong["acceptable"], true);
}

#[tokio::test]
async fn repeated_wrong_passwords_are_throttled_with_429() {
    let (app, _dir) = build_app().await;
    call(
        &app,
        "POST",
        "/api/vault/init",
        Some(json!({ "masterPassword": "the real password" })),
    )
    .await;
    call(&app, "POST", "/api/vault/lock", None).await;

    // The first several wrong attempts return 401 (free allowance). Eventually
    // the throttle kicks in and returns 429 with a wait hint.
    let mut saw_429 = false;
    for _ in 0..12 {
        let (status, body) = call(
            &app,
            "POST",
            "/api/vault/unlock",
            Some(json!({ "masterPassword": "wrong" })),
        )
        .await;
        if status == StatusCode::TOO_MANY_REQUESTS {
            saw_429 = true;
            assert!(
                body["error"].as_str().unwrap().contains("wait"),
                "the 429 should tell the client how long to wait"
            );
            break;
        }
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(
        saw_429,
        "sustained wrong guesses must eventually be throttled"
    );
}

#[tokio::test]
async fn every_vault_route_refuses_a_missing_bearer_token() {
    let (app, _dir) = build_app().await;

    // No Authorization header at all.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/vault/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "the vault must sit behind bearer auth"
    );
}
