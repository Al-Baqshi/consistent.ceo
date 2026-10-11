//! Voice session WebSocket route.
//!
//! Browser (or Tauri) opens `/api/voice/session`, streams PCM16 16kHz binary
//! frames in, and receives PCM16 24kHz binary frames out. The gateway brokers
//! this to Gemini Live (`BidiGenerateContent`).
//!
//! Authentication: `POST /api/voice/ticket` (bearer required) for a one-time,
//! 60-second ticket, then `GET /api/voice/session?ticket=…`. Clients that can
//! set headers may send `Authorization: Bearer` on the upgrade instead.
//!
//! Control messages use text frames carrying JSON:
//! - client → server: `{"type":"config","systemInstruction":"…"}`,
//!   `{"type":"text","text":"…"}`, `{"type":"audioDone"}` (push-to-talk: commit
//!   the buffered audio now instead of waiting for trailing silence)
//! - server → client: `{"type":"setupComplete"}`, `{"type":"text","delta":"…"}`,
//!   `{"type":"inputTranscription","delta":"…"}` (what the user said),
//!   `{"type":"turnComplete"}`, `{"type":"interrupted"}`, `{"type":"error","message":"…"}`

use axum::{
    Extension, Json, Router,
    extract::{
        Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use consistent_world_audio::gemini_live::{LiveConfig, LiveEvent, LiveSession};
use dashmap::DashMap;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::state::ServerState;

/// Public (unauthenticated at the router level) voice WebSocket route; auth
/// happens inside via ticket or bearer header.
pub fn voice_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/voice/session", get(voice_session))
        .with_state(state)
}

/// Protected route (bearer required) that mints a voice ticket. Merged into the
/// authenticated tree by `server::build_router_with_webhooks`.
pub fn voice_ticket_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/voice/ticket", post(issue_ticket))
        .with_state(state)
}

/// How long a voice ticket is good for. Long enough to open a socket, short
/// enough that a leaked URL is worthless by the time anyone reads the log.
const TICKET_TTL: Duration = Duration::from_secs(60);

// State kept for a live ticket: who it's for and when it was minted.
struct Ticket {
    workspace_id: String,
    principal_id: String,
    issued: Instant,
}

/// One-time tickets, keyed by an opaque random id.
///
/// A browser's `WebSocket` constructor cannot set headers, which is why the
/// bearer used to ride in the query string — where it lands in access logs,
/// proxy logs, and `Referer`. A ticket is the standard way out: it is minted by
/// an ordinary authenticated request, is single-use, and dies in a minute.
static TICKETS: LazyLock<DashMap<String, Ticket>> = LazyLock::new(DashMap::new);

#[derive(Debug, Deserialize)]
struct VoiceSessionQuery {
    ticket: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TicketResponse {
    ticket: String,
    expires_in_seconds: u64,
}

// Mints a fresh ticket and opportunistically sweeps expired ones first.
async fn issue_ticket(
    Extension(ctx): Extension<crate::control_api::AuthContext>,
) -> Json<TicketResponse> {
    let now = Instant::now();
    TICKETS.retain(|_, t| now.duration_since(t.issued) < TICKET_TTL);
    let ticket = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    TICKETS.insert(
        ticket.clone(),
        Ticket {
            workspace_id: ctx.workspace_id,
            principal_id: ctx.principal_id,
            issued: now,
        },
    );
    Json(TicketResponse {
        ticket,
        expires_in_seconds: TICKET_TTL.as_secs(),
    })
}

/// Redeem a ticket, consuming it. Expired tickets are refused, not merely swept.
fn redeem_ticket(ticket: &str) -> Option<(String, String)> {
    let (_, t) = TICKETS.remove(ticket)?;
    (Instant::now().duration_since(t.issued) < TICKET_TTL)
        .then_some((t.workspace_id, t.principal_id))
}

/// Resolve `(workspace_id, principal_id)` for a socket upgrade.
///
/// Two ways in, neither of which puts a long-lived credential in a URL:
/// a one-time `?ticket=`, or an ordinary `Authorization: Bearer` header for
/// clients that can set one (the Tauri native path, the mobile app).
async fn ws_auth(
    state: &ServerState,
    ticket: Option<&str>,
    headers: &HeaderMap,
) -> Option<(String, String)> {
    if let Some(ticket) = ticket.map(str::trim).filter(|t| !t.is_empty())
        && let Some(ids) = redeem_ticket(ticket)
    {
        return Some(ids);
    }

    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())?;

    if let Some(store) = state.auth_store.as_ref()
        && let Some(identity) = store.resolve_session(bearer).await
    {
        return Some((identity.workspace_id, identity.user_id));
    }
    // The static-token path, resolved by the same code the HTTP middleware uses
    // — this used to be a second, subtly different copy of the parse.
    let expected = state.auth_token.as_deref()?;
    let allow_principals = state
        .app_config
        .as_ref()
        .map(|cfg| cfg.gateway_static_principals)
        .unwrap_or(false);
    crate::server::static_principal_from_token(bearer, expected, allow_principals)
        .map(|ctx| (ctx.workspace_id, ctx.principal_id))
}

/// Builds the entity-aware base system instruction for a voice session.
///
/// The speech model is only the mouth and ears — this instruction is how the
/// gateway (the brain) tells it which entities exist for THIS viewer, so
/// spoken references like "the skirting business" land on the right entity.
/// Personal Spaces of other users are invisible here for the same reason they
/// are invisible everywhere else: `is_entity_visible` gates on the viewer.
async fn build_voice_context(
    state: &ServerState,
    workspace_id: &str,
    principal_id: &str,
) -> String {
    let mut catalog = String::new();
    let mut mission_lines = String::new();
    if let Some(control) = state.control_state.as_ref() {
        control
            .ensure_personal_space(principal_id, workspace_id)
            .await;
        let entities = control.entities.read().await;
        let missions = control.missions.read().await;
        for e in entities.iter() {
            if !crate::control_api::is_entity_visible(e, workspace_id, Some(principal_id)) {
                continue;
            }
            let flavor = if e.is_personal {
                " — the user's private Personal Space; loose thoughts and unfiled items belong here"
            } else {
                ""
            };
            let desc = e
                .description
                .as_deref()
                .or(e.setup.what_it_is.as_deref())
                .unwrap_or("");
            catalog.push_str(&format!(
                "- {} (id: {}){}{}{}\n",
                e.name,
                e.id,
                if desc.is_empty() { "" } else { ": " },
                desc,
                flavor
            ));
            for m in missions.iter().filter(|m| {
                m.entity_id == e.id
                    && matches!(
                        m.status,
                        crate::state::MissionStatus::Planned | crate::state::MissionStatus::Active
                    )
            }) {
                mission_lines.push_str(&format!("- [{}] {}\n", e.name, m.title));
            }
        }
    }
    let mut out = String::from(
        "You are Consistent, the user's voice companion — a sharp operating partner for their \
         businesses and goals. Speak like a trusted human co-founder: warm, direct, brief. \
         Spoken answers should usually be one to three sentences.\n\
         Be a validator, not a cheerleader: when the user proposes something weak, push back \
         with the concrete failure scenario. Never invent facts about their businesses — if \
         you don't know, say so.\n\n",
    );
    if catalog.is_empty() {
        out.push_str("The user has no entities yet — help them describe their first one.\n");
    } else {
        out.push_str(
            "The user's entities (when they mention a company, project, or goal by name, \
             connect what they say to the right entity and refer to it naturally):\n",
        );
        out.push_str(&catalog);
    }
    if !mission_lines.is_empty() {
        out.push_str("\nOpen missions you both know about:\n");
        out.push_str(&mission_lines);
    }
    out
}

// WS upgrade handshake: authenticate, check credit, then hand off to handle_socket.
async fn voice_session(
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
    Query(q): Query<VoiceSessionQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let (workspace_id, principal_id) = match ws_auth(&state, q.ticket.as_deref(), &headers).await {
        Some(ids) => ids,
        None => return StatusCode::UNAUTHORIZED.into_response(),
    };

    if let Some(ledger) = state.credit_ledger.as_ref()
        && ledger.credit_exhausted(&workspace_id).await
    {
        return StatusCode::PAYMENT_REQUIRED.into_response();
    }

    ws.on_upgrade(move |socket| handle_socket(socket, state, workspace_id, principal_id))
        .into_response()
}

// Client -> server control messages, carried as JSON text frames.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientControl {
    Config {
        #[serde(default)]
        system_instruction: Option<String>,
        #[serde(default)]
        voice: Option<String>,
        #[serde(default)]
        model: Option<String>,
        /// Response modalities, e.g. `["TEXT"]` for clients that cannot play
        /// raw PCM (the Expo app until it ships a native audio streamer).
        /// Defaults to AUDIO when omitted.
        #[serde(default)]
        modalities: Option<Vec<String>>,
    },
    Text {
        text: String,
    },
    /// Push-to-talk: the client finished speaking — commit the buffered audio
    /// upstream now instead of waiting for trailing silence.
    AudioDone,
}

// Server -> client control messages, carried as JSON text frames.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ServerControl<'a> {
    SetupComplete,
    Text { delta: &'a str },
    InputTranscription { delta: &'a str },
    TurnComplete,
    Interrupted,
    Error { message: &'a str },
}

fn text_frame<T: Serialize>(v: &T) -> Message {
    Message::Text(serde_json::to_string(v).unwrap_or_default().into())
}

// Bridges the client WebSocket and the upstream Gemini Live session for the
// life of one voice call, relaying audio/text frames in both directions.
async fn handle_socket(
    socket: WebSocket,
    state: ServerState,
    workspace_id: String,
    principal_id: String,
) {
    // On-demand brain box: wake the RunPod voice pod for the session and let
    // the idle reaper stop it later. No-op unless RUNPOD_API_KEY+VOICE_POD_ID
    // are configured.
    let pod = crate::voice_pod::global();
    if let Some(p) = pod.as_ref() {
        p.on_session_start();
    }
    struct PodSessionGuard(Option<std::sync::Arc<crate::voice_pod::VoicePodManager>>);
    impl Drop for PodSessionGuard {
        fn drop(&mut self) {
            if let Some(p) = self.0.as_ref() {
                p.on_session_end();
            }
        }
    }
    let _pod_guard = PodSessionGuard(pod);
    let api_key = match std::env::var("GEMINI_API_KEY").or_else(|_| std::env::var("GOOGLE_API_KEY"))
    {
        Ok(k) if !k.is_empty() => k,
        _ => {
            let (mut tx, _) = socket.split();
            let _ = tx
                .send(text_frame(&ServerControl::Error {
                    message: "GEMINI_API_KEY not configured",
                }))
                .await;
            return;
        }
    };

    // Entity-aware base instruction, computed once per session. Client config
    // can only append to it — the entity catalog is the gateway's to control.
    let base_instruction = build_voice_context(&state, &workspace_id, &principal_id).await;
    let merge_instruction = move |extra: Option<String>| -> Option<String> {
        Some(match extra {
            Some(x) if !x.trim().is_empty() => format!("{base_instruction}\n\n{x}"),
            _ => base_instruction.clone(),
        })
    };

    let (mut ws_tx, mut ws_rx) = socket.split();
    let mut live: Option<LiveSession> = None;
    let mut pending_config: Option<LiveConfig> = None;
    // Every voice session can queue work on the operator's machine. It only
    // ever *queues* it: the action lands in the approval queue exactly like
    // one typed in chat, so a misheard sentence in a car cannot act on its own.
    let with_tools = |mut cfg: LiveConfig| -> LiveConfig {
        cfg.tools = vec![run_on_computer_declaration()];
        cfg
    };

    loop {
        tokio::select! {
            client_msg = ws_rx.next() => {
                let Some(msg) = client_msg else { break };
                let msg = match msg {
                    Ok(m) => m,
                    Err(e) => {
                        warn!("voice ws recv error: {e}");
                        break;
                    }
                };
                match msg {
                    Message::Binary(audio) => {
                        if live.is_none() {
                            let mut cfg = pending_config.take().unwrap_or_default();
                            cfg.system_instruction =
                                merge_instruction(cfg.system_instruction.take());
                            match LiveSession::connect(&api_key, with_tools(cfg)).await {
                                Ok(s) => live = Some(s),
                                Err(e) => {
                                    let msg = format!("live connect failed: {e}");
                                    let _ = ws_tx.send(text_frame(&ServerControl::Error { message: &msg })).await;
                                    break;
                                }
                            }
                        }
                        if let Some(s) = &live
                            && let Err(e) = s.send_audio(audio.to_vec()).await {
                                warn!("live send_audio failed: {e}");
                                break;
                            }
                    }
                    Message::Text(t) => {
                        match serde_json::from_str::<ClientControl>(t.as_str()) {
                            Ok(ClientControl::Config { system_instruction, voice, model, modalities }) => {
                                let mut cfg = LiveConfig::default();
                                if let Some(m) = model { cfg.model = m; }
                                cfg.system_instruction = system_instruction;
                                cfg.voice = voice;
                                if let Some(m) = modalities.filter(|m| !m.is_empty()) {
                                    cfg.response_modalities = m;
                                }
                                pending_config = Some(cfg);
                            }
                            Ok(ClientControl::Text { text }) => {
                                if live.is_none() {
                                    let mut cfg = pending_config.take().unwrap_or_default();
                            cfg.system_instruction =
                                merge_instruction(cfg.system_instruction.take());
                                    match LiveSession::connect(&api_key, with_tools(cfg)).await {
                                        Ok(s) => live = Some(s),
                                        Err(e) => {
                                            let msg = format!("live connect failed: {e}");
                                            let _ = ws_tx.send(text_frame(&ServerControl::Error { message: &msg })).await;
                                            break;
                                        }
                                    }
                                }
                                if let Some(s) = &live
                                    && let Err(e) = s.send_text(text).await {
                                        warn!("live send_text failed: {e}");
                                        break;
                                    }
                            }
                            Ok(ClientControl::AudioDone) => {
                                // Nothing to commit if no audio was ever streamed.
                                if let Some(s) = &live
                                    && let Err(e) = s.send_audio_stream_end().await {
                                        warn!("live send_audio_stream_end failed: {e}");
                                        break;
                                    }
                            }
                            Err(e) => {
                                debug!("voice ws unknown control json: {e}");
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            live_evt = async {
                match &mut live {
                    Some(s) => s.events.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(evt) = live_evt else {
                    info!("live event stream closed");
                    // Tell the client instead of silently dropping the socket —
                    // an invalid upstream API key or retired model lands here.
                    let _ = ws_tx
                        .send(text_frame(&ServerControl::Error {
                            message: "voice model connection closed — check the upstream API key and model",
                        }))
                        .await;
                    break;
                };
                match evt {
                    LiveEvent::SetupComplete => {
                        let _ = ws_tx.send(text_frame(&ServerControl::SetupComplete)).await;
                    }
                    LiveEvent::Audio(bytes) => {
                        if ws_tx.send(Message::Binary(bytes.into())).await.is_err() { break }
                    }
                    LiveEvent::Text(delta) => {
                        let _ = ws_tx.send(text_frame(&ServerControl::Text { delta: &delta })).await;
                    }
                    LiveEvent::InputTranscription(delta) => {
                        let _ = ws_tx.send(text_frame(&ServerControl::InputTranscription { delta: &delta })).await;
                    }
                    LiveEvent::TurnComplete => {
                        let _ = ws_tx.send(text_frame(&ServerControl::TurnComplete)).await;
                    }
                    LiveEvent::Interrupted => {
                        let _ = ws_tx.send(text_frame(&ServerControl::Interrupted)).await;
                    }
                    LiveEvent::Error(m) => {
                        let _ = ws_tx.send(text_frame(&ServerControl::Error { message: &m })).await;
                        break;
                    }
                    LiveEvent::ToolCall { id, name, args } => {
                        let result = handle_voice_tool_call(
                            &state,
                            &workspace_id,
                            &principal_id,
                            &name,
                            &args,
                        )
                        .await;
                        if let Some(s) = &live
                            && let Err(e) = s.send_tool_response(id, name, result).await
                        {
                            warn!("live send_tool_response failed: {e}");
                            break;
                        }
                    }
                    LiveEvent::Raw(_) => {}
                }
            }
        }
    }

    if let Some(s) = &live {
        let _ = s.close().await;
    }
}


/// The one function a voice session may call.
///
/// Voice is the most error-prone input the product has — a car, road noise,
/// a half-finished sentence — so it can queue work but never authorise it.
/// The reply the model speaks says so explicitly.
fn run_on_computer_declaration() -> serde_json::Value {
    serde_json::json!({
        "name": "run_on_computer",
        "description": "Queue a real action on the user's own computer (organise or edit files, \
                        run project work, drive the browser). The action does NOT run until the \
                        user approves it on their phone or desktop. Use when the user asks you to \
                        DO something on their machine. Always tell them it is waiting for approval.",
        "parameters": {
            "type": "object",
            "properties": {
                "instruction": {
                    "type": "string",
                    "description": "Exactly what to do, in enough detail for an agent working alone."
                },
                "kind": {
                    "type": "string",
                    "enum": ["files", "shell", "browser", "general"],
                    "description": "What the action mostly touches."
                },
                "entity_name": {
                    "type": "string",
                    "description": "The project or business this belongs to, if the user named one."
                }
            },
            "required": ["instruction"]
        }
    })
}

/// Execute a function call from a voice session by reusing the same chat tool
/// the typed path uses — one implementation, one safety story.
async fn handle_voice_tool_call(
    state: &ServerState,
    workspace_id: &str,
    principal_id: &str,
    name: &str,
    args: &serde_json::Value,
) -> serde_json::Value {
    if name != "run_on_computer" {
        return serde_json::json!({ "ok": false, "error": format!("unknown function: {name}") });
    }
    let Some(control) = state.control_state.clone() else {
        return serde_json::json!({ "ok": false, "error": "workspace unavailable" });
    };
    // Same gate as chat: machine actions exist only where the gateway *is*
    // the user's machine.
    if state.local_files.is_none() {
        return serde_json::json!({
            "ok": false,
            "error": "this gateway is not running on the user's own computer, so it cannot act on it"
        });
    }
    let tool = crate::chat_tools::RunOnMyComputerTool {
        control,
        workspace_id: std::sync::Arc::from(workspace_id),
        entity_id: None,
        principal_id: std::sync::Arc::from(principal_id),
    };
    match consistent_world_tools::Tool::execute(&tool, args.clone()).await {
        Ok(result) => serde_json::from_str(&result.output)
            .unwrap_or_else(|_| serde_json::json!({ "ok": !result.is_error })),
        Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};
    use std::sync::Arc;

    async fn test_state() -> ServerState {
        let memory: Arc<dyn MemoryStore> =
            Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
        let control = Arc::new(crate::state::ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        ServerState::new("t".into(), Some("secret".into()), memory).with_control_state(control)
    }

    #[test]
    fn audio_done_control_parses() {
        let c: ClientControl = serde_json::from_str(r#"{"type":"audioDone"}"#).unwrap();
        assert!(matches!(c, ClientControl::AudioDone));
    }

    fn bearer_headers(token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {token}").parse().unwrap(),
        );
        h
    }

    #[tokio::test]
    async fn ws_auth_reads_the_bearer_from_the_header() {
        let state = test_state().await;
        assert_eq!(
            ws_auth(&state, None, &bearer_headers("secret")).await,
            Some(("default".to_string(), "bearer".to_string()))
        );
        assert_eq!(ws_auth(&state, None, &bearer_headers("wrong")).await, None);
        assert_eq!(ws_auth(&state, None, &HeaderMap::new()).await, None);
    }

    #[tokio::test]
    async fn ws_auth_no_longer_accepts_a_token_in_the_query_string() {
        // The bearer used to ride in `?token=`, where it lands in access logs.
        // A query value is now only ever a ticket, and a raw token is not one.
        let state = test_state().await;
        assert_eq!(
            ws_auth(&state, Some("secret"), &HeaderMap::new()).await,
            None
        );
        assert_eq!(
            ws_auth(&state, Some("secret.ws-alpha.user-1"), &HeaderMap::new()).await,
            None
        );
    }

    #[tokio::test]
    async fn a_voice_ticket_works_once_and_only_once() {
        let ctx = crate::control_api::AuthContext {
            principal_id: "user-1".into(),
            workspace_id: "ws-alpha".into(),
            kind: crate::control_api::PrincipalKind::User,
        };
        let Json(issued) = issue_ticket(Extension(ctx)).await;
        assert_eq!(
            redeem_ticket(&issued.ticket),
            Some(("ws-alpha".to_string(), "user-1".to_string()))
        );
        // Single use: a replayed ticket buys nothing.
        assert_eq!(redeem_ticket(&issued.ticket), None);
    }

    #[tokio::test]
    async fn voice_context_lists_own_entities_and_hides_other_users_personal_space() {
        let state = test_state().await;
        let control = state.control_state.as_ref().unwrap();
        control.entities.write().await.push(crate::state::Entity {
            id: "ent-biz".into(),
            name: "Skirting Co".into(),
            description: Some("skirting boards business".into()),
            owner: Some("ws1".into()),
            status: "Active".into(),
            dept: "General".into(),
            ..Default::default()
        });
        // Bob's personal space must never leak into Alice's voice context.
        control.ensure_personal_space("bob", "ws1").await;

        let ctx = build_voice_context(&state, "ws1", "alice").await;
        assert!(ctx.contains("Skirting Co"), "business entity listed: {ctx}");
        assert!(
            ctx.contains(&crate::state::personal_space_id("ws1", "alice")),
            "alice's personal space auto-provisioned and listed: {ctx}"
        );
        assert!(
            !ctx.contains(&crate::state::personal_space_id("ws1", "bob")),
            "bob's personal space hidden from alice: {ctx}"
        );
        assert!(ctx.contains("validator"), "persona keeps adversarial mode");
    }
}