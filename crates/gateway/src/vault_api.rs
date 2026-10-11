//! Vault HTTP endpoints: password manager, per-entity environments, and the
//! several ways of opening the vault.
//!
//! Every route sits inside the bearer-auth layer and operates on the calling
//! principal's own vault ([`crate::vault_manager::VaultManager`]). There is no
//! route that names another user's vault, so one account can never read
//! another's — sharing goes through [`create_share`]/[`accept_share`], where the
//! payload is sealed to the recipient's public key and the gateway only relays
//! ciphertext.
//!
//! # Trust model — read this before extending
//!
//! This is **not** Bitwarden's zero-knowledge model, and it cannot be. An
//! unlocked vault is readable by the gateway process, because the entire point is
//! that the gateway injects those credentials into the agents it runs. A design
//! where only the browser could decrypt would mean no entity could ever use a
//! secret unattended.
//!
//! So the guarantees on offer are deliberately different ones:
//!
//! - **Encrypted at rest.** A stolen disk, backup or container image yields
//!   nothing without an unlock credential.
//! - **A bounded window.** Vaults auto-lock on idle, and locking wipes the key.
//! - **Per-entity isolation.** One entity's agents resolve one entity's secrets.
//! - **No single point of loss.** Passkeys, device keys and recovery codes are
//!   all independent doors onto the same key.
//! - **Ciphertext-only sharing.** Shares transit the gateway sealed.
//!
//! What follows from that: unlock material (a master password, a PRF output, a
//! device key) does travel to the gateway over TLS, and the gateway holds the
//! derived key in memory while unlocked. Treat the gateway host as being exactly
//! as sensitive as the vault itself, because it is.
//!
//! # Namespaces
//!
//! - **Items** — human logins, cards, notes. Listing returns metadata only; the
//!   secret half requires a second call, so a UI can render a list without ever
//!   pulling every password over the wire.
//! - **Secrets** — the `KEY=value` space, scoped. `?scope=` selects it and
//!   defaults to `global`, so every pre-existing caller keeps working unchanged.

// `ApiErr` carries a `HeaderMap` so a throttled unlock can send `Retry-After`,
// which makes it big enough to trip `result_large_err` (deny-level under CI's
// `-D warnings`). It is the per-request rejection path — never hot — and
// boxing it would cost axum's `IntoResponse` for the tuple, so allow it here.
#![allow(clippy::result_large_err)]

use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::RETRY_AFTER},
    routing::{delete, get, post, put},
};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use consistent_world_credentials::crypto::{KEY_LEN, SecretKey};
use consistent_world_credentials::generator::{
    PasswordSpec, generate_passphrase, generate_password,
};
use consistent_world_credentials::model::{
    AuditEntry, ItemKind, ItemSecret, NewItem, ShareEnvelope, VaultExport,
};
use consistent_world_credentials::recovery::random_key;
use consistent_world_credentials::scope::Scope;
use consistent_world_credentials::vault::{UnlockKind, UnlockMethodInfo, Vault};

use crate::control_api::AuthContext;
use crate::state::ServerState;
use crate::vault_manager::VaultManager;

/// Wire every vault endpoint into the router. Grouped by comment into the
/// same namespaces the module header describes.
pub fn vault_routes(state: ServerState) -> Router {
    Router::new()
        // lifecycle
        .route("/api/vault/status", get(status))
        .route("/api/vault/init", post(init))
        .route("/api/vault/unlock", post(unlock))
        .route("/api/vault/unlock/key", post(unlock_with_key))
        .route("/api/vault/unlock/recovery", post(unlock_with_recovery))
        .route("/api/vault/lock", post(lock))
        .route("/api/vault/change-password", post(change_password))
        // ways in
        .route(
            "/api/vault/unlock-methods",
            get(list_unlock_methods).post(add_unlock_method),
        )
        .route(
            "/api/vault/unlock-methods/{id}",
            delete(remove_unlock_method),
        )
        .route("/api/vault/random-key", get(random_key_bytes))
        .route("/api/vault/recovery-code", post(new_recovery_code))
        // items
        .route("/api/vault/items", get(list_items).post(create_item))
        .route(
            "/api/vault/items/{id}",
            get(get_item_secret).put(update_item).delete(delete_item),
        )
        .route("/api/vault/items/{id}/metadata", put(update_item_metadata))
        .route("/api/vault/items/{id}/scope", put(move_item))
        .route("/api/vault/items/{id}/totp", get(item_totp))
        .route("/api/vault/item-scopes", get(item_scopes))
        // migration + setup helpers
        .route("/api/vault/import/csv", post(import_csv))
        .route("/api/vault/password-strength", post(password_strength))
        // secrets / environments
        .route("/api/vault/secrets", get(list_secrets).post(set_secret))
        .route(
            "/api/vault/secrets/{key}",
            get(get_secret).delete(delete_secret),
        )
        .route("/api/vault/scopes", get(list_scopes))
        .route("/api/vault/env", get(resolved_env))
        // durability
        .route("/api/vault/export", post(export))
        .route("/api/vault/import", post(import))
        .route("/api/vault/audit", get(audit))
        .route("/api/vault/health", get(health))
        .route("/api/vault/breach-check", get(breach_check))
        // tools
        .route("/api/vault/generate", post(generate))
        // sharing
        .route("/api/vault/public-key", get(public_key))
        .route("/api/vault/share", post(create_share))
        .route("/api/vault/share/accept", post(accept_share))
        .with_state(state)
}

// A vault-configured deployment always has this set; a bare 503 here means
// the gateway was started without vault support at all.
fn manager(state: &ServerState) -> Result<&Arc<VaultManager>, ApiErr> {
    state
        .vault_manager
        .as_ref()
        .ok_or_else(|| err(StatusCode::SERVICE_UNAVAILABLE, "vault is not configured"))
}

/// Resolve the caller's unlocked vault.
///
/// `423 Locked` is deliberate and load-bearing: it is the signal the web, mobile
/// and desktop clients use to show the unlock prompt, and it is distinct from
/// `401`, which would mean the *session* expired.
fn unlocked(state: &ServerState, ctx: &AuthContext) -> Result<Arc<Vault>, ApiErr> {
    manager(state)?
        .unlocked(&ctx.principal_id)
        .ok_or_else(|| err(StatusCode::LOCKED, "vault is locked"))
}

/// [`unlocked`] for reads a client repeats on a timer, such as a live TOTP code.
///
/// Same `423` contract, but it does not reset the idle clock: a poll is not a
/// person using the vault, and counting it as one would let an open tab defeat
/// the auto-lock. See [`VaultManager::peek_unlocked`].
fn unlocked_passive(state: &ServerState, ctx: &AuthContext) -> Result<Arc<Vault>, ApiErr> {
    manager(state)?
        .peek_unlocked(&ctx.principal_id)
        .ok_or_else(|| err(StatusCode::LOCKED, "vault is locked"))
}

/// Status, extra headers (only `Retry-After` today, on a 429), and the JSON body.
type ApiErr = (StatusCode, HeaderMap, Json<ApiError>);

/// Uniform error body for every vault endpoint.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub error: String,
}

fn err(code: StatusCode, message: impl Into<String>) -> ApiErr {
    (
        code,
        HeaderMap::new(),
        Json(ApiError {
            error: message.into(),
        }),
    )
}

/// Map a vault error to a response.
///
/// Anything that is not a recognised precondition becomes a generic 400 with the
/// underlying message. Vault errors are already written to be safe to show a
/// user — notably, a failed decrypt does not reveal whether the credential was
/// wrong or the file was tampered with.
fn vault_err(e: anyhow::Error) -> ApiErr {
    let msg = e.to_string();
    let code = if msg.contains("not found") {
        StatusCode::NOT_FOUND
    } else if msg.contains("locked") {
        StatusCode::LOCKED
    } else {
        StatusCode::BAD_REQUEST
    };
    err(code, msg)
}

/// `?scope=entity:acme`, defaulting to global.
#[derive(Debug, Default, Deserialize)]
struct ScopeQuery {
    #[serde(default)]
    scope: Option<String>,
}

impl ScopeQuery {
    fn parse(&self) -> Result<Scope, ApiErr> {
        match &self.scope {
            None => Ok(Scope::Global),
            Some(raw) => raw
                .parse()
                .map_err(|e: anyhow::Error| err(StatusCode::BAD_REQUEST, e.to_string())),
        }
    }
}

/// Decode a base64 32-byte unlock key.
///
/// Length is checked here rather than deeper in: a short key would otherwise be
/// silently padded or rejected with a confusing decrypt failure, and a client
/// truncating its PRF output should hear about it at the door.
fn decode_key(b64: &str) -> Result<SecretKey, ApiErr> {
    let raw = B64
        .decode(b64.trim())
        .map_err(|_| err(StatusCode::BAD_REQUEST, "key must be base64"))?;
    let arr: [u8; KEY_LEN] = raw.as_slice().try_into().map_err(|_| {
        err(
            StatusCode::BAD_REQUEST,
            format!("key must be {KEY_LEN} bytes"),
        )
    })?;
    Ok(SecretKey::from_bytes(arr))
}

// ---- lifecycle --------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusResponse {
    /// Whether this principal has a vault file yet.
    initialized: bool,
    unlocked: bool,
    /// Seconds until idle auto-lock, when unlocked.
    #[serde(skip_serializing_if = "Option::is_none")]
    locks_in_seconds: Option<u64>,
    idle_timeout_seconds: u64,
    /// Every enrolled way in. Present even while locked, so a client knows
    /// whether to offer a passkey button before anyone has typed anything.
    #[serde(default)]
    unlock_methods: Vec<UnlockMethodInfo>,
}

async fn status_for(state: &ServerState, ctx: &AuthContext) -> Result<StatusResponse, ApiErr> {
    let m = manager(state)?;
    let vault = m.handle(&ctx.principal_id);
    let unlock_methods = if vault.exists() {
        vault.unlock_methods().unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(StatusResponse {
        initialized: vault.exists(),
        unlocked: vault.is_unlocked(),
        locks_in_seconds: m.seconds_until_lock(&ctx.principal_id),
        idle_timeout_seconds: m.idle_timeout().as_secs(),
        unlock_methods,
    })
}

async fn status(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<StatusResponse>, ApiErr> {
    Ok(Json(status_for(&state, &ctx).await?))
}

// Shared body shape: both creating a vault and unlocking one take just the
// master password.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MasterPasswordRequest {
    master_password: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitResponse {
    #[serde(flatten)]
    status: StatusResponse,
    /// Shown once, at creation, and never retrievable again. A client that
    /// discards this has handed the user a vault with one fewer way back in.
    recovery_code: String,
}

async fn init(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<MasterPasswordRequest>,
) -> Result<Json<InitResponse>, ApiErr> {
    let m = manager(&state)?;
    let vault = m.handle(&ctx.principal_id);
    if vault.exists() {
        return Err(err(StatusCode::CONFLICT, "vault already exists"));
    }
    let outcome = vault.init(&body.master_password).map_err(vault_err)?;
    let recovery_code = outcome.recovery_code.formatted().to_string();
    m.touch(&ctx.principal_id);

    Ok(Json(InitResponse {
        status: status_for(&state, &ctx).await?,
        recovery_code,
    }))
}

/// `429 Too Many Requests` with a `Retry-After` header, the standard way to say
/// "backing off". Returned when the unlock throttle is in effect.
fn throttled(seconds: u64) -> ApiErr {
    // The header is the machine-readable half; the JSON body carries the same
    // number for a client that only parses the body.
    let mut headers = HeaderMap::new();
    headers.insert(RETRY_AFTER, HeaderValue::from(seconds));
    (
        StatusCode::TOO_MANY_REQUESTS,
        headers,
        Json(ApiError {
            error: format!("too many attempts; wait {seconds}s before trying again"),
        }),
    )
}

/// Shared guard for every credential-based unlock path: refuse while a backoff is
/// pending, then translate the attempt's result into throttle bookkeeping.
///
/// One counter per principal is what keeps the three unlock routes and
/// [`change_password`] consistent: a passkey guess, a password guess and a
/// "current password" guess all escalate the same backoff, so an attacker cannot
/// dodge the limit by alternating between them.
fn check_throttle(m: &VaultManager, principal_id: &str) -> Result<(), ApiErr> {
    match m.unlock_retry_after(principal_id) {
        Some(seconds) => Err(throttled(seconds)),
        None => Ok(()),
    }
}

async fn unlock(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<MasterPasswordRequest>,
) -> Result<Json<StatusResponse>, ApiErr> {
    let m = manager(&state)?;
    let vault = m.handle(&ctx.principal_id);
    if !vault.exists() {
        return Err(err(StatusCode::NOT_FOUND, "vault has not been created yet"));
    }
    check_throttle(m, &ctx.principal_id)?;

    if vault.unlock(&body.master_password).is_err() {
        let wait = m.note_unlock_failure(&ctx.principal_id);
        // Uniform message: never distinguish a wrong password from a damaged file.
        return match wait {
            Some(seconds) => Err(throttled(seconds)),
            None => Err(err(StatusCode::UNAUTHORIZED, "could not unlock the vault")),
        };
    }
    m.note_unlock_success(&ctx.principal_id);
    m.touch(&ctx.principal_id);
    Ok(Json(status_for(&state, &ctx).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeyUnlockRequest {
    /// Which enrolled method this key belongs to.
    method_id: String,
    /// 32 bytes, base64. A WebAuthn PRF output, or a device key from the OS
    /// keychain.
    key: String,
}

/// Unlock with a passkey PRF output or a device key.
async fn unlock_with_key(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<KeyUnlockRequest>,
) -> Result<Json<StatusResponse>, ApiErr> {
    let m = manager(&state)?;
    let vault = m.handle(&ctx.principal_id);
    if !vault.exists() {
        return Err(err(StatusCode::NOT_FOUND, "vault has not been created yet"));
    }
    check_throttle(m, &ctx.principal_id)?;

    let key = decode_key(&body.key)?;
    if vault.unlock_with_key(&body.method_id, &key).is_err() {
        return match m.note_unlock_failure(&ctx.principal_id) {
            Some(seconds) => Err(throttled(seconds)),
            None => Err(err(StatusCode::UNAUTHORIZED, "could not unlock the vault")),
        };
    }
    m.note_unlock_success(&ctx.principal_id);
    m.touch(&ctx.principal_id);
    Ok(Json(status_for(&state, &ctx).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecoveryUnlockRequest {
    /// As printed, in any formatting the user typed it.
    recovery_code: String,
}

async fn unlock_with_recovery(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<RecoveryUnlockRequest>,
) -> Result<Json<StatusResponse>, ApiErr> {
    let m = manager(&state)?;
    let vault = m.handle(&ctx.principal_id);
    if !vault.exists() {
        return Err(err(StatusCode::NOT_FOUND, "vault has not been created yet"));
    }
    check_throttle(m, &ctx.principal_id)?;

    if vault
        .unlock_with_recovery_code(&body.recovery_code)
        .is_err()
    {
        return match m.note_unlock_failure(&ctx.principal_id) {
            Some(seconds) => Err(throttled(seconds)),
            None => Err(err(StatusCode::UNAUTHORIZED, "could not unlock the vault")),
        };
    }
    m.note_unlock_success(&ctx.principal_id);
    m.touch(&ctx.principal_id);
    Ok(Json(status_for(&state, &ctx).await?))
}

async fn lock(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<StatusCode, ApiErr> {
    manager(&state)?.lock(&ctx.principal_id);
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

/// The context `Vault::change_master_password` attaches when the current
/// password fails to open the vault — the one failure that is a guess.
const WRONG_CURRENT_PASSWORD: &str = "current master password is incorrect";

/// Rewrap the user key under a new master password.
///
/// This route verifies `currentPassword` by unlocking with it, which makes it a
/// password oracle exactly like `/unlock`. So it gets the same treatment:
///
/// - **The vault must already be unlocked.** On a locked vault this would be a
///   second door — and a correct guess would leave the vault open behind it.
/// - **Guesses are throttled** on the same per-principal counter as the unlock
///   routes, checked before any Argon2id work is spent.
async fn change_password(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<ChangePasswordRequest>,
) -> Result<StatusCode, ApiErr> {
    let m = manager(&state)?;
    let vault = unlocked(&state, &ctx)?;
    // Rejected before the throttle: an empty new password is not a guess, and
    // should neither cost an attempt nor be refused with a 429.
    if body.new_password.is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "master password must not be empty"));
    }
    check_throttle(m, &ctx.principal_id)?;

    match vault.change_master_password(&body.current_password, &body.new_password) {
        Ok(()) => {
            m.note_unlock_success(&ctx.principal_id);
            m.touch(&ctx.principal_id);
            Ok(StatusCode::NO_CONTENT)
        }
        Err(e) if e.to_string() == WRONG_CURRENT_PASSWORD => {
            match m.note_unlock_failure(&ctx.principal_id) {
                Some(seconds) => Err(throttled(seconds)),
                None => Err(err(StatusCode::BAD_REQUEST, WRONG_CURRENT_PASSWORD)),
            }
        }
        // Anything else (an unwritable file, say) happened after the current
        // password was proven, so it is not a failed guess.
        Err(e) => Err(vault_err(e)),
    }
}

// ---- unlock methods ----------------------------------------------------------

/// Listable while locked by design — see [`StatusResponse::unlock_methods`].
async fn list_unlock_methods(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let vault = manager(&state)?.handle(&ctx.principal_id);
    if !vault.exists() {
        return Err(err(StatusCode::NOT_FOUND, "vault has not been created yet"));
    }
    let methods = vault.unlock_methods().map_err(vault_err)?;
    Ok(Json(serde_json::json!({ "methods": methods })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddUnlockMethodRequest {
    /// `passkey` or `device-key`. A master password is not enrollable this way.
    kind: UnlockKind,
    label: String,
    /// 32 bytes, base64.
    key: String,
    /// Public parameters the client needs to reproduce this key later — a
    /// passkey's `prfSaltB64` and `credentialIdB64`.
    #[serde(default)]
    params: BTreeMap<String, String>,
}

async fn add_unlock_method(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<AddUnlockMethodRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiErr> {
    let key = decode_key(&body.key)?;
    let id = unlocked(&state, &ctx)?
        .add_key_unlock_method(body.kind, &body.label, &key, body.params)
        .map_err(vault_err)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

async fn remove_unlock_method(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiErr> {
    unlocked(&state, &ctx)?
        .remove_unlock_method(&id)
        .map_err(vault_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// 32 fresh bytes from the OS CSPRNG.
///
/// Two callers, one primitive: the web uses it as the salt for a WebAuthn PRF
/// evaluation, and mobile uses it as the device key itself, generated here
/// because React Native ships no WebCrypto and pulling in a native crypto module
/// would force a dev-client rebuild.
///
/// That the gateway generates a device key is not a weakening: enrolling any
/// unlock method means sending its key here anyway (see the trust-model note at
/// the top of this file). What matters is where the key comes to *rest* — on
/// mobile that is the device keychain, behind biometrics.
async fn random_key_bytes() -> Json<serde_json::Value> {
    let bytes = B64.encode(random_key().expose());
    Json(serde_json::json!({ "keyB64": bytes, "saltB64": bytes }))
}

/// Mint an additional recovery code. Shown once.
async fn new_recovery_code(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiErr> {
    let code = unlocked(&state, &ctx)?
        .add_recovery_code()
        .map_err(vault_err)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "recoveryCode": code.formatted() })),
    ))
}

// ---- items ------------------------------------------------------------------

/// List items, optionally for one entity.
///
/// `?scope=entity:acme` returns just that entity's password set; no `scope`
/// returns every item across all entities, each tagged with its own scope so a
/// UI can group them. Unlike environment variables, items do not fall back to
/// global — an entity's passwords are its own.
async fn list_items(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<ItemScopeQuery>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let scope = q.parse()?;
    let items = unlocked(&state, &ctx)?
        .list_items(scope.as_ref())
        .map_err(vault_err)?;
    Ok(Json(serde_json::json!({ "items": items })))
}

/// `?scope=` where absence means "all entities" rather than "global".
#[derive(Debug, Default, Deserialize)]
struct ItemScopeQuery {
    #[serde(default)]
    scope: Option<String>,
}

impl ItemScopeQuery {
    /// `None` = every scope; `Some(scope)` = that one. An explicit `scope=all`
    /// also means every scope, for a client that would rather be explicit.
    fn parse(&self) -> Result<Option<Scope>, ApiErr> {
        match self.scope.as_deref() {
            None | Some("all") | Some("") => Ok(None),
            Some(raw) => raw
                .parse()
                .map(Some)
                .map_err(|e: anyhow::Error| err(StatusCode::BAD_REQUEST, e.to_string())),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateItemRequest {
    kind: ItemKind,
    name: String,
    /// Which entity to file this under. Defaults to `global` (the CEO's own set).
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    uris: Vec<String>,
    #[serde(default)]
    favorite: bool,
    #[serde(default)]
    secret: ItemSecret,
}

async fn create_item(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<CreateItemRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiErr> {
    if body.name.trim().is_empty() {
        return Err(err(StatusCode::BAD_REQUEST, "name is required"));
    }
    let scope = ScopeQuery { scope: body.scope }.parse()?;
    let id = unlocked(&state, &ctx)?
        .add_item(NewItem {
            kind: body.kind,
            name: body.name,
            scope,
            folder: body.folder,
            uris: body.uris,
            favorite: body.favorite,
            secret: body.secret,
        })
        .map_err(vault_err)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

/// Which entities hold items, and how many each — for an entity switcher.
async fn item_scopes(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let scopes = unlocked(&state, &ctx)?
        .list_item_scopes()
        .map_err(vault_err)?;
    let out: Vec<serde_json::Value> = scopes
        .into_iter()
        .map(|(scope, count)| {
            serde_json::json!({
                "scope": scope.to_string(),
                "label": scope.label(),
                "itemCount": count,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "scopes": out })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MoveItemRequest {
    /// The entity to move this item to. `global` is the CEO's own set.
    scope: String,
}

/// Move an item to a different entity's set, keeping its id, key and secret.
async fn move_item(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<MoveItemRequest>,
) -> Result<StatusCode, ApiErr> {
    let scope: Scope = body
        .scope
        .parse()
        .map_err(|e: anyhow::Error| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    unlocked(&state, &ctx)?
        .set_item_scope(&id, scope)
        .map_err(vault_err)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_item_secret(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<ItemSecret>, ApiErr> {
    let secret = unlocked(&state, &ctx)?
        .item_secret(&id)
        .map_err(vault_err)?;
    Ok(Json(secret))
}

async fn update_item(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(secret): Json<ItemSecret>,
) -> Result<StatusCode, ApiErr> {
    unlocked(&state, &ctx)?
        .update_item_secret(&id, &secret)
        .map_err(vault_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateMetadataRequest {
    #[serde(default)]
    name: Option<String>,
    /// Double option: absent leaves the folder alone, explicit `null` clears it.
    #[serde(default, deserialize_with = "double_option")]
    folder: Option<Option<String>>,
    #[serde(default)]
    uris: Option<Vec<String>>,
    #[serde(default)]
    favorite: Option<bool>,
}

/// Distinguish "field omitted" from "field set to null".
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Deserialize::deserialize(d).map(Some)
}

async fn update_item_metadata(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<UpdateMetadataRequest>,
) -> Result<StatusCode, ApiErr> {
    unlocked(&state, &ctx)?
        .update_item_metadata(&id, body.name, body.folder, body.uris, body.favorite)
        .map_err(vault_err)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_item(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiErr> {
    unlocked(&state, &ctx)?
        .delete_item(&id)
        .map_err(vault_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The current TOTP code for an item that has a stored 2FA seed.
///
/// This response carries only the six digits and a countdown, so a client can
/// show a live code without implementing RFC 6238 itself. It is *not* what keeps
/// the seed off the client: the seed is part of the item's secret half, and
/// [`get_item_secret`] returns it with the rest so it can be edited.
///
/// Clients refresh this on a timer, so it reads through [`unlocked_passive`]:
/// a ticking code is not activity, and must not hold the vault open past its
/// idle window. A missing or unparseable seed is a 400, distinct from the 404
/// of a missing item.
async fn item_totp(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let secret = unlocked_passive(&state, &ctx)?
        .item_secret(&id)
        .map_err(vault_err)?;
    let seed = secret
        .totp_secret
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "this item has no TOTP secret"))?;

    let totp = consistent_world_credentials::totp::Totp::from_stored_secret(&seed)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;
    let code = totp.now();

    Ok(Json(serde_json::json!({
        "code": code.code,
        "secondsRemaining": code.seconds_remaining,
        "period": code.period,
        "issuer": totp.issuer,
        "account": totp.account,
    })))
}

// ---- secrets / environment variables ----------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SecretsResponse {
    scope: String,
    keys: Vec<String>,
}

async fn list_secrets(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<SecretsResponse>, ApiErr> {
    let scope = q.parse()?;
    let keys = unlocked(&state, &ctx)?
        .list_scoped_keys(&scope)
        .map_err(vault_err)?;
    Ok(Json(SecretsResponse {
        scope: scope.to_string(),
        keys,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetSecretRequest {
    key: String,
    value: String,
    /// Defaults to global when absent.
    #[serde(default)]
    scope: Option<String>,
}

async fn set_secret(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<SetSecretRequest>,
) -> Result<StatusCode, ApiErr> {
    let scope = ScopeQuery { scope: body.scope }.parse()?;
    unlocked(&state, &ctx)?
        .set_scoped(&scope, &body.key, &body.value)
        .map_err(vault_err)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Reveal one value. Separate from the list endpoint so a UI can render every
/// key without the values ever leaving the process.
///
/// `?resolve=true` follows the entity → global fallback, which is what an agent
/// runtime wants; the default reads the named scope only, which is what a
/// settings screen wants so it can show which values are genuinely set here.
#[derive(Deserialize)]
struct GetSecretQuery {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resolve: bool,
}

async fn get_secret(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(key): Path<String>,
    Query(q): Query<GetSecretQuery>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let scope = ScopeQuery { scope: q.scope }.parse()?;
    let vault = unlocked(&state, &ctx)?;
    let value = if q.resolve {
        vault.resolve(&scope, &key)
    } else {
        vault.get_scoped(&scope, &key)
    }
    .map_err(vault_err)?;

    Ok(Json(serde_json::json!({
        "key": key,
        "value": value,
        "scope": scope.to_string(),
    })))
}

async fn delete_secret(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(key): Path<String>,
    Query(q): Query<ScopeQuery>,
) -> Result<StatusCode, ApiErr> {
    let scope = q.parse()?;
    unlocked(&state, &ctx)?
        .delete_scoped(&scope, &key)
        .map_err(vault_err)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScopeSummary {
    scope: String,
    label: String,
    key_count: usize,
}

/// Every scope holding at least one variable, for a settings screen's sidebar.
async fn list_scopes(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let vault = unlocked(&state, &ctx)?;
    let mut out = Vec::new();
    for scope in vault.list_scopes().map_err(vault_err)? {
        let keys = vault.list_scoped_keys(&scope).map_err(vault_err)?;
        out.push(ScopeSummary {
            scope: scope.to_string(),
            label: scope.label(),
            key_count: keys.len(),
        });
    }
    Ok(Json(serde_json::json!({ "scopes": out })))
}

/// The full environment an entity runs with: global defaults overlaid with the
/// entity's own values.
///
/// This returns plaintext values — it is the endpoint an agent runtime calls to
/// populate a child process's environment, so there is no way around that. It is
/// behind auth and scoped to the caller's own vault.
async fn resolved_env(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Query(q): Query<ScopeQuery>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let scope = q.parse()?;
    let env = unlocked(&state, &ctx)?.env_for(&scope).map_err(vault_err)?;
    Ok(Json(serde_json::json!({
        "scope": scope.to_string(),
        "env": env,
    })))
}

// ---- durability ---------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportRequest {
    /// Protects the backup. Not the master password — a backup that reused it
    /// would turn one compromise into two.
    passphrase: String,
}

async fn export(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<ExportRequest>,
) -> Result<Json<VaultExport>, ApiErr> {
    let backup = unlocked(&state, &ctx)?
        .export(&body.passphrase)
        .map_err(vault_err)?;
    Ok(Json(backup))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportRequest {
    passphrase: String,
    backup: VaultExport,
}

async fn import(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<ImportRequest>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let (items, variables) = unlocked(&state, &ctx)?
        .import(&body.backup, &body.passphrase)
        .map_err(vault_err)?;
    Ok(Json(
        serde_json::json!({ "itemsAdded": items, "variablesAdded": variables }),
    ))
}

async fn audit(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let entries: Vec<AuditEntry> = unlocked(&state, &ctx)?.audit_log().map_err(vault_err)?;
    Ok(Json(serde_json::json!({ "entries": entries })))
}

/// Password-hygiene report: weak, reused and stale passwords across the vault.
///
/// The scan decrypts every item server-side but the response is value-free —
/// only ids, names and reasons — so a UI can render the whole report without any
/// password crossing the wire.
async fn health(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<consistent_world_credentials::model::HealthReport>, ApiErr> {
    let report = unlocked(&state, &ctx)?.health_report().map_err(vault_err)?;
    Ok(Json(report))
}

/// Check every password against Have I Been Pwned, privacy-preservingly.
///
/// Each password is SHA-1'd inside the vault; only the 5-char hash prefix is sent
/// to HIBP (k-anonymity — see `consistent_world_credentials::breach`), and the
/// suffix is matched against the response here. The password itself never leaves
/// the process and its full hash never leaves the host.
///
/// Degrades gracefully: if HIBP is unreachable for some prefixes, those are
/// reported as `unreachable: true` rather than failing the whole check — a home
/// server with flaky connectivity should still get the answer for what it could
/// reach.
async fn breach_check(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    use consistent_world_credentials::breach;

    let targets = unlocked(&state, &ctx)?
        .breach_scan_targets()
        .map_err(vault_err)?;

    // One HIBP request per distinct prefix, not per password.
    let mut by_prefix: std::collections::HashMap<String, Vec<_>> = std::collections::HashMap::new();
    for t in targets {
        by_prefix.entry(t.prefix.clone()).or_default().push(t);
    }

    let client = reqwest::Client::new();
    let mut breached: Vec<serde_json::Value> = Vec::new();
    let mut checked = 0usize;
    let mut unreachable = false;

    for (prefix, group) in by_prefix {
        let body = match client
            .get(format!("https://api.pwnedpasswords.com/range/{prefix}"))
            // Padding obscures how many suffixes actually share this prefix.
            .header("Add-Padding", "true")
            .header("User-Agent", "Consistent-CEO-Vault")
            .timeout(std::time::Duration::from_secs(8))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp.text().await.unwrap_or_default(),
            _ => {
                // Could not reach HIBP for this prefix; skip its items.
                unreachable = true;
                continue;
            }
        };

        for t in group {
            checked += 1;
            let count = breach::count_in_range(&body, &t.suffix);
            if count > 0 {
                breached.push(serde_json::json!({
                    "id": t.id,
                    "name": t.name,
                    "scope": t.scope,
                    "count": count,
                }));
            }
        }
    }

    // Most-exposed first.
    breached.sort_by(|a, b| {
        b["count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["count"].as_u64().unwrap_or(0))
    });

    Ok(Json(serde_json::json!({
        "checked": checked,
        "breached": breached,
        "unreachable": unreachable,
    })))
}

// ---- migration from other password managers ---------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportCsvRequest {
    /// The raw text of a CSV export from Bitwarden, Chrome, 1Password, LastPass,
    /// Safari, or anything close to a generic login CSV.
    csv: String,
    /// When false, the response reports what *would* import without storing
    /// anything, so a UI can show a confirmation screen first.
    #[serde(default = "default_true")]
    commit: bool,
    /// File every imported login under this entity. Defaults to `global`.
    #[serde(default)]
    scope: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Parse another manager's CSV export and, unless it is a dry run, store the
/// logins it contains.
///
/// A dry run is the default UX: parse, show the user "142 logins from Bitwarden,
/// 3 rows skipped", let them confirm, then re-post with `commit`. The skipped
/// rows are returned verbatim so a partial import is never silent.
async fn import_csv(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<ImportCsvRequest>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let scope = ScopeQuery { scope: body.scope }.parse()?;
    let parsed = consistent_world_credentials::import::parse_csv(&body.csv)
        .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?
        .into_scope(scope);

    // Count before the items are consumed by the commit loop, so the response is
    // the same shape whether or not anything was stored.
    let detected = parsed.items.len();
    let format = parsed.format.label();
    let skipped = parsed.skipped;

    let mut imported = 0usize;
    if body.commit {
        let vault = unlocked(&state, &ctx)?;
        for item in parsed.items {
            vault.add_item(item).map_err(vault_err)?;
            imported += 1;
        }
    }

    Ok(Json(serde_json::json!({
        "format": format,
        // How many logins the file yielded. On a dry run this is what *would* be
        // stored; on a commit it equals `imported`.
        "detected": detected,
        "imported": imported,
        "committed": body.commit,
        "skipped": skipped,
    })))
}

// ---- master password strength ------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StrengthRequest {
    password: String,
    /// Strings that should count as weak *for this user* — their email, name,
    /// workspace. A password built from these is guessable by anyone who knows
    /// them, however random it looks to a generic dictionary.
    #[serde(default)]
    context: Vec<String>,
}

/// Estimate a candidate master password's strength.
///
/// Deliberately needs no unlocked vault: it is called at *creation*, before a
/// vault exists. It stays behind auth so it is not a public cracking oracle, but
/// the password is only measured, never stored.
async fn password_strength(
    Extension(_ctx): Extension<AuthContext>,
    Json(body): Json<StrengthRequest>,
) -> Json<consistent_world_credentials::strength::Strength> {
    let context: Vec<&str> = body.context.iter().map(String::as_str).collect();
    Json(consistent_world_credentials::strength::estimate(
        &body.password,
        &context,
    ))
}

// ---- generator --------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateRequest {
    /// `"password"` (default) or `"passphrase"`.
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    length: Option<usize>,
    #[serde(default)]
    lowercase: Option<bool>,
    #[serde(default)]
    uppercase: Option<bool>,
    #[serde(default)]
    digits: Option<bool>,
    #[serde(default)]
    symbols: Option<bool>,
    #[serde(default)]
    avoid_ambiguous: Option<bool>,
    #[serde(default)]
    words: Option<usize>,
    #[serde(default)]
    separator: Option<String>,
    #[serde(default)]
    capitalize: Option<bool>,
}

/// Generation needs no vault access — it is pure randomness — but it stays
/// behind auth so an unauthenticated caller cannot use the gateway as an oracle.
async fn generate(Json(body): Json<GenerateRequest>) -> Result<Json<serde_json::Value>, ApiErr> {
    let value = if body.mode.as_deref() == Some("passphrase") {
        generate_passphrase(
            body.words.unwrap_or(5),
            body.separator.as_deref().unwrap_or("-"),
            body.capitalize.unwrap_or(true),
        )
    } else {
        let d = PasswordSpec::default();
        generate_password(&PasswordSpec {
            length: body.length.unwrap_or(d.length),
            lowercase: body.lowercase.unwrap_or(d.lowercase),
            uppercase: body.uppercase.unwrap_or(d.uppercase),
            digits: body.digits.unwrap_or(d.digits),
            symbols: body.symbols.unwrap_or(d.symbols),
            avoid_ambiguous: body.avoid_ambiguous.unwrap_or(d.avoid_ambiguous),
        })
    }
    .map_err(|e| err(StatusCode::BAD_REQUEST, e.to_string()))?;

    Ok(Json(serde_json::json!({ "value": value })))
}

// ---- sharing ----------------------------------------------------------------

async fn public_key(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, ApiErr> {
    let vault = manager(&state)?.handle(&ctx.principal_id);
    if !vault.exists() {
        return Err(err(StatusCode::NOT_FOUND, "vault has not been created yet"));
    }
    let key = vault.public_key_b64().map_err(vault_err)?;
    Ok(Json(serde_json::json!({ "publicKey": key })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateShareRequest {
    item_id: String,
    /// The recipient's X25519 public key, from their `GET /api/vault/public-key`.
    recipient_public_key: String,
    #[serde(default)]
    scenario: Option<String>,
    /// RFC3339. After this instant the recipient's client refuses the envelope.
    #[serde(default)]
    expires_at: Option<String>,
}

/// Produce a sealed envelope. The gateway never sees the plaintext: the item key
/// inside is encrypted to the recipient's public key.
async fn create_share(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(body): Json<CreateShareRequest>,
) -> Result<Json<ShareEnvelope>, ApiErr> {
    let envelope = unlocked(&state, &ctx)?
        .create_share(
            &body.item_id,
            &body.recipient_public_key,
            &ctx.principal_id,
            body.scenario,
            body.expires_at,
        )
        .map_err(vault_err)?;
    Ok(Json(envelope))
}

async fn accept_share(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Json(envelope): Json<ShareEnvelope>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiErr> {
    let id = unlocked(&state, &ctx)?
        .accept_share(&envelope)
        .map_err(vault_err)?;
    Ok((StatusCode::CREATED, Json(serde_json::json!({ "id": id }))))
}

#[cfg(test)]
mod tests {
    //! What the idle window and the unlock throttle do over HTTP. Both are
    //! shrunk through the manager (a one-second window, one free guess) so each
    //! test spends as few Argon2id derivations as possible — they run for real
    //! here. The wider wire contract lives in `tests/vault_api_test.rs`.

    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use std::time::Duration;
    use tower::util::ServiceExt;

    use crate::vault_throttle::UnlockThrottle;

    const TOKEN: &str = "test-secret-token";
    const CHANGE: &str = "/api/vault/change-password";
    const PASSWORD: &str = "the real password";

    async fn app_with(manager: VaultManager) -> Router {
        let memory: Arc<dyn consistent_world_memory::store::MemoryStore> = Arc::new(
            consistent_world_memory::store::SurrealMemoryStore::connect_memory()
                .await
                .unwrap(),
        );
        let state = ServerState::new("test".into(), Some(TOKEN.into()), memory)
            .with_vault_manager(Arc::new(manager));
        crate::server::build_router(state)
    }

    /// One free wrong guess, then a minute's backoff.
    fn strict_throttle() -> UnlockThrottle {
        UnlockThrottle::new(1, Duration::from_secs(60), Duration::from_secs(60))
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
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
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, json)
    }

    async fn init(app: &Router) {
        let (status, _, _) = call(
            app,
            "POST",
            "/api/vault/init",
            Some(json!({ "masterPassword": PASSWORD })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }

    fn change(current: &str, new: &str) -> Option<Value> {
        Some(json!({ "currentPassword": current, "newPassword": new }))
    }

    #[tokio::test]
    async fn change_password_is_not_a_way_into_a_locked_vault() {
        let dir = tempfile::tempdir().unwrap();
        let app = app_with(VaultManager::new(dir.path().to_path_buf())).await;
        init(&app).await;
        call(&app, "POST", "/api/vault/lock", None).await;

        // Even with the *right* current password: on a locked vault this route
        // would otherwise unlock it as a side effect.
        let (status, _, _) = call(&app, "POST", CHANGE, change(PASSWORD, "a new one")).await;
        assert_eq!(status, StatusCode::LOCKED);
        let (_, _, body) = call(&app, "GET", "/api/vault/status", None).await;
        assert_eq!(body["unlocked"], false, "the vault must stay locked");
    }

    #[tokio::test]
    async fn wrong_current_passwords_share_the_unlock_throttle() {
        let dir = tempfile::tempdir().unwrap();
        let manager = VaultManager::new(dir.path().to_path_buf()).with_throttle(strict_throttle());
        let app = app_with(manager).await;
        init(&app).await;

        let (status, _, body) = call(&app, "POST", CHANGE, change("a guess", "mine now")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "the free attempt");
        assert_eq!(body["error"], WRONG_CURRENT_PASSWORD);

        let (status, headers, _) = call(&app, "POST", CHANGE, change("another", "mine now")).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 = headers
            .get(RETRY_AFTER)
            .expect("a 429 carries Retry-After")
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(retry_after > 0);

        // During the backoff even the right password is refused, before any
        // Argon2id work is spent on it...
        let (status, _, _) = call(&app, "POST", CHANGE, change(PASSWORD, "mine now")).await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        // ...and the unlock routes see the same counter, so alternating between
        // doors buys an attacker nothing.
        call(&app, "POST", "/api/vault/lock", None).await;
        let (status, headers, _) = call(
            &app,
            "POST",
            "/api/vault/unlock",
            Some(json!({ "masterPassword": PASSWORD })),
        )
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert!(headers.contains_key(RETRY_AFTER));
    }

    #[tokio::test]
    async fn the_right_current_password_changes_it_and_clears_the_streak() {
        let dir = tempfile::tempdir().unwrap();
        let manager = VaultManager::new(dir.path().to_path_buf()).with_throttle(strict_throttle());
        let app = app_with(manager).await;
        init(&app).await;

        let (status, _, _) = call(&app, "POST", CHANGE, change("a typo", "brand new pw")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _, _) = call(&app, "POST", CHANGE, change(PASSWORD, "brand new pw")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // With one free guess, a second failure would be throttled had the
        // success not cleared the first.
        let (status, _, _) = call(&app, "POST", CHANGE, change("a typo", "x")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        call(&app, "POST", "/api/vault/lock", None).await;
        let (status, _, _) = call(
            &app,
            "POST",
            "/api/vault/unlock",
            Some(json!({ "masterPassword": "brand new pw" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "the new password opens the vault");
    }

    #[tokio::test]
    async fn a_polled_totp_code_does_not_hold_the_vault_open() {
        let dir = tempfile::tempdir().unwrap();
        let manager =
            VaultManager::with_idle_timeout(dir.path().to_path_buf(), Duration::from_millis(1000));
        let app = app_with(manager).await;
        init(&app).await;
        // Creating the item is real activity and starts the idle window.
        let (status, _, body) = call(
            &app,
            "POST",
            "/api/vault/items",
            Some(json!({
                "kind": "login",
                "name": "GitHub",
                "secret": { "totpSecret": "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ" }
            })),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let totp = format!("/api/vault/items/{}/totp", body["id"].as_str().unwrap());

        let (status, _, _) = call(&app, "GET", &totp, None).await;
        assert_eq!(status, StatusCode::OK);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let (status, _, _) = call(&app, "GET", &totp, None).await;
        assert_eq!(status, StatusCode::OK, "still inside the window");

        // 1.2s since the last real activity. Had the poll above counted, the
        // window would run to 1.6s and this would still succeed.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let (status, _, _) = call(&app, "GET", &totp, None).await;
        assert_eq!(status, StatusCode::LOCKED);
    }
}
