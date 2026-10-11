//! Own authentication store (username + password), replacing Clerk.
//!
//! - `auth_users`: credential record keyed by `user_id` (which becomes the
//!   request's `principal_id`). Holds a unique lowercased username, an Argon2
//!   password hash, optional email/phone (never required — the product does not
//!   depend on email), Argon2-hashed one-time backup codes for self-custody
//!   account recovery, and the user's [`UserProfile`] (identity, intent, and
//!   working style — see `user_profile.rs`).
//! - `auth_sessions`: opaque, revocable bearer tokens keyed by the SHA-256 of the
//!   token. Device-aware (`device_label`) so the mobile app can later act as an
//!   authenticator / trusted device.
//!
//! All tables are SCHEMALESS, matching the rest of the gateway. Argon2 (slow,
//! brute-force resistant) protects passwords and backup codes; session tokens are
//! high-entropy random, so a fast SHA-256 lookup is sufficient and safe.
//!
//! Every Argon2 call goes through [`tokio::task::spawn_blocking`]. At OWASP
//! parameters a single hash is ~19 MiB and hundreds of milliseconds of CPU;
//! running that inline on a tokio worker means a few hundred concurrent
//! registrations stall every other future on the runtime, including `/health`.

use anyhow::{Result, anyhow, bail};
use argon2::password_hash::SaltString;
use argon2::password_hash::rand_core::OsRng;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use uuid::Uuid;

use crate::user_profile::UserProfile;

/// How long a freshly-minted session token is valid.
const SESSION_TTL_DAYS: i64 = 30;
/// How long a scoped API key is valid. Long-lived on purpose: an integration
/// shouldn't silently die at the 30-day session horizon. Revocation, not
/// expiry, is the intended kill switch.
const API_KEY_TTL_DAYS: i64 = 3650;
/// Marker on `auth_sessions` rows that are API keys rather than interactive
/// sessions. `resolve_session` stays kind-agnostic, so a key authenticates
/// every `/api/*` route exactly like a browser session does.
const SESSION_KIND_API: &str = "api";
/// Human label cap for an API key name.
const API_KEY_NAME_MAX: usize = 64;
/// Number of one-time recovery codes generated at registration.
const BACKUP_CODE_COUNT: usize = 10;
/// Wrong codes an approval request tolerates before it is burned. The code is
/// only six digits, so without this an attacker who knows a request id can
/// simply enumerate the million possibilities inside the two-minute window.
const APPROVAL_MAX_ATTEMPTS: u32 = 5;
/// Cap on the free-text `context` an (unauthenticated) approval request carries.
/// It is rendered on the approving device, so it is attacker-controlled text on
/// a security-critical screen.
const APPROVAL_CONTEXT_MAX: usize = 160;
/// How stale `last_used_at` may get before resolving a session writes it back.
/// Without a floor this would be a database write on every authenticated
/// request; a minute of resolution is plenty for "when was this last used".
const LAST_USED_REFRESH_SECS: i64 = 60;
/// How long an interactive session may sit unused before it stops working,
/// regardless of its absolute expiry.
///
/// Absolute expiry alone is a weak bound once the gateway is reachable from
/// outside the LAN: a token lifted from a phone that has not been opened in
/// weeks would still authenticate for the rest of its 30 days. Idle expiry
/// closes that window without nagging anyone who actually uses the app — a
/// session in regular use is refreshed on every request.
///
/// API keys are deliberately exempt: an integration that runs monthly is not
/// an abandoned session, and revocation is its intended kill switch.
const SESSION_IDLE_EXPIRY_DAYS: i64 = 14;

/// Has this session been unused long enough to stop trusting it?
///
/// Pure so the rule is testable without a database. A missing or unparseable
/// `last_used_at` falls back to `created_at`; if neither parses, the session is
/// left alone — a malformed timestamp is a bug in our own bookkeeping, not
/// evidence against the user.
pub fn session_is_idle_expired(
    kind: Option<&str>,
    created_at: &str,
    last_used_at: Option<&str>,
    now: chrono::DateTime<Utc>,
) -> bool {
    if kind == Some(SESSION_KIND_API) {
        return false;
    }
    let reference = last_used_at
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .or_else(|| chrono::DateTime::parse_from_rfc3339(created_at).ok());
    let Some(reference) = reference else {
        return false;
    };
    (now - reference.with_timezone(&Utc)).num_days() >= SESSION_IDLE_EXPIRY_DAYS
}

/// Handle to the auth tables (`auth_users`, `auth_sessions`, `auth_devices`,
/// `auth_approvals`, `auth_passkeys`). Cheap to clone via `Arc`; every method
/// below borrows `self`.
pub struct AuthStore {
    db: Arc<Surreal<Any>>,
}

/// Public identity of an authenticated user (no secrets).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthUser {
    pub user_id: String,
    pub username: String,
    pub workspace_id: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    /// Who they are, what they want, and how they work. Defaulted (not absent)
    /// for rows written before the profile existed.
    #[serde(default)]
    pub profile: UserProfile,
}

/// Resolved principal for a session token, fed into `AuthContext`.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    pub user_id: String,
    pub workspace_id: String,
}

#[derive(Serialize, Deserialize)]
struct UserRow {
    user_id: String,
    username: String,
    password_hash: String,
    // The Surreal SDK serializer rejects a serialized `None` (externally-tagged
    // enum) — skip None so it's omitted rather than emitted. See entity_store.rs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    phone: Option<String>,
    workspace_id: String,
    #[serde(default)]
    backup_code_hashes: Vec<String>,
    /// `None` on rows predating user profiles — `auth_users` is SCHEMALESS, so
    /// this needed no migration. Modelled as `Option` rather than a bare
    /// `#[serde(default)]` struct because a SELECT of an absent field comes back
    /// as `null`, which a non-Option field would reject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<UserProfile>,
    created_at: String,
}

#[derive(Serialize, Deserialize)]
struct SessionRow {
    user_id: String,
    workspace_id: String,
    /// Doubles as the API key's human name — an API key *is* a session, and a
    /// key's name plays exactly the role a device label plays for a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_label: Option<String>,
    /// `None` (or absent, on rows predating this field) = interactive session.
    /// `Some("api")` = long-lived scoped API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    /// Stable public handle for the row — for API keys *and* for interactive
    /// sessions — so list/revoke never have to expose the row id (which is the
    /// SHA-256 of the live token). Absent on sessions predating this field:
    /// those cannot be revoked individually, only by "sign out everywhere".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    key_id: Option<String>,
    created_at: String,
    expires_at: String,
    /// Last time this token authenticated a request, refreshed at most once per
    /// [`LAST_USED_REFRESH_SECS`]. Absent on rows predating the field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used_at: Option<String>,
    #[serde(default)]
    revoked: bool,
    /// Set on sessions minted for a paired device (`pairing.rs`), so revoking
    /// the device ends every session it holds. Absent everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
}

/// A live interactive session, as shown on a "where am I signed in" screen.
/// API keys are deliberately not in here — they have their own surface.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub device_label: Option<String>,
    pub created_at: String,
    pub expires_at: String,
    pub last_used_at: Option<String>,
    /// The session making the request. A UI labels this one and doesn't offer
    /// to revoke the chair you're sitting on.
    pub current: bool,
}

/// An API key as shown back to its owner. The raw token is *never* in here —
/// it is returned exactly once, at creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeyInfo {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub expires_at: String,
}

/// Captures a create/delete response's record id. The Surreal SDK can't
/// round-trip a `RecordId` through `serde_json::Value` ("invalid type: enum"),
/// so responses are typed against `RecordId` directly and every read SELECTs
/// explicit fields (never `*`) to keep the id out of the payload entirely.
#[derive(Deserialize)]
struct CreatedId {
    #[allow(dead_code)]
    id: surrealdb::RecordId,
}

// Explicit column lists for every SELECT, so a row never round-trips its
// SCHEMALESS table id — see the `CreatedId` doc comment above for why.
const USER_FIELDS: &str = "user_id, username, password_hash, email, phone, workspace_id, backup_code_hashes, profile, created_at";
const SESSION_FIELDS: &str = "user_id, workspace_id, device_label, kind, key_id, created_at, expires_at, last_used_at, revoked, device_id";
const DEVICE_FIELDS: &str =
    "device_id, user_id, label, platform, created_at, last_seen, public_key";
const APPROVAL_FIELDS: &str = "request_id, user_id, username, code, status, context, created_at, expires_at, session_token, session_token_hash, code_challenge, attempts, consumed";
const PASSKEY_FIELDS: &str = "passkey_id, user_id, passkey_json, created_at";

/// `id` plus the one field that decides whether a handle may be revoked through
/// the session surface or the API-key surface. Read as a pair so neither
/// endpoint can be used to delete the other kind of row.
#[derive(Deserialize)]
struct SessionHandle {
    id: surrealdb::RecordId,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    key_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct PasskeyRow {
    passkey_id: String,
    user_id: String,
    passkey_json: String,
    created_at: String,
}

/// What [`AuthStore::delete_user`] removed. Counts only — it exists so the
/// deletion can be logged and tested, and carries nothing about the account.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeletedUser {
    /// Whether an `auth_users` row existed. That one row holds the username,
    /// password hash, email, phone, recovery-code hashes, and profile.
    pub account: bool,
    /// Interactive sessions (`auth_sessions` rows that are not API keys).
    pub sessions: usize,
    /// API keys (`auth_sessions` rows with `kind = "api"`).
    pub api_keys: usize,
    /// Trusted authenticator devices (`auth_devices`).
    pub devices: usize,
    /// WebAuthn credentials (`auth_passkeys`).
    pub passkeys: usize,
    /// Sign-in approval requests bound to the account (`auth_approvals`).
    pub approvals: usize,
}

/// A stored WebAuthn credential, with the row id needed to write it back.
#[derive(Debug, Clone)]
pub struct StoredPasskey {
    pub passkey_id: String,
    pub passkey_json: String,
}

/// A trusted authenticator device the user has registered (e.g. their phone).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceInfo {
    pub device_id: String,
    pub label: String,
    pub platform: String,
    pub created_at: String,
    /// Paired by scanning the computer's code and holds a device key, so it
    /// can sign back in from anywhere without a password.
    #[serde(default)]
    pub paired: bool,
    #[serde(default)]
    pub last_seen: String,
}

/// What device login needs to verify a signature from a paired device.
#[derive(Debug, Clone)]
pub struct PairedDeviceKey {
    pub user_id: String,
    pub label: String,
    /// Ed25519 public key, base64url without padding.
    pub public_key: String,
}

/// A pending login-approval request, as shown to the authenticator device.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub request_id: String,
    pub code: String,
    pub context: Option<String>,
    pub created_at: String,
    pub expires_at: String,
}

/// Result of starting an approval request (returned to the requesting client).
pub struct ApprovalStart {
    pub request_id: String,
    pub code: String,
    pub expires_at: String,
}

/// What the *approving* side is shown about a request it reached by link rather
/// than from its own pending list. Deliberately thin: enough to decide, and no
/// code — the whole point is that the code comes off the requesting device's
/// screen and is typed in by hand.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDetail {
    pub request_id: String,
    pub status: String,
    pub context: Option<String>,
    pub expires_at: String,
}

/// Why an approve/deny attempt did or didn't take. A flat boolean forced the
/// API to answer 409 for "wrong code", "already approved", and "expired"
/// alike — and gave the approving user no way to tell a typo from an attack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// The request moved to its new state.
    Ok,
    /// No such request, or it isn't the caller's. Deliberately the same answer
    /// for both, so a request id can't be probed for ownership.
    NotFound,
    /// Already approved or denied.
    NotPending,
    Expired,
    /// The six-digit code did not match. The attempt was counted.
    BadCode,
    /// Too many wrong codes; the request is burned.
    TooManyAttempts,
}

#[derive(Serialize, Deserialize)]
struct DeviceRow {
    device_id: String,
    user_id: String,
    label: String,
    platform: String,
    created_at: String,
    last_seen: String,
    /// Ed25519 public key (base64url) for devices paired by code. Absent on
    /// authenticator rows registered the older way, which cannot device-login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_key: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct ApprovalRow {
    request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    user_id: Option<String>,
    username: String,
    code: String,
    status: String, // pending | approved | denied
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context: Option<String>,
    created_at: String,
    expires_at: String,
    /// The minted token, held only until the requesting client collects it.
    /// `poll_approval` clears it in the same statement that marks the row
    /// consumed, so a live 30-day credential never sits in this table.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_token: Option<String>,
    /// Survives collection, so denying an already-approved request can still
    /// revoke the session it minted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_token_hash: Option<String>,
    /// PKCE: base64url(SHA-256(verifier)), supplied when the request started.
    /// Only the holder of the matching verifier can collect the token, so a
    /// leaked request id is not a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code_challenge: Option<String>,
    /// Wrong six-digit codes seen so far.
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    consumed: bool,
}

/// PKCE `S256`: base64url-no-pad of the verifier's SHA-256, exactly as RFC 7636
/// defines it, so an off-the-shelf client implementation interoperates.
pub fn pkce_challenge(verifier: &str) -> String {
    use base64::Engine;
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(h.finalize())
}

/// Stable local id for an external identity. Derived rather than random so the
/// same person signing in on a second machine resolves to the same account, and
/// namespaced by issuer so two issuers' subjects can never collide.
fn external_user_id(issuer: &str, subject: &str) -> String {
    let mut h = Sha256::new();
    h.update(issuer.trim().as_bytes());
    h.update([0u8]);
    h.update(subject.trim().as_bytes());
    // 32 hex chars, matching the shape of a locally-generated `Uuid::simple`.
    hex_lower(&h.finalize())[..32].to_string()
}

/// Short numeric code shown on both the requesting client and the authenticator
/// so the user can confirm they match before approving.
fn gen_approval_code() -> String {
    let n = u128::from_be_bytes(*Uuid::new_v4().as_bytes()) % 1_000_000;
    format!("{n:06}")
}

// An unparseable `expires_at` reads as "not expired" rather than panicking or
// failing the request over a malformed timestamp.
fn approval_expired(row: &ApprovalRow, now: chrono::DateTime<Utc>) -> bool {
    chrono::DateTime::parse_from_rfc3339(&row.expires_at)
        .map(|e| e < now)
        .unwrap_or(false)
}

fn normalize_username(u: &str) -> String {
    u.trim().to_lowercase()
}

/// 3–32 characters, letters/digits/`._-` only. Shared by registration and
/// account-claiming so both paths agree on what a username is.
pub fn validate_username(u: &str) -> Result<()> {
    let len = u.chars().count();
    if !(3..=32).contains(&len) {
        bail!("username must be 3–32 characters");
    }
    if !u
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        bail!("username may only contain letters, numbers, and . _ -");
    }
    Ok(())
}

/// Minimum length only — Argon2 downstream does the actual work of making a
/// weak password expensive to guess.
pub fn validate_password(p: &str) -> Result<()> {
    if p.chars().count() < 8 {
        bail!("password must be at least 8 characters");
    }
    Ok(())
}

fn hash_secret(secret: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("argon2 hash: {e}"))
}

fn verify_secret(secret: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// [`hash_secret`] off the async runtime. Every caller in this file goes through
/// this or [`blocking`]; none may call the sync form from an async fn.
async fn hash_secret_async(secret: String) -> Result<String> {
    blocking(move || hash_secret(&secret)).await?
}

/// [`verify_secret`] off the async runtime. A join error (runtime shutting down)
/// reads as "did not verify" — failing closed is the only safe direction here.
async fn verify_secret_async(secret: String, phc: String) -> bool {
    blocking(move || verify_secret(&secret, &phc))
        .await
        .unwrap_or(false)
}

/// Run CPU-bound password work on the blocking pool.
async fn blocking<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow!("password hashing task failed: {e}"))
}

/// Compare two secrets without leaking their common prefix through timing.
/// Length is compared first and is not itself secret (the code is fixed-width).
fn constant_time_eq(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.ct_eq(b).into()
}

/// Free text from an unauthenticated caller that will be rendered on the
/// approving device. Strip control characters — no ANSI escapes, no newline
/// games in a push notification — and cap the length.
fn sanitize_context(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(APPROVAL_CONTEXT_MAX)
        .collect();
    let trimmed = cleaned.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// Session rows are keyed by this hash rather than the raw token, so a DB leak
// (backup, replica, `SELECT *`) never exposes a usable bearer token.
fn token_hash(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex_lower(&h.finalize())
}

/// `auth_sessions:abc` → `abc`. Surreal hands ids back as `RecordId`; the raw
/// key is what `delete(("auth_sessions", key))` needs.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1.trim_matches('`').to_string())
        .unwrap_or(s)
}

/// 256-bit opaque token (two v4 UUIDs, hex).
fn gen_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Human-friendly one-time recovery codes, e.g. `A1B2C-D3E4F`.
fn gen_backup_codes(n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            let raw = Uuid::new_v4().simple().to_string();
            format!("{}-{}", raw[..5].to_uppercase(), raw[5..10].to_uppercase())
        })
        .collect()
}

fn to_auth_user(r: &UserRow) -> AuthUser {
    AuthUser {
        user_id: r.user_id.clone(),
        username: r.username.clone(),
        workspace_id: r.workspace_id.clone(),
        email: r.email.clone(),
        phone: r.phone.clone(),
        profile: r.profile.clone().unwrap_or_default(),
    }
}

impl AuthStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Define the auth tables and their indexes. Idempotent — safe to call on
    /// every startup, not just the first.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE auth_users SCHEMALESS;
                DEFINE FIELD username ON auth_users TYPE string;
                DEFINE INDEX auth_users_username_idx ON auth_users FIELDS username UNIQUE;
                DEFINE TABLE auth_sessions SCHEMALESS;
                DEFINE TABLE auth_devices SCHEMALESS;
                DEFINE FIELD user_id ON auth_devices TYPE string;
                DEFINE INDEX auth_devices_user_idx ON auth_devices FIELDS user_id;
                DEFINE TABLE auth_approvals SCHEMALESS;
                DEFINE FIELD user_id ON auth_approvals TYPE option<string>;
                DEFINE INDEX auth_approvals_user_idx ON auth_approvals FIELDS user_id;
                DEFINE TABLE auth_passkeys SCHEMALESS;
                DEFINE FIELD user_id ON auth_passkeys TYPE string;
                DEFINE INDEX auth_passkeys_user_idx ON auth_passkeys FIELDS user_id;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("AuthStore schema warning: {}", err);
        }
        Ok(())
    }

    /// Whether `username` is both valid and unclaimed — the live check behind
    /// a registration form's "username available" indicator.
    pub async fn username_available(&self, username: &str) -> Result<bool> {
        let uname = normalize_username(username);
        validate_username(&uname)?;
        Ok(self.get_user_by_username(&uname).await?.is_none())
    }

    async fn get_user_by_username(&self, uname: &str) -> Result<Option<UserRow>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {USER_FIELDS} FROM auth_users WHERE username = $u LIMIT 1"
            ))
            .bind(("u", uname.to_string()))
            .await?;
        let rows: Vec<UserRow> = res.take(0)?;
        Ok(rows.into_iter().next())
    }

    async fn get_user_by_id(&self, user_id: &str) -> Result<Option<UserRow>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {USER_FIELDS} FROM auth_users WHERE user_id = $id LIMIT 1"
            ))
            .bind(("id", user_id.to_string()))
            .await?;
        let rows: Vec<UserRow> = res.take(0)?;
        Ok(rows.into_iter().next())
    }

    /// Public identity for an already-known `user_id`. `None` covers both
    /// "no such user" and any read error — callers treat the two alike.
    pub async fn get_auth_user(&self, user_id: &str) -> Option<AuthUser> {
        self.get_user_by_id(user_id)
            .await
            .ok()
            .flatten()
            .map(|r| to_auth_user(&r))
    }

    /// Register a new user. Returns the public identity plus the plaintext backup
    /// codes (shown to the user exactly once).
    pub async fn register(
        &self,
        username: &str,
        password: &str,
        email: Option<String>,
        phone: Option<String>,
    ) -> Result<(AuthUser, Vec<String>)> {
        let uname = normalize_username(username);
        validate_username(&uname)?;
        validate_password(password)?;
        if self.get_user_by_username(&uname).await?.is_some() {
            bail!("username taken");
        }
        let user_id = Uuid::new_v4().simple().to_string();
        let workspace_id = user_id.clone(); // single-user workspace by default
        let codes = gen_backup_codes(BACKUP_CODE_COUNT);

        // Eleven Argon2id hashes — the password plus every backup code. That is
        // seconds of CPU and ~200 MiB; it goes to the blocking pool as one unit
        // rather than eleven separate hops.
        let to_hash = codes.clone();
        let password = password.to_string();
        let (password_hash, code_hashes) = blocking(move || -> Result<(String, Vec<String>)> {
            let pw = hash_secret(&password)?;
            let codes = to_hash
                .iter()
                .map(|c| hash_secret(c))
                .collect::<Result<Vec<_>>>()?;
            Ok((pw, codes))
        })
        .await??;

        let row = UserRow {
            user_id: user_id.clone(),
            username: uname,
            password_hash,
            email: email
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty()),
            phone: phone
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()),
            workspace_id,
            backup_code_hashes: code_hashes,
            // Written on first run, not at registration — a brand-new account
            // has answered nothing yet.
            profile: None,
            created_at: Utc::now().to_rfc3339(),
        };
        let user = to_auth_user(&row);
        let _: Option<CreatedId> = self
            .db
            .create(("auth_users", user_id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("create user: {e}"))?;
        Ok((user, codes))
    }

    /// Verify a username/password pair. The error is deliberately generic
    /// ("invalid credentials") whether the username is unknown or the
    /// password is wrong, so a login form can't be used to enumerate accounts.
    pub async fn login(&self, username: &str, password: &str) -> Result<AuthUser> {
        let uname = normalize_username(username);
        let row = self
            .get_user_by_username(&uname)
            .await?
            .ok_or_else(|| anyhow!("invalid credentials"))?;
        if !verify_secret_async(password.to_string(), row.password_hash.clone()).await {
            bail!("invalid credentials");
        }
        Ok(to_auth_user(&row))
    }

    /// Recover with a one-time backup code. Consumes the code and returns the
    /// user so the caller can issue a session. To also set a new password in the
    /// same step (which is what a recovery screen actually wants), see
    /// [`AuthStore::reset_password_with_code`].
    pub async fn recover(&self, username: &str, code: &str) -> Result<AuthUser> {
        let row = self.consume_backup_code(username, code).await?;
        Ok(to_auth_user(&row))
    }

    /// Verify and burn one backup code, returning the user row with the code
    /// already removed. The linear scan is up to ten Argon2 verifies, so the
    /// whole scan goes to the blocking pool as one unit.
    async fn consume_backup_code(&self, username: &str, code: &str) -> Result<UserRow> {
        let uname = normalize_username(username);
        let mut row = self
            .get_user_by_username(&uname)
            .await?
            .ok_or_else(|| anyhow!("invalid recovery"))?;
        let code_norm = code.trim().to_uppercase();
        let hashes = row.backup_code_hashes.clone();
        let idx =
            blocking(move || hashes.iter().position(|h| verify_secret(&code_norm, h))).await?;
        let Some(idx) = idx else {
            bail!("invalid recovery");
        };
        row.backup_code_hashes.remove(idx);
        self.db
            .query("UPDATE type::thing('auth_users', $id) SET backup_code_hashes = $codes")
            .bind(("id", row.user_id.clone()))
            .bind(("codes", row.backup_code_hashes.clone()))
            .await
            .map_err(|e| anyhow!("consume backup code: {e}"))?;
        Ok(row)
    }

    // ----- User profile (identity, intent, working style) -----
    //
    // Both calls are keyed by `user_id`, which is the caller's own
    // `principal_id` — there is no path here that reads or writes another
    // user's profile.

    /// Materialise the single-operator account used by token / no-auth
    /// deployments.
    ///
    /// Those modes synthesise a principal (`bearer`, `anonymous`) that has no
    /// `auth_users` row, so every user-scoped read 404s — which silently
    /// disabled the whole profile and first-run surface on exactly the local
    /// setup most operators run. Creating the row on demand keeps the profile
    /// keyed by principal like every other deployment.
    ///
    /// The row is deliberately unloggable: `password_hash` is not a valid PHC
    /// string, and [`verify_secret`] rejects anything it cannot parse, so this
    /// cannot become a passwordless door into a hosted instance.
    pub async fn ensure_local_operator(
        &self,
        user_id: &str,
        workspace_id: &str,
    ) -> Result<AuthUser> {
        if let Some(row) = self.get_user_by_id(user_id).await? {
            return Ok(to_auth_user(&row));
        }
        let row = UserRow {
            user_id: user_id.to_string(),
            username: user_id.to_string(),
            password_hash: String::new(),
            email: None,
            phone: None,
            workspace_id: workspace_id.to_string(),
            backup_code_hashes: Vec::new(),
            profile: None,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        let auth_user = to_auth_user(&row);
        let _: Option<UserRow> = self
            .db
            .upsert(("auth_users", user_id))
            .content(row)
            .await
            .map_err(|e| anyhow!("provision local operator: {e}"))?;
        Ok(auth_user)
    }

    /// Whether an account has credentials of its own yet.
    ///
    /// The local-operator row starts with no password: it is the machine's
    /// owner, reachable because you are sitting at the machine, not because you
    /// know a secret. This reports whether that is still the case, so the UI can
    /// offer to add a password rather than guessing.
    pub async fn has_password(&self, user_id: &str) -> Result<bool> {
        Ok(self
            .get_user_by_id(user_id)
            .await?
            .is_some_and(|row| !row.password_hash.is_empty()))
    }

    /// Give the local-operator account a username and password of its own.
    ///
    /// This is the upgrade from "whoever is sitting at this computer" to "an
    /// account you can sign into from a phone or another browser". It keeps the
    /// same `user_id`, which is the point: every entity, mission, and note
    /// already created on this machine is keyed to it, so claiming the account
    /// carries all of that forward instead of stranding it under a principal
    /// nobody can log in as.
    ///
    /// Refuses if the account already has a password — that is a change, and
    /// changing one requires knowing the old one
    /// ([`AuthStore::change_password`]).
    /// Returns the identity plus one-time recovery codes, shown exactly once.
    pub async fn claim_local_account(
        &self,
        user_id: &str,
        username: &str,
        password: &str,
    ) -> Result<(AuthUser, Vec<String>)> {
        let mut row = self
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| anyhow!("unknown account"))?;
        if !row.password_hash.is_empty() {
            bail!("this account already has a password");
        }
        let uname = normalize_username(username);
        validate_username(&uname)?;
        validate_password(password)?;
        if self
            .get_user_by_username(&uname)
            .await?
            .is_some_and(|existing| existing.user_id != row.user_id)
        {
            bail!("username taken");
        }

        let hash = hash_secret_async(password.to_string()).await?;
        // Backup codes now, not later: the moment this account has a password is
        // the moment it can be locked out of.
        let codes = gen_backup_codes(BACKUP_CODE_COUNT);
        let to_hash = codes.clone();
        let code_hashes = blocking(move || {
            to_hash
                .iter()
                .map(|c| hash_secret(c))
                .collect::<Result<Vec<_>>>()
        })
        .await??;

        self.db
            .query(
                "UPDATE type::thing('auth_users', $id) \
                 SET username = $u, password_hash = $h, backup_code_hashes = $codes",
            )
            .bind(("id", user_id.to_string()))
            .bind(("u", uname.clone()))
            .bind(("h", hash))
            .bind(("codes", code_hashes.clone()))
            .await
            .map_err(|e| anyhow!("claim account: {e}"))?;

        row.username = uname;
        row.backup_code_hashes = code_hashes;
        tracing::info!(
            user_id,
            "local account claimed with a username and password"
        );
        Ok((to_auth_user(&row), codes))
    }

    /// Find or create the local account an external identity maps onto.
    ///
    /// External identity is *additive*. The rest of the gateway keys off a
    /// local `user_id` — entities, missions, the vault all do — so a verified
    /// external subject has to become one of those before it means anything.
    /// This is the same upsert shape as [`AuthStore::ensure_local_operator`],
    /// with the id derived from `issuer + subject` so the same person on a
    /// second machine lands on the same row.
    ///
    /// Like the local-operator row, the account has no usable password hash and
    /// so cannot be logged into with one: it is reachable only by presenting a
    /// token the issuer signed.
    pub async fn ensure_external_user(
        &self,
        issuer: &str,
        subject: &str,
        display_name: Option<&str>,
    ) -> Result<AuthUser> {
        if issuer.trim().is_empty() || subject.trim().is_empty() {
            bail!("external identity needs both an issuer and a subject");
        }
        let user_id = external_user_id(issuer, subject);
        if let Some(row) = self.get_user_by_id(&user_id).await? {
            return Ok(to_auth_user(&row));
        }
        // Deterministic and collision-free against local usernames, which are
        // 3–32 characters of `[a-z0-9._-]` and so can never start with `ext-`
        // followed by exactly this shape... but more to the point, are chosen
        // by people, and this one is not.
        let username = format!("ext-{}", &user_id[..12]);
        let profile = display_name
            .and_then(|n| crate::user_profile::tidy_answer(n, 64))
            .map(|name| UserProfile {
                display_name: Some(name),
                ..Default::default()
            });
        let row = UserRow {
            user_id: user_id.clone(),
            username,
            password_hash: String::new(),
            email: None,
            phone: None,
            workspace_id: user_id.clone(),
            backup_code_hashes: Vec::new(),
            profile,
            created_at: Utc::now().to_rfc3339(),
        };
        let auth_user = to_auth_user(&row);
        let _: Option<UserRow> = self
            .db
            .upsert(("auth_users", user_id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("provision external user: {e}"))?;
        tracing::info!(
            user_id,
            issuer,
            "mapped an external identity onto a local account"
        );
        Ok(auth_user)
    }

    /// The caller's profile. A user with no profile yet gets the default
    /// (working style `Unset`, not onboarded), never an error.
    pub async fn get_user_profile(&self, user_id: &str) -> Result<UserProfile> {
        Ok(self
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| anyhow!("unknown user"))?
            .profile
            .unwrap_or_default())
    }

    /// Overwrite the caller's profile wholesale. Callers that want patch
    /// semantics read first, merge, then write — the merge belongs in the API
    /// layer where the request shape is known.
    pub async fn set_user_profile(&self, user_id: &str, profile: &UserProfile) -> Result<()> {
        self.db
            .query("UPDATE type::thing('auth_users', $id) SET profile = $p")
            .bind(("id", user_id.to_string()))
            .bind(("p", profile.clone()))
            .await
            .map_err(|e| anyhow!("update profile: {e}"))?;
        Ok(())
    }

    /// Change a password, proving knowledge of the current one.
    ///
    /// Requiring the current password is what stops a stolen session token from
    /// becoming permanent ownership of the account: the thief holds a bearer
    /// token, not a password, so they cannot lock the owner out. The caller is
    /// expected to follow a success with [`AuthStore::revoke_all_sessions`],
    /// which evicts the thief.
    ///
    /// The local-operator row (see [`AuthStore::ensure_local_operator`]) has an
    /// empty `password_hash`, which is not a parseable PHC string, so it fails
    /// verification here just as it does at login. That row stays unloggable
    /// and cannot be converted into a password account.
    pub async fn change_password(
        &self,
        user_id: &str,
        current_password: &str,
        new_password: &str,
    ) -> Result<()> {
        let row = self
            .get_user_by_id(user_id)
            .await?
            .ok_or_else(|| anyhow!("unknown user"))?;
        if !verify_secret_async(current_password.to_string(), row.password_hash.clone()).await {
            bail!("current password is incorrect");
        }
        self.set_password(user_id, new_password).await
    }

    /// Whether `password` is this account's current password, for actions that
    /// must re-prove ownership rather than trust a session token alone (account
    /// deletion). The same Argon2 verification login and change-password use,
    /// off the async runtime.
    ///
    /// `Ok(false)` for an unknown user and for a row with no usable hash (the
    /// local operator, an external identity): those never verify. `Err` is a
    /// database failure, which a caller must not read as a wrong password.
    pub async fn verify_password(&self, user_id: &str, password: &str) -> Result<bool> {
        let Some(row) = self.get_user_by_id(user_id).await? else {
            return Ok(false);
        };
        Ok(verify_secret_async(password.to_string(), row.password_hash).await)
    }

    /// Reset a forgotten password with a one-time backup code. This is the only
    /// path that sets a password without the old one, and the code is burned in
    /// the process. Callers must revoke every session on success — a recovery
    /// means the owner had lost access, so anything still signed in is suspect.
    pub async fn reset_password_with_code(
        &self,
        username: &str,
        code: &str,
        new_password: &str,
    ) -> Result<AuthUser> {
        validate_password(new_password)?;
        let row = self.consume_backup_code(username, code).await?;
        self.set_password(&row.user_id, new_password).await?;
        Ok(to_auth_user(&row))
    }

    async fn set_password(&self, user_id: &str, new_password: &str) -> Result<()> {
        validate_password(new_password)?;
        let hash = hash_secret_async(new_password.to_string()).await?;
        self.db
            .query("UPDATE type::thing('auth_users', $id) SET password_hash = $h")
            .bind(("id", user_id.to_string()))
            .bind(("h", hash))
            .await
            .map_err(|e| anyhow!("update password: {e}"))?;
        Ok(())
    }

    /// Mint an opaque session token for a user. The raw token is returned to the
    /// caller; only its SHA-256 is persisted.
    pub async fn create_session(
        &self,
        user_id: &str,
        workspace_id: &str,
        device_label: Option<String>,
    ) -> Result<String> {
        let token = gen_token();
        let now = Utc::now();
        let row = SessionRow {
            user_id: user_id.to_string(),
            workspace_id: workspace_id.to_string(),
            device_label,
            kind: None,
            // Interactive sessions get a public handle too, so a user can see
            // and revoke one device without ever handling the token itself.
            key_id: Some(Uuid::new_v4().simple().to_string()),
            created_at: now.to_rfc3339(),
            expires_at: (now + Duration::days(SESSION_TTL_DAYS)).to_rfc3339(),
            last_used_at: Some(now.to_rfc3339()),
            revoked: false,
            device_id: None,
        };
        let _: Option<CreatedId> = self
            .db
            .create(("auth_sessions", token_hash(&token).as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("create session: {e}"))?;
        Ok(token)
    }

    /// Resolve a bearer token to a principal, or `None` if unknown/revoked/expired.
    pub async fn resolve_session(&self, token: &str) -> Option<SessionIdentity> {
        let hash = token_hash(token);
        let mut res = self
            .db
            .query(format!(
                "SELECT {SESSION_FIELDS} FROM type::thing('auth_sessions', $th)"
            ))
            .bind(("th", hash.clone()))
            .await
            .ok()?;
        let rows: Vec<SessionRow> = res.take(0).ok()?;
        let row = rows.into_iter().next()?;
        if row.revoked {
            return None;
        }
        let now = Utc::now();
        if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&row.expires_at)
            && exp < now
        {
            return None;
        }
        if session_is_idle_expired(
            row.kind.as_deref(),
            &row.created_at,
            row.last_used_at.as_deref(),
            now,
        ) {
            return None;
        }
        self.touch_session(&hash, row.last_used_at.as_deref(), now)
            .await;
        Some(SessionIdentity {
            user_id: row.user_id,
            workspace_id: row.workspace_id,
        })
    }

    /// Record that a session was just used, at most once per
    /// [`LAST_USED_REFRESH_SECS`]. Best-effort: a failed write must never fail
    /// the request it was bookkeeping for.
    async fn touch_session(
        &self,
        token_hash: &str,
        last_used_at: Option<&str>,
        now: chrono::DateTime<Utc>,
    ) {
        let stale = last_used_at
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_none_or(|t| (now - t.with_timezone(&Utc)).num_seconds() >= LAST_USED_REFRESH_SECS);
        if !stale {
            return;
        }
        if let Err(e) = self
            .db
            .query("UPDATE type::thing('auth_sessions', $th) SET last_used_at = $t")
            .bind(("th", token_hash.to_string()))
            .bind(("t", now.to_rfc3339()))
            .await
        {
            tracing::debug!(error = %e, "session last_used_at refresh failed");
        }
    }

    /// Delete a session by its raw token (i.e. what `logout` calls with the
    /// caller's own bearer token).
    pub async fn revoke_session(&self, token: &str) -> Result<()> {
        let _: Option<CreatedId> = self
            .db
            .delete(("auth_sessions", token_hash(token).as_str()))
            .await
            .map_err(|e| anyhow!("delete session: {e}"))?;
        Ok(())
    }

    // ----- Session management ("where am I signed in", "sign out everywhere") -----
    //
    // Interactive sessions only. API keys live in the same table but have their
    // own list/revoke surface, and signing out of every browser must not silently
    // break a user's integrations.

    /// The caller's live interactive sessions, newest first.
    pub async fn list_sessions(
        &self,
        user_id: &str,
        current_token: Option<&str>,
    ) -> Result<Vec<SessionInfo>> {
        let current_handle = match current_token {
            Some(t) => self.session_handle_for_token(t).await?,
            None => None,
        };
        let mut res = self
            .db
            .query(format!(
                "SELECT {SESSION_FIELDS} FROM auth_sessions WHERE user_id = $u"
            ))
            .bind(("u", user_id.to_string()))
            .await?;
        let rows: Vec<SessionRow> = res.take(0)?;
        let now = Utc::now();
        let mut out: Vec<SessionInfo> = rows
            .into_iter()
            .filter(|r| !r.revoked && r.kind.as_deref() != Some(SESSION_KIND_API))
            .filter(|r| {
                chrono::DateTime::parse_from_rfc3339(&r.expires_at)
                    .map(|e| e >= now)
                    .unwrap_or(true)
            })
            .filter_map(|r| {
                let id = r.key_id?;
                Some(SessionInfo {
                    current: current_handle.as_deref() == Some(id.as_str()),
                    id,
                    device_label: r.device_label,
                    created_at: r.created_at,
                    expires_at: r.expires_at,
                    last_used_at: r.last_used_at,
                })
            })
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(out)
    }

    /// The public handle of the session a raw token belongs to.
    async fn session_handle_for_token(&self, token: &str) -> Result<Option<String>> {
        let mut res = self
            .db
            .query("SELECT id, kind, key_id FROM type::thing('auth_sessions', $th)")
            .bind(("th", token_hash(token)))
            .await?;
        let rows: Vec<SessionHandle> = res.take(0)?;
        Ok(rows.into_iter().next().and_then(|r| r.key_id))
    }

    /// Revoke one interactive session the caller owns. Returns false if it isn't
    /// theirs, doesn't exist, or is an API key (those revoke through their own
    /// endpoint, so neither surface can delete the other's rows).
    pub async fn revoke_session_by_id(&self, user_id: &str, session_id: &str) -> Result<bool> {
        let mut res = self
            .db
            .query(
                "SELECT id, kind, key_id FROM auth_sessions \
                 WHERE user_id = $u AND key_id = $id LIMIT 1",
            )
            .bind(("u", user_id.to_string()))
            .bind(("id", session_id.to_string()))
            .await?;
        let rows: Vec<SessionHandle> = res.take(0)?;
        let Some(found) = rows
            .into_iter()
            .find(|r| r.kind.as_deref() != Some(SESSION_KIND_API))
        else {
            return Ok(false);
        };
        let _: Option<CreatedId> = self
            .db
            .delete(("auth_sessions", record_id_key(&found.id).as_str()))
            .await
            .map_err(|e| anyhow!("delete session: {e}"))?;
        Ok(true)
    }

    /// Sign out everywhere. Deletes every interactive session for `user_id`,
    /// optionally sparing the one making the request. Returns how many died.
    ///
    /// This is the control that makes a stolen session recoverable, and it is
    /// what `change-password` calls on success.
    pub async fn revoke_all_sessions(
        &self,
        user_id: &str,
        except_token: Option<&str>,
    ) -> Result<usize> {
        let keep = except_token.map(token_hash);
        let mut res = self
            .db
            .query("SELECT id, kind, key_id FROM auth_sessions WHERE user_id = $u")
            .bind(("u", user_id.to_string()))
            .await?;
        let rows: Vec<SessionHandle> = res.take(0)?;
        let mut killed = 0usize;
        for row in rows {
            if row.kind.as_deref() == Some(SESSION_KIND_API) {
                continue;
            }
            let key = record_id_key(&row.id);
            if keep.as_deref() == Some(key.as_str()) {
                continue;
            }
            let _: Option<CreatedId> = self
                .db
                .delete(("auth_sessions", key.as_str()))
                .await
                .map_err(|e| anyhow!("delete session: {e}"))?;
            killed += 1;
        }
        Ok(killed)
    }

    /// Revoke a session known only by the SHA-256 of its token. The approval
    /// flow keeps that hash so a request denied after it was approved can still
    /// kill the session it minted.
    async fn revoke_session_by_hash(&self, token_hash: &str) -> Result<()> {
        let _: Option<CreatedId> = self
            .db
            .delete(("auth_sessions", token_hash))
            .await
            .map_err(|e| anyhow!("delete session: {e}"))?;
        Ok(())
    }

    // ----- Scoped API keys (long-lived, machine-to-machine) -----
    //
    // An API key is an `auth_sessions` row with `kind = "api"` and a long
    // expiry. No new table, and no new auth path: `resolve_session` already
    // enforces revocation + expiry, so a key works on every `/api/*` route the
    // moment it is minted.

    /// Mint a long-lived API key for `user_id`. Returns the public info plus the
    /// raw token — the *only* time the raw token exists outside the caller.
    pub async fn create_api_key(
        &self,
        user_id: &str,
        workspace_id: &str,
        name: Option<&str>,
    ) -> Result<(ApiKeyInfo, String)> {
        let name = name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("API key")
            .chars()
            .take(API_KEY_NAME_MAX)
            .collect::<String>();
        let token = gen_token();
        let key_id = Uuid::new_v4().simple().to_string();
        let now = Utc::now();
        let created_at = now.to_rfc3339();
        let expires_at = (now + Duration::days(API_KEY_TTL_DAYS)).to_rfc3339();
        let row = SessionRow {
            user_id: user_id.to_string(),
            workspace_id: workspace_id.to_string(),
            device_label: Some(name.clone()),
            kind: Some(SESSION_KIND_API.to_string()),
            key_id: Some(key_id.clone()),
            created_at: created_at.clone(),
            expires_at: expires_at.clone(),
            last_used_at: None,
            revoked: false,
            device_id: None,
        };
        let _: Option<CreatedId> = self
            .db
            .create(("auth_sessions", token_hash(&token).as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("create api key: {e}"))?;
        Ok((
            ApiKeyInfo {
                id: key_id,
                name,
                created_at,
                expires_at,
            },
            token,
        ))
    }

    /// Every live API key the caller owns. Never includes the raw token.
    pub async fn list_api_keys(&self, user_id: &str) -> Result<Vec<ApiKeyInfo>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {SESSION_FIELDS} FROM auth_sessions WHERE user_id = $u AND kind = $k"
            ))
            .bind(("u", user_id.to_string()))
            .bind(("k", SESSION_KIND_API.to_string()))
            .await?;
        let rows: Vec<SessionRow> = res.take(0)?;
        let mut keys: Vec<ApiKeyInfo> = rows
            .into_iter()
            .filter(|r| !r.revoked)
            .filter_map(|r| {
                Some(ApiKeyInfo {
                    id: r.key_id?,
                    name: r.device_label.unwrap_or_else(|| "API key".to_string()),
                    created_at: r.created_at,
                    expires_at: r.expires_at,
                })
            })
            .collect();
        keys.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(keys)
    }

    /// Delete an API key the caller owns. Returns false if it isn't theirs (or
    /// isn't a key), so a stolen id can't revoke someone else's integration.
    pub async fn revoke_api_key(&self, user_id: &str, key_id: &str) -> Result<bool> {
        let mut res = self
            .db
            .query(
                "SELECT id FROM auth_sessions \
                 WHERE user_id = $u AND kind = $k AND key_id = $id LIMIT 1",
            )
            .bind(("u", user_id.to_string()))
            .bind(("k", SESSION_KIND_API.to_string()))
            .bind(("id", key_id.to_string()))
            .await?;
        let rows: Vec<CreatedId> = res.take(0)?;
        let Some(found) = rows.into_iter().next() else {
            return Ok(false);
        };
        let _: Option<CreatedId> = self
            .db
            .delete(("auth_sessions", record_id_key(&found.id).as_str()))
            .await
            .map_err(|e| anyhow!("delete api key: {e}"))?;
        Ok(true)
    }

    // ----- Authenticator devices -----

    /// Register a trusted authenticator device for a user (e.g. their phone).
    pub async fn register_device(
        &self,
        user_id: &str,
        label: &str,
        platform: &str,
    ) -> Result<String> {
        let device_id = Uuid::new_v4().simple().to_string();
        let now = Utc::now().to_rfc3339();
        let row = DeviceRow {
            device_id: device_id.clone(),
            user_id: user_id.to_string(),
            label: label.trim().to_string(),
            platform: platform.trim().to_string(),
            created_at: now.clone(),
            last_seen: now,
            public_key: None,
        };
        let _: Option<CreatedId> = self
            .db
            .create(("auth_devices", device_id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("create device: {e}"))?;
        Ok(device_id)
    }

    /// Every trusted authenticator device registered to the caller.
    pub async fn list_devices(&self, user_id: &str) -> Result<Vec<DeviceInfo>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {DEVICE_FIELDS} FROM auth_devices WHERE user_id = $u"
            ))
            .bind(("u", user_id.to_string()))
            .await?;
        let rows: Vec<DeviceRow> = res.take(0)?;
        Ok(rows
            .into_iter()
            .map(|r| DeviceInfo {
                paired: r.public_key.is_some(),
                device_id: r.device_id,
                label: r.label,
                platform: r.platform,
                created_at: r.created_at,
                last_seen: r.last_seen,
            })
            .collect())
    }

    /// Register a device that proved presence by scanning the computer's
    /// pairing code (`pairing.rs`). Holds an Ed25519 public key so it can
    /// sign in again later without a password.
    pub async fn register_paired_device(
        &self,
        user_id: &str,
        label: &str,
        platform: &str,
        public_key: &str,
    ) -> Result<String> {
        let device_id = Uuid::new_v4().simple().to_string();
        let now = Utc::now().to_rfc3339();
        let row = DeviceRow {
            device_id: device_id.clone(),
            user_id: user_id.to_string(),
            label: label.trim().to_string(),
            platform: platform.trim().to_string(),
            created_at: now.clone(),
            last_seen: now,
            public_key: Some(public_key.to_string()),
        };
        let _: Option<CreatedId> = self
            .db
            .create(("auth_devices", device_id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("create paired device: {e}"))?;
        Ok(device_id)
    }

    /// The key a paired device signs with, or `None` for an unknown or
    /// unpaired device (and for a revoked one, whose row is gone).
    pub async fn paired_device_key(&self, device_id: &str) -> Result<Option<PairedDeviceKey>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {DEVICE_FIELDS} FROM auth_devices WHERE device_id = $d LIMIT 1"
            ))
            .bind(("d", device_id.to_string()))
            .await?;
        let rows: Vec<DeviceRow> = res.take(0)?;
        Ok(rows.into_iter().next().and_then(|r| {
            r.public_key.map(|public_key| PairedDeviceKey {
                user_id: r.user_id,
                label: r.label,
                public_key,
            })
        }))
    }

    /// Mint a session owned by a paired device. Revoking the device revokes it.
    pub async fn create_device_session(
        &self,
        user_id: &str,
        workspace_id: &str,
        device_id: &str,
        label: &str,
    ) -> Result<String> {
        let token = gen_token();
        let now = Utc::now();
        let row = SessionRow {
            user_id: user_id.to_string(),
            workspace_id: workspace_id.to_string(),
            device_label: Some(label.to_string()),
            kind: None,
            key_id: Some(Uuid::new_v4().simple().to_string()),
            created_at: now.to_rfc3339(),
            expires_at: (now + Duration::days(SESSION_TTL_DAYS)).to_rfc3339(),
            last_used_at: Some(now.to_rfc3339()),
            revoked: false,
            device_id: Some(device_id.to_string()),
        };
        let _: Option<CreatedId> = self
            .db
            .create(("auth_sessions", token_hash(&token).as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("create device session: {e}"))?;
        if let Err(e) = self
            .db
            .query("UPDATE type::thing('auth_devices', $d) SET last_seen = $t")
            .bind(("d", device_id.to_string()))
            .bind(("t", now.to_rfc3339()))
            .await
        {
            tracing::debug!(error = %e, "device last_seen refresh failed");
        }
        Ok(token)
    }

    /// Revoke a device the caller owns. Returns false if it isn't theirs.
    pub async fn revoke_device(&self, user_id: &str, device_id: &str) -> Result<bool> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {DEVICE_FIELDS} FROM auth_devices WHERE device_id = $d AND user_id = $u LIMIT 1"
            ))
            .bind(("d", device_id.to_string()))
            .bind(("u", user_id.to_string()))
            .await?;
        let rows: Vec<DeviceRow> = res.take(0)?;
        if rows.is_empty() {
            return Ok(false);
        }
        let _: Option<CreatedId> = self
            .db
            .delete(("auth_devices", device_id))
            .await
            .map_err(|e| anyhow!("delete device: {e}"))?;
        // A paired device's sessions die with it: "remove this phone" must
        // sign the phone out now, not when its token next expires.
        self.db
            .query("UPDATE auth_sessions SET revoked = true WHERE device_id = $d AND user_id = $u")
            .bind(("d", device_id.to_string()))
            .bind(("u", user_id.to_string()))
            .await
            .map_err(|e| anyhow!("revoke device sessions: {e}"))?
            .check()
            .map_err(|e| anyhow!("revoke device sessions: {e}"))?;
        Ok(true)
    }

    // ----- Device-approval login (mobile as authenticator) -----

    /// Start a login-approval request for `username`. Always returns a request +
    /// code (even for an unknown username, to avoid enumeration) — an unknown
    /// user's request simply has no owning device and can never be approved.
    /// Start a login-approval request.
    ///
    /// `username` is optional, and the two cases are different flows:
    ///
    /// - **Named** — "my phone approves my laptop". The request is bound to that
    ///   account up front, and only that account can approve it. An unknown
    ///   username still gets a request back (so the endpoint can't be used to
    ///   enumerate accounts); it simply has no owner and can never be approved.
    /// - **Anonymous** — the desktop-to-browser flow, where the app has no idea
    ///   who is about to sign in. The request is claimed by whoever authenticates
    ///   in the browser and types the code. This is the device-authorization
    ///   shape (RFC 8628), and the code is what binds the two halves together.
    ///
    /// `code_challenge` is PKCE: base64url(SHA-256(verifier)). The verifier must
    /// be presented to collect the token, so learning a request id is not enough
    /// to steal the session it produces.
    pub async fn start_approval(
        &self,
        username: Option<&str>,
        context: Option<String>,
        code_challenge: Option<String>,
    ) -> Result<ApprovalStart> {
        // Nothing else ever deleted from this table. Sweeping here bounds it by
        // the arrival rate inside one two-minute TTL window rather than letting
        // it grow for the life of the deployment.
        self.sweep_approvals().await;

        let uname = username.map(normalize_username).unwrap_or_default();
        let user_id = match username {
            Some(_) => self.get_user_by_username(&uname).await?.map(|u| u.user_id),
            None => None,
        };
        let request_id = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let code = gen_approval_code();
        let now = Utc::now();
        let expires_at = (now + Duration::minutes(2)).to_rfc3339();
        let row = ApprovalRow {
            request_id: request_id.clone(),
            user_id,
            username: uname,
            code: code.clone(),
            status: "pending".to_string(),
            context: context.as_deref().and_then(sanitize_context),
            created_at: now.to_rfc3339(),
            expires_at: expires_at.clone(),
            session_token: None,
            session_token_hash: None,
            code_challenge,
            attempts: 0,
            consumed: false,
        };
        let _: Option<CreatedId> = self
            .db
            .create(("auth_approvals", request_id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("create approval: {e}"))?;
        Ok(ApprovalStart {
            request_id,
            code,
            expires_at,
        })
    }

    /// Requests waiting on `user_id` to approve or deny, e.g. for an
    /// authenticator device's "sign-in requests" screen.
    pub async fn list_pending_approvals(&self, user_id: &str) -> Result<Vec<PendingApproval>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {APPROVAL_FIELDS} FROM auth_approvals WHERE user_id = $u AND status = 'pending'"
            ))
            .bind(("u", user_id.to_string()))
            .await?;
        let rows: Vec<ApprovalRow> = res.take(0)?;
        let now = Utc::now();
        Ok(rows
            .into_iter()
            .filter(|r| !approval_expired(r, now))
            .map(|r| PendingApproval {
                request_id: r.request_id,
                code: r.code,
                context: r.context,
                created_at: r.created_at,
                expires_at: r.expires_at,
            })
            .collect())
    }

    async fn get_approval(&self, request_id: &str) -> Result<Option<ApprovalRow>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {APPROVAL_FIELDS} FROM auth_approvals WHERE request_id = $r LIMIT 1"
            ))
            .bind(("r", request_id.to_string()))
            .await?;
        let rows: Vec<ApprovalRow> = res.take(0)?;
        Ok(rows.into_iter().next())
    }

    /// Approve a pending request (authenticator side), minting a session for the
    /// requesting client.
    ///
    /// `code` is the six digits shown on the requesting client, and it is what
    /// binds this approval to *that* request rather than to any request the
    /// approving user happens to be holding. Without it the digits were
    /// decoration: anyone who could get a request in front of the user could
    /// have it approved by a single confirming tap.
    pub async fn approve_approval(
        &self,
        request_id: &str,
        user_id: &str,
        code: &str,
        device_label: Option<String>,
    ) -> Result<ApprovalOutcome> {
        let Some(row) = self.get_approval(request_id).await? else {
            return Ok(ApprovalOutcome::NotFound);
        };
        // A request that named an account is that account's alone. One that
        // didn't (the browser flow) is claimed here, by whoever authenticated
        // and can type the code off the requesting device's screen.
        match row.user_id.as_deref() {
            Some(owner) if owner != user_id => return Ok(ApprovalOutcome::NotFound),
            _ => {}
        }
        if row.status != "pending" {
            return Ok(ApprovalOutcome::NotPending);
        }
        if approval_expired(&row, Utc::now()) {
            return Ok(ApprovalOutcome::Expired);
        }
        if row.attempts >= APPROVAL_MAX_ATTEMPTS {
            return Ok(ApprovalOutcome::TooManyAttempts);
        }
        if !constant_time_eq(code.trim(), &row.code) {
            let attempts = row.attempts + 1;
            // Burn the request rather than leave a six-digit code standing after
            // a handful of wrong guesses.
            let burned = attempts >= APPROVAL_MAX_ATTEMPTS;
            let status = if burned { "denied" } else { "pending" };
            self.db
                .query("UPDATE type::thing('auth_approvals', $r) SET attempts = $a, status = $s")
                .bind(("r", request_id.to_string()))
                .bind(("a", attempts))
                .bind(("s", status.to_string()))
                .await
                .map_err(|e| anyhow!("record approval attempt: {e}"))?;
            tracing::warn!(
                request_id,
                attempts,
                burned,
                "login approval rejected: wrong code"
            );
            return Ok(if burned {
                ApprovalOutcome::TooManyAttempts
            } else {
                ApprovalOutcome::BadCode
            });
        }

        let workspace_id = self
            .get_user_by_id(user_id)
            .await?
            .map(|u| u.workspace_id)
            .unwrap_or_else(|| user_id.to_string());
        let token = self
            .create_session(user_id, &workspace_id, device_label)
            .await?;
        let hash = token_hash(&token);
        self.db
            .query(
                "UPDATE type::thing('auth_approvals', $r) \
                 SET status = 'approved', session_token = $t, session_token_hash = $h, user_id = $u",
            )
            .bind(("r", request_id.to_string()))
            .bind(("t", token))
            .bind(("h", hash))
            .bind(("u", user_id.to_string()))
            .await
            .map_err(|e| anyhow!("approve: {e}"))?;
        Ok(ApprovalOutcome::Ok)
    }

    /// Deny a request. Denying one that was already approved *revokes the
    /// session it minted* — the case this matters in is "I tapped approve by
    /// accident", where a silent no-op leaves the attacker signed in.
    pub async fn deny_approval(&self, request_id: &str, user_id: &str) -> Result<ApprovalOutcome> {
        let Some(row) = self.get_approval(request_id).await? else {
            return Ok(ApprovalOutcome::NotFound);
        };
        if row.user_id.as_deref() != Some(user_id) {
            return Ok(ApprovalOutcome::NotFound);
        }
        if let Some(hash) = row.session_token_hash.as_deref() {
            self.revoke_session_by_hash(hash).await?;
            tracing::info!(
                request_id,
                "denied an approved login; its session was revoked"
            );
        }
        self.db
            .query(
                "UPDATE type::thing('auth_approvals', $r) \
                 SET status = 'denied', session_token = NONE, session_token_hash = NONE",
            )
            .bind(("r", request_id.to_string()))
            .await
            .map_err(|e| anyhow!("deny: {e}"))?;
        Ok(ApprovalOutcome::Ok)
    }

    /// Read a request the approving side reached by link. No code, and no
    /// token: this is what the browser shows before the user confirms.
    pub async fn approval_detail(&self, request_id: &str) -> Result<Option<ApprovalDetail>> {
        let Some(row) = self.get_approval(request_id).await? else {
            return Ok(None);
        };
        let status = if row.status == "pending" && approval_expired(&row, Utc::now()) {
            "expired".to_string()
        } else {
            row.status
        };
        Ok(Some(ApprovalDetail {
            request_id: row.request_id,
            status,
            context: row.context,
            expires_at: row.expires_at,
        }))
    }

    /// Poll by the requesting client. Returns `(status, token)` where the session
    /// token is handed back exactly once on approval, and cleared from the row in
    /// the same statement that marks it consumed — the table must never hold a
    /// live 30-day credential in plaintext once it has been collected.
    ///
    /// `verifier` is the PKCE secret matching the challenge given at start. A
    /// request id travels through a browser URL and a polling loop; the verifier
    /// never leaves the client that started the request, so it — not the id —
    /// is what proves this is the same client coming back for its token.
    pub async fn poll_approval(
        &self,
        request_id: &str,
        verifier: Option<&str>,
    ) -> Result<(String, Option<String>)> {
        let Some(row) = self.get_approval(request_id).await? else {
            return Ok(("unknown".to_string(), None));
        };
        if let Some(challenge) = row.code_challenge.as_deref() {
            let presented = verifier.map(pkce_challenge);
            if presented.as_deref() != Some(challenge) {
                tracing::warn!(request_id, "approval polled without its PKCE verifier");
                // Indistinguishable from "no such request", so a stolen id
                // learns nothing about whether it was ever approved.
                return Ok(("unknown".to_string(), None));
            }
        }
        if row.status == "pending" && approval_expired(&row, Utc::now()) {
            return Ok(("expired".to_string(), None));
        }
        if row.status == "approved" && !row.consumed {
            self.db
                .query(
                    "UPDATE type::thing('auth_approvals', $r) \
                     SET consumed = true, session_token = NONE",
                )
                .bind(("r", request_id.to_string()))
                .await
                .map_err(|e| anyhow!("consume approval: {e}"))?;
            return Ok(("approved".to_string(), row.session_token));
        }
        Ok((row.status, None))
    }

    /// Delete approval rows that are finished with: collected, or past their
    /// expiry (which covers denied ones, kept until then so the requesting
    /// client learns it was refused rather than seeing the request vanish).
    /// Best-effort — a sweep failure must not fail the request that triggered it.
    pub async fn sweep_approvals(&self) {
        let read = async {
            let mut res = self
                .db
                .query(format!("SELECT {APPROVAL_FIELDS} FROM auth_approvals"))
                .await?;
            let rows: Vec<ApprovalRow> = res.take(0)?;
            Ok::<_, anyhow::Error>(rows)
        };
        let rows = match read.await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!(error = %e, "approval sweep read failed");
                return;
            }
        };
        let now = Utc::now();
        for row in rows {
            let done = row.consumed || approval_expired(&row, now);
            if !done {
                continue;
            }
            let res: Result<Option<CreatedId>, _> = self
                .db
                .delete(("auth_approvals", row.request_id.as_str()))
                .await;
            if let Err(e) = res {
                tracing::debug!(error = %e, "approval sweep delete failed");
            }
        }
    }

    // ----- Passkeys (WebAuthn credentials, stored as JSON) -----

    /// Resolve a username to its user id (public helper for passkey login).
    pub async fn user_id_for_username(&self, username: &str) -> Result<Option<String>> {
        let uname = normalize_username(username);
        Ok(self.get_user_by_username(&uname).await?.map(|u| u.user_id))
    }

    /// Store a newly-registered WebAuthn credential for `user_id`.
    pub async fn add_passkey(&self, user_id: &str, passkey_json: &str) -> Result<()> {
        let passkey_id = Uuid::new_v4().simple().to_string();
        let row = PasskeyRow {
            passkey_id: passkey_id.clone(),
            user_id: user_id.to_string(),
            passkey_json: passkey_json.to_string(),
            created_at: Utc::now().to_rfc3339(),
        };
        let _: Option<CreatedId> = self
            .db
            .create(("auth_passkeys", passkey_id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow!("create passkey: {e}"))?;
        Ok(())
    }

    /// Serialized `Passkey` JSON blobs for a user (deserialized by the caller).
    pub async fn list_passkey_json(&self, user_id: &str) -> Result<Vec<String>> {
        Ok(self
            .list_passkeys(user_id)
            .await?
            .into_iter()
            .map(|p| p.passkey_json)
            .collect())
    }

    /// A user's passkeys with their row ids, so a login can write the updated
    /// credential (the sign counter) back to the one that was actually used.
    pub async fn list_passkeys(&self, user_id: &str) -> Result<Vec<StoredPasskey>> {
        let mut res = self
            .db
            .query(format!(
                "SELECT {PASSKEY_FIELDS} FROM auth_passkeys WHERE user_id = $u"
            ))
            .bind(("u", user_id.to_string()))
            .await?;
        let rows: Vec<PasskeyRow> = res.take(0)?;
        Ok(rows
            .into_iter()
            .map(|r| StoredPasskey {
                passkey_id: r.passkey_id,
                passkey_json: r.passkey_json,
            })
            .collect())
    }

    /// Persist a credential after a successful assertion. This is what makes
    /// cloned-authenticator detection work at all: `webauthn-rs` compares the
    /// asserted sign counter against the stored one, so a counter that is never
    /// written back means the check always compares against zero.
    pub async fn update_passkey(&self, passkey_id: &str, passkey_json: &str) -> Result<()> {
        self.db
            .query("UPDATE type::thing('auth_passkeys', $id) SET passkey_json = $j")
            .bind(("id", passkey_id.to_string()))
            .bind(("j", passkey_json.to_string()))
            .await
            .map_err(|e| anyhow!("update passkey: {e}"))?;
        Ok(())
    }

    // ----- Account deletion -----

    /// Remove every row this store holds for `user_id`, in one transaction.
    ///
    /// That is all five auth tables:
    ///
    /// - `auth_users` — the account row, which is also where the username (and
    ///   so its unique-index claim), password hash, email, phone, recovery-code
    ///   hashes, and [`UserProfile`] live. There is no separate username or
    ///   profile table to clean up.
    /// - `auth_sessions` — every interactive session *and* every API key, since
    ///   a key is a session row with `kind = "api"`.
    /// - `auth_devices` — trusted authenticator devices.
    /// - `auth_passkeys` — WebAuthn credentials.
    /// - `auth_approvals` — sign-in approval requests bound to the account,
    ///   whether named for it up front or claimed by it. Matched on `user_id`
    ///   and, while the row still exists, on the stored username; the anonymous
    ///   browser-flow requests nobody has claimed (empty username, no owner)
    ///   belong to no one and are left for the sweep.
    ///
    /// The username is read *before* the transaction because the approval match
    /// needs it and the transaction deletes the row it comes from.
    ///
    /// Idempotent: a second call finds nothing and returns all zeroes. This
    /// does not verify anything — re-authentication is the caller's job (see
    /// `DELETE /api/auth/account`), and it touches nothing outside these tables.
    pub async fn delete_user(&self, user_id: &str) -> Result<DeletedUser> {
        let username = self
            .get_user_by_id(user_id)
            .await?
            .map(|row| row.username)
            .filter(|name| !name.is_empty());
        // The clause is one of two constant strings, never user text; the
        // username itself is always a bound parameter.
        let approvals_match = if username.is_some() {
            "user_id = $u OR username = $name"
        } else {
            "user_id = $u"
        };

        // Count first, for the summary. The deletes below match on the same
        // predicates, so anything created in between is still removed — it is
        // only missing from the count.
        let mut count = self
            .db
            .query(format!(
                "SELECT VALUE user_id FROM auth_users WHERE user_id = $u; \
                 SELECT VALUE kind FROM auth_sessions WHERE user_id = $u; \
                 SELECT VALUE device_id FROM auth_devices WHERE user_id = $u; \
                 SELECT VALUE passkey_id FROM auth_passkeys WHERE user_id = $u; \
                 SELECT VALUE request_id FROM auth_approvals WHERE {approvals_match};"
            ))
            .bind(("u", user_id.to_string()));
        if let Some(name) = username.clone() {
            count = count.bind(("name", name));
        }
        let mut res = count.await.map_err(|e| anyhow!("count account rows: {e}"))?;
        let accounts: Vec<String> = res.take(0)?;
        let kinds: Vec<Option<String>> = res.take(1)?;
        let devices: Vec<String> = res.take(2)?;
        let passkeys: Vec<String> = res.take(3)?;
        let approvals: Vec<String> = res.take(4)?;
        let api_keys = kinds
            .iter()
            .filter(|k| k.as_deref() == Some(SESSION_KIND_API))
            .count();

        // One transaction, so a failure part-way cannot leave an account that
        // still has its password but has lost its sessions, or the reverse.
        let mut delete = self
            .db
            .query(format!(
                "BEGIN TRANSACTION; \
                 DELETE auth_users WHERE user_id = $u; \
                 DELETE auth_sessions WHERE user_id = $u; \
                 DELETE auth_devices WHERE user_id = $u; \
                 DELETE auth_passkeys WHERE user_id = $u; \
                 DELETE auth_approvals WHERE {approvals_match}; \
                 COMMIT TRANSACTION;"
            ))
            .bind(("u", user_id.to_string()));
        if let Some(name) = username {
            delete = delete.bind(("name", name));
        }
        delete
            .await
            .map_err(|e| anyhow!("delete account: {e}"))?
            .check()
            .map_err(|e| anyhow!("delete account: {e}"))?;

        Ok(DeletedUser {
            account: !accounts.is_empty(),
            sessions: kinds.len() - api_keys,
            api_keys,
            devices: devices.len(),
            passkeys: passkeys.len(),
            approvals: approvals.len(),
        })
    }
}

#[cfg(test)]
mod idle_expiry_tests {
    use super::{SESSION_KIND_API, session_is_idle_expired};
    use chrono::{Duration, Utc};

    #[test]
    fn a_session_in_regular_use_never_idles_out() {
        let now = Utc::now();
        let yesterday = (now - Duration::days(1)).to_rfc3339();
        assert!(!session_is_idle_expired(
            None,
            &(now - Duration::days(29)).to_rfc3339(),
            Some(&yesterday),
            now
        ));
    }

    #[test]
    fn a_long_abandoned_session_stops_working_before_its_absolute_expiry() {
        let now = Utc::now();
        // Created three weeks ago (still inside the 30-day TTL) but untouched
        // for the last three weeks: the stolen-phone case.
        let stale = (now - Duration::days(21)).to_rfc3339();
        assert!(session_is_idle_expired(None, &stale, Some(&stale), now));
    }

    #[test]
    fn a_never_used_session_ages_from_its_creation() {
        let now = Utc::now();
        assert!(session_is_idle_expired(
            None,
            &(now - Duration::days(20)).to_rfc3339(),
            None,
            now
        ));
        assert!(!session_is_idle_expired(
            None,
            &(now - Duration::days(2)).to_rfc3339(),
            None,
            now
        ));
    }

    #[test]
    fn api_keys_are_exempt_because_revocation_is_their_kill_switch() {
        let now = Utc::now();
        let ancient = (now - Duration::days(300)).to_rfc3339();
        assert!(!session_is_idle_expired(
            Some(SESSION_KIND_API),
            &ancient,
            Some(&ancient),
            now
        ));
    }

    #[test]
    fn a_malformed_timestamp_never_locks_someone_out() {
        let now = Utc::now();
        assert!(!session_is_idle_expired(
            None,
            "not-a-date",
            Some("junk"),
            now
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn username_validation() {
        assert!(validate_username("moe_baqshi").is_ok());
        assert!(validate_username("ab").is_err()); // too short
        assert!(validate_username("has space").is_err());
        assert!(validate_username("bad/slash").is_err());
    }

    #[test]
    fn password_hash_roundtrip() {
        let phc = hash_secret("correct horse battery").unwrap();
        assert!(verify_secret("correct horse battery", &phc));
        assert!(!verify_secret("wrong password", &phc));
    }

    #[test]
    fn backup_codes_are_unique_and_formatted() {
        let codes = gen_backup_codes(10);
        assert_eq!(codes.len(), 10);
        for c in &codes {
            assert_eq!(c.len(), 11); // XXXXX-XXXXX
            assert!(c.contains('-'));
        }
        // extremely unlikely to collide
        let mut sorted = codes.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), codes.len());
    }

    async fn mem_store() -> Arc<AuthStore> {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = AuthStore::new(Arc::new(db));
        store.init_schema().await.expect("init");
        store
    }

    #[tokio::test]
    async fn local_operator_is_provisioned_once_and_cannot_be_logged_into() {
        let store = mem_store().await;

        // Token / no-auth deployments synthesise this principal, so it has no
        // account until something asks for one.
        assert!(store.get_auth_user("bearer").await.is_none());

        let created = store
            .ensure_local_operator("bearer", "default")
            .await
            .expect("provision");
        assert_eq!(created.user_id, "bearer");
        assert_eq!(created.workspace_id, "default");

        // The profile surface works from here on.
        let profile = store.get_user_profile("bearer").await.expect("profile");
        assert_eq!(
            profile.working_style,
            crate::user_profile::WorkingStyle::Unset
        );

        // Idempotent: a second call must not clobber a profile written between
        // the two, which would silently reset the operator's onboarding.
        let mut edited = profile.clone();
        edited.working_style = crate::user_profile::WorkingStyle::Linear;
        store
            .set_user_profile("bearer", &edited)
            .await
            .expect("write profile");
        store
            .ensure_local_operator("bearer", "default")
            .await
            .expect("second call");
        assert_eq!(
            store
                .get_user_profile("bearer")
                .await
                .expect("reread")
                .working_style,
            crate::user_profile::WorkingStyle::Linear
        );

        // The row is not a passwordless door.
        assert!(
            store.login("bearer", "").await.is_err(),
            "an empty password must never authenticate the local operator row"
        );
    }

    #[tokio::test]
    async fn api_key_round_trip() {
        let store = mem_store().await;
        let (user, _codes) = store
            .register("keyowner", "correct horse battery", None, None)
            .await
            .expect("register");

        // Mint.
        let (info, token) = store
            .create_api_key(&user.user_id, &user.workspace_id, Some("  Zapier  "))
            .await
            .expect("create key");
        assert_eq!(info.name, "Zapier"); // trimmed
        assert_eq!(token.len(), 64); // two hex uuids

        // Resolves exactly like an interactive session.
        let identity = store.resolve_session(&token).await.expect("resolves");
        assert_eq!(identity.user_id, user.user_id);
        assert_eq!(identity.workspace_id, user.workspace_id);

        // Listed, without the raw token anywhere in the payload.
        let keys = store.list_api_keys(&user.user_id).await.expect("list");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].id, info.id);
        assert_eq!(keys[0].name, "Zapier");
        let dump = serde_json::to_string(&keys).unwrap();
        assert!(!dump.contains(&token), "raw token must never be listed");

        // Long expiry — an integration shouldn't die at the session horizon.
        let exp = chrono::DateTime::parse_from_rfc3339(&keys[0].expires_at).unwrap();
        assert!(exp > Utc::now() + Duration::days(3000));

        // Revoke: gone from the list, and no longer authenticates.
        assert!(
            store
                .revoke_api_key(&user.user_id, &info.id)
                .await
                .expect("revoke")
        );
        assert!(
            store
                .list_api_keys(&user.user_id)
                .await
                .expect("list")
                .is_empty()
        );
        assert!(store.resolve_session(&token).await.is_none());
    }

    #[tokio::test]
    async fn api_keys_are_scoped_to_their_owner() {
        let store = mem_store().await;
        let (alice, _) = store
            .register("alice", "correct horse battery", None, None)
            .await
            .expect("register alice");
        let (bob, _) = store
            .register("bob", "correct horse battery", None, None)
            .await
            .expect("register bob");
        let (info, token) = store
            .create_api_key(&alice.user_id, &alice.workspace_id, Some("ci"))
            .await
            .expect("create key");

        // Bob can neither see nor revoke Alice's key.
        assert!(store.list_api_keys(&bob.user_id).await.unwrap().is_empty());
        assert!(!store.revoke_api_key(&bob.user_id, &info.id).await.unwrap());
        assert!(store.resolve_session(&token).await.is_some());
    }

    #[tokio::test]
    async fn interactive_sessions_are_not_api_keys() {
        let store = mem_store().await;
        let (user, _) = store
            .register("sessiononly", "correct horse battery", None, None)
            .await
            .expect("register");
        let token = store
            .create_session(&user.user_id, &user.workspace_id, Some("laptop".into()))
            .await
            .expect("session");
        assert!(store.resolve_session(&token).await.is_some());
        assert!(store.list_api_keys(&user.user_id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn user_profile_round_trips() {
        use crate::user_profile::{OnboardingAnswers, WorkingStyle};

        let store = mem_store().await;
        let (user, _) = store
            .register("profileowner", "correct horse battery", None, None)
            .await
            .expect("register");

        // A fresh account has a default profile, not an error and not `None`.
        assert_eq!(user.profile, crate::user_profile::UserProfile::default());
        let initial = store
            .get_user_profile(&user.user_id)
            .await
            .expect("read fresh profile");
        assert_eq!(initial.working_style, WorkingStyle::Unset);
        assert!(!initial.is_onboarded());

        let profile = UserProfile {
            display_name: Some("Moe".to_string()),
            role: Some("Founder, building an operating system".to_string()),
            primary_intent: Some("Keep my launches from slipping".to_string()),
            working_style: WorkingStyle::Linear,
            onboarded_at: Some("2026-08-01T09:00:00+00:00".to_string()),
            show_all_nav: true,
            answers: OnboardingAnswers {
                identity: Some("Founder".to_string()),
                intent: Some("Ship one thing".to_string()),
                focus_choice: Some("linear".to_string()),
                ..Default::default()
            },
        };
        store
            .set_user_profile(&user.user_id, &profile)
            .await
            .expect("write profile");

        let read_back = store
            .get_user_profile(&user.user_id)
            .await
            .expect("read profile");
        assert_eq!(read_back, profile);
        assert!(read_back.is_onboarded());

        // It also travels on the identity every auth path already returns.
        let via_user = store.get_auth_user(&user.user_id).await.expect("auth user");
        assert_eq!(via_user.profile.working_style, WorkingStyle::Linear);
        // ...and survives a login, which re-reads the row from scratch.
        let via_login = store
            .login("profileowner", "correct horse battery")
            .await
            .expect("login");
        assert_eq!(via_login.profile, profile);
    }

    #[tokio::test]
    async fn user_profiles_are_scoped_to_their_owner() {
        use crate::user_profile::WorkingStyle;

        let store = mem_store().await;
        let (alice, _) = store
            .register("alicep", "correct horse battery", None, None)
            .await
            .expect("register alice");
        let (bob, _) = store
            .register("bobp", "correct horse battery", None, None)
            .await
            .expect("register bob");

        store
            .set_user_profile(
                &alice.user_id,
                &UserProfile {
                    working_style: WorkingStyle::Linear,
                    role: Some("alice only".to_string()),
                    ..Default::default()
                },
            )
            .await
            .expect("write alice profile");

        // Writing Alice's profile leaves Bob's untouched.
        let bobs = store.get_user_profile(&bob.user_id).await.expect("bob");
        assert_eq!(bobs, UserProfile::default());
        assert_eq!(
            store
                .get_user_profile(&alice.user_id)
                .await
                .expect("alice")
                .role
                .as_deref(),
            Some("alice only")
        );
    }

    // ----- Session revocation -----

    #[tokio::test]
    async fn revoke_all_invalidates_a_token_that_previously_resolved() {
        let store = mem_store().await;
        let (user, _) = store
            .register("evictme", "correct horse battery", None, None)
            .await
            .expect("register");

        let phone = store
            .create_session(&user.user_id, &user.workspace_id, Some("phone".into()))
            .await
            .expect("phone session");
        let laptop = store
            .create_session(&user.user_id, &user.workspace_id, Some("laptop".into()))
            .await
            .expect("laptop session");
        assert!(store.resolve_session(&phone).await.is_some());
        assert!(store.resolve_session(&laptop).await.is_some());

        // Sign out everywhere, keeping the laptop we're sitting at.
        let killed = store
            .revoke_all_sessions(&user.user_id, Some(&laptop))
            .await
            .expect("revoke all");
        assert_eq!(killed, 1);
        assert!(
            store.resolve_session(&phone).await.is_none(),
            "a revoked token must stop authenticating"
        );
        assert!(
            store.resolve_session(&laptop).await.is_some(),
            "the caller's own session must survive"
        );
    }

    #[tokio::test]
    async fn signing_out_everywhere_leaves_api_keys_alone() {
        let store = mem_store().await;
        let (user, _) = store
            .register("keeperofkeys", "correct horse battery", None, None)
            .await
            .expect("register");
        let (_, key) = store
            .create_api_key(&user.user_id, &user.workspace_id, Some("ci"))
            .await
            .expect("key");
        let session = store
            .create_session(&user.user_id, &user.workspace_id, None)
            .await
            .expect("session");

        store
            .revoke_all_sessions(&user.user_id, None)
            .await
            .expect("revoke all");
        assert!(store.resolve_session(&session).await.is_none());
        assert!(
            store.resolve_session(&key).await.is_some(),
            "signing out of every browser must not break integrations"
        );
    }

    #[tokio::test]
    async fn sessions_are_listable_revocable_and_scoped_to_their_owner() {
        let store = mem_store().await;
        let (alice, _) = store
            .register("alices", "correct horse battery", None, None)
            .await
            .expect("register alice");
        let (bob, _) = store
            .register("bobs", "correct horse battery", None, None)
            .await
            .expect("register bob");

        let current = store
            .create_session(&alice.user_id, &alice.workspace_id, Some("laptop".into()))
            .await
            .expect("session");
        let other = store
            .create_session(&alice.user_id, &alice.workspace_id, Some("phone".into()))
            .await
            .expect("session");
        // An API key shares the table but is not a session.
        store
            .create_api_key(&alice.user_id, &alice.workspace_id, Some("ci"))
            .await
            .expect("key");

        let listed = store
            .list_sessions(&alice.user_id, Some(&current))
            .await
            .expect("list");
        assert_eq!(listed.len(), 2, "API keys must not appear as sessions");
        assert_eq!(listed.iter().filter(|s| s.current).count(), 1);
        assert!(listed.iter().all(|s| s.last_used_at.is_some()));
        let dump = serde_json::to_string(&listed).unwrap();
        assert!(!dump.contains(&current), "raw tokens must never be listed");

        let phone = listed
            .iter()
            .find(|s| s.device_label.as_deref() == Some("phone"))
            .expect("phone listed");

        // Bob can neither see nor revoke Alice's sessions.
        assert!(
            store
                .list_sessions(&bob.user_id, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !store
                .revoke_session_by_id(&bob.user_id, &phone.id)
                .await
                .unwrap()
        );
        assert!(store.resolve_session(&other).await.is_some());

        assert!(
            store
                .revoke_session_by_id(&alice.user_id, &phone.id)
                .await
                .unwrap()
        );
        assert!(store.resolve_session(&other).await.is_none());
    }

    // ----- change-password -----

    #[tokio::test]
    async fn changing_a_password_requires_the_current_one() {
        let store = mem_store().await;
        let (user, _) = store
            .register("pwowner", "correct horse battery", None, None)
            .await
            .expect("register");

        // What a stolen session token buys you: nothing here.
        assert!(
            store
                .change_password(&user.user_id, "not the password", "brand new password")
                .await
                .is_err(),
            "a session token alone must not be enough to take over the account"
        );
        assert!(
            store
                .login("pwowner", "correct horse battery")
                .await
                .is_ok(),
            "the failed attempt must not have changed anything"
        );

        store
            .change_password(&user.user_id, "correct horse battery", "brand new password")
            .await
            .expect("change with the current password");
        assert!(store.login("pwowner", "brand new password").await.is_ok());
        assert!(
            store
                .login("pwowner", "correct horse battery")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn the_local_operator_row_cannot_be_given_a_password() {
        let store = mem_store().await;
        store
            .ensure_local_operator("bearer", "default")
            .await
            .expect("provision");
        // Its `password_hash` is empty and unparseable, so there is no current
        // password to present — the row stays unloggable, as documented.
        assert!(
            store
                .change_password("bearer", "", "a brand new password")
                .await
                .is_err()
        );
        assert!(store.login("bearer", "a brand new password").await.is_err());
    }

    #[tokio::test]
    async fn a_backup_code_resets_the_password_and_is_burned() {
        let store = mem_store().await;
        let (user, codes) = store
            .register("forgetful", "correct horse battery", None, None)
            .await
            .expect("register");

        let reset = store
            .reset_password_with_code(&user.username, &codes[0], "a whole new password")
            .await
            .expect("reset");
        assert_eq!(reset.user_id, user.user_id);
        assert!(
            store
                .login("forgetful", "a whole new password")
                .await
                .is_ok()
        );
        assert!(
            store
                .login("forgetful", "correct horse battery")
                .await
                .is_err()
        );

        // One-time: the same code cannot be replayed.
        assert!(
            store
                .reset_password_with_code(&user.username, &codes[0], "third password")
                .await
                .is_err()
        );
        // A different, unused code still works.
        assert!(
            store
                .reset_password_with_code(&user.username, &codes[1], "fourth password")
                .await
                .is_ok()
        );
        // A too-short password is refused before the code is spent.
        assert!(
            store
                .reset_password_with_code(&user.username, &codes[2], "short")
                .await
                .is_err()
        );
        assert!(
            store
                .reset_password_with_code(&user.username, &codes[2], "a long enough password")
                .await
                .is_ok(),
            "a rejected password must not burn the recovery code"
        );
    }

    // ----- Login approval -----

    /// Drive an approval to the point where the requester is polling.
    async fn pending_approval(store: &Arc<AuthStore>, username: &str) -> (AuthUser, ApprovalStart) {
        let (user, _) = store
            .register(username, "correct horse battery", None, None)
            .await
            .expect("register");
        let start = store
            .start_approval(Some(username), Some("Desktop app".into()), None)
            .await
            .expect("start");
        (user, start)
    }

    #[tokio::test]
    async fn approving_requires_the_code_shown_on_the_requesting_client() {
        let store = mem_store().await;
        let (user, start) = pending_approval(&store, "approver").await;

        // A wrong code approves nothing — this is the check that stops a
        // one-tap "yes" from authorising a request the user never saw.
        assert_eq!(
            store
                .approve_approval(&start.request_id, &user.user_id, "000000", None)
                .await
                .unwrap(),
            ApprovalOutcome::BadCode
        );
        assert_eq!(
            store.poll_approval(&start.request_id, None).await.unwrap(),
            ("pending".to_string(), None)
        );

        assert_eq!(
            store
                .approve_approval(&start.request_id, &user.user_id, &start.code, None)
                .await
                .unwrap(),
            ApprovalOutcome::Ok
        );
        let (status, token) = store.poll_approval(&start.request_id, None).await.unwrap();
        assert_eq!(status, "approved");
        assert!(store.resolve_session(&token.unwrap()).await.is_some());
    }

    #[tokio::test]
    async fn a_guessed_code_burns_the_request() {
        let store = mem_store().await;
        let (user, start) = pending_approval(&store, "guessedat").await;

        for _ in 0..(APPROVAL_MAX_ATTEMPTS - 1) {
            assert_eq!(
                store
                    .approve_approval(&start.request_id, &user.user_id, "000000", None)
                    .await
                    .unwrap(),
                ApprovalOutcome::BadCode
            );
        }
        assert_eq!(
            store
                .approve_approval(&start.request_id, &user.user_id, "000000", None)
                .await
                .unwrap(),
            ApprovalOutcome::TooManyAttempts
        );
        // Even the right code is no good now: six digits do not survive an
        // unbounded number of guesses inside the two-minute window.
        assert_eq!(
            store
                .approve_approval(&start.request_id, &user.user_id, &start.code, None)
                .await
                .unwrap(),
            ApprovalOutcome::NotPending
        );
    }

    #[tokio::test]
    async fn an_approval_is_only_the_owners_to_grant() {
        let store = mem_store().await;
        let (_, start) = pending_approval(&store, "victimuser").await;
        let (attacker, _) = store
            .register("attackeruser", "correct horse battery", None, None)
            .await
            .expect("register");

        // Knowing the request id and even the code buys an outsider nothing.
        assert_eq!(
            store
                .approve_approval(&start.request_id, &attacker.user_id, &start.code, None)
                .await
                .unwrap(),
            ApprovalOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn a_collected_token_is_cleared_from_the_approval_row() {
        let store = mem_store().await;
        let (user, start) = pending_approval(&store, "hygienic").await;
        store
            .approve_approval(&start.request_id, &user.user_id, &start.code, None)
            .await
            .unwrap();

        let (_, token) = store.poll_approval(&start.request_id, None).await.unwrap();
        assert!(token.is_some());

        // The row must not still be holding a live 30-day credential.
        let row = store
            .get_approval(&start.request_id)
            .await
            .unwrap()
            .expect("row");
        assert!(row.consumed);
        assert!(
            row.session_token.is_none(),
            "the plaintext token must be cleared as it is handed over"
        );
        // A second poll hands out nothing.
        assert_eq!(
            store
                .poll_approval(&start.request_id, None)
                .await
                .unwrap()
                .1,
            None
        );
    }

    #[tokio::test]
    async fn denying_an_accidental_approval_revokes_its_session() {
        let store = mem_store().await;
        let (user, start) = pending_approval(&store, "fatfingered").await;
        store
            .approve_approval(&start.request_id, &user.user_id, &start.code, None)
            .await
            .unwrap();
        let (_, token) = store.poll_approval(&start.request_id, None).await.unwrap();
        let token = token.expect("token");
        assert!(store.resolve_session(&token).await.is_some());

        // "I tapped approve by accident." A silent no-op would leave the
        // attacker signed in for 30 days.
        assert_eq!(
            store
                .deny_approval(&start.request_id, &user.user_id)
                .await
                .unwrap(),
            ApprovalOutcome::Ok
        );
        assert!(
            store.resolve_session(&token).await.is_none(),
            "denying an approved request must kill the session it minted"
        );
    }

    #[tokio::test]
    async fn approval_context_is_sanitized_and_capped() {
        let store = mem_store().await;
        let (user, _) = store
            .register("contextual", "correct horse battery", None, None)
            .await
            .expect("register");
        let hostile = format!("Sign in\r\n\u{1b}[31mURGENT{}", "!".repeat(500));
        store
            .start_approval(Some("contextual"), Some(hostile), None)
            .await
            .expect("start");

        let pending = store
            .list_pending_approvals(&user.user_id)
            .await
            .expect("list");
        let context = pending[0].context.as_deref().expect("context");
        assert!(context.chars().count() <= APPROVAL_CONTEXT_MAX);
        assert!(
            !context.contains('\n') && !context.contains('\u{1b}'),
            "attacker-controlled text on a security screen must not carry control characters"
        );
    }

    #[tokio::test]
    async fn the_approval_table_does_not_grow_without_bound() {
        let store = mem_store().await;
        let (user, start) = pending_approval(&store, "sweeper").await;
        store
            .approve_approval(&start.request_id, &user.user_id, &start.code, None)
            .await
            .unwrap();
        store.poll_approval(&start.request_id, None).await.unwrap();

        // Nothing ever deleted from this table before; the sweep runs on the
        // next start and collects everything already finished with.
        store.sweep_approvals().await;
        assert!(
            store
                .get_approval(&start.request_id)
                .await
                .unwrap()
                .is_none(),
            "a consumed approval must be swept"
        );
    }

    // ----- Loopback browser login (device-authorization shape) -----

    /// The desktop flow: the app starts a request naming nobody, the user signs
    /// in wherever they like and types the code, and the app collects the
    /// session by proving it is the client that started the request.
    #[tokio::test]
    async fn an_unnamed_request_is_claimed_by_whoever_approves_it() {
        let store = mem_store().await;
        let (user, _) = store
            .register("browseruser", "correct horse battery", None, None)
            .await
            .expect("register");

        let verifier = "a-high-entropy-verifier-from-the-desktop";
        let start = store
            .start_approval(
                None,
                Some("Consistent for macOS".into()),
                Some(pkce_challenge(verifier)),
            )
            .await
            .expect("start");

        // The browser shows what is asking, without ever seeing the code.
        let detail = store
            .approval_detail(&start.request_id)
            .await
            .unwrap()
            .expect("detail");
        assert_eq!(detail.status, "pending");
        assert_eq!(detail.context.as_deref(), Some("Consistent for macOS"));
        assert!(
            !serde_json::to_string(&detail)
                .unwrap()
                .contains(&start.code),
            "the code must come off the requesting device, not out of this endpoint"
        );

        assert_eq!(
            store
                .approve_approval(&start.request_id, &user.user_id, &start.code, None)
                .await
                .unwrap(),
            ApprovalOutcome::Ok
        );

        let (status, token) = store
            .poll_approval(&start.request_id, Some(verifier))
            .await
            .unwrap();
        assert_eq!(status, "approved");
        let identity = store
            .resolve_session(&token.expect("token"))
            .await
            .expect("resolves");
        assert_eq!(
            identity.user_id, user.user_id,
            "the session belongs to whoever approved"
        );
    }

    /// Without PKCE, anyone who learned a request id — it travels through a
    /// browser URL — could poll for the session it produced.
    #[tokio::test]
    async fn a_stolen_request_id_cannot_collect_the_session() {
        let store = mem_store().await;
        let (user, _) = store
            .register("pkceuser", "correct horse battery", None, None)
            .await
            .expect("register");
        let verifier = "the-desktop-keeps-this-to-itself";
        let start = store
            .start_approval(None, None, Some(pkce_challenge(verifier)))
            .await
            .expect("start");
        store
            .approve_approval(&start.request_id, &user.user_id, &start.code, None)
            .await
            .unwrap();

        // An attacker with the id and no verifier.
        assert_eq!(
            store.poll_approval(&start.request_id, None).await.unwrap(),
            ("unknown".to_string(), None)
        );
        assert_eq!(
            store
                .poll_approval(&start.request_id, Some("guessed"))
                .await
                .unwrap(),
            ("unknown".to_string(), None)
        );

        // The real client still collects — the failed polls must not have
        // consumed the token on its behalf.
        let (status, token) = store
            .poll_approval(&start.request_id, Some(verifier))
            .await
            .unwrap();
        assert_eq!(status, "approved");
        assert!(token.is_some());
    }

    #[tokio::test]
    async fn a_named_request_still_belongs_only_to_that_account() {
        let store = mem_store().await;
        let (owner, _) = store
            .register("namedowner", "correct horse battery", None, None)
            .await
            .expect("register owner");
        let (other, _) = store
            .register("someoneelse", "correct horse battery", None, None)
            .await
            .expect("register other");
        let start = store
            .start_approval(Some("namedowner"), None, None)
            .await
            .expect("start");

        assert_eq!(
            store
                .approve_approval(&start.request_id, &other.user_id, &start.code, None)
                .await
                .unwrap(),
            ApprovalOutcome::NotFound,
            "naming an account must keep the request that account's alone"
        );
        assert_eq!(
            store
                .approve_approval(&start.request_id, &owner.user_id, &start.code, None)
                .await
                .unwrap(),
            ApprovalOutcome::Ok
        );
    }

    // ----- Claiming the machine-owner account -----

    #[tokio::test]
    async fn claiming_upgrades_the_local_account_without_replacing_it() {
        let store = mem_store().await;
        let owner = store
            .ensure_local_operator("bearer", "default")
            .await
            .expect("provision");

        // Before: exists, holds this machine's work, cannot be signed into.
        assert!(!store.has_password("bearer").await.unwrap());
        assert!(store.login("bearer", "").await.is_err());

        let (claimed, codes) = store
            .claim_local_account("bearer", "  Moe  ", "correct horse battery")
            .await
            .expect("claim");

        // The same account: the id is what every entity and mission is keyed
        // to, so a new row here would strand all of it.
        assert_eq!(claimed.user_id, owner.user_id);
        assert_eq!(claimed.workspace_id, owner.workspace_id);
        assert_eq!(claimed.username, "moe", "usernames normalise");
        assert_eq!(codes.len(), BACKUP_CODE_COUNT);

        // After: signs in like any other account, from anywhere.
        let via_login = store
            .login("moe", "correct horse battery")
            .await
            .expect("login");
        assert_eq!(via_login.user_id, owner.user_id);
        assert!(store.has_password("bearer").await.unwrap());

        // And the codes actually work, so the account is recoverable.
        assert!(
            store
                .reset_password_with_code("moe", &codes[0], "a different password")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn claiming_cannot_overwrite_an_account_that_has_a_password() {
        let store = mem_store().await;
        let (user, _) = store
            .register("realuser", "correct horse battery", None, None)
            .await
            .expect("register");

        // Otherwise this would be a password reset with no proof of anything.
        assert!(
            store
                .claim_local_account(&user.user_id, "hijacked", "my password now")
                .await
                .is_err()
        );
        assert!(
            store
                .login("realuser", "correct horse battery")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn claiming_refuses_a_username_someone_else_has() {
        let store = mem_store().await;
        store
            .register("taken", "correct horse battery", None, None)
            .await
            .expect("register");
        store
            .ensure_local_operator("bearer", "default")
            .await
            .expect("provision");

        let err = store
            .claim_local_account("bearer", "taken", "correct horse battery")
            .await
            .expect_err("must refuse");
        assert!(err.to_string().contains("taken"));
        // A rejected claim leaves the account exactly as it was.
        assert!(!store.has_password("bearer").await.unwrap());
    }

    #[tokio::test]
    async fn claiming_validates_the_username_and_password() {
        let store = mem_store().await;
        store
            .ensure_local_operator("bearer", "default")
            .await
            .expect("provision");
        assert!(
            store
                .claim_local_account("bearer", "ab", "long enough pw")
                .await
                .is_err()
        );
        assert!(
            store
                .claim_local_account("bearer", "has space", "long enough pw")
                .await
                .is_err()
        );
        assert!(
            store
                .claim_local_account("bearer", "fine", "short")
                .await
                .is_err()
        );
        assert!(!store.has_password("bearer").await.unwrap());
    }

    // ----- External identity (the additive seam) -----

    #[tokio::test]
    async fn an_external_subject_maps_onto_one_stable_local_account() {
        let store = mem_store().await;

        let first = store
            .ensure_external_user("https://id.example", "user_abc", Some("Moe"))
            .await
            .expect("map");
        // The same person on a second machine: same row, not a second account.
        let again = store
            .ensure_external_user("https://id.example", "user_abc", Some("Moe"))
            .await
            .expect("map again");
        assert_eq!(first.user_id, again.user_id);
        assert_eq!(first.workspace_id, again.workspace_id);
        assert_eq!(
            store
                .get_user_profile(&first.user_id)
                .await
                .unwrap()
                .display_name
                .as_deref(),
            Some("Moe")
        );

        // A different subject, and the same subject from a different issuer,
        // are different people.
        let other_subject = store
            .ensure_external_user("https://id.example", "user_xyz", None)
            .await
            .unwrap();
        let other_issuer = store
            .ensure_external_user("https://id.elsewhere", "user_abc", None)
            .await
            .unwrap();
        assert_ne!(first.user_id, other_subject.user_id);
        assert_ne!(
            first.user_id, other_issuer.user_id,
            "one issuer's subject must never collide with another's"
        );
    }

    #[tokio::test]
    async fn an_external_account_has_no_password_to_log_in_with() {
        let store = mem_store().await;
        let user = store
            .ensure_external_user("https://id.example", "user_abc", None)
            .await
            .expect("map");
        // Same property the local-operator row has: no usable hash, so the
        // account is reachable only by presenting a token the issuer signed.
        assert!(store.login(&user.username, "").await.is_err());
        assert!(
            store
                .login(&user.username, "anything at all")
                .await
                .is_err()
        );
        assert!(
            store
                .change_password(&user.user_id, "", "a new password")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn external_mapping_refuses_an_empty_issuer_or_subject() {
        let store = mem_store().await;
        assert!(
            store
                .ensure_external_user("", "user_abc", None)
                .await
                .is_err()
        );
        assert!(
            store
                .ensure_external_user("https://id.example", "  ", None)
                .await
                .is_err()
        );
    }

    // ----- Account deletion -----

    /// Everything the store can hold for one account: a session, an API key, an
    /// authenticator device, a passkey, a pending sign-in approval, recovery
    /// codes, and a profile. Returns the user, a session token, an API key
    /// token, the recovery codes, and the approval request id.
    async fn fully_populated(
        store: &Arc<AuthStore>,
        username: &str,
    ) -> (AuthUser, String, String, Vec<String>, String) {
        let (user, codes) = store
            .register(username, "correct horse battery", None, None)
            .await
            .expect("register");
        let session = store
            .create_session(&user.user_id, &user.workspace_id, Some("phone".into()))
            .await
            .expect("session");
        let (_, key) = store
            .create_api_key(&user.user_id, &user.workspace_id, Some("ci"))
            .await
            .expect("api key");
        store
            .register_device(&user.user_id, "iPhone", "ios")
            .await
            .expect("device");
        store
            .add_passkey(&user.user_id, r#"{"stand-in":"passkey"}"#)
            .await
            .expect("passkey");
        store
            .set_user_profile(
                &user.user_id,
                &UserProfile {
                    display_name: Some(format!("{username} display")),
                    ..Default::default()
                },
            )
            .await
            .expect("profile");
        let approval = store
            .start_approval(Some(username), Some("Desktop app".into()), None)
            .await
            .expect("approval");
        (user, session, key, codes, approval.request_id)
    }

    #[tokio::test]
    async fn deleting_a_user_removes_every_row_and_spares_everyone_else() {
        let store = mem_store().await;
        let (alice, a_session, a_key, a_codes, a_request) =
            fully_populated(&store, "alicedel").await;
        let (bob, b_session, b_key, b_codes, b_request) = fully_populated(&store, "bobdel").await;
        // An unclaimed browser-flow request names nobody, so it is no one's to
        // delete.
        let anonymous = store
            .start_approval(None, Some("Consistent for macOS".into()), None)
            .await
            .expect("anonymous approval");

        let deleted = store.delete_user(&alice.user_id).await.expect("delete");
        assert_eq!(
            deleted,
            DeletedUser {
                account: true,
                sessions: 1,
                api_keys: 1,
                devices: 1,
                passkeys: 1,
                approvals: 1,
            }
        );

        // The account row, and everything that lived on it.
        assert!(store.get_auth_user(&alice.user_id).await.is_none());
        assert!(store.get_user_profile(&alice.user_id).await.is_err());
        assert!(
            store
                .login("alicedel", "correct horse battery")
                .await
                .is_err()
        );
        assert!(
            store
                .reset_password_with_code("alicedel", &a_codes[0], "a new password")
                .await
                .is_err(),
            "recovery codes must die with the account"
        );
        assert!(
            store.username_available("alicedel").await.unwrap(),
            "the username is released"
        );
        assert!(!store.verify_password(&alice.user_id, "correct horse battery").await.unwrap());
        // Sessions and API keys stop authenticating.
        assert!(store.resolve_session(&a_session).await.is_none());
        assert!(store.resolve_session(&a_key).await.is_none());
        assert!(
            store
                .list_sessions(&alice.user_id, None)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.list_api_keys(&alice.user_id).await.unwrap().is_empty());
        // Devices, passkeys, approvals.
        assert!(store.list_devices(&alice.user_id).await.unwrap().is_empty());
        assert!(store.list_passkeys(&alice.user_id).await.unwrap().is_empty());
        assert!(
            store
                .list_pending_approvals(&alice.user_id)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.get_approval(&a_request).await.unwrap().is_none());

        // Bob is untouched, row for row.
        let bob_now = store.get_auth_user(&bob.user_id).await.expect("bob");
        assert_eq!(
            bob_now.profile.display_name.as_deref(),
            Some("bobdel display")
        );
        assert!(store.resolve_session(&b_session).await.is_some());
        assert!(store.resolve_session(&b_key).await.is_some());
        assert_eq!(store.list_devices(&bob.user_id).await.unwrap().len(), 1);
        assert_eq!(store.list_passkeys(&bob.user_id).await.unwrap().len(), 1);
        assert!(store.get_approval(&b_request).await.unwrap().is_some());
        assert!(store.get_approval(&anonymous.request_id).await.unwrap().is_some());
        assert!(store.login("bobdel", "correct horse battery").await.is_ok());
        assert!(
            store
                .reset_password_with_code("bobdel", &b_codes[0], "bob's new password")
                .await
                .is_ok(),
            "another account's recovery codes must survive"
        );

        // Idempotent: nothing left to find the second time.
        assert_eq!(
            store.delete_user(&alice.user_id).await.expect("again"),
            DeletedUser::default()
        );
    }

    #[tokio::test]
    async fn verify_password_checks_the_current_password_only() {
        let store = mem_store().await;
        let (user, _) = store
            .register("verifier", "correct horse battery", None, None)
            .await
            .expect("register");
        assert!(store.verify_password(&user.user_id, "correct horse battery").await.unwrap());
        assert!(!store.verify_password(&user.user_id, "wrong password").await.unwrap());
        assert!(!store.verify_password("no-such-user", "correct horse battery").await.unwrap());

        // The local-operator row has no hash, so nothing proves ownership of it.
        store
            .ensure_local_operator("bearer", "default")
            .await
            .expect("provision");
        assert!(!store.verify_password("bearer", "").await.unwrap());
    }

    #[test]
    fn pkce_challenge_matches_rfc7636() {
        // The worked example from RFC 7636 appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn approval_codes_compare_in_constant_time() {
        assert!(constant_time_eq("123456", "123456"));
        assert!(!constant_time_eq("123456", "123457"));
        assert!(!constant_time_eq("123456", "12345"));
    }

    #[test]
    fn token_hash_is_stable_and_hex() {
        let t = gen_token();
        assert_eq!(token_hash(&t), token_hash(&t));
        assert_eq!(token_hash(&t).len(), 64); // sha256 hex
        assert!(token_hash(&t).chars().all(|c| c.is_ascii_hexdigit()));
    }
}
