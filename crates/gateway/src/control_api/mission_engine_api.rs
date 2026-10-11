//! Mission engine: work that continues without the operator.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Body for attaching (or reconfiguring) a mission engine.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachMissionEngineRequest {
    /// Working directory for the agent. Must resolve inside the entity's roots.
    pub cwd: Option<String>,
    /// How often to wake the agent. Defaults to every 4 hours.
    pub beat_interval_minutes: Option<i64>,
    /// Stop after this many beats. 0 / omitted = no cap.
    pub max_beats: Option<i64>,
    /// Stop once accumulated spend reaches this. 0 / omitted = no cap.
    pub budget_usd: Option<f64>,
}

/// Current state of a mission's attached autonomous engine, if any.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionEngineStatus {
    pub mission_id: String,
    pub attached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_beat_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub beat_count: i64,
    pub cost_usd: f64,
    pub paused: bool,
    /// Set while the Claude usage window is spent; the engine resumes itself
    /// at this instant without the operator doing anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_on_usage_until: Option<String>,
}

/// Reads a mission engine's beat-loop task metadata into a [`MissionEngineStatus`].
pub(super) fn engine_status_from_task(
    mission_id: &str,
    task: Option<&Task>,
) -> MissionEngineStatus {
    let Some(t) = task else {
        return MissionEngineStatus {
            mission_id: mission_id.to_string(),
            attached: false,
            task_id: None,
            cwd: None,
            next_beat_at: None,
            last_summary: None,
            session_id: None,
            beat_count: 0,
            cost_usd: 0.0,
            paused: false,
            waiting_on_usage_until: None,
        };
    };
    use crate::mission_engine as me;
    let num = |k: &str| {
        crate::state::task_meta_str(t, k)
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    MissionEngineStatus {
        mission_id: mission_id.to_string(),
        attached: true,
        task_id: Some(t.id.clone()),
        cwd: crate::state::task_meta_str(t, me::META_CWD),
        next_beat_at: crate::state::task_meta_str(t, me::META_NEXT_BEAT_AT),
        last_summary: crate::state::task_meta_str(t, me::META_LAST_SUMMARY),
        session_id: crate::state::task_meta_str(t, me::META_SESSION_ID),
        beat_count: num(me::META_BEAT_COUNT) as i64,
        cost_usd: num(me::META_COST_USD),
        paused: crate::state::task_meta_str(t, me::META_PAUSED).as_deref() == Some("true"),
        waiting_on_usage_until: crate::state::task_meta_str(t, me::META_WAITING_UNTIL)
            .filter(|v| !v.is_empty()),
    }
}

/// Finds a mission's engine beat-loop task, if one is attached.
///
/// The task must also belong to the mission's entity: the mission link lives
/// in free-form task metadata, so without this a task filed under some other
/// entity could pose as this mission's engine.
pub(super) async fn find_engine_task(s: &ControlState, mission: &Mission) -> Option<Task> {
    let tasks = s.tasks.read().await;
    tasks
        .iter()
        .find(|t| {
            t.task_kind.as_deref() == Some(crate::mission_engine::BEAT_TASK_KIND)
                && t.entity_id.as_deref() == Some(mission.entity_id.as_str())
                && crate::state::task_meta_str(t, crate::mission_engine::META_MISSION_ID).as_deref()
                    == Some(mission.id.as_str())
        })
        .cloned()
}

/// The mission `mission_id`, if its entity is visible to the caller.
///
/// Every engine route goes through this: "beat now" starts an unattended,
/// permission-bypassing Claude run in the entity's repo, so a mission in
/// another tenant (or a teammate's Personal Space) must read as missing.
/// Each guard is taken and dropped on its own (lock-order rule 2 on
/// `ControlState`).
pub(super) async fn visible_mission(
    s: &ControlState,
    workspace: &WorkspaceCtx,
    mission_id: &str,
) -> Result<Mission, StatusCode> {
    let mission = {
        let missions = s.missions.read().await;
        missions.iter().find(|m| m.id == mission_id).cloned()
    }
    .ok_or(StatusCode::NOT_FOUND)?;
    let visible = {
        let entities = s.entities.read().await;
        entities.iter().any(|e| {
            e.id == mission.entity_id
                && is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
        })
    };
    if !visible {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(mission)
}

/// Validates a caller-supplied `beatIntervalMinutes`: `None` passes through,
/// anything outside one minute to one year
/// (`1..=mission_engine::MAX_INTERVAL_MINUTES`) is a 400 rather than a value
/// that overflows `next_beat_at` after a paid run.
pub(super) fn validate_beat_interval(minutes: Option<i64>) -> Result<Option<i64>, StatusCode> {
    match minutes {
        Some(m) if !(1..=crate::mission_engine::MAX_INTERVAL_MINUTES).contains(&m) => {
            Err(StatusCode::BAD_REQUEST)
        }
        other => Ok(other),
    }
}

/// `GET` handler: a mission's engine status.
pub(super) async fn get_mission_engine(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(mission_id): Path<String>,
) -> Result<Json<MissionEngineStatus>, StatusCode> {
    let s = control(&state)?;
    let mission = visible_mission(s, &workspace, &mission_id).await?;
    let task = find_engine_task(s, &mission).await;
    Ok(Json(engine_status_from_task(&mission_id, task.as_ref())))
}

/// Attach a Claude Code engine to a mission: from here the gateway keeps
/// working on it without the operator present.
pub(super) async fn attach_mission_engine(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(mission_id): Path<String>,
    Json(body): Json<AttachMissionEngineRequest>,
) -> Result<Json<MissionEngineStatus>, StatusCode> {
    let s = control(&state)?;
    let beat_interval_minutes = validate_beat_interval(body.beat_interval_minutes)?;
    let workspace_id = workspace.id;

    let mission = {
        let missions = s.missions.read().await;
        missions
            .iter()
            .find(|m| m.id == mission_id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    let entity = {
        let entities = s.entities.read().await;
        entities
            .iter()
            .find(|e| {
                e.id == mission.entity_id
                    && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
            })
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    // Same jail as a one-off run: the agent can only work inside the entity's
    // own roots, whatever cadence it runs on.
    let (cwd, _roots) = resolve_entity_agent_run_cwd(&entity, body.cwd.as_deref()).await?;
    let cwd_string = cwd.to_string_lossy().to_string();

    if let Some(existing) = find_engine_task(s, &mission).await {
        // Re-attaching an existing engine reconfigures and unpauses it rather
        // than stacking a second beat loop on the same repo.
        let mut tasks = s.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == existing.id) {
            let meta = t.metadata.get_or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = meta.as_object_mut() {
                obj.insert(
                    crate::mission_engine::META_CWD.to_string(),
                    serde_json::json!(cwd_string),
                );
                obj.insert(
                    crate::mission_engine::META_PAUSED.to_string(),
                    serde_json::json!("false"),
                );
                if let Some(m) = beat_interval_minutes {
                    obj.insert(
                        crate::mission_engine::META_INTERVAL_MINUTES.to_string(),
                        serde_json::json!(m.to_string()),
                    );
                }
            }
            crate::state::set_task_status_by_human(t, TaskStatus::InProgress);
            let updated = t.clone();
            drop(tasks);
            return Ok(Json(engine_status_from_task(&mission_id, Some(&updated))));
        }
        drop(tasks);
    }

    let interval = beat_interval_minutes.unwrap_or(crate::mission_engine::DEFAULT_INTERVAL_MINUTES);
    let task = internal_create_task(
        s,
        &workspace_id,
        Some(workspace.principal_id.as_str()),
        CreateTaskRequest {
            title: format!("Engine · {}", mission.title),
            dept: Some("Engineering".to_string()),
            roles: Some(vec!["Mission engine".to_string()]),
            entity_id: Some(mission.entity_id.clone()),
            mission_id: Some(mission_id.clone()),
            priority: Some("medium".to_string()),
            requires_human_approval: Some(false),
            task_kind: Some(crate::mission_engine::BEAT_TASK_KIND.to_string()),
            content: Some(mission.goal.clone()),
            director_id: Some("engineering".to_string()),
            agent_id: None,
            connection_id: None,
            external_source: Some("claude".to_string()),
            external_id: None,
            metadata: Some(serde_json::json!({
                crate::mission_engine::META_MISSION_ID: mission_id.clone(),
                crate::mission_engine::META_CWD: cwd_string,
                crate::mission_engine::META_INTERVAL_MINUTES: interval.to_string(),
                crate::mission_engine::META_MAX_BEATS: body.max_beats.unwrap_or(0).to_string(),
                crate::mission_engine::META_BUDGET_USD: body.budget_usd.unwrap_or(0.0).to_string(),
                crate::mission_engine::META_BEAT_COUNT: "0",
                crate::mission_engine::META_COST_USD: "0",
                crate::mission_engine::META_PAUSED: "false",
            })),
            due_at: None,
        },
    )
    .await?;

    let created = {
        let mut tasks = s.tasks.write().await;
        tasks.iter_mut().find(|t| t.id == task.id).map(|t| {
            // InProgress so it reads as live work, and so the beat ticker owns
            // it rather than the orchestrator's director pickup.
            crate::state::set_task_status_by_human(t, TaskStatus::InProgress);
            t.progress = 5;
            t.clone()
        })
    };
    Ok(Json(engine_status_from_task(&mission_id, created.as_ref())))
}

/// Detach the engine. The mission and its history stay; only the beat loop stops.
pub(super) async fn detach_mission_engine(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(mission_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let s = control(&state)?;
    let mission = visible_mission(s, &workspace, &mission_id).await?;
    let Some(existing) = find_engine_task(s, &mission).await else {
        return Err(StatusCode::NOT_FOUND);
    };
    if let Some(active) =
        crate::state::task_meta_str(&existing, crate::mission_engine::META_ACTIVE_RUN_ID)
        && !active.is_empty()
    {
        s.agent_processes.cancel(&active).await;
    }
    let mut tasks = s.tasks.write().await;
    if let Some(t) = tasks.iter_mut().find(|t| t.id == existing.id) {
        crate::state::set_task_status_by_human(t, TaskStatus::Cancelled);
        t.result = Some("Engine detached by the operator.".to_string());
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Run a beat now instead of waiting for the cadence. Also the way to restart
/// an engine that paused itself because it was blocked.
pub(super) async fn beat_mission_engine_now(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(mission_id): Path<String>,
) -> Result<Json<MissionEngineStatus>, StatusCode> {
    let s = control(&state)?;
    let mission = visible_mission(s, &workspace, &mission_id).await?;
    let Some(existing) = find_engine_task(s, &mission).await else {
        return Err(StatusCode::NOT_FOUND);
    };
    let updated = {
        let mut tasks = s.tasks.write().await;
        tasks.iter_mut().find(|t| t.id == existing.id).map(|t| {
            let meta = t.metadata.get_or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = meta.as_object_mut() {
                obj.insert(
                    crate::mission_engine::META_PAUSED.to_string(),
                    serde_json::json!("false"),
                );
                // Clearing the schedule makes the next tick pick it up.
                obj.remove(crate::mission_engine::META_NEXT_BEAT_AT);
            }
            crate::state::set_task_status_by_human(t, TaskStatus::InProgress);
            t.clone()
        })
    };
    Ok(Json(engine_status_from_task(&mission_id, updated.as_ref())))
}
