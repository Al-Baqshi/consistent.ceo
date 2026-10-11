//! Agent groups and their chat surface.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// `GET` handler: lists agent groups.
pub(super) async fn list_agent_groups(
    State(state): State<ServerState>,
) -> Result<Json<Vec<AgentGroup>>, StatusCode> {
    let s = control(&state)?;
    let groups = s.agent_groups.read().await;
    Ok(Json(groups.clone()))
}

/// `POST` handler: creates an agent group (a director) and its default
/// (disabled) heartbeat state.
///
/// An empty or missing id gets a fresh uuid — the settings page sends `""`
/// when the user leaves the id blank, which used to be a bare 400. Errors carry
/// a JSON `{"error": …}` body so the page can say what went wrong.
pub(super) async fn create_agent_group(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateAgentGroupRequest>,
) -> Result<Json<AgentGroup>, (StatusCode, Json<serde_json::Value>)> {
    let error = |status: StatusCode, message: &str| {
        (status, Json(serde_json::json!({ "error": message })))
    };
    // Directors are shared by every workspace on the instance.
    require_instance_admin(&auth, &state)
        .map_err(|status| error(status, "only the operator of this instance can add directors"))?;
    let s = control(&state).map_err(|status| error(status, "control state unavailable"))?;
    if body.name.trim().is_empty() {
        return Err(error(StatusCode::BAD_REQUEST, "a director needs a name"));
    }

    let id = body
        .id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let mut groups = s.agent_groups.write().await;
    if groups.iter().any(|g| g.id == id) {
        return Err(error(
            StatusCode::CONFLICT,
            &format!("a director with id `{id}` already exists"),
        ));
    }

    let group = AgentGroup {
        id: id.clone(),
        name: body.name.trim().to_string(),
        icon: body.icon.unwrap_or_else(|| "🧭".to_string()),
        description: body
            .description
            .unwrap_or_else(|| "User-created director".to_string()),
    };
    groups.push(group.clone());
    drop(groups);

    let mut states = s.director_states.write().await;
    if !states.iter().any(|d| d.id == id) {
        states.push(DirectorState {
            id: group.id.clone(),
            heartbeat_enabled: false,
            auto_task_kinds: vec![],
        });
    }
    Ok(Json(group))
}

/// Query-string filter for listing agents by group.
#[derive(Debug, Deserialize)]
pub(super) struct AgentsQuery {
    pub(super) group_id: Option<String>,
}

/// Query-string filters for listing action-gate events.
#[derive(Debug, Deserialize)]
pub(super) struct ActionEventsQuery {
    pub(super) task_id: Option<String>,
    pub(super) entity_id: Option<String>,
    pub(super) agent_id: Option<String>,
    pub(super) capability: Option<String>,
    pub(super) outcome: Option<String>,
    pub(super) limit: Option<usize>,
    pub(super) offset: Option<usize>,
}

/// Wire string for an [`ActionOutcome`] value.
pub(super) fn action_outcome_as_str(outcome: &ActionOutcome) -> &'static str {
    match outcome {
        ActionOutcome::Allowed => "allowed",
        ActionOutcome::Denied => "denied",
        ActionOutcome::ApprovalRequired => "approval_required",
        ActionOutcome::Failed => "failed",
    }
}

/// Whether an action-gate event belongs to the caller's workspace: by its
/// entity when it has one, else by its task, else (an entity- and task-less
/// event) only in the default workspace — the same rule tasks and the operator
/// feed use. `ActionEvent` has no workspace field of its own.
pub(super) fn action_event_visible(
    event: &ActionEvent,
    entity_ids: &HashSet<String>,
    tasks: &[Task],
    workspace_id: &str,
) -> bool {
    if let Some(ref eid) = event.entity_id {
        return entity_ids.contains(eid);
    }
    if let Some(ref tid) = event.task_id
        && let Some(task) = tasks.iter().find(|t| t.id == *tid)
    {
        return task_visible_in_workspace(task, entity_ids, workspace_id);
    }
    workspace_id == DEFAULT_WORKSPACE_ID
}

/// `GET` handler: lists the caller's action-gate audit log, filtered and
/// paginated.
pub(super) async fn list_action_events(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<ActionEventsQuery>,
) -> Result<Json<PaginatedResponse<ActionEvent>>, StatusCode> {
    let s = control(&state)?;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace.id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let mut out = s.action_events.read().await.clone();
    {
        let tasks = s.tasks.read().await;
        out.retain(|e| action_event_visible(e, &entity_ids, &tasks, &workspace.id));
    }
    if let Some(task_id) = q.task_id.as_deref() {
        out.retain(|e| e.task_id.as_deref() == Some(task_id));
    }
    if let Some(entity_id) = q.entity_id.as_deref() {
        out.retain(|e| e.entity_id.as_deref() == Some(entity_id));
    }
    if let Some(agent_id) = q.agent_id.as_deref() {
        out.retain(|e| e.agent_id.as_deref() == Some(agent_id));
    }
    if let Some(cap) = q.capability.as_deref() {
        out.retain(|e| e.capability.to_string() == cap);
    }
    if let Some(outcome) = q.outcome.as_deref() {
        out.retain(|e| action_outcome_as_str(&e.outcome) == outcome);
    }
    let total = out.len();
    out.reverse();
    let pag = apply_pagination(
        out,
        total,
        &PaginationQuery {
            limit: q.limit,
            offset: q.offset,
        },
    );
    Ok(Json(pag))
}

/// Query-string filters for listing the operator feed.
#[derive(Debug, Deserialize)]
pub(super) struct OperatorFeedQuery {
    pub(super) limit: Option<usize>,
    pub(super) offset: Option<usize>,
    pub(super) task_id: Option<String>,
    pub(super) mission_id: Option<String>,
    pub(super) entity_id: Option<String>,
    pub(super) connection_id: Option<String>,
}

/// Query-string filters for the Mission Control activity timeline.
#[derive(Debug, Deserialize)]
pub(super) struct ActivityQuery {
    pub(super) entity_id: Option<String>,
    pub(super) mission_id: Option<String>,
    pub(super) kind: Option<String>,
    pub(super) status: Option<String>,
    pub(super) source: Option<String>,
    pub(super) limit: Option<usize>,
}

/// Mission Control activity timeline: a unified, read-only, newest-first view
/// merging the operator feed (task lifecycle, verification warnings, approvals,
/// mission DoD, next actions) with raw captures and external agent runs. No new
/// persisted table — it is a read-model over existing sources. Visibility is
/// enforced per-source with the same predicates as the underlying endpoints.
pub(super) async fn list_activity(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<ActivityQuery>,
) -> Result<Json<Vec<crate::activity::ActivityItem>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let feed: Vec<OperatorFeedEvent> = s
        .operator_feed
        .read()
        .await
        .iter()
        .filter(|e| feed_visible_in_workspace(e, &entity_ids, &workspace_id))
        .cloned()
        .collect();
    let captures: Vec<RawCapture> = s
        .raw_captures
        .read()
        .await
        .iter()
        .filter(|c| c.workspace_id == workspace_id)
        .cloned()
        .collect();
    let runs: Vec<ExternalAgentRun> = s
        .external_agent_runs
        .read()
        .await
        .iter()
        .filter(|r| r.workspace_id == workspace_id)
        .cloned()
        .collect();
    let verifications: Vec<VerificationRun> = s
        .verifications
        .read()
        .await
        .iter()
        .filter(|v| v.workspace_id == workspace_id)
        .cloned()
        .collect();
    let filter = crate::activity::ActivityFilter {
        entity_id: q.entity_id.as_deref(),
        mission_id: q.mission_id.as_deref(),
        kind: q.kind.as_deref(),
        status: q.status.as_deref(),
        source: q.source.as_deref(),
        limit: q.limit,
    };
    Ok(Json(crate::activity::build_activity(
        &feed,
        &captures,
        &runs,
        &verifications,
        &filter,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(entity: Option<&str>, task: Option<&str>) -> ActionEvent {
        ActionEvent {
            id: "a".into(),
            timestamp: String::new(),
            scope_type: Default::default(),
            scope_id: String::new(),
            entity_id: entity.map(str::to_string),
            task_id: task.map(str::to_string),
            agent_id: None,
            capability: consistent_world_common::capabilities::CapabilityId::MessageInternal,
            execution_level: consistent_world_common::capabilities::ExecutionLevel::Coordinate,
            target: None,
            outcome: ActionOutcome::Allowed,
            reason: None,
        }
    }

    #[test]
    fn action_events_are_scoped_to_the_callers_workspace() {
        let mine: HashSet<String> = ["ent-a".to_string()].into_iter().collect();
        let tasks = vec![
            Task {
                id: "task-a".into(),
                entity_id: Some("ent-a".into()),
                ..Task::default()
            },
            Task {
                id: "task-b".into(),
                entity_id: Some("ent-b".into()),
                ..Task::default()
            },
        ];
        assert!(action_event_visible(&action(Some("ent-a"), None), &mine, &tasks, "ws-a"));
        assert!(!action_event_visible(&action(Some("ent-b"), None), &mine, &tasks, "ws-a"));
        assert!(action_event_visible(&action(None, Some("task-a")), &mine, &tasks, "ws-a"));
        assert!(!action_event_visible(&action(None, Some("task-b")), &mine, &tasks, "ws-a"));
        assert!(!action_event_visible(&action(None, None), &mine, &tasks, "ws-a"));
        assert!(action_event_visible(&action(None, None), &mine, &tasks, "default"));
    }
}
