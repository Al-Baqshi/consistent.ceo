//! Own-auth HTTP endpoints (username + password), replacing Clerk.
//!
//! Public routes (`check-username`, `register`, `login`, `recover`) mint session
//! tokens and must sit OUTSIDE the bearer-auth layer. Protected routes (`me`,
//! `logout`, `change-password`, `DELETE account`) run inside it and read the
//! resolved [`AuthContext`].

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode, header},
    routing::{delete, get, post, put},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use webauthn_rs::prelude::{Passkey, PublicKeyCredential, RegisterPublicKeyCredential};

use crate::auth_store::AuthStore;
use crate::control_api::AuthContext;
use crate::state::ServerState;
use crate::user_profile::{
    ANSWER_MAX, OnboardingAnswers, UserProfile, WorkingStyle, classify_working_style, tidy_answer,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckUsernameRequest {
    username: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckUsernameResponse {
    available: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterRequest {
    username: String,
    password: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    /// Optional device label for the session (future authenticator support).
    #[serde(default)]
    device: Option<String>,
    /// Required when the deployment sets CONSISTENT_WORLD_INVITE_CODE.
    #[serde(default)]
    invite_code: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AuthTokenResponse {
    token: String,
    user_id: String,
    username: String,
    workspace_id: String,
    /// One-time recovery codes, present only on register.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recovery_codes: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginRequest {
    username: String,
    password: String,
    #[serde(default)]
    device: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoverRequest {
    username: String,
    code: String,
    #[serde(default)]
    device: Option<String>,
    /// Set a new password in the same step. This is the only way to set a
    /// password without knowing the old one, and it is what a "I forgot my
    /// password" screen posts — `change-password` now requires the current one,
    /// so recovery could not otherwise reach a usable account.
    #[serde(default)]
    new_password: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MeResponse {
    user_id: String,
    username: String,
    workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phone: Option<String>,
    /// Identity, intent, and working style. Always present (defaulted for
    /// accounts that predate profiles) so the client can gate first-run on
    /// `profile.onboardedAt` rather than on localStorage.
    profile: UserProfile,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangePasswordRequest {
    /// Proof that the caller is the account owner and not merely the holder of
    /// a stolen session token.
    current_password: String,
    new_password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChangePasswordResponse {
    /// How many other sessions were signed out. Surfacing the number is the
    /// point: it tells the user whether anyone else was in the account.
    revoked_sessions: usize,
}

/// Unauthenticated auth routes. Merged at the top level, NOT behind the bearer layer.
pub fn auth_public_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/auth/check-username", post(check_username))
        .route("/api/auth/register", post(register))
        .route("/api/auth/login", post(login))
        .route("/api/auth/recover", post(recover))
        // Device-approval login: a client starts a request and polls for the token.
        .route("/api/auth/approval/start", post(approval_start))
        .route("/api/auth/approval/{id}", get(approval_poll))
        // Passkey (WebAuthn) login.
        .route("/api/auth/passkey/login/start", post(passkey_login_start))
        .route("/api/auth/passkey/login/finish", post(passkey_login_finish))
        .with_state(state)
}

/// Authenticated auth routes. Merged into the protected tree (bearer required).
pub fn auth_protected_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/auth/me", get(me))
        // The user-level surface: identity + profile, and the first-run flow
        // that fills it in. `/api/me` is the canonical spelling; the older
        // `/api/auth/me` stays as an alias and now carries the profile too.
        .route("/api/me", get(me))
        .route("/api/me/profile", put(update_profile))
        .route("/api/me/onboarding", post(submit_onboarding))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/change-password", post(change_password))
        // First run on your own machine: trade the local token for a session,
        // and later give that account credentials of its own.
        .route("/api/auth/local-session", post(local_session))
        // Read the account's state, or delete it outright (re-authenticated).
        .route(
            "/api/auth/account",
            get(account_status).delete(delete_account),
        )
        .route("/api/auth/claim", post(claim_account))
        // Live sessions: see them, revoke one, or sign out everywhere.
        .route(
            "/api/auth/sessions",
            get(list_sessions).delete(revoke_all_sessions),
        )
        .route("/api/auth/sessions/{id}", delete(revoke_session))
        // Authenticator device management + login-approval (the phone approves).
        .route("/api/auth/devices", get(list_devices).post(register_device))
        .route("/api/auth/devices/{id}", delete(revoke_device))
        // Scoped API keys: how an external platform talks to this gateway.
        .route(
            "/api/auth/api-keys",
            get(list_api_keys).post(create_api_key),
        )
        .route("/api/auth/api-keys/{id}", delete(revoke_api_key))
        .route("/api/auth/approvals", get(list_approvals))
        .route("/api/auth/approvals/{id}", get(approval_detail))
        .route("/api/auth/approvals/{id}/approve", post(approve))
        .route("/api/auth/approvals/{id}/deny", post(deny))
        // Passkey (WebAuthn) registration (adds a passkey to your account).
        .route(
            "/api/auth/passkey/register/start",
            post(passkey_register_start),
        )
        .route(
            "/api/auth/passkey/register/finish",
            post(passkey_register_finish),
        )
        .with_state(state)
}

// Every handler below starts with this: `auth_store` is `None` on a
// deployment that never configured own-auth, and that reads as 503 rather
// than a panic.
fn store(state: &ServerState) -> Result<&Arc<AuthStore>, StatusCode> {
    state
        .auth_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// After minting an identity, make sure tenancy + Personal Space exist right away
/// (rather than lazily on the next authed request).
async fn ensure_bootstrapped(state: &ServerState, user_id: &str, workspace_id: &str) {
    if let Some(ts) = state.tenant_store.as_ref() {
        let _ = ts.ensure_tenant(user_id, workspace_id).await;
    }
    if let Some(control) = state.control_state.as_ref() {
        control.ensure_personal_space(user_id, workspace_id).await;
    }
}

/// Live "is this username free" check for a registration form.
async fn check_username(
    State(state): State<ServerState>,
    Json(body): Json<CheckUsernameRequest>,
) -> Result<Json<CheckUsernameResponse>, StatusCode> {
    let available = store(&state)?
        .username_available(&body.username)
        .await
        .unwrap_or(false);
    Ok(Json(CheckUsernameResponse { available }))
}

async fn register(
    State(state): State<ServerState>,
    Json(body): Json<RegisterRequest>,
) -> Result<Json<AuthTokenResponse>, StatusCode> {
    // Invite-gated signups: hosted deployments can set an invite code to keep
    // the trial-credit faucet closed to strangers. Unset (self-host) = open.
    if let Ok(required) = std::env::var("CONSISTENT_WORLD_INVITE_CODE")
        && !required.trim().is_empty()
        && body.invite_code.as_deref().map(str::trim) != Some(required.trim())
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let s = store(&state)?;
    let (user, codes) = s
        .register(&body.username, &body.password, body.email, body.phone)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "auth register failed");
            if e.to_string().contains("taken") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            }
        })?;
    ensure_bootstrapped(&state, &user.user_id, &user.workspace_id).await;
    let token = s
        .create_session(&user.user_id, &user.workspace_id, body.device)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(AuthTokenResponse {
        token,
        user_id: user.user_id,
        username: user.username,
        workspace_id: user.workspace_id,
        recovery_codes: codes,
    }))
}

async fn login(
    State(state): State<ServerState>,
    Json(body): Json<LoginRequest>,
) -> Result<Json<AuthTokenResponse>, StatusCode> {
    let s = store(&state)?;
    // Per-account lockout, on top of the per-IP limiter. IP throttling alone
    // does nothing against a distributed guess at one account's password.
    let throttle_key = crate::rate_limit::account_key("login", &body.username);
    crate::rate_limit::check_account(&throttle_key)?;
    let user = match s.login(&body.username, &body.password).await {
        Ok(user) => user,
        Err(_) => {
            crate::rate_limit::record_account_failure(&throttle_key);
            return Err(StatusCode::UNAUTHORIZED);
        }
    };
    crate::rate_limit::record_account_success(&throttle_key);
    ensure_bootstrapped(&state, &user.user_id, &user.workspace_id).await;
    let token = s
        .create_session(&user.user_id, &user.workspace_id, body.device)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(AuthTokenResponse {
        token,
        user_id: user.user_id,
        username: user.username,
        workspace_id: user.workspace_id,
        recovery_codes: Vec::new(),
    }))
}

/// Recover with a one-time backup code, optionally setting a new password in
/// the same step.
///
/// Either way this signs every existing session out: reaching for a recovery
/// code means the owner had lost their way into the account, so anything still
/// holding a token is suspect.
async fn recover(
    State(state): State<ServerState>,
    Json(body): Json<RecoverRequest>,
) -> Result<Json<AuthTokenResponse>, StatusCode> {
    let s = store(&state)?;
    let throttle_key = crate::rate_limit::account_key("recover", &body.username);
    crate::rate_limit::check_account(&throttle_key)?;

    let user = match body.new_password.as_deref() {
        Some(new_password) => {
            s.reset_password_with_code(&body.username, &body.code, new_password)
                .await
        }
        None => s.recover(&body.username, &body.code).await,
    };
    let user = match user {
        Ok(user) => user,
        Err(_) => {
            crate::rate_limit::record_account_failure(&throttle_key);
            return Err(StatusCode::UNAUTHORIZED);
        }
    };
    crate::rate_limit::record_account_success(&throttle_key);

    let revoked = s
        .revoke_all_sessions(&user.user_id, None)
        .await
        .unwrap_or(0);
    tracing::info!(
        user = %user.user_id,
        revoked_sessions = revoked,
        password_reset = body.new_password.is_some(),
        "account recovered with a backup code"
    );

    ensure_bootstrapped(&state, &user.user_id, &user.workspace_id).await;
    let token = s
        .create_session(&user.user_id, &user.workspace_id, body.device)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(AuthTokenResponse {
        token,
        user_id: user.user_id,
        username: user.username,
        workspace_id: user.workspace_id,
        recovery_codes: Vec::new(),
    }))
}

/// The caller's own identity + profile, keyed off the resolved [`AuthContext`].
async fn me(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<MeResponse>, StatusCode> {
    ensure_profile_owner(&state, &ctx).await?;
    let user = store(&state)?
        .get_auth_user(&ctx.principal_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(MeResponse {
        user_id: user.user_id,
        username: user.username,
        workspace_id: user.workspace_id,
        email: user.email,
        phone: user.phone,
        profile: user.profile,
    }))
}

/// Revoke the caller's own session. No token in the request is a no-op, not
/// an error — logging out with nothing to log out of still succeeds.
async fn logout(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    let s = store(&state)?;
    if let Some(token) = bearer_token(&headers) {
        let _ = s.revoke_session(&token).await;
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Change the caller's password.
///
/// Requires the current password, and signs every *other* session out on
/// success. The two go together: proving the old password is what stops a
/// stolen token from taking the account, and evicting the other sessions is
/// what ejects the thief who is already inside.
async fn change_password(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordRequest>,
) -> Result<Json<ChangePasswordResponse>, StatusCode> {
    let s = store(&state)?;
    s.change_password(
        &ctx.principal_id,
        &body.current_password,
        &body.new_password,
    )
    .await
    .map_err(|e| {
        tracing::info!(user = %ctx.principal_id, "password change refused: {e}");
        StatusCode::BAD_REQUEST
    })?;
    let revoked_sessions = s
        .revoke_all_sessions(&ctx.principal_id, bearer_token(&headers).as_deref())
        .await
        .unwrap_or(0);
    tracing::info!(
        user = %ctx.principal_id,
        revoked_sessions,
        "password changed; other sessions signed out"
    );
    Ok(Json(ChangePasswordResponse { revoked_sessions }))
}

// ===== Sessions ("where am I signed in", "sign out everywhere") =====

async fn list_sessions(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
) -> Result<Json<Vec<crate::auth_store::SessionInfo>>, StatusCode> {
    let sessions = store(&state)?
        .list_sessions(&ctx.principal_id, bearer_token(&headers).as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(sessions))
}

async fn revoke_session(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let ok = store(&state)?
        .revoke_session_by_id(&ctx.principal_id, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RevokeAllResponse {
    revoked_sessions: usize,
}

/// Sign out everywhere. Keeps the caller's own session — a control you can only
/// use once, by locking yourself out too, is a control nobody uses.
async fn revoke_all_sessions(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    headers: HeaderMap,
) -> Result<Json<RevokeAllResponse>, StatusCode> {
    let revoked_sessions = store(&state)?
        .revoke_all_sessions(&ctx.principal_id, bearer_token(&headers).as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tracing::info!(user = %ctx.principal_id, revoked_sessions, "signed out everywhere");
    Ok(Json(RevokeAllResponse { revoked_sessions }))
}

// ===== First run on your own machine =====

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRequest {
    username: String,
    password: String,
}

/// Turn "whoever holds this machine's token" into a real session.
///
/// The desktop app double-clicks into a working app; making someone fill in a
/// sign-in form to reach their own local data would be friction for nothing.
/// But the old way of achieving that — injecting the gateway's static token
/// into the webview — handed every page a permanent skeleton key. This trades
/// that token, once, for an ordinary revocable session.
///
/// Two gates, both necessary:
///
/// - **The caller must be a static principal.** They hold the machine's deploy
///   token, which already authenticates every `/api/*` route, so this grants no
///   access they did not have. It converts a machine credential into a user
///   session, which is strictly less dangerous than what it replaces. (This is
///   what [`PrincipalKind`] was added for: before it, a handler could not tell
///   the token holder from a signed-in person.)
/// - **The gateway must be bound to loopback.** On anything reachable from a
///   network the static token is an operator secret, and quietly turning it into
///   a user session would be a privilege translation nobody asked for.
///
/// The session is minted for the local-operator row, which is the *same*
/// `principal_id` the static path has always used — so every entity, mission,
/// and note already on this machine belongs to it. Nothing is stranded.
async fn local_session(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<AuthTokenResponse>, StatusCode> {
    if !ctx.is_static() {
        // A real user session asking for this is either a mistake or an
        // escalation attempt; either way the answer is no.
        return Err(StatusCode::FORBIDDEN);
    }
    if !loopback_only(&state) {
        tracing::warn!("refused a local-session request on a network-bound gateway");
        return Err(StatusCode::FORBIDDEN);
    }
    let s = store(&state)?;
    let user = s
        .ensure_local_operator(&ctx.principal_id, &ctx.workspace_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "local operator provisioning failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    ensure_bootstrapped(&state, &user.user_id, &user.workspace_id).await;
    let token = s
        .create_session(
            &user.user_id,
            &user.workspace_id,
            Some("This computer".into()),
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    tracing::info!(user = %user.user_id, "issued a local session to the machine owner");
    Ok(Json(AuthTokenResponse {
        token,
        user_id: user.user_id,
        username: user.username,
        workspace_id: user.workspace_id,
        recovery_codes: Vec::new(),
    }))
}

/// Whether this gateway is bound to loopback. Absent config reads as "not
/// loopback": a missing answer to a security question is a no.
fn loopback_only(state: &ServerState) -> bool {
    state
        .app_config
        .as_ref()
        .map(|cfg| crate::billing::is_loopback_bind(&cfg.gateway_bind))
        .unwrap_or(false)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountStatusResponse {
    /// False for a machine-owner account that has never been given credentials.
    /// While it is false, this account exists only on this computer.
    has_password: bool,
    username: String,
}

async fn account_status(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<AccountStatusResponse>, StatusCode> {
    let s = store(&state)?;
    ensure_profile_owner(&state, &ctx).await?;
    let user = s
        .get_auth_user(&ctx.principal_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let has_password = s
        .has_password(&ctx.principal_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(AccountStatusResponse {
        has_password,
        username: user.username,
    }))
}

/// Give this machine's account a username and password, so it can be reached
/// from a phone or another browser. Keeps the same account — and therefore all
/// the work already in it — rather than starting a second one.
async fn claim_account(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<ClaimRequest>,
) -> Result<Json<ClaimResponse>, StatusCode> {
    let (user, recovery_codes) = store(&state)?
        .claim_local_account(&ctx.principal_id, &body.username, &body.password)
        .await
        .map_err(|e| {
            tracing::info!(user = %ctx.principal_id, "claim refused: {e}");
            if e.to_string().contains("taken") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            }
        })?;
    Ok(Json(ClaimResponse {
        username: user.username,
        recovery_codes,
    }))
}

/// No token: the caller's existing session is still perfectly good, and minting
/// a second one here would only invite the client to throw the first away.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimResponse {
    username: String,
    /// Shown exactly once. The account could not be locked out of before it had
    /// a password; now it can.
    recovery_codes: Vec<String>,
}

// ===== Account deletion (App Store guideline 5.1.1(v)) =====

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteAccountRequest {
    /// The account's current password. A session token alone must not be
    /// enough to destroy an account, for the same reason it is not enough to
    /// change its password: a stolen token is not ownership.
    password: String,
}

/// `{ "error": "<message>" }`, the body of every refusal from
/// [`delete_account`]. The rest of this file answers with bare status codes,
/// but an irreversible action owes the client a reason it can show.
#[derive(Serialize)]
struct AccountError {
    error: String,
}

type AccountErr = (StatusCode, Json<AccountError>);

fn account_err(code: StatusCode, message: &str) -> AccountErr {
    (
        code,
        Json(AccountError {
            error: message.to_string(),
        }),
    )
}

/// The refusal for every caller that is not a deletable account.
const NO_ACCOUNT_TO_DELETE: &str = "no account to delete";

/// `DELETE /api/auth/account`: permanently delete the caller's own account.
///
/// Body: `{ "password": "<current password>" }`. Answers `204 No Content` on
/// success; every refusal carries `{ "error": "<message>" }`.
///
/// # Who can
///
/// Only a signed-in account ([`PrincipalKind::User`]), proving it with the
/// current password, checked with the same Argon2 verification login and
/// [`change_password`] use. Wrong guesses count against the same per-account
/// lockout as `login`, keyed on the same username bucket, so a stolen session
/// token gains no guesses by trying them here instead. A wrong password is a
/// `403` rather than a `401` so a client can tell "try again" from "you were
/// signed out"; nothing is deleted and the session keeps working.
///
/// Everyone else gets `400 no account to delete`: the static deploy token (and
/// its scoped `secret.workspace.principal` form), the open `anonymous`
/// principal, and the machine owner's account (`bearer`), even when it has
/// been claimed with a password. That account *is* this machine: every entity
/// and mission on it is keyed to it, and the next request from the desktop
/// would provision it again. Removing it means removing the install (below).
///
/// # What is deleted
///
/// - The account row: username (released for someone else to register),
///   password hash, email, phone, recovery-code hashes, and the profile
///   (display name, role, intent, working style, onboarding answers).
/// - Every session, including the one making this request, and every API key.
/// - Trusted authenticator devices, passkeys, and sign-in approval requests
///   bound to the account. All of the above go in one transaction
///   ([`AuthStore::delete_user`]).
/// - The personal credential vault, `~/.consistent-world/vaults/<user_id>.json`,
///   plus any `<user_id>.v<N>.bak` copies a format upgrade left beside it. An
///   unlocked handle is locked first, which zeroizes its keys. This runs
///   *before* the account rows go, so a disk error answers `500` with the
///   account intact and the request can be retried.
/// - Companion push registrations (Expo push tokens) in the account's own
///   workspace — the one registration minted, whose id is the user id — so the
///   phone stops being notified. Registrations in any other workspace are left.
///
/// # What is not
///
/// - The workspace and its contents: entities (the Personal Space included),
///   missions, tasks, notes, entity documents, records and uploaded files,
///   heartbeats, director lessons, canvas, feed events, and approval-queue
///   items. Those rows are scoped to a workspace other members may share, and
///   carry no owner this handler could use to tell personal from shared.
///   Nothing pauses that workspace's scheduled work either.
/// - Chat sessions and conversation memory, which the memory store keys by
///   session and workspace, not by account.
/// - Files on disk outside the vault: entity workspace directories, agent run
///   output, and anything an agent wrote there.
/// - Tenancy bookkeeping (the `users` and `workspaces` rows in `tenant_store`)
///   and the workspace's credit balance. Neither store exposes a removal call.
/// - Log lines that recorded the user id, and backups or exports the user took.
///
/// A self-hosted owner can remove everything — every account, workspace,
/// entity, vault, and log on the machine — by stopping the gateway and deleting
/// `~/.consistent-world`.
///
/// # Responses
///
/// | Status | `error` | When |
/// |---|---|---|
/// | 204 | — | Deleted. The bearer token no longer authenticates. |
/// | 400 | `no account to delete` | Not a deletable account (above), or its row is already gone. |
/// | 400 | `password is required` | Missing or malformed JSON body. |
/// | 400 | `this account has no password to confirm deletion with` | An external-identity account. |
/// | 403 | `password is incorrect` | Wrong password. Nothing deleted. |
/// | 429 | `too many attempts; try again later` | The per-account lockout is in effect. |
/// | 500 | (varies) | Storage failure. |
/// | 503 | `accounts are not enabled on this gateway` | No auth store configured. |
///
/// A missing or rejected bearer token is the middleware's plain `401`, as on
/// every protected route.
///
/// [`PrincipalKind::User`]: crate::control_api::PrincipalKind::User
async fn delete_account(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    body: Result<Json<DeleteAccountRequest>, JsonRejection>,
) -> Result<StatusCode, AccountErr> {
    let s = store(&state)
        .map_err(|code| account_err(code, "accounts are not enabled on this gateway"))?;
    // Who first, then the body: a static token has nothing to delete whatever
    // it sends.
    if ctx.is_static() || is_local_operator(&ctx.principal_id) {
        return Err(account_err(StatusCode::BAD_REQUEST, NO_ACCOUNT_TO_DELETE));
    }
    let Ok(Json(body)) = body else {
        return Err(account_err(
            StatusCode::BAD_REQUEST,
            "password is required",
        ));
    };
    let user = s
        .get_auth_user(&ctx.principal_id)
        .await
        .ok_or_else(|| account_err(StatusCode::BAD_REQUEST, NO_ACCOUNT_TO_DELETE))?;
    let has_password = s.has_password(&user.user_id).await.map_err(|e| {
        tracing::warn!(error = %e, "account deletion: could not read the account");
        account_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not read the account",
        )
    })?;
    if !has_password {
        // An external identity: its issuer, not a password, proves ownership,
        // so there is nothing here to re-authenticate with.
        return Err(account_err(
            StatusCode::BAD_REQUEST,
            "this account has no password to confirm deletion with",
        ));
    }

    // The login bucket, not one of its own: a password guess is a password
    // guess whichever endpoint it arrives through.
    let throttle_key = crate::rate_limit::account_key("login", &user.username);
    crate::rate_limit::check_account(&throttle_key)
        .map_err(|code| account_err(code, "too many attempts; try again later"))?;
    let verified = s
        .verify_password(&user.user_id, &body.password)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "account deletion: password check failed");
            account_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not check the password",
            )
        })?;
    if !verified {
        crate::rate_limit::record_account_failure(&throttle_key);
        tracing::info!(user = %user.user_id, "account deletion refused: wrong password");
        return Err(account_err(StatusCode::FORBIDDEN, "password is incorrect"));
    }
    crate::rate_limit::record_account_success(&throttle_key);

    // The vault goes first: it is the step that can fail on disk, and failing
    // here leaves the account whole for a retry rather than half-deleted.
    let vault_files = match state.vault_manager.as_ref() {
        Some(manager) => remove_vault_files(manager, &user.user_id).map_err(|e| {
            tracing::warn!(error = %e, user = %user.user_id, "account deletion: vault removal failed");
            account_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not remove the credential vault; the account was not deleted",
            )
        })?,
        None => 0,
    };
    let push_devices = forget_push_devices(&state, &user).await;
    let deleted = s.delete_user(&user.user_id).await.map_err(|e| {
        tracing::warn!(error = %e, user = %user.user_id, "account deletion failed");
        account_err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "account deletion failed; try again",
        )
    })?;
    // Counts and the id only; nothing the account contained.
    tracing::info!(
        user = %user.user_id,
        sessions = deleted.sessions,
        api_keys = deleted.api_keys,
        devices = deleted.devices,
        passkeys = deleted.passkeys,
        approvals = deleted.approvals,
        vault_files,
        push_devices,
        "account deleted at its owner's request"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a principal's credential vault from disk: the vault file, and any
/// `<stem>.v<N>.bak` copies a format upgrade left beside it. Returns how many
/// files went; a vault that was never created is zero, not an error.
///
/// The path comes from the manager's own handle, so it is sanitised exactly as
/// every other vault path is. Sanitised ids never contain `.`, which is what
/// makes the `<stem>.v` prefix this principal's alone. The handle is locked
/// first so no key material outlives the file.
fn remove_vault_files(
    manager: &crate::vault_manager::VaultManager,
    principal_id: &str,
) -> std::io::Result<usize> {
    use std::io::ErrorKind;

    let vault = manager.handle(principal_id);
    vault.lock();
    let path = vault.path().to_path_buf();
    let mut removed = 0;
    match std::fs::remove_file(&path) {
        Ok(()) => removed += 1,
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().and_then(|s| s.to_str())) else {
        return Ok(removed);
    };
    let backup_prefix = format!("{stem}.v");
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(removed),
        Err(e) => return Err(e),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(&backup_prefix) && name.ends_with(".bak") {
            std::fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

/// Remove the companion push registrations in the account's own workspace, so
/// a deleted account's phone stops being notified. Returns how many went.
///
/// Only when the workspace id is the user id, which is the single-user
/// workspace registration mints: there, every registration came from this
/// account's own sessions. A workspace that could be shared is left alone.
/// Best-effort — a failure is logged and the deletion carries on, because the
/// account rows going is what the user asked for.
async fn forget_push_devices(state: &ServerState, user: &crate::auth_store::AuthUser) -> usize {
    if user.workspace_id != user.user_id {
        return 0;
    }
    let Some(devices) = state
        .control_state
        .as_ref()
        .and_then(|control| control.device_store.get())
    else {
        return 0;
    };
    let registered = match devices.list_by_workspace(&user.workspace_id).await {
        Ok(registered) => registered,
        Err(e) => {
            tracing::warn!(error = %e, "account deletion: could not list push devices");
            return 0;
        }
    };
    let mut removed = 0;
    for device in registered {
        match devices.delete(&device.id).await {
            Ok(()) => removed += 1,
            Err(e) => tracing::warn!(error = %e, "account deletion: push device not removed"),
        }
    }
    removed
}

// ===== User profile + first-run onboarding =====
//
// The first thing a user ever sees is a three-question conversation. It records
// who they are and what they want, and infers *how they work* — holistic (many
// things in motion) or linear (one thing until it's done). That single bit then
// reshapes the environment client-side: a linear user gets a focused nav and
// lands on `/today`; a holistic user gets everything.
//
// Every handler here is scoped to `ctx.principal_id`. Answer contents are
// personal data and are never logged.

/// Partial update of the caller's profile. Absent fields are left alone, so the
/// settings page can flip working style without resending everything.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct UpdateProfileRequest {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    primary_intent: Option<String>,
    /// `"holistic"` | `"linear"` | `"unset"`. Anything else is a 400 rather
    /// than a silent coercion — a typo must not shrink someone's sidebar.
    #[serde(default)]
    working_style: Option<String>,
    /// The escape hatch: show the full nav even in the focused environment.
    #[serde(default)]
    show_all_nav: Option<bool>,
    /// Records a neutral default so first-run stops asking. This is what "Skip"
    /// sends: onboarded, working style untouched (i.e. `unset` = show all).
    #[serde(default)]
    mark_onboarded: Option<bool>,
}

/// Single submit of the whole first-run flow. Three questions is not enough to
/// justify a server-side session state machine — the client collects the
/// answers and posts them once.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct OnboardingRequest {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    answers: OnboardingAnswers,
}

/// Principals that token / no-auth deployments synthesise instead of signing a
/// real account in. They stand for "whoever holds the gateway token", i.e. the
/// operator of this machine, and are the only ids we will materialise an
/// account for on demand. Everything else must have registered.
fn is_local_operator(principal_id: &str) -> bool {
    matches!(principal_id, "bearer" | "anonymous")
}

/// Make sure the caller has a row to hang a profile off. A no-op for real
/// accounts; for the synthetic local operator it provisions one, without which
/// `/api/me` 404s and the first-run flow never renders.
async fn ensure_profile_owner(state: &ServerState, ctx: &AuthContext) -> Result<(), StatusCode> {
    if !is_local_operator(&ctx.principal_id) {
        return Ok(());
    }
    store(state)?
        .ensure_local_operator(&ctx.principal_id, &ctx.workspace_id)
        .await
        .map(|_| ())
        .map_err(|e| {
            tracing::warn!(error = %e, "local operator provisioning failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn load_profile(state: &ServerState, user_id: &str) -> Result<UserProfile, StatusCode> {
    store(state)?
        .get_user_profile(user_id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)
}

async fn save_profile(
    state: &ServerState,
    user_id: &str,
    profile: &UserProfile,
) -> Result<(), StatusCode> {
    store(state)?
        .set_user_profile(user_id, profile)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "profile write failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

async fn update_profile(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<UpdateProfileRequest>,
) -> Result<Json<UserProfile>, StatusCode> {
    ensure_profile_owner(&state, &ctx).await?;
    let mut profile = load_profile(&state, &ctx.principal_id).await?;

    if let Some(style) = body.working_style.as_deref() {
        profile.working_style = WorkingStyle::parse(style).ok_or(StatusCode::BAD_REQUEST)?;
    }
    if let Some(name) = body.display_name.as_deref() {
        profile.display_name = tidy_answer(name, ANSWER_MAX);
    }
    if let Some(role) = body.role.as_deref() {
        profile.role = tidy_answer(role, ANSWER_MAX);
    }
    if let Some(intent) = body.primary_intent.as_deref() {
        profile.primary_intent = tidy_answer(intent, ANSWER_MAX);
    }
    if let Some(show_all) = body.show_all_nav {
        profile.show_all_nav = show_all;
    }
    // Idempotent: the first mark wins, so a later settings edit can't rewrite
    // when the user actually started.
    if body.mark_onboarded == Some(true) && !profile.is_onboarded() {
        profile.onboarded_at = Some(chrono::Utc::now().to_rfc3339());
    }

    save_profile(&state, &ctx.principal_id, &profile).await?;
    Ok(Json(profile))
}

async fn submit_onboarding(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<OnboardingRequest>,
) -> Result<Json<UserProfile>, StatusCode> {
    ensure_profile_owner(&state, &ctx).await?;
    let mut profile = load_profile(&state, &ctx.principal_id).await?;

    let answers = OnboardingAnswers {
        identity: body
            .answers
            .identity
            .as_deref()
            .and_then(|s| tidy_answer(s, ANSWER_MAX)),
        intent: body
            .answers
            .intent
            .as_deref()
            .and_then(|s| tidy_answer(s, ANSWER_MAX)),
        focus: body
            .answers
            .focus
            .as_deref()
            .and_then(|s| tidy_answer(s, ANSWER_MAX)),
        focus_choice: body
            .answers
            .focus_choice
            .as_deref()
            // Keep only a choice we actually understand, so the stored answer
            // and the classification can never disagree.
            .and_then(WorkingStyle::parse)
            .filter(|s| *s != WorkingStyle::Unset)
            .map(|s| s.as_str().to_string()),
        autonomy: body
            .answers
            .autonomy
            .as_deref()
            .and_then(|s| tidy_answer(s, 64)),
    };

    // Deterministic, offline, instant. No LLM on the first screen a user sees.
    profile.working_style = classify_working_style(&answers);
    profile.role = answers.identity.clone().or(profile.role);
    profile.primary_intent = answers.intent.clone().or(profile.primary_intent);
    if let Some(name) = body.display_name.as_deref() {
        profile.display_name = tidy_answer(name, ANSWER_MAX).or(profile.display_name);
    }
    profile.answers = answers;
    if !profile.is_onboarded() {
        profile.onboarded_at = Some(chrono::Utc::now().to_rfc3339());
    }

    save_profile(&state, &ctx.principal_id, &profile).await?;

    // Give the AI grounding from minute one: the Personal Space entity's own
    // narrative gets seeded from answers 1 and 2. Best-effort — a failure here
    // must not lose the profile we just wrote.
    if let Some(control) = state.control_state.as_ref() {
        crate::user_profile::seed_personal_space_narrative(
            control,
            &ctx.principal_id,
            &ctx.workspace_id,
            &profile.answers,
        )
        .await;
    }

    // Style only — never the answers themselves.
    tracing::info!(
        user = %ctx.principal_id,
        working_style = profile.working_style.as_str(),
        "user onboarding recorded"
    );
    Ok(Json(profile))
}

// ===== Scoped API keys (inbound integrations) =====

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct CreateApiKeyRequest {
    /// Human label, e.g. "Zapier" or "ci". Defaults to "API key".
    #[serde(default)]
    name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateApiKeyResponse {
    id: String,
    name: String,
    created_at: String,
    expires_at: String,
    /// Shown exactly once — it is not recoverable from any later read.
    token: String,
}

/// Mint a long-lived key bound to the caller's user + workspace. The raw token
/// is in this response and nowhere else, ever.
async fn create_api_key(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    body: Option<Json<CreateApiKeyRequest>>,
) -> Result<Json<CreateApiKeyResponse>, StatusCode> {
    let name = body.and_then(|Json(b)| b.name);
    let (info, token) = store(&state)?
        .create_api_key(&ctx.principal_id, &ctx.workspace_id, name.as_deref())
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "api key creation failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    tracing::info!(user = %ctx.principal_id, key_id = %info.id, "api key created");
    Ok(Json(CreateApiKeyResponse {
        id: info.id,
        name: info.name,
        created_at: info.created_at,
        expires_at: info.expires_at,
        token,
    }))
}

async fn list_api_keys(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<crate::auth_store::ApiKeyInfo>>, StatusCode> {
    let keys = store(&state)?
        .list_api_keys(&ctx.principal_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(keys))
}

async fn revoke_api_key(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let ok = store(&state)?
        .revoke_api_key(&ctx.principal_id, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// Pull the raw token out of `Authorization: Bearer <token>`, or `None` if the
/// header is absent, malformed, or empty.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

// ===== Device-approval login =====
//
// Two flows share this machinery. "My phone approves my laptop" names an
// account up front. The desktop-to-browser login doesn't: the app opens the
// system browser at `/login?approval=<id>`, the user signs in there and types
// the code shown in the app, and the app collects the session by polling with
// its PKCE verifier. Everything stays on localhost.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalStartRequest {
    /// Omitted by the browser flow, where the app doesn't know who is about to
    /// sign in — whoever authenticates and types the code claims the request.
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    context: Option<String>,
    /// PKCE `S256` challenge: base64url(SHA-256(verifier)).
    #[serde(default)]
    code_challenge: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ApprovalPollQuery {
    /// The PKCE verifier. Required when the request was started with a
    /// challenge, which the desktop flow always does.
    #[serde(default)]
    verifier: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalStartResponse {
    request_id: String,
    code: String,
    expires_at: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApprovalPollResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RegisterDeviceRequest {
    label: String,
    #[serde(default)]
    platform: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RegisterDeviceResponse {
    device_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApproveRequest {
    /// The six digits shown on the requesting client. Required: it is what ties
    /// this approval to the request the user is actually looking at.
    code: String,
    #[serde(default)]
    device: Option<String>,
}

/// Map a store-level outcome onto a status code the approving client can act on.
fn approval_status(outcome: crate::auth_store::ApprovalOutcome) -> Result<StatusCode, StatusCode> {
    use crate::auth_store::ApprovalOutcome as O;
    match outcome {
        O::Ok => Ok(StatusCode::NO_CONTENT),
        O::NotFound => Err(StatusCode::NOT_FOUND),
        O::NotPending => Err(StatusCode::CONFLICT),
        O::Expired => Err(StatusCode::GONE),
        O::BadCode => Err(StatusCode::FORBIDDEN),
        O::TooManyAttempts => Err(StatusCode::TOO_MANY_REQUESTS),
    }
}

/// Public: a client (desktop/web) starts a login for someone to approve.
async fn approval_start(
    State(state): State<ServerState>,
    Json(body): Json<ApprovalStartRequest>,
) -> Result<Json<ApprovalStartResponse>, StatusCode> {
    let start = store(&state)?
        .start_approval(body.username.as_deref(), body.context, body.code_challenge)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ApprovalStartResponse {
        request_id: start.request_id,
        code: start.code,
        expires_at: start.expires_at,
    }))
}

/// Public: the requesting client polls; the session token is returned once
/// approved, and only to a caller holding the PKCE verifier.
async fn approval_poll(
    State(state): State<ServerState>,
    Path(id): Path<String>,
    Query(q): Query<ApprovalPollQuery>,
) -> Result<Json<ApprovalPollResponse>, StatusCode> {
    let (status, token) = store(&state)?
        .poll_approval(&id, q.verifier.as_deref())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ApprovalPollResponse { status, token }))
}

/// What the approving browser shows before the user confirms: which app is
/// asking, and whether the request is still live. No code — that is read off
/// the requesting device and typed in, which is what ties the two together.
async fn approval_detail(
    State(state): State<ServerState>,
    Path(id): Path<String>,
) -> Result<Json<crate::auth_store::ApprovalDetail>, StatusCode> {
    store(&state)?
        .approval_detail(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn register_device(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<RegisterDeviceRequest>,
) -> Result<Json<RegisterDeviceResponse>, StatusCode> {
    let platform = body.platform.unwrap_or_else(|| "unknown".to_string());
    let device_id = store(&state)?
        .register_device(&ctx.principal_id, &body.label, &platform)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(RegisterDeviceResponse { device_id }))
}

async fn list_devices(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<crate::auth_store::DeviceInfo>>, StatusCode> {
    let devices = store(&state)?
        .list_devices(&ctx.principal_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(devices))
}

async fn revoke_device(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let ok = store(&state)?
        .revoke_device(&ctx.principal_id, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if ok {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

async fn list_approvals(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<crate::auth_store::PendingApproval>>, StatusCode> {
    let list = store(&state)?
        .list_pending_approvals(&ctx.principal_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(list))
}

async fn approve(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<ApproveRequest>,
) -> Result<StatusCode, StatusCode> {
    let outcome = store(&state)?
        .approve_approval(&id, &ctx.principal_id, &body.code, body.device)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    approval_status(outcome)
}

async fn deny(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let outcome = store(&state)?
        .deny_approval(&id, &ctx.principal_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    approval_status(outcome)
}

// ===== Passkeys (WebAuthn) =====

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PasskeyChallengeResponse {
    /// The raw WebAuthn challenge (CreationChallengeResponse or
    /// RequestChallengeResponse) to hand to the platform authenticator.
    challenge: serde_json::Value,
    state_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasskeyRegisterFinishRequest {
    state_id: String,
    credential: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasskeyLoginStartRequest {
    username: String,
}

/// No username. The account is whichever one the challenge was issued for, and
/// it is read from the ceremony state — see [`passkey_login_finish`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PasskeyLoginFinishRequest {
    state_id: String,
    credential: serde_json::Value,
}

// Mirrors `store` above: no passkey service configured reads as 503, not a
// missing-route 404.
fn passkey_svc(state: &ServerState) -> Result<&Arc<crate::passkey::PasskeyService>, StatusCode> {
    state
        .passkey
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

fn parse_passkeys(json: &[String]) -> Vec<Passkey> {
    json.iter()
        .filter_map(|j| serde_json::from_str(j).ok())
        .collect()
}

/// Begin adding a passkey to the caller's own account. Existing passkeys are
/// excluded from the challenge so the same authenticator can't be re-enrolled.
async fn passkey_register_start(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<PasskeyChallengeResponse>, StatusCode> {
    let s = store(&state)?;
    let pk = passkey_svc(&state)?;
    let user = s
        .get_auth_user(&ctx.principal_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    let existing = parse_passkeys(
        &s.list_passkey_json(&ctx.principal_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );
    let (ccr, state_id) = pk
        .start_register(&user.user_id, &user.username, &existing)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let challenge = serde_json::to_value(ccr).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(PasskeyChallengeResponse {
        challenge,
        state_id,
    }))
}

/// Verify the ceremony from `passkey_register_start` and persist the
/// resulting credential to the caller's account.
async fn passkey_register_finish(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<PasskeyRegisterFinishRequest>,
) -> Result<StatusCode, StatusCode> {
    let s = store(&state)?;
    let pk = passkey_svc(&state)?;
    let cred: RegisterPublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| StatusCode::BAD_REQUEST)?;
    let passkey = pk
        .finish_register(&body.state_id, &cred)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let pj = serde_json::to_string(&passkey).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    s.add_passkey(&ctx.principal_id, &pj)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Public: begin a passkey login for a known username. 404s if the account
/// has no passkeys, since there is nothing a challenge could be issued against.
async fn passkey_login_start(
    State(state): State<ServerState>,
    Json(body): Json<PasskeyLoginStartRequest>,
) -> Result<Json<PasskeyChallengeResponse>, StatusCode> {
    let s = store(&state)?;
    let pk = passkey_svc(&state)?;
    let user_id = s
        .user_id_for_username(&body.username)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let passkeys = parse_passkeys(
        &s.list_passkey_json(&user_id)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    );
    if passkeys.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    // The user id travels into the ceremony state; the finish step reads it back
    // from there rather than trusting the request body.
    let (rcr, state_id) = pk
        .start_auth(&user_id, &passkeys)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let challenge = serde_json::to_value(rcr).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(PasskeyChallengeResponse {
        challenge,
        state_id,
    }))
}

/// Finish a passkey login.
///
/// Two bindings make this safe, and both were missing:
///
/// 1. The account comes from the ceremony state, not from the request. A
///    client-supplied username meant an attacker could sign their *own*
///    challenge with their *own* passkey and ask for a session as anyone.
/// 2. The asserted credential must be one of that user's stored credentials.
///    `webauthn-rs` verifies the assertion against the credential list embedded
///    in the ceremony, so this is belt-and-braces — but it is the check that
///    keeps the guarantee local and legible rather than implied by a dependency.
async fn passkey_login_finish(
    State(state): State<ServerState>,
    Json(body): Json<PasskeyLoginFinishRequest>,
) -> Result<Json<AuthTokenResponse>, StatusCode> {
    let s = store(&state)?;
    let pk = passkey_svc(&state)?;
    let cred: PublicKeyCredential =
        serde_json::from_value(body.credential).map_err(|_| StatusCode::BAD_REQUEST)?;
    let (user_id, result) = pk
        .finish_auth(&body.state_id, &cred)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    let stored = s
        .list_passkeys(&user_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let matched = stored.iter().find_map(|row| {
        let mut pk: Passkey = serde_json::from_str(&row.passkey_json).ok()?;
        (pk.cred_id() == result.cred_id()).then(|| {
            // Persist the sign counter (and backup-state flags) so a cloned
            // authenticator can actually be detected on a later assertion.
            let changed = pk.update_credential(&result).unwrap_or(false);
            (row.passkey_id.clone(), pk, changed)
        })
    });
    let Some((passkey_id, updated, changed)) = matched else {
        tracing::warn!(
            user = %user_id,
            "passkey assertion verified but its credential is not registered to that user"
        );
        return Err(StatusCode::UNAUTHORIZED);
    };
    if changed
        && let Ok(json) = serde_json::to_string(&updated)
        && let Err(e) = s.update_passkey(&passkey_id, &json).await
    {
        tracing::warn!(error = %e, "passkey counter write-back failed");
    }

    let user = s
        .get_auth_user(&user_id)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    ensure_bootstrapped(&state, &user.user_id, &user.workspace_id).await;
    let token = s
        .create_session(&user.user_id, &user.workspace_id, None)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(AuthTokenResponse {
        token,
        user_id: user.user_id,
        username: user.username,
        workspace_id: user.workspace_id,
        recovery_codes: Vec::new(),
    }))
}
