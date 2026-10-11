//! End-to-end HTTP tests for the auth API.
//!
//! `auth_store.rs` proves the store invariants in isolation. These prove the
//! *contract*: that the wire shape a real client sends is the one the handlers
//! demand, and — for the security fixes — that the attack actually fails when it
//! is delivered as an HTTP request through the real router, middleware and all.
//!
//! Each test drives the fully assembled router through `oneshot`. Argon2id runs
//! for real here (eleven hashes per registration), so these are a handful of
//! focused flows rather than an exhaustive matrix.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::util::ServiceExt;

use consistent_world_gateway::auth_store::AuthStore;
use consistent_world_gateway::server;
use consistent_world_gateway::state::ServerState;
use consistent_world_gateway::vault_manager::VaultManager;
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};

const PASSWORD: &str = "correct horse battery";

/// The static deploy token, which stands for "whoever holds this machine".
const STATIC_TOKEN: &str = "static-token";

/// A router wired like production, with an in-memory auth store and a passkey
/// service.
async fn build_app() -> axum::Router {
    build_app_bound("127.0.0.1").await
}

/// Same, bound to `bind`. The first-run local-session path is gated on a
/// loopback bind, so testing that gate means being able to move it.
async fn build_app_bound(bind: &str) -> axum::Router {
    server::build_router(build_state(bind).await)
}

/// Same, with per-user credential vaults rooted at `root`, so a test can see
/// account deletion take the vault file with it.
async fn build_app_with_vaults(root: &std::path::Path) -> (axum::Router, Arc<VaultManager>) {
    let vaults = Arc::new(VaultManager::new(root.to_path_buf()));
    let state = build_state("127.0.0.1")
        .await
        .with_vault_manager(vaults.clone());
    (server::build_router(state), vaults)
}

async fn build_state(bind: &str) -> ServerState {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());

    let db = surrealdb::engine::any::connect("memory").await.unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    let auth = AuthStore::new(Arc::new(db));
    auth.init_schema().await.unwrap();

    let passkey = consistent_world_gateway::passkey::PasskeyService::new(
        "localhost",
        "http://localhost:18800",
    )
    .unwrap();

    // A loopback bind, as a self-hosted gateway runs. This also takes the
    // per-IP register limiter out of play: it is keyed on the peer address,
    // which `oneshot` never sets, so every test in the process would otherwise
    // share one five-per-minute bucket.
    let config = Arc::new(consistent_world_gateway::config::AppConfig {
        gateway_bind: bind.to_string(),
        ..Default::default()
    });

    ServerState::new("test".into(), Some(STATIC_TOKEN.into()), memory)
        .with_auth_store(auth)
        .with_passkey(passkey)
        .with_app_config(config)
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

/// Register an account and return `(token, user_id, recovery codes)`.
async fn register(app: &axum::Router, username: &str) -> (String, String, Vec<String>) {
    let (status, body) = call(
        app,
        "POST",
        "/api/auth/register",
        None,
        Some(json!({ "username": username, "password": PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "register failed: {body}");
    let codes = body["recoveryCodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect();
    (
        body["token"].as_str().unwrap().to_string(),
        body["userId"].as_str().unwrap().to_string(),
        codes,
    )
}

async fn login(app: &axum::Router, username: &str, password: &str) -> (StatusCode, Value) {
    call(
        app,
        "POST",
        "/api/auth/login",
        None,
        Some(json!({ "username": username, "password": password })),
    )
    .await
}

/// Finding 1, over the wire: passkey login is not an account-takeover primitive.
///
/// The attack was to sign your *own* challenge with your *own* passkey and post
/// someone else's username alongside it. The account is now read from the
/// ceremony state, so there is no username field left to lie in — a body that
/// carries one is simply ignored, and no path here can mint a session for an
/// account whose passkey did not sign.
#[tokio::test]
async fn passkey_login_cannot_mint_a_session_for_another_user() {
    let app = build_app().await;
    let (_, victim_id, _) = register(&app, "victim").await;
    register(&app, "attacker").await;

    // The attacker starts a ceremony against their own account (the victim has
    // no passkey at all, which is exactly the case the old code allowed).
    let (status, _) = call(
        &app,
        "POST",
        "/api/auth/passkey/login/start",
        None,
        Some(json!({ "username": "attacker" })),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "no passkeys enrolled yet, so there is no challenge to issue"
    );

    // The old shape of the attack: a username the client picks, next to a
    // state id and credential of the attacker's own.
    let (status, body) = call(
        &app,
        "POST",
        "/api/auth/passkey/login/finish",
        None,
        Some(json!({
            "username": "victim",
            "stateId": "00000000000000000000000000000000",
            "credential": {}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body.get("token").is_none(),
        "no request to this endpoint may return a session for a user who did not sign"
    );

    // And the victim's account is untouched: their password still works, and
    // nothing has been signed in on their behalf.
    let (status, _) = login(&app, "victim", PASSWORD).await;
    assert_eq!(status, StatusCode::OK);
    let (status, sessions) = call(&app, "GET", "/api/auth/sessions", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{sessions}");
    assert!(!victim_id.is_empty());
}

/// Findings 2 + 3 together, which is how they matter: proving the old password
/// evicts a thief, and the eviction is what makes proving it worth anything.
#[tokio::test]
async fn changing_a_password_needs_the_old_one_and_evicts_every_other_session() {
    let app = build_app().await;
    let (owner, _, _) = register(&app, "owner").await;

    // A second session — stand-in for the stolen token.
    let (status, body) = login(&app, "owner", PASSWORD).await;
    assert_eq!(status, StatusCode::OK);
    let thief = body["token"].as_str().unwrap().to_string();
    let (status, _) = call(&app, "GET", "/api/auth/me", Some(&thief), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the stolen token works, as it would"
    );

    // The thief cannot take the account: they hold a token, not a password.
    let (status, _) = call(
        &app,
        "POST",
        "/api/auth/change-password",
        Some(&thief),
        Some(json!({ "currentPassword": "guess", "newPassword": "locked you out" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A body with no `currentPassword` at all — the old shape — is rejected by
    // deserialization, not silently accepted.
    let (status, _) = call(
        &app,
        "POST",
        "/api/auth/change-password",
        Some(&thief),
        Some(json!({ "newPassword": "locked you out" })),
    )
    .await;
    assert!(
        status.is_client_error(),
        "the request shape that required no current password must not be accepted"
    );

    // The owner changes it properly, and the thief is thrown out.
    let (status, body) = call(
        &app,
        "POST",
        "/api/auth/change-password",
        Some(&owner),
        Some(json!({ "currentPassword": PASSWORD, "newPassword": "a brand new password" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revokedSessions"], json!(1));

    let (status, _) = call(&app, "GET", "/api/auth/me", Some(&thief), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the evicted session must stop working"
    );
    let (status, _) = call(&app, "GET", "/api/auth/me", Some(&owner), None).await;
    assert_eq!(status, StatusCode::OK, "the caller keeps their own session");

    assert_eq!(
        login(&app, "owner", "a brand new password").await.0,
        StatusCode::OK
    );
}

/// Finding 2: the surface that makes "sign out everywhere" possible at all.
/// Interactive sessions could previously be neither listed nor revoked.
#[tokio::test]
async fn sessions_can_be_listed_and_signed_out_everywhere() {
    let app = build_app().await;
    let (first, _, _) = register(&app, "listowner").await;
    let (_, body) = login(&app, "listowner", PASSWORD).await;
    let second = body["token"].as_str().unwrap().to_string();

    let (status, sessions) = call(&app, "GET", "/api/auth/sessions", Some(&first), None).await;
    assert_eq!(status, StatusCode::OK);
    let sessions = sessions.as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    assert_eq!(
        sessions
            .iter()
            .filter(|s| s["current"] == json!(true))
            .count(),
        1,
        "the caller's own session is labelled"
    );
    assert!(
        !sessions.iter().any(|s| s.to_string().contains(&first)),
        "raw tokens must never appear in the listing"
    );

    // Revoke one by its handle.
    let other = sessions
        .iter()
        .find(|s| s["current"] == json!(false))
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/api/auth/sessions/{other}"),
        Some(&first),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(&second), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );

    // And the blunt instrument.
    let (_, body) = login(&app, "listowner", PASSWORD).await;
    let third = body["token"].as_str().unwrap().to_string();
    let (status, body) = call(&app, "DELETE", "/api/auth/sessions", Some(&first), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revokedSessions"], json!(1));
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(&third), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(&first), None)
            .await
            .0,
        StatusCode::OK
    );
}

/// Finding 4: the six digits are load-bearing, not decorative. This is the flow
/// Phase 2's desktop login rests on, so the binding is tested at the HTTP layer
/// the desktop will actually speak.
#[tokio::test]
async fn approving_a_login_requires_the_code_from_the_requesting_client() {
    let app = build_app().await;
    let (phone, _, _) = register(&app, "approver").await;

    // The desktop starts a request; it learns a request id and a code.
    let (status, start) = call(
        &app,
        "POST",
        "/api/auth/approval/start",
        None,
        Some(json!({ "username": "approver", "context": "Desktop app" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request_id = start["requestId"].as_str().unwrap().to_string();
    let code = start["code"].as_str().unwrap().to_string();

    // The old request shape — device only, no code — must not approve anything.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/auth/approvals/{request_id}/approve"),
        Some(&phone),
        Some(json!({ "device": "iPhone" })),
    )
    .await;
    assert!(
        status.is_client_error(),
        "an approval without the code must be refused"
    );

    // A wrong code is refused, and the request stays pending.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/auth/approvals/{request_id}/approve"),
        Some(&phone),
        Some(json!({ "code": "000000", "device": "iPhone" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, poll) = call(
        &app,
        "GET",
        &format!("/api/auth/approval/{request_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(poll["status"], json!("pending"));
    assert!(poll.get("token").is_none());

    // The real code approves, and the requesting client collects its session.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/auth/approvals/{request_id}/approve"),
        Some(&phone),
        Some(json!({ "code": code, "device": "iPhone" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (_, poll) = call(
        &app,
        "GET",
        &format!("/api/auth/approval/{request_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(poll["status"], json!("approved"));
    let minted = poll["token"].as_str().unwrap();
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(minted), None)
            .await
            .0,
        StatusCode::OK
    );

    // Collected exactly once.
    let (_, poll) = call(
        &app,
        "GET",
        &format!("/api/auth/approval/{request_id}"),
        None,
        None,
    )
    .await;
    assert!(poll.get("token").is_none());
}

/// The whole desktop login, over HTTP, exactly as the app and the browser will
/// drive it.
///
/// The app starts a request naming nobody and shows the code. The user signs in
/// in their browser — a different client, same gateway, same origin — reads the
/// request, types the code, and approves. The app collects the session with its
/// PKCE verifier. Nothing leaves localhost at any point.
#[tokio::test]
async fn the_desktop_signs_in_through_the_browser() {
    let app = build_app().await;

    // 1. The app, which has no idea who is about to sign in.
    let verifier = "the-desktop-generated-this-and-keeps-it";
    let challenge = consistent_world_gateway::auth_store::pkce_challenge(verifier);
    let (status, start) = call(
        &app,
        "POST",
        "/api/auth/approval/start",
        None,
        Some(json!({ "context": "Consistent on this computer", "codeChallenge": challenge })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let request_id = start["requestId"].as_str().unwrap().to_string();
    let code = start["code"].as_str().unwrap().to_string();

    // Nothing to collect yet, even holding the verifier.
    let (_, poll) = call(
        &app,
        "GET",
        &format!("/api/auth/approval/{request_id}?verifier={verifier}"),
        None,
        None,
    )
    .await;
    assert_eq!(poll["status"], json!("pending"));

    // 2. The browser: a separate client that signs in on its own.
    let (browser, _, _) = register(&app, "desktopuser").await;
    let (status, detail) = call(
        &app,
        "GET",
        &format!("/api/auth/approvals/{request_id}"),
        Some(&browser),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["context"], json!("Consistent on this computer"));
    assert!(
        !detail.to_string().contains(&code),
        "the browser must not be handed the code it is asking the user to type"
    );

    // 3. The user types the code they can see in the app.
    let (status, _) = call(
        &app,
        "POST",
        &format!("/api/auth/approvals/{request_id}/approve"),
        Some(&browser),
        Some(json!({ "code": code, "device": "Chrome on macOS" })),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 4. Someone who learned the request id — it travelled through a URL —
    //    still gets nothing.
    let (_, stolen) = call(
        &app,
        "GET",
        &format!("/api/auth/approval/{request_id}"),
        None,
        None,
    )
    .await;
    assert_eq!(stolen["status"], json!("unknown"));
    assert!(stolen.get("token").is_none());

    // 5. The app collects, and is now a real user session — not a static token.
    let (_, poll) = call(
        &app,
        "GET",
        &format!("/api/auth/approval/{request_id}?verifier={verifier}"),
        None,
        None,
    )
    .await;
    assert_eq!(poll["status"], json!("approved"));
    let desktop_token = poll["token"].as_str().unwrap();

    let (status, me) = call(&app, "GET", "/api/auth/me", Some(desktop_token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["username"], json!("desktopuser"));

    // And it shows up where the user can see and revoke it.
    let (_, sessions) = call(&app, "GET", "/api/auth/sessions", Some(&browser), None).await;
    let labels: Vec<&str> = sessions
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["deviceLabel"].as_str())
        .collect();
    assert!(
        labels.contains(&"Chrome on macOS"),
        "the approved sign-in must be listed as its own session: {labels:?}"
    );
}

/// First run on your own machine: double-click, and you are in.
///
/// The desktop trades the local token it already holds for an ordinary session,
/// so no page ever holds the token itself and the user fills in nothing. What
/// they get is a real revocable session for the same principal the static path
/// has always used — so everything already on the machine is still theirs.
#[tokio::test]
async fn the_machine_owner_gets_a_session_without_filling_anything_in() {
    let app = build_app().await;

    let (status, session) = call(
        &app,
        "POST",
        "/api/auth/local-session",
        Some(STATIC_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let token = session["token"].as_str().unwrap().to_string();
    assert_ne!(
        token, STATIC_TOKEN,
        "the deploy token must not be handed back"
    );

    // It is an ordinary session: it authenticates, and it can be revoked.
    let (status, me) = call(&app, "GET", "/api/auth/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        me["userId"], session["userId"],
        "the session is for the principal the static path already used, so \
         nothing created on this machine is stranded"
    );

    let (_, sessions) = call(&app, "GET", "/api/auth/sessions", Some(&token), None).await;
    assert_eq!(sessions.as_array().unwrap().len(), 1);

    // The account has no password yet, and the UI asks about exactly this.
    let (status, account) = call(&app, "GET", "/api/auth/account", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(account["hasPassword"], json!(false));
}

/// The two gates on the first-run path, which are the whole of its safety.
#[tokio::test]
async fn a_local_session_needs_the_machine_token_and_a_loopback_bind() {
    let app = build_app().await;

    // A real user session cannot ask for one: that would be an escalation from
    // "a signed-in person" to "the operator of this box".
    let (user, _, _) = register(&app, "ordinary").await;
    let (status, _) = call(&app, "POST", "/api/auth/local-session", Some(&user), None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Nor can an unauthenticated caller.
    let (status, _) = call(&app, "POST", "/api/auth/local-session", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // And on a network-bound gateway the static token is an operator secret,
    // not a person — so it is refused there even though it is the right token.
    let exposed = build_app_bound("0.0.0.0").await;
    let (status, _) = call(
        &exposed,
        "POST",
        "/api/auth/local-session",
        Some(STATIC_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

/// Claiming turns the machine account into one you can reach from a phone,
/// carrying everything in it across rather than starting a second account.
#[tokio::test]
async fn claiming_the_local_account_keeps_it_and_makes_it_reachable() {
    let app = build_app().await;
    let (_, session) = call(
        &app,
        "POST",
        "/api/auth/local-session",
        Some(STATIC_TOKEN),
        None,
    )
    .await;
    let token = session["token"].as_str().unwrap().to_string();
    let user_id = session["userId"].as_str().unwrap().to_string();

    let (status, claimed) = call(
        &app,
        "POST",
        "/api/auth/claim",
        Some(&token),
        Some(json!({ "username": "moe", "password": "correct horse battery" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["username"], json!("moe"));
    assert_eq!(claimed["recoveryCodes"].as_array().unwrap().len(), 10);

    // The existing session keeps working — claiming is not a re-login.
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(&token), None)
            .await
            .0,
        StatusCode::OK
    );

    // And now it signs in from anywhere, as the same account.
    let (status, from_phone) = login(&app, "moe", "correct horse battery").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        from_phone["userId"],
        json!(user_id),
        "the phone must land in the same workspace, not a new one"
    );

    // Claiming twice would be a password change without the old password.
    let (status, _) = call(
        &app,
        "POST",
        "/api/auth/claim",
        Some(&token),
        Some(json!({ "username": "someoneelse", "password": "another password" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// The external-identity seam is inert unless a deployment configures an
/// issuer. This is the promise the whole product rests on: a self-hosted
/// gateway is unaffected by it, and a JWT is just an unrecognised bearer.
#[tokio::test]
async fn external_identity_is_off_unless_configured() {
    let app = build_app().await;

    // A structurally valid JWT — three base64url segments — signed by nobody
    // this gateway has ever heard of.
    let jwt = "eyJhbGciOiJFZERTQSIsImtpZCI6ImsxIn0\
               .eyJzdWIiOiJ1c2VyX2FiYyIsImlzcyI6Imh0dHBzOi8vaWQuZXhhbXBsZSJ9\
               .c2lnbmF0dXJl";
    let (status, _) = call(&app, "GET", "/api/auth/me", Some(jwt), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "with no issuer configured a JWT is just an unknown bearer"
    );

    // And the ordinary local path is untouched by the branch existing.
    let (token, _, _) = register(&app, "unaffected").await;
    let (status, me) = call(&app, "GET", "/api/auth/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["username"], json!("unaffected"));
}

/// Finding 3's other half: with `change-password` now requiring the current
/// password, recovery has to be able to set one — otherwise a forgotten
/// password is unrecoverable by design.
#[tokio::test]
async fn a_backup_code_resets_the_password_and_clears_every_session() {
    let app = build_app().await;
    let (old_session, _, codes) = register(&app, "recovered").await;

    let (status, body) = call(
        &app,
        "POST",
        "/api/auth/recover",
        None,
        Some(json!({
            "username": "recovered",
            "code": codes[0],
            "newPassword": "a whole new password"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(
        login(&app, "recovered", "a whole new password").await.0,
        StatusCode::OK
    );
    assert_eq!(
        login(&app, "recovered", PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(&old_session), None)
            .await
            .0,
        StatusCode::UNAUTHORIZED,
        "recovery means access was lost; every prior session is suspect"
    );

    // The code is spent.
    let (status, _) = call(
        &app,
        "POST",
        "/api/auth/recover",
        None,
        Some(json!({ "username": "recovered", "code": codes[0] })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ===== Account deletion (App Store guideline 5.1.1(v)) =====

async fn delete_account(
    app: &axum::Router,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    call(app, "DELETE", "/api/auth/account", token, body).await
}

/// The whole contract the phone is written against: a wrong password deletes
/// nothing and leaves you signed in; the right one takes the account, every
/// session, the recovery codes, and the vault — and nobody else's.
#[tokio::test]
async fn deleting_an_account_needs_the_password_and_then_removes_it() {
    let vault_root = tempfile::tempdir().unwrap();
    let (app, vaults) = build_app_with_vaults(vault_root.path()).await;
    let (token, user_id, codes) = register(&app, "leaving").await;
    let (_, body) = login(&app, "leaving", PASSWORD).await;
    let second_session = body["token"].as_str().unwrap().to_string();
    let (bystander, bystander_id, _) = register(&app, "staying").await;

    // A vault each, so we can see which ones survive.
    vaults
        .handle(&user_id)
        .init("vault master password")
        .unwrap();
    vaults
        .handle(&bystander_id)
        .init("vault master password")
        .unwrap();
    let vault_file = vaults.handle(&user_id).path().to_path_buf();
    let bystander_vault = vaults.handle(&bystander_id).path().to_path_buf();
    assert!(vault_file.exists());

    // (a) The wrong password: refused as "try again", not "signed out".
    let (status, body) = delete_account(&app, Some(&token), Some(json!({ "password": "not it" }))).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"], json!("password is incorrect"));
    let (status, me) = call(&app, "GET", "/api/auth/me", Some(&token), None).await;
    assert_eq!(status, StatusCode::OK, "a refused deletion must not sign you out");
    assert_eq!(me["userId"], json!(user_id));
    assert!(vault_file.exists(), "a refused deletion must not touch the vault");

    // No body, or a body without the password, is a client error with a reason.
    let (status, body) = delete_account(&app, Some(&token), Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], json!("password is required"));
    let (status, _) = delete_account(&app, Some(&token), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // (b) The right password.
    let (status, body) = delete_account(&app, Some(&token), Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // Every session is gone, including the one that asked.
    for session in [&token, &second_session] {
        assert_eq!(
            call(&app, "GET", "/api/auth/me", Some(session), None)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    // The credentials are gone: no password login, no recovery code.
    assert_eq!(
        login(&app, "leaving", PASSWORD).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, _) = call(
        &app,
        "POST",
        "/api/auth/recover",
        None,
        Some(json!({ "username": "leaving", "code": codes[0], "newPassword": "back again please" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The username is free again.
    let (status, body) = call(
        &app,
        "POST",
        "/api/auth/check-username",
        None,
        Some(json!({ "username": "leaving" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["available"], json!(true));
    // The vault file is gone; the bystander's is not.
    assert!(!vault_file.exists(), "the deleted account's vault must go");
    assert!(bystander_vault.exists(), "another account's vault must stay");

    // The bystander is untouched.
    let (status, me) = call(&app, "GET", "/api/auth/me", Some(&bystander), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["username"], json!("staying"));
    assert_eq!(login(&app, "staying", PASSWORD).await.0, StatusCode::OK);
}

/// Only a signed-in account can delete itself. The deploy token and the
/// machine owner's account are this machine, not an account in it.
#[tokio::test]
async fn only_a_signed_in_account_can_delete_itself() {
    let app = build_app().await;

    let (status, body) = delete_account(
        &app,
        Some(STATIC_TOKEN),
        Some(json!({ "password": PASSWORD })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], json!("no account to delete"));

    // Nor the machine owner's session, even though it is a user session.
    let (_, session) = call(
        &app,
        "POST",
        "/api/auth/local-session",
        Some(STATIC_TOKEN),
        None,
    )
    .await;
    let owner = session["token"].as_str().unwrap().to_string();
    let (status, body) = delete_account(&app, Some(&owner), Some(json!({ "password": "" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], json!("no account to delete"));
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(&owner), None)
            .await
            .0,
        StatusCode::OK,
        "the machine owner's session is untouched"
    );

    // And no token at all is the middleware's ordinary 401.
    let (status, _) = delete_account(&app, None, Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

/// Guessing the password here spends the same budget as guessing it at login,
/// so a stolen session token buys no extra attempts by switching doors.
#[tokio::test]
async fn account_deletion_shares_the_login_lockout() {
    let app = build_app().await;
    let (token, _, _) = register(&app, "lockedout").await;

    // Five free failures, then the sixth imposes a backoff.
    for _ in 0..6 {
        let (status, _) =
            delete_account(&app, Some(&token), Some(json!({ "password": "a wrong guess" }))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    // Even the right password is refused while the lockout holds, and the
    // account is still there.
    let (status, body) = delete_account(&app, Some(&token), Some(json!({ "password": PASSWORD }))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], json!("too many attempts; try again later"));
    assert_eq!(
        login(&app, "lockedout", PASSWORD).await.0,
        StatusCode::TOO_MANY_REQUESTS,
        "the guesses counted against the login budget too"
    );
    assert_eq!(
        call(&app, "GET", "/api/auth/me", Some(&token), None)
            .await
            .0,
        StatusCode::OK
    );
}
