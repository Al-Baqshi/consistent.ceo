//! Builds the axum [`Router`], wires the bearer-auth middleware, and hosts the
//! handful of routes owned directly by this module (health, sessions, metrics,
//! schedule CRUD) alongside the routers merged in from other modules
//! (control API, billing, vault, voice, webhooks, dashboard).
#[allow(unused_imports)]
use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};
use tracing::error;
use uuid::Uuid;

use crate::scheduler::ScheduledTask;
use crate::state::ServerState;

use crate::control_api::{AuthContext, PrincipalKind};

/// Resolve the static deploy secret to a principal.
///
/// Two forms. The bare secret authenticates as the fixed `bearer` principal —
/// this is the local desktop sidecar's login, and its legitimate use. The
/// `<secret>.<workspace>.<principal>` form fabricates *any* principal in *any*
/// workspace with no database lookup, which means anyone holding the deploy
/// secret can act as anyone; it is off unless `allow_principals` is set.
///
/// Either way the result is tagged [`PrincipalKind::Static`], so a handler that
/// cares can tell it apart from a real signed-in user — before this, downstream
/// code could not distinguish the two at all.
pub(crate) fn static_principal_from_token(
    token: &str,
    expected_secret: &str,
    allow_principals: bool,
) -> Option<AuthContext> {
    if secret_matches(token, expected_secret) {
        return Some(AuthContext {
            principal_id: "bearer".to_string(),
            workspace_id: "default".to_string(),
            kind: PrincipalKind::Static,
        });
    }
    if !allow_principals {
        return None;
    }
    // `<secret>.<workspace>.<principal>`: the secret is the first
    // `expected_secret.len()` bytes, followed by a dot. Slicing by length and
    // comparing in constant time keeps `strip_prefix`'s byte-by-byte early exit
    // from timing out the secret one character at a time.
    let split = expected_secret.len();
    if token.as_bytes().get(split) != Some(&b'.') || !token.is_char_boundary(split) {
        return None;
    }
    let (candidate, rest) = token.split_at(split);
    if !secret_matches(candidate, expected_secret) {
        return None;
    }
    let remainder = &rest[1..];
    let (workspace_id, principal_id) = remainder.split_once('.')?;
    let workspace_id = workspace_id.trim();
    let principal_id = principal_id.trim();
    if workspace_id.is_empty() || principal_id.is_empty() {
        return None;
    }
    Some(AuthContext {
        principal_id: principal_id.to_string(),
        workspace_id: workspace_id.to_string(),
        kind: PrincipalKind::Static,
    })
}

/// Compare a presented token with the deploy secret in constant time.
///
/// Both sides are hashed first so the comparison runs over two fixed-length
/// digests: `==` on the raw strings returns at the first differing byte (and
/// immediately on a length mismatch), which over enough requests leaks the
/// secret's length and then its prefix.
fn secret_matches(candidate: &str, expected: &str) -> bool {
    use sha2::{Digest, Sha256};
    use subtle::ConstantTimeEq;
    let candidate = Sha256::digest(candidate.as_bytes());
    let expected = Sha256::digest(expected.as_bytes());
    candidate.as_slice().ct_eq(expected.as_slice()).into()
}

/// Principal ids that stand for the machine's owner rather than a registered
/// account: `bearer` (the static deploy token, and the local-operator row the
/// desktop's `/api/auth/local-session` mints a session for) and `anonymous`
/// (no token configured at all). Mirrors `auth_api::is_local_operator`.
/// Registered accounts get a random uuid id, so no sign-up can claim these.
fn is_machine_owner_principal(principal_id: &str) -> bool {
    matches!(principal_id, "bearer" | "anonymous")
}

/// Whether this gateway is a self-hosted, single-operator install: bound to
/// loopback and not tunnelled through the relay. Only processes on this
/// machine can reach it, so every account on it was created by whoever is
/// sitting at it. Absent config reads as "no" — a missing answer to a security
/// question is a no.
///
/// The relay matters because it forwards `/g/<id>/api/auth/register` from the
/// internet to a loopback-bound gateway: with it on, a stranger can sign up.
pub(crate) fn gateway_is_self_hosted(state: &ServerState) -> bool {
    let loopback = state
        .app_config
        .as_ref()
        .map(|cfg| crate::billing::is_loopback_bind(&cfg.gateway_bind))
        .unwrap_or(false);
    loopback && crate::relay_client::RelayClientConfig::from_env(0).is_none()
}

/// Who may change instance-global state — LLM providers and their keys,
/// routing overrides, cloud config, directors, shell verifications.
///
/// Every registered user gets a workspace of their own, and on a hosted
/// deployment sign-up is open, so "authenticated" says nothing about trust.
/// An instance admin is one of:
///
/// - a **static principal**: the deploy-secret holder, or the no-token
///   `anonymous` operator (only possible on loopback — startup refuses an
///   open, network-bound gateway);
/// - the **machine owner's account** (`bearer` / `anonymous`), which is what
///   the desktop app's local session and a claimed local account sign in as;
/// - any signed-in user of a **self-hosted** gateway
///   ([`gateway_is_self_hosted`]), so a single person running it on their own
///   machine keeps the settings page exactly as before.
///
/// A self-registered user of a network-bound (hosted) gateway is none of these.
pub(crate) fn is_instance_admin(ctx: &AuthContext, self_hosted: bool) -> bool {
    ctx.is_static() || is_machine_owner_principal(&ctx.principal_id) || self_hosted
}

// When no secret is configured, everyone is the open `anonymous/default`
// principal; otherwise the bearer token must match the static-secret scheme.
fn auth_context_from_request(
    request: &Request<Body>,
    expected_secret: Option<&str>,
    allow_static_principals: bool,
) -> Option<AuthContext> {
    match expected_secret {
        None => Some(AuthContext {
            principal_id: "anonymous".to_string(),
            workspace_id: "default".to_string(),
            kind: PrincipalKind::Static,
        }),
        Some(secret) => {
            let bearer = extract_bearer(request)?;
            static_principal_from_token(&bearer, secret, allow_static_principals)
        }
    }
}

/// Whether this deployment opted into static/service principals.
fn allow_static_principals(state: &ServerState) -> bool {
    state
        .app_config
        .as_ref()
        .map(|cfg| cfg.gateway_static_principals)
        .unwrap_or(false)
}

/// Zero-config entry point (no webhook store) — used by tests and any caller
/// that doesn't need outbound webhook CRUD.
pub fn build_router(state: ServerState) -> Router {
    build_router_with_webhooks(state, None)
}

/// Same router, plus outbound webhook CRUD when a store is available.
///
/// `ServerState` deliberately doesn't carry the webhook store — the webhook
/// routes own their own state (see [`crate::webhooks::routes`]), so this is the
/// one seam where it's handed in. `build_router` stays the zero-config entry
/// point used by tests.
pub fn build_router_with_webhooks(
    state: ServerState,
    webhooks: crate::webhooks::WebhookState,
) -> Router {
    let inference = crate::control_api::inference_routes(state.clone()).layer(
        middleware::from_fn_with_state(state.clone(), crate::billing::require_credit),
    );

    let protected = Router::new()
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}/messages", get(get_session_messages))
        .route("/api/metrics", get(get_metrics))
        .route("/api/runtime/status", get(get_runtime_status))
        .route("/api/schedule", get(list_schedule).post(add_schedule))
        .route("/api/schedule/{id}", delete(remove_schedule))
        .with_state(state.clone())
        .merge(crate::control_api::control_routes(state.clone()))
        .merge(crate::canvas_api::canvas_routes(state.clone()))
        .merge(inference)
        .merge(crate::billing::billing_routes(state.clone()))
        .merge(crate::auth_api::auth_protected_routes(state.clone()))
        .merge(crate::pairing::protected_routes(state.clone()))
        .merge(crate::vault_api::vault_routes(state.clone()))
        .merge(crate::voice::voice_ticket_routes(state.clone()))
        .merge(crate::webhooks::routes(webhooks))
        // Order matters: the limiter runs *after* auth resolves (axum layers
        // apply outside-in on the response path, so the last layer added is
        // the outermost), which is what lets it key on the principal rather
        // than a phone's ever-changing IP.
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::rate_limit::limit_authenticated,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer_token,
        ));

    // Public auth endpoints mint accounts (and trial credits on hosted
    // deployments) — throttle them per client IP. No-op on loopback binds.
    let public_auth = crate::auth_api::auth_public_routes(state.clone())
        .merge(crate::pairing::public_routes(state.clone()))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            crate::rate_limit::limit_public_auth,
        ));

    Router::new()
        .route("/health", get(health))
        .with_state(state.clone())
        .merge(protected)
        .merge(public_auth)
        .merge(crate::voice::voice_routes(state))
        .merge(crate::dashboard::dashboard_routes())
}

async fn health(State(state): State<ServerState>) -> Json<HealthResponse> {
    // Restart counts, not panic payloads: `/health` is public, and a panic
    // message can carry a path or an id. `SubsystemRegistry::snapshot` has the
    // detail for an authenticated caller.
    let (loop_restarts, loops_restarted) = match state.subsystems.as_ref() {
        Some(registry) => {
            let counts = registry.restart_counts().await;
            let restarted = counts.values().filter(|n| **n > 0).count();
            (Some(counts), restarted)
        }
        None => (None, 0),
    };

    Json(HealthResponse {
        // A restarted loop still serves traffic, so this stays "ok" — but it
        // says "degraded" once a loop has crashed, because a monitoring check
        // that only ever reads `status` should still notice.
        status: if loops_restarted > 0 {
            "degraded"
        } else {
            "ok"
        },
        version: state.version,
        memory_backend: state.memory_backend.clone(),
        loop_restarts,
    })
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    version: String,
    memory_backend: String,
    /// `loop name → times it has panicked and been restarted`. Omitted when no
    /// supervisor registry is attached (a bare `ServerState` in tests).
    #[serde(skip_serializing_if = "Option::is_none")]
    loop_restarts: Option<std::collections::BTreeMap<&'static str, u32>>,
}

#[derive(Debug, Deserialize)]
struct SessionQuery {
    agent_id: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Serialize)]
struct PaginatedSessionResponse<T> {
    items: Vec<T>,
    total: usize,
    limit: usize,
    offset: usize,
    #[serde(rename = "hasMore")]
    has_more: bool,
}

/// The principal a chat session belongs to, when it belongs to one.
///
/// `consistent_world_common::types::Session` has no workspace or owner field
/// (and the memory store does not persist `scope_*`), so ownership is read off
/// the shape the agent-group chat gives its sessions:
/// `web-{agent_id}-{principal_id}`, with `peer_id` = the principal. The legacy
/// shared `web-{group}-user` session, channel-adapter sessions
/// (`telegram-<peer>`) and scheduler runs belong to no user — they are the
/// instance operator's.
pub(crate) fn session_owner(session: &consistent_world_common::types::Session) -> Option<&str> {
    let peer = session.peer_id.as_str();
    (session.channel == consistent_world_common::types::ChannelKind::Web
        && !peer.is_empty()
        && peer != "user"
        && session.id == format!("web-{}-{}", session.agent_id, peer))
        .then_some(peer)
}

/// Owned sessions are visible to their owner only; unowned ones (channels,
/// schedules, the legacy shared web session) to the instance admin.
fn session_visible_to(
    session: &consistent_world_common::types::Session,
    principal_id: &str,
    admin: bool,
) -> bool {
    match session_owner(session) {
        Some(owner) => owner == principal_id,
        None => admin,
    }
}

async fn list_sessions(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Query(q): Query<SessionQuery>,
) -> impl IntoResponse {
    let admin = is_instance_admin(&auth, gateway_is_self_hosted(&state));
    match state.memory.list_sessions().await {
        Ok(sessions) => {
            let filtered: Vec<_> = sessions
                .into_iter()
                .filter(|s| session_visible_to(s, &auth.principal_id, admin))
                .filter(|s| {
                    if let Some(ref agent_id) = q.agent_id {
                        &s.agent_id == agent_id
                    } else {
                        true
                    }
                })
                .collect();
            let total = filtered.len();
            let limit = q.limit.unwrap_or(50).min(200);
            let offset = q.offset.unwrap_or(0);
            let has_more = offset + limit < total;
            let sliced = if offset >= filtered.len() {
                Vec::new()
            } else {
                let end = (offset + limit).min(filtered.len());
                filtered[offset..end].to_vec()
            };
            Json(PaginatedSessionResponse {
                items: sliced,
                total,
                limit,
                offset,
                has_more,
            })
            .into_response()
        }
        Err(err) => {
            error!("Failed to list sessions: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_session_messages(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let admin = is_instance_admin(&auth, gateway_is_self_hosted(&state));
    let session = match state.memory.list_sessions().await {
        Ok(sessions) => sessions.into_iter().find(|s| s.id == session_id),
        Err(err) => {
            error!("Failed to list sessions: {err:?}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    // Messages can exist without a session row; those are nobody's in
    // particular, so only the admin may read them. A miss is a 404 either
    // way — "not yours" must not be distinguishable from "not there".
    let allowed = match session.as_ref() {
        Some(s) => session_visible_to(s, &auth.principal_id, admin),
        None => admin,
    };
    if !allowed {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.memory.get_session_messages(&session_id, 100).await {
        Ok(msgs) => {
            let pairs: Vec<serde_json::Value> = msgs
                .into_iter()
                .map(|(role, content)| serde_json::json!({ "role": role, "content": content }))
                .collect();
            Json(pairs).into_response()
        }
        Err(err) => {
            error!("Failed to get session messages: {err:?}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn get_metrics(State(state): State<ServerState>) -> impl IntoResponse {
    match &state.cost_tracker {
        Some(tracker) => {
            let snapshot = tracker.lock().unwrap().snapshot();
            Json(snapshot).into_response()
        }
        None => Json(consistent_world_common::telemetry::MetricsSnapshot {
            total_requests: 0,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_cost_usd: 0.0,
            per_model: std::collections::HashMap::new(),
            per_agent: std::collections::HashMap::new(),
        })
        .into_response(),
    }
}

async fn get_runtime_status(State(state): State<ServerState>) -> impl IntoResponse {
    match &state.runtime_status {
        Some(status) => Json(status).into_response(),
        None => Json(crate::state::RuntimeStatus {
            messaging_enabled: false,
            adapters: Vec::new(),
            agent_ids: Vec::new(),
            plugin_count: 0,
        })
        .into_response(),
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddScheduleRequest {
    cron_expression: String,
    agent_id: String,
    task_description: String,
    entity_id: Option<String>,
    connection_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ScheduleTaskResponse {
    id: String,
    cron_expression: String,
    next_run: String,
    agent_id: String,
    task_description: String,
    entity_id: Option<String>,
    connection_id: Option<String>,
}

/// Whether a schedule belongs to the caller's workspace. Rows written before
/// schedules carried a workspace read as the default workspace's, and the
/// instance admin can still see (and clean up) those.
fn schedule_visible_to(task: &ScheduledTask, workspace_id: &str, admin: bool) -> bool {
    match task.workspace_id.as_deref() {
        Some(ws) => ws == workspace_id,
        None => workspace_id == "default" || admin,
    }
}

fn schedule_response(t: ScheduledTask) -> ScheduleTaskResponse {
    ScheduleTaskResponse {
        id: t.id,
        cron_expression: t.cron_expression,
        next_run: t.next_run.to_rfc3339(),
        agent_id: t.agent_id,
        task_description: t.task_description,
        entity_id: t.entity_id,
        connection_id: t.connection_id,
    }
}

async fn list_schedule(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<ScheduleTaskResponse>>, StatusCode> {
    let Some(scheduler) = &state.scheduler else {
        return Ok(Json(Vec::new()));
    };
    let admin = is_instance_admin(&auth, gateway_is_self_hosted(&state));
    let tasks = scheduler
        .list_tasks()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let out: Vec<ScheduleTaskResponse> = tasks
        .into_iter()
        .filter(|t| schedule_visible_to(t, &auth.workspace_id, admin))
        .map(schedule_response)
        .collect();
    Ok(Json(out))
}

/// A fired schedule runs one of this instance's runtime agents, under the
/// tool policy of the entity it is bound to. So the entity must be one the
/// caller can see — before this, any `entityId` was accepted and the run
/// borrowed another tenant's roots — and the connection must belong to it.
/// An entity-less schedule runs with no entity policy at all, which only the
/// instance admin may ask for.
async fn validate_schedule_scope(
    state: &ServerState,
    auth: &AuthContext,
    entity_id: Option<&str>,
    connection_id: Option<&str>,
) -> Result<(), StatusCode> {
    let Some(entity_id) = entity_id else {
        if connection_id.is_some() {
            return Err(StatusCode::BAD_REQUEST);
        }
        return if is_instance_admin(auth, gateway_is_self_hosted(state)) {
            Ok(())
        } else {
            Err(StatusCode::FORBIDDEN)
        };
    };
    let control = state
        .control_state
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let visible = control.entities.read().await.iter().any(|e| {
        e.id == entity_id
            && crate::control_api::is_entity_visible(
                e,
                &auth.workspace_id,
                Some(auth.principal_id.as_str()),
            )
    });
    if !visible {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(connection_id) = connection_id {
        let owned = control
            .entity_connections
            .read()
            .await
            .iter()
            .any(|c| c.id == connection_id && c.entity_id == entity_id);
        if !owned {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    Ok(())
}

async fn add_schedule(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<AddScheduleRequest>,
) -> Result<Json<ScheduleTaskResponse>, StatusCode> {
    let Some(scheduler) = &state.scheduler else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let entity_id = body
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let connection_id = body
        .connection_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    validate_schedule_scope(
        &state,
        &auth,
        entity_id.as_deref(),
        connection_id.as_deref(),
    )
    .await?;
    let id = Uuid::new_v4().to_string();
    let task = ScheduledTask {
        id: id.clone(),
        cron_expression: body.cron_expression.clone(),
        next_run: chrono::Utc::now(),
        agent_id: body.agent_id.clone(),
        task_description: body.task_description.clone(),
        entity_id,
        connection_id,
        workspace_id: Some(auth.workspace_id.clone()),
        created_by: Some(auth.principal_id.clone()),
    };
    scheduler
        .add_task(task)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let tasks = scheduler
        .list_tasks()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let saved = tasks
        .into_iter()
        .find(|t| t.id == id)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(schedule_response(saved)))
}

async fn remove_schedule(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let Some(scheduler) = &state.scheduler else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let admin = is_instance_admin(&auth, gateway_is_self_hosted(&state));
    let tasks = scheduler
        .list_tasks()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // Someone else's schedule is indistinguishable from a missing one.
    if !tasks
        .iter()
        .any(|t| t.id == id && schedule_visible_to(t, &auth.workspace_id, admin))
    {
        return Err(StatusCode::NOT_FOUND);
    }
    scheduler
        .remove_task(&id)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(StatusCode::NO_CONTENT)
}

fn extract_bearer(request: &Request<Body>) -> Option<String> {
    request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Trial credit seeded on first sign-in. 500 cents = $5 unless overridden via
/// CONSISTENT_WORLD_TRIAL_CENTS (0 disables the grant).
const TRIAL_CREDIT_CENTS: i64 = 500;

fn trial_credit_cents() -> i64 {
    std::env::var("CONSISTENT_WORLD_TRIAL_CENTS")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .map(|v| v.max(0))
        .unwrap_or(TRIAL_CREDIT_CENTS)
}

// Runs on every authenticated request (cheap once the tenant/personal-space
// rows already exist): first-touch creates the tenant row and seeds trial
// credits, then unconditionally makes sure the user's Personal Space entity
// exists.
async fn bootstrap_tenant(state: &ServerState, principal_id: &str, workspace_id: &str) {
    let Some(tenant_store) = state.tenant_store.as_ref() else {
        return;
    };
    match tenant_store.ensure_tenant(principal_id, workspace_id).await {
        Ok(true) => {
            // Trial credits only mean something where the credit gate is on. A
            // self-hosted gateway would otherwise grow a ledger nobody reads.
            let cents = trial_credit_cents();
            if cents > 0
                && let Some(ledger) = state.credit_ledger.as_ref()
                && ledger.enforce
            {
                ledger.topup_cents(workspace_id, cents).await;
                tracing::info!(
                    workspace_id,
                    principal_id,
                    cents,
                    "Seeded trial credits for new workspace"
                );
            }
        }
        Ok(false) => {}
        Err(e) => tracing::warn!(error = %e, principal_id, "ensure_tenant failed"),
    }

    // Every user is an entity: ensure their private Personal Space exists. Runs
    // unconditionally (not gated on first sign-in) so existing accounts are
    // back-filled on their next request; the read-lock fast path keeps it cheap.
    if let Some(control) = state.control_state.as_ref() {
        control
            .ensure_personal_space(principal_id, workspace_id)
            .await;
    }
}

// Auth middleware: tries own-auth session tokens, then external-identity
// JWTs, then the static bearer scheme, in that order, and stashes the
// resolved `AuthContext` on the request for downstream handlers.
/// True when this request arrived over the relay tunnel rather than from a
/// process on this machine.
///
/// The relay strips any client-supplied copy of this header before setting its
/// own, so the marker cannot be forged *off*. It can be forged *on* by a local
/// process — which only restricts that process, never elevates it.
fn arrived_over_relay(request: &Request<Body>) -> bool {
    request
        .headers()
        .get(consistent_world_relay::protocol::RELAY_MARKER_HEADER)
        .is_some()
}

async fn require_bearer_token(
    State(state): State<ServerState>,
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // 1. Own-auth session token (the normal path): resolve the opaque bearer to a
    //    user via the auth store.
    if let (Some(store), Some(bearer)) = (state.auth_store.as_ref(), extract_bearer(&request))
        && let Some(identity) = store.resolve_session(&bearer).await
    {
        let ctx = AuthContext {
            principal_id: identity.user_id,
            workspace_id: identity.workspace_id,
            kind: PrincipalKind::User,
        };
        bootstrap_tenant(&state, &ctx.principal_id, &ctx.workspace_id).await;
        request.extensions_mut().insert(ctx);
        return Ok(next.run(request).await);
    }

    // 2. External identity: a JWT from a configured issuer, verified against
    //    its JWKS and mapped onto a local `auth_users` row. Inert unless
    //    `[gateway.auth.external] issuer` is set, which no self-hosted instance
    //    sets — local accounts remain the way in.
    //
    //    This sits ahead of the static path deliberately: with no static token
    //    configured, step 3 hands back the open `anonymous` context and would
    //    swallow every JWT before it was ever looked at.
    if let (Some(verifier), Some(store), Some(bearer)) = (
        state.external_identity.as_ref(),
        state.auth_store.as_ref(),
        extract_bearer(&request),
    ) && crate::external_identity::looks_like_jwt(&bearer)
    {
        match verifier.verify(&bearer).await {
            Ok(identity) => {
                let user = store
                    .ensure_external_user(
                        &identity.issuer,
                        &identity.subject,
                        identity.name.as_deref(),
                    )
                    .await
                    .map_err(|e| {
                        tracing::warn!(error = %e, "could not map external identity to a local account");
                        StatusCode::INTERNAL_SERVER_ERROR
                    })?;
                let ctx = AuthContext {
                    principal_id: user.user_id,
                    workspace_id: user.workspace_id,
                    kind: PrincipalKind::User,
                };
                bootstrap_tenant(&state, &ctx.principal_id, &ctx.workspace_id).await;
                request.extensions_mut().insert(ctx);
                return Ok(next.run(request).await);
            }
            // Not a token this issuer signed. Fall through rather than reject:
            // a scoped static bearer is also three dot-separated parts.
            Err(e) => tracing::debug!(error = %e, "bearer did not verify as an external JWT"),
        }
    }

    // 3. Static bearer token: the local desktop sidecar's login, plus the
    //    `secret.workspace.principal` impersonation form when a deployment has
    //    deliberately opted in. When no static token is configured, this yields
    //    the open `anonymous/default` context — safe only because the startup
    //    guard refuses to bind off-localhost without auth configured.
    let ctx = auth_context_from_request(
        &request,
        state.auth_token.as_deref(),
        allow_static_principals(&state),
    )
    .ok_or(StatusCode::UNAUTHORIZED)?;
    // Reaching this machine from outside requires a real signed-in session.
    // A deploy secret is a *local* credential: it exists so the desktop shell
    // can talk to its own sidecar, and the startup guard already refuses to
    // bind off-loopback while one is in use. The relay keeps the bind on
    // loopback, so that guard cannot see this case — this is the same rule,
    // applied where the principal is actually known.
    if matches!(ctx.kind, PrincipalKind::Static) && arrived_over_relay(&request) {
        tracing::warn!(
            principal = %ctx.principal_id,
            "refused a static-principal request arriving over the relay"
        );
        return Err(StatusCode::FORBIDDEN);
    }
    bootstrap_tenant(&state, &ctx.principal_id, &ctx.workspace_id).await;
    request.extensions_mut().insert(ctx);
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn req_with(headers: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::builder().uri("/api/tasks");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn the_relay_marker_is_recognised_in_any_casing() {
        assert!(arrived_over_relay(&req_with(&[(
            "x-consistent-relay",
            "1"
        )])));
        assert!(arrived_over_relay(&req_with(&[(
            "X-Consistent-Relay",
            "1"
        )])));
        // A local request carries no marker and stays unrestricted.
        assert!(!arrived_over_relay(&req_with(&[(
            "authorization",
            "Bearer abc"
        )])));
    }

    #[test]
    fn a_real_session_token_is_not_shaped_like_anything_the_relay_could_guess() {
        // Regression: the relay once tried to classify tokens by shape. Session
        // tokens and the auto-provisioned deploy token are both two uuid
        // simple-forms concatenated — 64 hex chars, no separators, identical.
        // No predicate over the string can tell them apart, which is why the
        // decision lives in the gateway instead.
        let session_shaped = format!("{}{}", "a".repeat(32), "b".repeat(32));
        assert_eq!(session_shaped.len(), 64);
        assert!(!session_shaped.contains('.'));
        // Meanwhile the *dotted* form is the static token's impersonation
        // syntax — the exact opposite of the old guess.
        let ctx = static_principal_from_token("secret.ws.user", "secret", true).unwrap();
        assert!(matches!(ctx.kind, PrincipalKind::Static));
    }

    #[test]
    fn parses_scoped_bearer_token_when_static_principals_are_enabled() {
        let ctx = static_principal_from_token("secret.ws-alpha.user-1", "secret", true).unwrap();
        assert_eq!(ctx.workspace_id, "ws-alpha");
        assert_eq!(ctx.principal_id, "user-1");
        assert!(ctx.is_static());
    }

    #[test]
    fn scoped_bearer_token_is_refused_by_default() {
        // The impersonation form fabricates a principal with no database
        // lookup. Off unless a deployment deliberately turns it on.
        assert!(static_principal_from_token("secret.ws-alpha.user-1", "secret", false).is_none());
    }

    #[test]
    fn auth_context_uses_legacy_secret_token() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Bearer secret")
            .body(Body::empty())
            .unwrap();
        // The bare secret is the desktop sidecar's login and keeps working
        // without the flag — but it is tagged as a static principal.
        let ctx = auth_context_from_request(&req, Some("secret"), false).unwrap();
        assert_eq!(ctx.workspace_id, "default");
        assert_eq!(ctx.principal_id, "bearer");
        assert!(ctx.is_static());
    }

    #[test]
    fn auth_context_uses_scoped_token_when_secret_matches_prefix() {
        let req = Request::builder()
            .header(header::AUTHORIZATION, "Bearer secret.ws-team.user-9")
            .body(Body::empty())
            .unwrap();
        let ctx = auth_context_from_request(&req, Some("secret"), true).unwrap();
        assert_eq!(ctx.workspace_id, "ws-team");
        assert_eq!(ctx.principal_id, "user-9");
    }

    fn ctx(principal: &str, workspace: &str, kind: PrincipalKind) -> AuthContext {
        AuthContext {
            principal_id: principal.to_string(),
            workspace_id: workspace.to_string(),
            kind,
        }
    }

    #[test]
    fn instance_admin_is_the_operator_not_any_signed_in_stranger() {
        let hosted = false;
        let self_hosted = true;
        // Deploy-secret holder and the no-token operator: always admin.
        assert!(is_instance_admin(
            &ctx("bearer", "default", PrincipalKind::Static),
            hosted
        ));
        assert!(is_instance_admin(
            &ctx("anonymous", "default", PrincipalKind::Static),
            hosted
        ));
        // Scoped static tokens also hold the deploy secret.
        assert!(is_instance_admin(
            &ctx("svc", "ws-team", PrincipalKind::Static),
            hosted
        ));
        // The desktop's local session is a User session on the local-operator row.
        assert!(is_instance_admin(
            &ctx("bearer", "default", PrincipalKind::User),
            hosted
        ));
        // A self-registered account on a hosted gateway is a tenant, not an admin…
        let uid = "3f2a9c0d1e5b4a7f8c6d2e1f0a9b8c7d";
        let stranger = ctx(uid, uid, PrincipalKind::User);
        assert!(!is_instance_admin(&stranger, hosted));
        // …but the single person running it on their own machine keeps admin.
        assert!(is_instance_admin(&stranger, self_hosted));
    }

    #[tokio::test]
    async fn a_gateway_without_config_is_not_treated_as_self_hosted() {
        let memory: std::sync::Arc<dyn consistent_world_memory::store::MemoryStore> =
            std::sync::Arc::new(
                consistent_world_memory::store::SurrealMemoryStore::connect_memory()
                    .await
                    .unwrap(),
            );
        let state = ServerState::new("t".into(), None, memory);
        assert!(!gateway_is_self_hosted(&state));
    }

    #[test]
    fn the_static_secret_still_matches_exactly_and_only_exactly() {
        assert!(static_principal_from_token("secret", "secret", false).is_some());
        assert!(static_principal_from_token("secreT", "secret", false).is_none());
        assert!(static_principal_from_token("secret ", "secret", false).is_none());
        assert!(static_principal_from_token("secre", "secret", false).is_none());
        assert!(static_principal_from_token("", "secret", false).is_none());
        // The scoped form requires the exact secret before the first dot.
        assert!(static_principal_from_token("secrex.ws.user", "secret", true).is_none());
        assert!(static_principal_from_token("secretx.ws.user", "secret", true).is_none());
        assert!(static_principal_from_token("secret.ws", "secret", true).is_none());
        // Multi-byte input around the split point must not panic.
        assert!(static_principal_from_token("secré.ws.user", "secret", true).is_none());
        let scoped =
            static_principal_from_token("secret.ws.user.with.dots", "secret", true).unwrap();
        assert_eq!(scoped.workspace_id, "ws");
        assert_eq!(scoped.principal_id, "user.with.dots");
    }

    fn session(id: &str, agent: &str, peer: &str) -> consistent_world_common::types::Session {
        consistent_world_common::types::Session {
            id: id.to_string(),
            agent_id: agent.to_string(),
            channel: consistent_world_common::types::ChannelKind::Web,
            peer_id: peer.to_string(),
            scope_type: Default::default(),
            scope_id: String::new(),
            channel_binding_metadata: None,
            resolution_reason: None,
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
        }
    }

    #[test]
    fn chat_sessions_are_visible_to_their_owner_only() {
        let alice = session("web-ops-alice", "ops", "alice");
        assert_eq!(session_owner(&alice), Some("alice"));
        assert!(session_visible_to(&alice, "alice", false));
        assert!(!session_visible_to(&alice, "mallory", false));
        // Not even the admin reads a user's own conversation.
        assert!(!session_visible_to(&alice, "bearer", true));
        // The legacy shared session and channel sessions are the operator's.
        let shared = session("web-ops-user", "ops", "user");
        assert_eq!(session_owner(&shared), None);
        assert!(session_visible_to(&shared, "bearer", true));
        assert!(!session_visible_to(&shared, "alice", false));
        // A peer id that does not match the id shape is not trusted as an owner.
        let spoof = session("telegram-42", "ops", "alice");
        assert_eq!(session_owner(&spoof), None);
        assert!(!session_visible_to(&spoof, "alice", false));
    }

    #[test]
    fn schedules_are_scoped_to_their_workspace() {
        let mut t = ScheduledTask {
            id: "s1".into(),
            cron_expression: "0 0 * * * *".into(),
            next_run: chrono::Utc::now(),
            agent_id: "a".into(),
            task_description: "d".into(),
            entity_id: None,
            connection_id: None,
            workspace_id: Some("ws-a".into()),
            created_by: Some("alice".into()),
        };
        assert!(schedule_visible_to(&t, "ws-a", false));
        assert!(!schedule_visible_to(&t, "ws-b", false));
        assert!(!schedule_visible_to(&t, "ws-b", true));
        // Legacy rows belong to the default workspace / the admin.
        t.workspace_id = None;
        assert!(schedule_visible_to(&t, "default", false));
        assert!(schedule_visible_to(&t, "ws-b", true));
        assert!(!schedule_visible_to(&t, "ws-b", false));
    }

    #[test]
    fn auth_context_allows_anonymous_when_secret_not_configured() {
        let req = Request::builder().body(Body::empty()).unwrap();
        let ctx = auth_context_from_request(&req, None, false).unwrap();
        assert_eq!(ctx.workspace_id, "default");
        assert_eq!(ctx.principal_id, "anonymous");
        // Nobody signed in — this is the token/no-auth operator, not a user.
        assert!(ctx.is_static());
    }
}
