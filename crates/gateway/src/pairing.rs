//! Pairing a phone to this computer by presence, and signing it back in from
//! anywhere with a key that never leaves the phone.
//!
//! # Why not a password, Bluetooth, or passkeys
//!
//! The QR used to carry only an address, and the phone then signed in with a
//! password. That binds a phone to "someone who knows the password", from
//! anywhere. A desktop install's owner account often has no password at all
//! (`AuthStore::ensure_local_operator`), so it could not pair a phone either.
//! Bluetooth would prove the same thing the screen proves, at the cost of a
//! native module on three platforms. Passkeys need an HTTPS domain the
//! self-hoster does not have.
//!
//! # The flow
//!
//! ```text
//! computer (signed in)          POST /api/pair/offer        → one-time code N (90 s), QR {url, workspace, v:1, nonce:N}
//! phone scans, makes an Ed25519 key, computes the 6-digit check from (N, key)
//! phone                         POST /api/pair/enroll       {nonce, publicKey, signature(N)} → request id
//! computer shows "iPhone wants to pair — 482 913 — Allow / Deny"; phone shows 482 913
//! computer (signed in)          POST /api/pair/requests/{id}/approve
//! phone                         POST /api/pair/requests/{id}/collect {signature(id)} → session + device id
//! later, from anywhere:
//! phone                         POST /api/auth/device/challenge {deviceId} → challenge (60 s)
//! phone                         POST /api/auth/device/login {challengeId, signature} → session
//! ```
//!
//! Presence is proven twice: the code exists only on the computer's screen,
//! and the owner approves on that computer after checking the six digits
//! match the phone in their hand. An attacker who somehow read the code would
//! show a different six digits (they derive from the attacker's key).
//!
//! # Why the transport is not restricted
//!
//! The gateway listens on loopback only. Every phone request arrives through
//! Tailscale Serve or the relay, both of which reach it *from* loopback, so a
//! "same Wi-Fi only" rule would refuse every pairing on a default install.
//! The screen code and the approval are the proof of presence; the network
//! path is not. The relay can carry pairing traffic but cannot forge it: every
//! phone step is signed by a key the relay never sees.
//!
//! # State
//!
//! Offers, pending requests and login challenges live in process memory: they
//! are seconds-to-minutes long and a restart rightly voids them. The durable
//! parts are the device row (with its public key) and the sessions it holds,
//! both in the auth store; revoking the device ends its sessions.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::control_api::AuthContext;
use crate::state::ServerState;

const OFFER_TTL: Duration = Duration::from_secs(90);
/// How long a scanned request waits for the owner to press Allow.
const REQUEST_TTL: Duration = Duration::from_secs(180);
const CHALLENGE_TTL: Duration = Duration::from_secs(60);
/// Caps on live in-memory state, so a flood of public calls cannot grow it.
const MAX_REQUESTS: usize = 64;
const MAX_CHALLENGES: usize = 1024;
const LABEL_MAX: usize = 64;

/// Domain-separated messages the phone signs. Each names its purpose so a
/// signature made for one step can never be replayed as another.
pub fn enroll_message(nonce: &str, public_key: &str) -> String {
    format!("consistent-pair-enroll|{nonce}|{public_key}")
}
pub fn collect_message(request_id: &str) -> String {
    format!("consistent-pair-collect|{request_id}")
}
pub fn login_message(challenge_id: &str, challenge: &str) -> String {
    format!("consistent-device-login|{challenge_id}|{challenge}")
}

/// The six digits both screens show: derived from the offer and the phone's
/// key, so the phone computes it before sending anything and an attacker's
/// key yields different digits.
pub fn confirmation_code(nonce: &str, public_key: &str) -> String {
    let digest = Sha256::digest(format!("consistent-pair-code|{nonce}|{public_key}").as_bytes());
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    format!("{n:06}")
}

/// Verify an Ed25519 signature. Keys and signatures are base64url, no padding.
pub fn verify(public_key: &str, message: &str, signature: &str) -> bool {
    let (Ok(pk), Ok(sig)) = (B64.decode(public_key.trim()), B64.decode(signature.trim())) else {
        return false;
    };
    if pk.len() != 32 || sig.len() != 64 {
        return false;
    }
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pk)
        .verify(message.as_bytes(), &sig)
        .is_ok()
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    for chunk in bytes.chunks_mut(16) {
        chunk.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    // Two v4 UUIDs carry 244 random bits; hash so the output has no fixed bits.
    B64.encode(Sha256::digest(bytes))
}

fn rfc3339_in(d: Duration) -> String {
    (chrono::Utc::now() + chrono::Duration::from_std(d).unwrap_or_default()).to_rfc3339()
}

struct Offer {
    user_id: String,
    expires: Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RequestState {
    Pending,
    /// An approval is being written; a second Allow must not mint twice.
    Approving,
    Approved {
        token: String,
        device_id: String,
    },
    Denied,
}

struct PairRequest {
    user_id: String,
    public_key: String,
    label: String,
    platform: String,
    code: String,
    created_at: String,
    expires: Instant,
    expires_at: String,
    state: RequestState,
}

struct Challenge {
    device_id: String,
    challenge: String,
    expires: Instant,
}

/// What the computer shows for a phone waiting to be allowed.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingPairing {
    pub request_id: String,
    pub label: String,
    pub platform: String,
    pub code: String,
    pub created_at: String,
    pub expires_at: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    /// Unknown, used, or expired code / request / challenge.
    Gone,
    BadSignature,
    BadKey,
    /// Not this caller's request, or not in a state that allows the step.
    Conflict,
    Busy,
}

impl From<PairError> for StatusCode {
    fn from(e: PairError) -> Self {
        match e {
            PairError::Gone => StatusCode::GONE,
            PairError::BadSignature => StatusCode::UNAUTHORIZED,
            PairError::BadKey => StatusCode::BAD_REQUEST,
            PairError::Conflict => StatusCode::CONFLICT,
            PairError::Busy => StatusCode::TOO_MANY_REQUESTS,
        }
    }
}

/// What a phone gets back when it collects an approved request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collected {
    Pending,
    Denied,
    Approved { token: String, device_id: String },
}

/// The short-lived half of pairing. One per process ([`SERVICE`]); tests
/// build their own.
#[derive(Default)]
pub struct PairingService {
    offers: Mutex<HashMap<String, Offer>>,
    requests: Mutex<HashMap<String, PairRequest>>,
    challenges: Mutex<HashMap<String, Challenge>>,
}

pub static SERVICE: LazyLock<PairingService> = LazyLock::new(PairingService::default);

impl PairingService {
    fn prune(&self) {
        let now = Instant::now();
        self.offers.lock().unwrap().retain(|_, o| o.expires > now);
        self.requests.lock().unwrap().retain(|_, r| r.expires > now);
        self.challenges
            .lock()
            .unwrap()
            .retain(|_, c| c.expires > now);
    }

    /// Mint a one-time code for `user_id` to show on screen.
    pub fn create_offer(&self, user_id: &str) -> (String, String) {
        self.prune();
        let nonce = random_token();
        self.offers.lock().unwrap().insert(
            nonce.clone(),
            Offer {
                user_id: user_id.to_string(),
                expires: Instant::now() + OFFER_TTL,
            },
        );
        (nonce, rfc3339_in(OFFER_TTL))
    }

    /// A phone presents the code and its key. The code is spent whatever
    /// happens next, so it can be used exactly once.
    pub fn enroll(
        &self,
        nonce: &str,
        public_key: &str,
        signature: &str,
        label: &str,
        platform: &str,
    ) -> Result<(String, String, String), PairError> {
        self.prune();
        match B64.decode(public_key.trim()) {
            Ok(k) if k.len() == 32 => {}
            _ => return Err(PairError::BadKey),
        }
        if !verify(public_key, &enroll_message(nonce, public_key), signature) {
            return Err(PairError::BadSignature);
        }
        let offer = self
            .offers
            .lock()
            .unwrap()
            .remove(nonce)
            .filter(|o| o.expires > Instant::now())
            .ok_or(PairError::Gone)?;
        let mut requests = self.requests.lock().unwrap();
        if requests.len() >= MAX_REQUESTS {
            return Err(PairError::Busy);
        }
        let id = random_token();
        let code = confirmation_code(nonce, public_key);
        let expires_at = rfc3339_in(REQUEST_TTL);
        let label: String = label.trim().chars().take(LABEL_MAX).collect();
        requests.insert(
            id.clone(),
            PairRequest {
                user_id: offer.user_id,
                public_key: public_key.trim().to_string(),
                label: if label.is_empty() {
                    "Phone".into()
                } else {
                    label
                },
                platform: platform.trim().chars().take(16).collect(),
                code: code.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                expires: Instant::now() + REQUEST_TTL,
                expires_at: expires_at.clone(),
                state: RequestState::Pending,
            },
        );
        Ok((id, code, expires_at))
    }

    /// Phones waiting for `user_id` to press Allow.
    pub fn pending_for(&self, user_id: &str) -> Vec<PendingPairing> {
        self.prune();
        let mut out: Vec<PendingPairing> = self
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, r)| r.user_id == user_id && r.state == RequestState::Pending)
            .map(|(id, r)| PendingPairing {
                request_id: id.clone(),
                label: r.label.clone(),
                platform: r.platform.clone(),
                code: r.code.clone(),
                created_at: r.created_at.clone(),
                expires_at: r.expires_at.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        out
    }

    /// Claim a pending request for approval. Returns (label, platform, key).
    pub fn begin_approve(
        &self,
        user_id: &str,
        request_id: &str,
    ) -> Result<(String, String, String), PairError> {
        let mut requests = self.requests.lock().unwrap();
        let r = requests.get_mut(request_id).ok_or(PairError::Gone)?;
        if r.user_id != user_id {
            // Same answer as a missing id, so ids cannot be probed.
            return Err(PairError::Gone);
        }
        if r.expires <= Instant::now() {
            return Err(PairError::Gone);
        }
        if r.state != RequestState::Pending {
            return Err(PairError::Conflict);
        }
        r.state = RequestState::Approving;
        Ok((r.label.clone(), r.platform.clone(), r.public_key.clone()))
    }

    /// Record the session the phone will collect.
    pub fn finish_approve(&self, request_id: &str, token: String, device_id: String) {
        if let Some(r) = self.requests.lock().unwrap().get_mut(request_id) {
            r.state = RequestState::Approved { token, device_id };
            // The phone polls every couple of seconds; give it a minute.
            r.expires = Instant::now() + Duration::from_secs(60);
        }
    }

    /// Put a request back to pending after a failed approval write.
    pub fn abort_approve(&self, request_id: &str) {
        if let Some(r) = self.requests.lock().unwrap().get_mut(request_id)
            && r.state == RequestState::Approving
        {
            r.state = RequestState::Pending;
        }
    }

    pub fn deny(&self, user_id: &str, request_id: &str) -> Result<(), PairError> {
        let mut requests = self.requests.lock().unwrap();
        let r = requests.get_mut(request_id).ok_or(PairError::Gone)?;
        if r.user_id != user_id {
            return Err(PairError::Gone);
        }
        if r.state != RequestState::Pending {
            return Err(PairError::Conflict);
        }
        r.state = RequestState::Denied;
        Ok(())
    }

    /// The phone asks how its request went, signing the request id with its
    /// key. An approved session is handed over exactly once.
    pub fn collect(&self, request_id: &str, signature: &str) -> Result<Collected, PairError> {
        let mut requests = self.requests.lock().unwrap();
        let r = requests.get(request_id).ok_or(PairError::Gone)?;
        if !verify(&r.public_key, &collect_message(request_id), signature) {
            return Err(PairError::BadSignature);
        }
        if r.expires <= Instant::now() {
            requests.remove(request_id);
            return Err(PairError::Gone);
        }
        match r.state.clone() {
            RequestState::Pending | RequestState::Approving => Ok(Collected::Pending),
            RequestState::Denied => {
                requests.remove(request_id);
                Ok(Collected::Denied)
            }
            RequestState::Approved { token, device_id } => {
                requests.remove(request_id);
                Ok(Collected::Approved { token, device_id })
            }
        }
    }

    /// Issue a login challenge for a device. Issued for any id, known or
    /// not, so the endpoint cannot be used to discover devices.
    pub fn challenge(&self, device_id: &str) -> Result<(String, String, String), PairError> {
        self.prune();
        let mut challenges = self.challenges.lock().unwrap();
        if challenges.len() >= MAX_CHALLENGES {
            return Err(PairError::Busy);
        }
        let id = random_token();
        let challenge = random_token();
        challenges.insert(
            id.clone(),
            Challenge {
                device_id: device_id.trim().to_string(),
                challenge: challenge.clone(),
                expires: Instant::now() + CHALLENGE_TTL,
            },
        );
        Ok((id, challenge, rfc3339_in(CHALLENGE_TTL)))
    }

    /// Spend a challenge. Returns (device id, challenge) when it is live.
    pub fn take_challenge(&self, challenge_id: &str) -> Option<(String, String)> {
        self.challenges
            .lock()
            .unwrap()
            .remove(challenge_id)
            .filter(|c| c.expires > Instant::now())
            .map(|c| (c.device_id, c.challenge))
    }
}

// ===== HTTP =====

pub fn public_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/pair/enroll", post(enroll))
        .route("/api/pair/requests/{id}/collect", post(collect))
        .route("/api/auth/device/challenge", post(device_challenge))
        .route("/api/auth/device/login", post(device_login))
        .with_state(state)
}

pub fn protected_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/pair/offer", post(create_offer))
        .route("/api/pair/requests", get(list_requests))
        .route("/api/pair/requests/{id}/approve", post(approve))
        .route("/api/pair/requests/{id}/deny", post(deny))
        .with_state(state)
}

fn auth_store(
    state: &ServerState,
) -> Result<&std::sync::Arc<crate::auth_store::AuthStore>, StatusCode> {
    state
        .auth_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// The account a phone pairs to. A signed-in person is themselves. The
/// machine token stands for the local owner account (the same mapping
/// `/api/auth/local-session` makes), and only on a loopback-bound gateway.
async fn owner_user_id(state: &ServerState, ctx: &AuthContext) -> Result<String, StatusCode> {
    if !ctx.is_static() {
        return Ok(ctx.principal_id.clone());
    }
    let loopback = state
        .app_config
        .as_ref()
        .map(|c| crate::billing::is_loopback_bind(&c.gateway_bind))
        .unwrap_or(false);
    if !loopback {
        return Err(StatusCode::FORBIDDEN);
    }
    let user = auth_store(state)?
        .ensure_local_operator(&ctx.principal_id, &ctx.workspace_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(user.user_id)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OfferResponse {
    nonce: String,
    expires_at: String,
    /// Where the phone will reach this computer, or `None` when nothing does
    /// yet (no Tailscale Serve, no relay). The UI says how to fix that.
    url: Option<String>,
    /// Which path `url` is: `relay`, `tailnet`, or `lan`.
    via: Option<&'static str>,
    /// The exact JSON the QR encodes.
    payload: serde_json::Value,
    /// The QR as an SVG document, ready to inline.
    qr_svg: Option<String>,
}

/// Build the QR payload. `v` stays 1 so older app builds still connect (they
/// ignore `nonce` and sign in with a password); new builds pair by key.
pub fn qr_payload(url: Option<&str>, workspace_id: &str, nonce: &str) -> serde_json::Value {
    serde_json::json!({
        "url": url,
        "workspace": workspace_id,
        "v": 1,
        "nonce": nonce,
    })
}

pub fn qr_svg(payload: &serde_json::Value) -> Option<String> {
    let code = qrcode::QrCode::new(payload.to_string().as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::svg::Color>()
            .min_dimensions(240, 240)
            .quiet_zone(true)
            .build(),
    )
}

async fn create_offer(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<OfferResponse>, StatusCode> {
    let user_id = owner_user_id(&state, &ctx).await?;
    let (port, bind) = state
        .app_config
        .as_ref()
        .map(|c| (c.gateway_port, c.gateway_bind.clone()))
        .unwrap_or((18800, "127.0.0.1".to_string()));
    // Shells out to the tailscale CLI; keep it off the async workers.
    let status =
        tokio::task::spawn_blocking(move || crate::remote_doctor::remote_status(port, &bind))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let via = if status.relay_url.is_some() {
        Some("relay")
    } else if status.serve_url.is_some() {
        Some("tailnet")
    } else if status.lan_url.is_some() {
        Some("lan")
    } else {
        None
    };
    let (nonce, expires_at) = SERVICE.create_offer(&user_id);
    let payload = qr_payload(status.recommended_url.as_deref(), &ctx.workspace_id, &nonce);
    let qr = status
        .recommended_url
        .as_ref()
        .and_then(|_| qr_svg(&payload));
    Ok(Json(OfferResponse {
        nonce,
        expires_at,
        url: status.recommended_url,
        via,
        payload,
        qr_svg: qr,
    }))
}

async fn list_requests(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
) -> Result<Json<Vec<PendingPairing>>, StatusCode> {
    let user_id = owner_user_id(&state, &ctx).await?;
    Ok(Json(SERVICE.pending_for(&user_id)))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ApproveResponse {
    device_id: String,
}

async fn approve(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<ApproveResponse>, StatusCode> {
    let user_id = owner_user_id(&state, &ctx).await?;
    let (label, platform, public_key) = SERVICE.begin_approve(&user_id, &id)?;
    let result = async {
        let store = auth_store(&state)?;
        let user = store
            .get_auth_user(&user_id)
            .await
            .ok_or(StatusCode::NOT_FOUND)?;
        let device_id = store
            .register_paired_device(&user_id, &label, &platform, &public_key)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let token = store
            .create_device_session(&user_id, &user.workspace_id, &device_id, &label)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok::<_, StatusCode>((token, device_id))
    }
    .await;
    match result {
        Ok((token, device_id)) => {
            SERVICE.finish_approve(&id, token, device_id.clone());
            tracing::info!(user = %user_id, device = %device_id, "paired a device");
            Ok(Json(ApproveResponse { device_id }))
        }
        Err(e) => {
            SERVICE.abort_approve(&id);
            Err(e)
        }
    }
}

async fn deny(
    State(state): State<ServerState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let user_id = owner_user_id(&state, &ctx).await?;
    SERVICE.deny(&user_id, &id)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnrollRequest {
    nonce: String,
    public_key: String,
    signature: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    platform: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EnrollResponse {
    request_id: String,
    code: String,
    expires_at: String,
}

async fn enroll(Json(body): Json<EnrollRequest>) -> Result<Json<EnrollResponse>, StatusCode> {
    let (request_id, code, expires_at) = SERVICE.enroll(
        &body.nonce,
        &body.public_key,
        &body.signature,
        body.label.as_deref().unwrap_or(""),
        body.platform.as_deref().unwrap_or("unknown"),
    )?;
    Ok(Json(EnrollResponse {
        request_id,
        code,
        expires_at,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignedRequest {
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CollectResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_id: Option<String>,
}

async fn collect(
    Path(id): Path<String>,
    Json(body): Json<SignedRequest>,
) -> Result<Json<CollectResponse>, StatusCode> {
    let out = match SERVICE.collect(&id, &body.signature)? {
        Collected::Pending => CollectResponse {
            status: "pending",
            token: None,
            device_id: None,
        },
        Collected::Denied => CollectResponse {
            status: "denied",
            token: None,
            device_id: None,
        },
        Collected::Approved { token, device_id } => CollectResponse {
            status: "approved",
            token: Some(token),
            device_id: Some(device_id),
        },
    };
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChallengeRequest {
    device_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChallengeResponse {
    challenge_id: String,
    challenge: String,
    expires_at: String,
}

async fn device_challenge(
    Json(body): Json<ChallengeRequest>,
) -> Result<Json<ChallengeResponse>, StatusCode> {
    let (challenge_id, challenge, expires_at) = SERVICE.challenge(&body.device_id)?;
    Ok(Json(ChallengeResponse {
        challenge_id,
        challenge,
        expires_at,
    }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceLoginRequest {
    challenge_id: String,
    signature: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceLoginResponse {
    token: String,
    user_id: String,
    username: String,
    workspace_id: String,
    device_id: String,
}

async fn device_login(
    State(state): State<ServerState>,
    Json(body): Json<DeviceLoginRequest>,
) -> Result<Json<DeviceLoginResponse>, StatusCode> {
    let (device_id, challenge) = SERVICE
        .take_challenge(&body.challenge_id)
        .ok_or(StatusCode::GONE)?;
    let store = auth_store(&state)?;
    // Unknown, unpaired and revoked devices all answer 401, like a bad
    // signature: nothing here tells a caller which device ids exist.
    let key = store
        .paired_device_key(&device_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !verify(
        &key.public_key,
        &login_message(&body.challenge_id, &challenge),
        &body.signature,
    ) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let user = store
        .get_auth_user(&key.user_id)
        .await
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let token = store
        .create_device_session(&user.user_id, &user.workspace_id, &device_id, &key.label)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(DeviceLoginResponse {
        token,
        user_id: user.user_id,
        username: user.username,
        workspace_id: user.workspace_id,
        device_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

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

    fn enroll(
        svc: &PairingService,
        phone: &Phone,
        nonce: &str,
    ) -> Result<(String, String, String), PairError> {
        let pk = phone.public_key();
        svc.enroll(
            nonce,
            &pk,
            &phone.sign(&enroll_message(nonce, &pk)),
            "Moe's iPhone",
            "ios",
        )
    }

    #[test]
    fn full_pairing_round_trip() {
        let svc = PairingService::default();
        let phone = Phone::new();
        let (nonce, _) = svc.create_offer("user_a");
        let (id, code, _) = enroll(&svc, &phone, &nonce).unwrap();
        // The phone computes the same six digits on its own.
        assert_eq!(code, confirmation_code(&nonce, &phone.public_key()));
        assert_eq!(code.len(), 6);

        let pending = svc.pending_for("user_a");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].code, code);
        assert!(svc.pending_for("user_b").is_empty());

        // Before approval the phone is told to wait.
        let sig = phone.sign(&collect_message(&id));
        assert_eq!(svc.collect(&id, &sig).unwrap(), Collected::Pending);

        let (label, platform, pk) = svc.begin_approve("user_a", &id).unwrap();
        assert_eq!((label.as_str(), platform.as_str()), ("Moe's iPhone", "ios"));
        assert_eq!(pk, phone.public_key());
        // A second Allow while the first is writing is refused.
        assert_eq!(svc.begin_approve("user_a", &id), Err(PairError::Conflict));
        svc.finish_approve(&id, "tok".into(), "dev1".into());

        assert_eq!(
            svc.collect(&id, &sig).unwrap(),
            Collected::Approved {
                token: "tok".into(),
                device_id: "dev1".into()
            }
        );
        // Handed over exactly once.
        assert_eq!(svc.collect(&id, &sig), Err(PairError::Gone));
    }

    #[test]
    fn an_offer_is_spent_by_its_first_scan() {
        let svc = PairingService::default();
        let (nonce, _) = svc.create_offer("user_a");
        enroll(&svc, &Phone::new(), &nonce).unwrap();
        assert_eq!(enroll(&svc, &Phone::new(), &nonce), Err(PairError::Gone));
        assert_eq!(enroll(&svc, &Phone::new(), "made-up"), Err(PairError::Gone));
    }

    #[test]
    fn signatures_bind_each_step_to_the_phone_key() {
        let svc = PairingService::default();
        let phone = Phone::new();
        let thief = Phone::new();
        let (nonce, _) = svc.create_offer("user_a");

        // Enrolling with someone else's key but your own signature fails.
        let pk = phone.public_key();
        assert_eq!(
            svc.enroll(
                &nonce,
                &pk,
                &thief.sign(&enroll_message(&nonce, &pk)),
                "x",
                "ios"
            ),
            Err(PairError::BadSignature)
        );
        // The failed attempt did not spend the code.
        let (id, _, _) = enroll(&svc, &phone, &nonce).unwrap();
        svc.begin_approve("user_a", &id).unwrap();
        svc.finish_approve(&id, "tok".into(), "dev1".into());

        // Only the enrolled key can collect the session.
        assert_eq!(
            svc.collect(&id, &thief.sign(&collect_message(&id))),
            Err(PairError::BadSignature)
        );
        // A signature for a different step does not transfer.
        assert_eq!(
            svc.collect(&id, &phone.sign(&enroll_message(&nonce, &pk))),
            Err(PairError::BadSignature)
        );
    }

    /// Pinned against the phone's implementation (`mobile/src/lib/pairingKey.ts`,
    /// `confirmationCode('nonce-1', 'pk-1')`). If this changes, the phone and
    /// the computer show different digits and nobody can pair.
    #[test]
    fn confirmation_code_matches_the_phone() {
        assert_eq!(confirmation_code("nonce-1", "pk-1"), "166872");
    }

    #[test]
    fn an_attacker_sees_different_digits() {
        let nonce = "n";
        let a = confirmation_code(nonce, &Phone::new().public_key());
        let b = confirmation_code(nonce, &Phone::new().public_key());
        // 1 in a million to collide; a fixed pair would be a broken derivation.
        assert_ne!(a, b);
    }

    #[test]
    fn other_accounts_cannot_see_or_approve_a_request() {
        let svc = PairingService::default();
        let (nonce, _) = svc.create_offer("user_a");
        let (id, _, _) = enroll(&svc, &Phone::new(), &nonce).unwrap();
        assert_eq!(svc.begin_approve("user_b", &id), Err(PairError::Gone));
        assert_eq!(svc.deny("user_b", &id), Err(PairError::Gone));
        svc.deny("user_a", &id).unwrap();
        assert_eq!(svc.begin_approve("user_a", &id), Err(PairError::Conflict));
    }

    #[test]
    fn denial_reaches_the_phone_once() {
        let svc = PairingService::default();
        let phone = Phone::new();
        let (nonce, _) = svc.create_offer("user_a");
        let (id, _, _) = enroll(&svc, &phone, &nonce).unwrap();
        svc.deny("user_a", &id).unwrap();
        let sig = phone.sign(&collect_message(&id));
        assert_eq!(svc.collect(&id, &sig).unwrap(), Collected::Denied);
        assert_eq!(svc.collect(&id, &sig), Err(PairError::Gone));
    }

    #[test]
    fn challenges_are_single_use() {
        let svc = PairingService::default();
        let (id, challenge, _) = svc.challenge("dev1").unwrap();
        assert_eq!(svc.take_challenge(&id), Some(("dev1".into(), challenge)));
        assert_eq!(svc.take_challenge(&id), None);
    }

    #[test]
    fn verify_rejects_malformed_input() {
        let phone = Phone::new();
        let sig = phone.sign("m");
        assert!(verify(&phone.public_key(), "m", &sig));
        assert!(!verify(&phone.public_key(), "m2", &sig));
        assert!(!verify("not-base64!!", "m", &sig));
        assert!(!verify(&phone.public_key(), "m", "short"));
    }

    #[test]
    fn qr_payload_stays_readable_by_older_apps() {
        let p = qr_payload(Some("https://box.tail.ts.net"), "default", "N");
        assert_eq!(p["v"], 1);
        assert_eq!(p["url"], "https://box.tail.ts.net");
        assert_eq!(p["nonce"], "N");
        let svg = qr_svg(&p).unwrap();
        assert!(svg.contains("<svg"));
    }
}
