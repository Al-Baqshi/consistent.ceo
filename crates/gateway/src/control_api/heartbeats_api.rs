//! Heartbeats and their per-entity results.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Body for creating a heartbeat (a scheduled director invocation).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateHeartbeatBody {
    pub(super) name: String,
    pub(super) target_kind: String,
    #[serde(default)]
    pub(super) target_id: Option<String>,
    pub(super) director_id: String,
    #[serde(default)]
    pub(super) cron: Option<String>,
    #[serde(default)]
    pub(super) cadence_secs: Option<u64>,
    #[serde(default)]
    pub(super) workspace_id: Option<String>,
    #[serde(default)]
    pub(super) condition: Option<String>,
    #[serde(default = "default_true")]
    pub(super) enabled: bool,
}

/// `serde(default = ...)` helper: a heartbeat is enabled unless said otherwise.
pub(super) fn default_true() -> bool {
    true
}

/// `GET` handler: lists heartbeats in the workspace.
pub(super) async fn list_heartbeats(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<Vec<crate::state::Heartbeat>>, StatusCode> {
    let store = state
        .heartbeat_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    use crate::heartbeat_store::HeartbeatStore;
    let rows = store.list_all().await.map_err(|e| {
        tracing::warn!(error = %e, "list_heartbeats failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(
        rows.into_iter()
            .filter(|h| h.workspace_id.as_deref() == Some(workspace.id.as_str()))
            .collect(),
    ))
}

/// Refuses a heartbeat target the caller cannot see.
///
/// A beat loads its target entity, that entity's memory and active mission
/// into a prompt and writes the result into the entity's feed, so a heartbeat
/// pointed at another tenant's entity (or a teammate's Personal Space) would
/// read their data and write into their timeline. `entity` targets must be
/// visible; `mission` targets must belong to a visible entity. Other kinds
/// carry no entity scope and pass through. Missing targets are a 404, the
/// same answer as an invisible one.
pub(super) async fn ensure_heartbeat_target_visible(
    state: &ServerState,
    workspace: &WorkspaceCtx,
    target_kind: &str,
    target_id: Option<&str>,
) -> Result<(), StatusCode> {
    let Some(target_id) = target_id.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(());
    };
    let entity_id = match target_kind {
        "entity" => target_id.to_string(),
        "mission" => {
            let s = control(state)?;
            let missions = s.missions.read().await;
            missions
                .iter()
                .find(|m| m.id == target_id)
                .map(|m| m.entity_id.clone())
                .ok_or(StatusCode::NOT_FOUND)?
        }
        _ => return Ok(()),
    };
    let s = control(state)?;
    let entities = s.entities.read().await;
    if entities.iter().any(|e| {
        e.id == entity_id
            && is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
    }) {
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

/// `POST` handler: creates a heartbeat, validating the cron expression (if
/// given) and computing its first `next_fire_at`.
pub(super) async fn create_heartbeat(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateHeartbeatBody>,
) -> Result<Json<crate::state::Heartbeat>, StatusCode> {
    let store = state
        .heartbeat_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    if body.name.trim().is_empty() || body.cron.is_none() && body.cadence_secs.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    ensure_heartbeat_target_visible(
        &state,
        &workspace,
        &body.target_kind,
        body.target_id.as_deref(),
    )
    .await?;
    if let Some(expr) = body.cron.as_deref() {
        cron::Schedule::from_str(expr).map_err(|_| StatusCode::BAD_REQUEST)?;
    }
    if body.cadence_secs.is_some_and(|secs| secs == 0) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let now = chrono::Utc::now();
    let hb = crate::state::Heartbeat {
        id: format!("hb_{}", Uuid::new_v4()),
        workspace_id: Some(workspace.id),
        name: body.name.trim().to_string(),
        enabled: body.enabled,
        target_kind: body.target_kind,
        target_id: body.target_id,
        director_id: body.director_id,
        cron: body.cron,
        cadence_secs: body.cadence_secs,
        condition: body.condition,
        last_fired_at: None,
        next_fire_at: None,
        lease_until: None,
        lease_token: None,
        created_at: now.to_rfc3339(),
        updated_at: now.to_rfc3339(),
    };
    let mut hb = hb;
    hb.next_fire_at = crate::heartbeat_store::next_fire_at(&hb, now).map(|dt| dt.to_rfc3339());
    use crate::heartbeat_store::HeartbeatStore;
    store.upsert(&hb).await.map_err(|e| {
        tracing::warn!(error = %e, "create_heartbeat failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(hb))
}

/// Partial-update body for a heartbeat.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateHeartbeatBody {
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) enabled: Option<bool>,
    #[serde(default)]
    pub(super) cron: Option<String>,
    #[serde(default)]
    pub(super) cadence_secs: Option<u64>,
    #[serde(default)]
    pub(super) condition: Option<String>,
}

/// `PATCH` handler: partial-updates a heartbeat, recomputing `next_fire_at`
/// when its schedule changed or it was just re-enabled.
pub(super) async fn update_heartbeat(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<UpdateHeartbeatBody>,
) -> Result<Json<crate::state::Heartbeat>, StatusCode> {
    let store = state
        .heartbeat_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    use crate::heartbeat_store::HeartbeatStore;
    let mut hb = store
        .get(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if hb.workspace_id.as_deref() != Some(workspace.id.as_str()) {
        return Err(StatusCode::NOT_FOUND);
    }
    if let Some(name) = body.name {
        hb.name = name;
    }
    if let Some(enabled) = body.enabled {
        hb.enabled = enabled;
    }
    // Same gate as create, for anything that leaves the heartbeat able to
    // fire: one stored before targets were validated must not be kept running
    // or rescheduled against an entity the caller cannot see. Disabling is
    // always allowed, so an orphaned beat can still be switched off.
    if hb.enabled {
        ensure_heartbeat_target_visible(
            &state,
            &workspace,
            &hb.target_kind,
            hb.target_id.as_deref(),
        )
        .await?;
    }
    let schedule_changed = body.cron.is_some() || body.cadence_secs.is_some();
    if body.cron.is_some() {
        if let Some(expr) = body.cron.as_deref() {
            cron::Schedule::from_str(expr).map_err(|_| StatusCode::BAD_REQUEST)?;
        }
        hb.cron = body.cron;
    }
    if body.cadence_secs.is_some() {
        hb.cadence_secs = body.cadence_secs;
    }
    if body.condition.is_some() {
        hb.condition = body.condition;
    }
    if hb.cron.is_none() && hb.cadence_secs.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if hb.cadence_secs.is_some_and(|secs| secs == 0) {
        return Err(StatusCode::BAD_REQUEST);
    }
    hb.updated_at = chrono::Utc::now().to_rfc3339();
    if schedule_changed || body.enabled == Some(true) {
        hb.next_fire_at =
            crate::heartbeat_store::next_fire_at(&hb, chrono::Utc::now()).map(|dt| dt.to_rfc3339());
    }
    store.upsert(&hb).await.map_err(|e| {
        tracing::warn!(heartbeat_id = %id, error = %e, "update_heartbeat failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(hb))
}

/// Handler: deletes a heartbeat.
pub(super) async fn delete_heartbeat(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let store = state
        .heartbeat_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    use crate::heartbeat_store::HeartbeatStore;
    let hb = store
        .get(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if hb.workspace_id.as_deref() != Some(workspace.id.as_str()) {
        return Err(StatusCode::NOT_FOUND);
    }
    store.delete(&id).await.map_err(|e| {
        tracing::warn!(heartbeat_id = %id, error = %e, "delete_heartbeat failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// Result of an on-demand admin database backup.
#[derive(Debug, Serialize)]
pub struct BackupResponse {
    pub key: String,
    pub bytes: usize,
}

/// Run a database backup to the configured S3 bucket right now.
pub(super) async fn run_admin_backup(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<BackupResponse>, StatusCode> {
    // A backup reads every tenant's data off the instance.
    require_instance_admin(&auth, &state)?;
    let cfg = state
        .app_config
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let Some(s3) = crate::backup::BackupConfig::from_env() else {
        tracing::warn!("admin backup requested but BACKUP_S3_* is not configured");
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let (key, bytes) = crate::backup::run_backup(cfg, &s3).await.map_err(|e| {
        tracing::warn!(error = %e, "admin backup failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(BackupResponse { key, bytes }))
}

/// `GET` handler: the last 50 heartbeat results for an entity (the beat
/// result log the chat UI surfaces).
pub(super) async fn list_entity_heartbeat_results(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
) -> Result<Json<Vec<crate::heartbeat_results_store::HeartbeatResult>>, StatusCode> {
    let Some(control) = state.control_state.as_ref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let visible = control.entities.read().await.iter().any(|e| {
        e.id == entity_id
            && is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
    });
    if !visible {
        return Err(StatusCode::NOT_FOUND);
    }
    let store = state
        .heartbeat_results_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    store
        .list_for_entity(&entity_id, 50)
        .await
        .map(Json)
        .map_err(|e| {
            tracing::warn!(entity_id = %entity_id, error = %e, "list_entity_heartbeat_results failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })
}

/// `GET` handler: lists registered storage backends.
pub(super) async fn list_storage_backends(
    State(state): State<ServerState>,
) -> Result<Json<Vec<StorageBackend>>, StatusCode> {
    let s = control(&state)?;
    Ok(Json(s.storage_backends.read().await.clone()))
}

/// `POST` handler: registers a storage backend.
pub(super) async fn create_storage_backend(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateStorageBackendRequest>,
) -> Result<Json<StorageBackend>, StatusCode> {
    // Storage backends are instance-wide configuration.
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    let id = Uuid::new_v4().to_string();
    let b = StorageBackend {
        id: id.clone(),
        name: body.name,
        backend_type: body.backend_type,
        config: body.config,
    };
    s.storage_backends.write().await.push(b.clone());
    Ok(Json(b))
}

/// `GET` handler: lists task-orchestration rules (default workspace only).
pub(super) async fn list_orchestration_rules(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<Vec<OrchestrationRule>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    if workspace_id != DEFAULT_WORKSPACE_ID {
        return Ok(Json(Vec::new()));
    }
    Ok(Json(s.orchestration_rules.read().await.clone()))
}

/// `POST` handler: creates a task-orchestration routing rule.
pub(super) async fn create_orchestration_rule(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateOrchestrationRuleRequest>,
) -> Result<Json<OrchestrationRule>, StatusCode> {
    // Orchestration rules route every tenant's work.
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    let id = Uuid::new_v4().to_string();
    let r = OrchestrationRule {
        id: id.clone(),
        match_project: body.match_project,
        match_department: body.match_department,
        match_tag: body.match_tag,
        director_id: body.director_id,
        model_id: body.model_id,
        retry_count: body.retry_count,
    };
    s.orchestration_rules.write().await.push(r.clone());
    Ok(Json(r))
}
