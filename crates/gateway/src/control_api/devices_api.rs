//! Devices: where the companion can reach the operator.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Body for registering (or re-registering) a companion device.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegisterDeviceRequest {
    /// `"ios"` | `"android"` | `"desktop"` | `"web"`.
    pub platform: String,
    /// Stable per-install id from the client. The idempotency key: reopening
    /// the app re-registers the same row instead of adding another.
    pub install_id: String,
    pub label: Option<String>,
    /// Expo push token. Omit for a poll-only device — that's the default
    /// posture, and it keeps notifications entirely on the operator's machines.
    pub push_token: Option<String>,
    /// What the user wants, separately from whether a token arrived this
    /// time. `Some(true)` with no token means "keep pushing to the token you
    /// have" (Expo failed to mint one on this launch); `Some(false)` means
    /// "stop". Older clients omit it and keep the old meaning: no token, no push.
    #[serde(default)]
    pub push: Option<bool>,
}

/// Extracts the device store, failing with 503 when it isn't wired up.
pub(super) fn device_store(
    s: &ControlState,
) -> Result<&std::sync::Arc<crate::device_store::SurrealDeviceStore>, StatusCode> {
    s.device_store.get().ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// `POST` handler: registers a companion device, upserting on `install_id`
/// so a reopened app updates its existing row instead of duplicating it.
pub(super) async fn register_device(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<RegisterDeviceRequest>,
) -> Result<Json<crate::device_store::Device>, StatusCode> {
    let s = control(&state)?;
    let store = device_store(s)?;

    let platform = body.platform.trim().to_ascii_lowercase();
    let install_id = body.install_id.trim().to_string();
    if platform.is_empty() || install_id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let push_token = body
        .push_token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());

    let now = chrono::Utc::now().to_rfc3339();
    // Re-registering keeps the row's id and its delivery cursor, so a restarted
    // app doesn't replay everything it already showed.
    let existing = store
        .find_by_install(&workspace.id, &install_id)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "device lookup failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let device = match existing {
        Some(mut d) => {
            d.platform = platform;
            d.label = body.label.or(d.label);
            // Every app launch re-registers. Clearing the token whenever one
            // didn't arrive switched push off silently after a cold start.
            d.push_token = match (push_token, body.push) {
                (Some(token), _) => Some(token),
                (None, Some(true)) => d.push_token,
                (None, _) => None,
            };
            d.enabled = true;
            d.last_seen_at = now;
            d
        }
        None => crate::device_store::Device {
            id: format!("dev_{}", uuid::Uuid::new_v4()),
            workspace_id: workspace.id.clone(),
            platform,
            install_id,
            label: body.label,
            push_token,
            enabled: true,
            last_seen_feed_event_id: None,
            created_at: now.clone(),
            last_seen_at: now,
        },
    };

    store.upsert(&device).await.map_err(|e| {
        tracing::warn!(error = %e, "device upsert failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(device))
}

/// `GET` handler: lists registered devices for the workspace.
pub(super) async fn list_devices(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<Vec<crate::device_store::Device>>, StatusCode> {
    let s = control(&state)?;
    let store = device_store(s)?;
    let devices = store.list_by_workspace(&workspace.id).await.map_err(|e| {
        tracing::warn!(error = %e, "device list failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(devices))
}

/// Handler: unregisters a device, scoped to the caller's workspace.
pub(super) async fn delete_device(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let s = control(&state)?;
    let store = device_store(s)?;
    // Scope the delete to the caller's workspace so an id from elsewhere 404s
    // rather than unregistering someone else's phone.
    let existing = store.get(&id).await.map_err(|e| {
        tracing::warn!(error = %e, "device lookup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    match existing {
        Some(d) if d.workspace_id == workspace.id => {
            store.delete(&id).await.map_err(|e| {
                tracing::warn!(error = %e, "device delete failed");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
            Ok(StatusCode::NO_CONTENT)
        }
        _ => Err(StatusCode::NOT_FOUND),
    }
}

/// `GET` handler: lists agents, optionally filtered by group.
pub(super) async fn list_agents(
    State(state): State<ServerState>,
    Query(q): Query<AgentsQuery>,
) -> Result<Json<Vec<Agent>>, StatusCode> {
    let s = control(&state)?;
    let agents = s.agents.read().await;
    let out: Vec<Agent> = if let Some(ref group_id) = q.group_id {
        agents
            .iter()
            .filter(|a| a.group_id == *group_id)
            .cloned()
            .collect()
    } else {
        agents.clone()
    };
    Ok(Json(out))
}

/// `POST` handler: creates an agent within an existing group.
pub(super) async fn create_agent(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateAgentRequest>,
) -> Result<Json<Agent>, StatusCode> {
    // Agents live in the instance-wide director groups.
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    if body.name.trim().is_empty() || body.role.trim().is_empty() || body.group_id.trim().is_empty()
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    let groups = s.agent_groups.read().await;
    if !groups.iter().any(|g| g.id == body.group_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    drop(groups);

    let id = body
        .id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string())
        .trim()
        .to_string();
    if id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut agents = s.agents.write().await;
    if agents.iter().any(|a| a.id == id) {
        return Err(StatusCode::CONFLICT);
    }
    let agent = Agent {
        id,
        group_id: body.group_id,
        name: body.name.trim().to_string(),
        role: body.role.trim().to_string(),
        kind: body.kind.unwrap_or_else(|| "general".to_string()),
        preferred_provider: body
            .preferred_provider
            .unwrap_or_else(|| "default".to_string()),
        preferred_model: body
            .preferred_model
            .unwrap_or_else(|| "default".to_string()),
        mission: body.mission.unwrap_or_default(),
        profile: body.profile.unwrap_or_default(),
    };
    agents.push(agent.clone());
    Ok(Json(agent))
}

/// The caller's own chat session with an agent group.
///
/// It used to be one `web-{group}-user` session shared by every user of the
/// instance, so everyone read everyone's messages. The principal is part of
/// the id now, which is also the shape `server::session_owner` recognises.
fn agent_chat_session_id(group_id: &str, principal_id: &str) -> String {
    format!("web-{group_id}-{principal_id}")
}

/// Key for the in-memory fallback cache — per group *and* per principal, for
/// the same reason.
fn agent_chat_cache_key(group_id: &str, principal_id: &str) -> String {
    format!("{group_id}#{principal_id}")
}

/// A message read back for the agent chat, with a sort key.
struct ChatLine {
    message: AgentMessage,
    /// RFC 3339; empty when the store could not say.
    at: String,
}

/// `HH:MM` (UTC, like the send path) from an RFC 3339 timestamp.
fn clock_time(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|dt| dt.with_timezone(&chrono::Utc).format("%H:%M").to_string())
        .unwrap_or_default()
}

/// A stable id for a message the store returned without one: the session, the
/// role and text, and how many identical messages came before it. Only used
/// when the tenant store (and so the stored ids) is unavailable.
fn synthetic_message_id(session_id: &str, role: &str, content: &str, occurrence: usize) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!(
        "{session_id}\u{1f}{role}\u{1f}{content}\u{1f}{occurrence}"
    ));
    format!("{session_id}-{}", hex::encode(&digest[..8]))
}

/// One session's messages, oldest first, with ids that are unique and stable.
async fn read_chat_lines(state: &ServerState, session_id: &str) -> (Vec<ChatLine>, bool) {
    if let Some(store) = state.tenant_store.as_ref() {
        match store.session_messages(session_id, 50).await {
            Ok(rows) => {
                return (
                    rows.into_iter()
                        .map(|m| ChatLine {
                            message: AgentMessage {
                                id: m.id,
                                text: m.content,
                                time: clock_time(&m.timestamp),
                                is_agent: m.role == "assistant",
                            },
                            at: m.timestamp,
                        })
                        .collect(),
                    true,
                );
            }
            Err(e) => tracing::warn!(error = %e, session_id, "agent chat: stored-message read failed; using the memory store"),
        }
    }
    let msgs = state
        .memory
        .get_session_messages(session_id, 50)
        .await
        .unwrap_or_default();
    let mut seen: HashMap<(String, String), usize> = HashMap::new();
    let lines = msgs
        .into_iter()
        .map(|(role, content)| {
            let n = seen.entry((role.clone(), content.clone())).or_insert(0);
            let id = synthetic_message_id(session_id, &role, &content, *n);
            *n += 1;
            ChatLine {
                message: AgentMessage {
                    id,
                    text: content,
                    time: String::new(),
                    is_agent: role == "assistant",
                },
                at: String::new(),
            }
        })
        .collect();
    (lines, false)
}

/// `GET` handler: the caller's chat history with an agent group, merged from
/// the memory store plus any in-memory fallback not yet persisted there.
///
/// Every message gets its real stored id and time, and the list is ordered by
/// time. It used to label each message `{session}-{role}` — the same id for
/// every user message, which crashed the dashboard's keyed list
/// (`each_key_duplicate`), defeated the fallback dedupe, and, sorted by that
/// id, put every assistant reply above every user message.
pub(super) async fn get_agent_messages(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Extension(auth): Extension<AuthContext>,
    Path(group_id): Path<String>,
) -> Result<Json<Vec<AgentMessage>>, StatusCode> {
    let s = control(&state)?;
    let memory = &state.memory;
    let principal = workspace.principal_id.as_str();
    let admin =
        crate::server::is_instance_admin(&auth, crate::server::gateway_is_self_hosted(&state));

    let sessions = memory.list_sessions().await.map_err(|e| {
        tracing::error!("Failed to list sessions: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    // The caller's own session, plus — for the instance admin — the agent's
    // unowned sessions (the legacy shared web session, channel adapters).
    // Never another user's.
    let agent_sessions: Vec<_> = sessions
        .iter()
        .filter(|s| s.agent_id == group_id)
        .filter(|s| match crate::server::session_owner(s) {
            Some(owner) => owner == principal,
            None => admin,
        })
        .collect();

    let mut lines: Vec<ChatLine> = Vec::new();
    let mut all_real_ids = true;
    for session in &agent_sessions {
        let (session_lines, real_ids) = read_chat_lines(&state, &session.id).await;
        all_real_ids &= real_ids;
        lines.extend(session_lines);
    }
    // Stable: lines without a timestamp keep their store order.
    lines.sort_by(|a, b| a.at.cmp(&b.at));
    let mut all_messages: Vec<AgentMessage> = Vec::with_capacity(lines.len());
    let mut ids: HashSet<String> = HashSet::new();
    for line in lines {
        if ids.insert(line.message.id.clone()) {
            all_messages.push(line.message);
        }
    }

    let fallback = s.agent_messages.read().await;
    if let Some(cached) = fallback.get(&agent_chat_cache_key(&group_id, principal)) {
        for msg in cached {
            if ids.contains(&msg.id) {
                continue;
            }
            // Without stored ids the cache cannot be matched by id; fall back
            // to "the store already has this exact message".
            if !all_real_ids
                && all_messages
                    .iter()
                    .any(|m| m.is_agent == msg.is_agent && m.text == msg.text)
            {
                continue;
            }
            ids.insert(msg.id.clone());
            all_messages.push(msg.clone());
        }
    }

    Ok(Json(all_messages))
}

/// `POST` handler: sends a user chat message to an agent group, creating the
/// caller's own web chat session with it on first use.
pub(super) async fn send_agent_message(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(group_id): Path<String>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<AgentMessage>, StatusCode> {
    let s = control(&state)?;
    let memory = &state.memory;
    let content = body.content.trim().to_string();
    if content.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let principal = workspace.principal_id.as_str();
    let session_id = agent_chat_session_id(&group_id, principal);

    let sessions = memory.list_sessions().await.unwrap_or_default();
    let existing = sessions.iter().find(|s| s.id == session_id);

    if existing.is_none() {
        let now = chrono::Utc::now();
        let session = consistent_world_common::types::Session {
            id: session_id.clone(),
            agent_id: group_id.clone(),
            channel: consistent_world_common::types::ChannelKind::Web,
            peer_id: principal.to_string(),
            scope_type: consistent_world_common::types::ScopeType::Principal,
            scope_id: format!("web:{}:{}", group_id, principal),
            channel_binding_metadata: None,
            resolution_reason: Some("agent-chat".to_string()),
            created_at: now,
            last_active: now,
        };
        let _ = memory.upsert_session(&session).await;
    }

    let msg_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().format("%H:%M").to_string();
    let msg = AgentMessage {
        id: msg_id.clone(),
        text: content.clone(),
        time: now.clone(),
        is_agent: false,
    };

    let _ = memory
        .save_message(&msg_id, &session_id, "user", &content)
        .await;

    s.agent_messages
        .write()
        .await
        .entry(agent_chat_cache_key(&group_id, principal))
        .or_default()
        .push(msg.clone());

    Ok(Json(msg))
}

/// Read-only view of the configured mission policy.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct MissionPolicyView {
    pub(super) starter_task_auto_queueing_enabled: bool,
    pub(super) followup_task_proposal_enabled: bool,
    pub(super) followup_auto_create_enabled: bool,
    pub(super) default_review_cadence_hours: u32,
}

/// `GET` handler: the configured mission policy.
pub(super) async fn get_mission_policy(
    State(state): State<ServerState>,
) -> Result<Json<MissionPolicyView>, StatusCode> {
    let cfg = state
        .app_config
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(MissionPolicyView {
        starter_task_auto_queueing_enabled: cfg.mission_policy.starter_task_auto_queueing_enabled,
        followup_task_proposal_enabled: cfg.mission_policy.followup_task_proposal_enabled,
        followup_auto_create_enabled: cfg.mission_policy.followup_auto_create_enabled,
        default_review_cadence_hours: cfg.mission_policy.default_review_cadence_hours,
    }))
}

#[cfg(test)]
mod agent_chat_tests {
    use super::*;

    #[test]
    fn each_user_gets_their_own_agent_chat_session() {
        let a = agent_chat_session_id("ops", "alice");
        let b = agent_chat_session_id("ops", "bob");
        assert_ne!(a, b);
        assert_ne!(agent_chat_cache_key("ops", "alice"), agent_chat_cache_key("ops", "bob"));
        // The id shape is the one `server::session_owner` attributes to a user.
        let session = consistent_world_common::types::Session {
            id: a.clone(),
            agent_id: "ops".into(),
            channel: consistent_world_common::types::ChannelKind::Web,
            peer_id: "alice".into(),
            scope_type: Default::default(),
            scope_id: String::new(),
            channel_binding_metadata: None,
            resolution_reason: None,
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
        };
        assert_eq!(crate::server::session_owner(&session), Some("alice"));
    }

    #[test]
    fn repeated_messages_get_distinct_stable_ids() {
        let first = synthetic_message_id("s", "user", "ok", 0);
        let second = synthetic_message_id("s", "user", "ok", 1);
        assert_ne!(first, second);
        assert_eq!(first, synthetic_message_id("s", "user", "ok", 0));
        assert_ne!(first, synthetic_message_id("s", "assistant", "ok", 0));
    }

    #[test]
    fn clock_time_reads_the_stored_timestamp() {
        assert_eq!(clock_time("2026-10-02T09:05:00+00:00"), "09:05");
        assert_eq!(clock_time("not a time"), "");
    }
}
