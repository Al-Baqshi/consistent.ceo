//! Human-in-the-loop approval requests and their resolution.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Query-string filters for listing approval requests.
#[derive(Debug, Deserialize)]
pub(super) struct ApprovalListQuery {
    pub(super) entity_id: Option<String>,
    pub(super) status: Option<String>,
}

/// Body for creating a human approval request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateApprovalRequestBody {
    pub(super) entity_id: String,
    pub(super) connection_id: Option<String>,
    pub(super) task_id: Option<String>,
    pub(super) action: String,
    pub(super) detail: Option<String>,
    pub(super) created_by: Option<String>,
}

/// Body for resolving an approval request.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PatchApprovalRequestBody {
    pub(super) status: ApprovalStatus,
}

/// `GET` handler: lists approval requests visible in the workspace, filtered
/// by entity/status.
pub(super) async fn list_approvals(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<ApprovalListQuery>,
) -> Result<Json<Vec<ApprovalRequest>>, StatusCode> {
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
    let mut out: Vec<ApprovalRequest> = s.approvals.read().await.clone();
    out.retain(|a| entity_ids.contains(&a.entity_id));
    if let Some(ref eid) = q.entity_id {
        out.retain(|a| a.entity_id == *eid);
    }
    if let Some(ref st) = q.status {
        let w = st.to_lowercase();
        out.retain(|a| {
            matches!(
                (&a.status, w.as_str()),
                (ApprovalStatus::Pending, "pending")
                    | (ApprovalStatus::Approved, "approved")
                    | (ApprovalStatus::Rejected, "rejected")
                    | (ApprovalStatus::Expired, "expired")
            )
        });
    }
    out.reverse();
    Ok(Json(out))
}

/// `GET` handler: one approval by id.
///
/// Exists because clients that hold an approval id — an inline chat card, a
/// push notification the user tapped — otherwise have to fetch the whole list
/// and filter it, which quietly fails for any approval already decided: the
/// mobile app's approvals list is pending-only, so a resolved approval it still
/// has a card for could never be rendered.
///
/// Scoped exactly like [`list_approvals`]: an approval outside the caller's
/// workspace is reported as missing rather than forbidden, so the endpoint does
/// not confirm that an id exists to someone who cannot see it.
pub(super) async fn get_approval(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<ApprovalRequest>, StatusCode> {
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
    let approvals = s.approvals.read().await;
    approvals
        .iter()
        .find(|a| a.id == id && entity_ids.contains(&a.entity_id))
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// `POST` handler: creates a pending approval request for an entity.
pub(super) async fn create_approval(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateApprovalRequestBody>,
) -> Result<Json<ApprovalRequest>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        let Some(entity) = entities.iter().find(|e| e.id == body.entity_id) else {
            return Err(StatusCode::BAD_REQUEST);
        };
        if !is_entity_visible(entity, &workspace_id, Some(workspace.principal_id.as_str())) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    // The linked task and connection must belong to the same entity. Approving
    // an approval ungates its task, so an approval filed against a visible
    // entity but pointing at someone else's task would let the caller flip
    // another tenant's gated task to Queued.
    if let Some(tid) = body.task_id.as_deref() {
        let tasks = s.tasks.read().await;
        if !tasks
            .iter()
            .any(|t| t.id == tid && t.entity_id.as_deref() == Some(body.entity_id.as_str()))
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    if let Some(cid) = body.connection_id.as_deref() {
        let connections = s.entity_connections.read().await;
        if !connections
            .iter()
            .any(|c| c.id == cid && c.entity_id == body.entity_id)
        {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    let now = Utc::now().to_rfc3339();
    let a = ApprovalRequest {
        id: Uuid::new_v4().to_string(),
        created_at: now.clone(),
        updated_at: now.clone(),
        entity_id: body.entity_id,
        connection_id: body.connection_id,
        task_id: body.task_id,
        action: body.action,
        status: ApprovalStatus::Pending,
        detail: body.detail,
        created_by: body.created_by,
    };
    s.enqueue_approval_request(a.clone()).await;
    Ok(Json(a))
}

/// `PATCH` handler: resolves an approval request — approving ungates its
/// linked task for the next executor tick, rejecting/expiring cancels a
/// still-active linked task — and logs the resolution to entity memory.
pub(super) async fn patch_approval(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<PatchApprovalRequestBody>,
) -> Result<Json<ApprovalRequest>, StatusCode> {
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
    let mut approvals = s.approvals.write().await;
    let a = approvals
        .iter_mut()
        .find(|x| x.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    if !entity_ids.contains(&a.entity_id) {
        return Err(StatusCode::FORBIDDEN);
    }
    a.status = body.status;
    a.updated_at = Utc::now().to_rfc3339();
    let out = a.clone();
    drop(approvals);

    if let Some(ref tid) = out.task_id {
        let changed = {
            let mut tasks = s.tasks.write().await;
            // Only the approval's own entity's task, and only one the caller
            // can see. Approvals created before `create_approval` validated
            // `taskId` (or through another path) may point anywhere; an
            // approval must never reach across tenants to ungate or cancel.
            let found = tasks.iter_mut().find(|t| {
                t.id == *tid
                    && t.entity_id.as_deref() == Some(out.entity_id.as_str())
                    && task_visible_in_workspace(t, &entity_ids, &workspace_id)
            });
            found.and_then(|t| {
                match body.status {
                    // Ungated and left Queued so the executor/orchestrator
                    // picks it up on the next tick. The stamp lets the
                    // executor run this task even on a Manual-mode entity —
                    // the human just said go.
                    ApprovalStatus::Approved => {
                        t.requires_human_approval = false;
                        crate::task_executor::write_meta(
                            t,
                            &[(
                                crate::task_executor::META_APPROVED_AT,
                                Utc::now().to_rfc3339().as_str(),
                            )],
                        );
                        // An approved executable task must actually be able to
                        // run. Backlog is owned by nobody for this shape: the
                        // orchestrator defers to the executor (specialized
                        // loop owner) and the executor only takes Queued — so
                        // an approved action left in Backlog would sit there
                        // silently forever.
                        let executable = t
                            .metadata
                            .as_ref()
                            .and_then(|m| m.get(crate::task_executor::META_EXECUTABLE))
                            .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
                            .unwrap_or(false);
                        if executable && t.status == TaskStatus::Backlog {
                            crate::state::set_task_status_by_human(t, TaskStatus::Queued);
                        }
                        Some(t.clone())
                    }
                    // A denied action must not sit queued-but-gated forever.
                    ApprovalStatus::Rejected | ApprovalStatus::Expired => {
                        if !matches!(
                            t.status,
                            TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
                        ) {
                            crate::state::set_task_status_by_human(t, TaskStatus::Cancelled);
                            t.result =
                                Some("Cancelled: the operator declined the approval.".to_string());
                        }
                        Some(t.clone())
                    }
                    ApprovalStatus::Pending => None,
                }
            })
        };
        if let Some(t) = changed {
            s.persist_task_row(&t).await;
        }
    }

    s.push_approval_feed(
        OperatorFeedKind::ApprovalRequestResolved,
        &out,
        Some(format!("status={:?}", out.status)),
    )
    .await;

    if matches!(body.status, ApprovalStatus::Approved) {
        let mem = state.memory.clone();
        let eid = out.entity_id.clone();
        let line = format!("approval:{}:{}", out.action, out.id);
        let _ = crate::entity_memory::append_entity_short_line(mem.clone(), &eid, &line).await;
        let _ = crate::entity_memory::promote_repeated_short_to_long(mem, &eid, &line, 3).await;
    }

    Ok(Json(out))
}

/// Files a run produced (browser screenshots today). Jailed to the run's own
/// artifact folder: the run id is validated as a bare uuid-ish token and the
/// resolved path must stay under the artifacts root, so a crafted id cannot
/// walk out of it.
pub(super) fn artifact_dir_for_run(run_id: &str) -> Option<std::path::PathBuf> {
    if run_id.is_empty()
        || run_id.len() > 64
        || !run_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let root = std::path::PathBuf::from(crate::config::expand_tilde_path(
        "~/.consistent-world/artifacts",
    ));
    let dir = root.join(run_id);
    // Canonicalize what exists; a missing directory is simply an empty listing.
    match dir.canonicalize() {
        Ok(resolved) => {
            let root = root.canonicalize().ok()?;
            resolved.starts_with(&root).then_some(resolved)
        }
        Err(_) => None,
    }
}

pub(super) async fn list_run_artifacts(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(run_id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = control(&state)?;
    // Only a run in the caller's workspace, so artifacts follow the same
    // visibility rules as everything else.
    {
        let runs = s.external_agent_runs.read().await;
        let Some(run) = runs.iter().find(|r| r.id == run_id) else {
            return Err(StatusCode::NOT_FOUND);
        };
        if run.workspace_id != workspace.id {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    let Some(dir) = artifact_dir_for_run(&run_id) else {
        return Ok(Json(serde_json::json!({ "items": [] })));
    };
    let mut items = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            items.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "bytes": meta.len(),
            }));
        }
    }
    Ok(Json(serde_json::json!({ "items": items })))
}

pub(super) async fn get_run_artifact(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((run_id, name)): Path<(String, String)>,
) -> Result<axum::response::Response, StatusCode> {
    let s = control(&state)?;
    {
        let runs = s.external_agent_runs.read().await;
        let Some(run) = runs.iter().find(|r| r.id == run_id) else {
            return Err(StatusCode::NOT_FOUND);
        };
        if run.workspace_id != workspace.id {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    // A file name, never a path: no separators, no traversal.
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let dir = artifact_dir_for_run(&run_id).ok_or(StatusCode::NOT_FOUND)?;
    let path = dir.join(&name);
    let resolved = path.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    if !resolved.starts_with(&dir) {
        return Err(StatusCode::FORBIDDEN);
    }
    let bytes = tokio::fs::read(&resolved)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;
    let content_type = match resolved.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("txt") | Some("log") => "text/plain; charset=utf-8",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    };
    use axum::response::IntoResponse;
    Ok(([(axum::http::header::CONTENT_TYPE, content_type)], bytes).into_response())
}

/// The payload the desktop/web UI renders as a pairing QR for the mobile app.
/// Bearer-protected, and never carries a secret: pairing ends in the normal
/// device-approval login — the QR only says where the gateway is reachable.
pub(super) async fn get_pair_payload(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<serde_json::Value>, StatusCode> {
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
    let Some(url) = status.recommended_url else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    Ok(Json(crate::remote_doctor::pair_payload(
        &url,
        &workspace.id,
    )))
}

/// Reachability doctor for the connection-details screen and CLI.
pub(super) async fn get_remote_status(
    State(state): State<ServerState>,
) -> Result<Json<crate::remote_doctor::RemoteStatus>, StatusCode> {
    let (port, bind) = state
        .app_config
        .as_ref()
        .map(|c| (c.gateway_port, c.gateway_bind.clone()))
        .unwrap_or((18800, "127.0.0.1".to_string()));
    let status =
        tokio::task::spawn_blocking(move || crate::remote_doctor::remote_status(port, &bind))
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(status))
}

/// Tenant isolation across the handlers that act on another record by id:
/// each must answer a principal from another workspace as if the record did
/// not exist, and do nothing.
#[cfg(test)]
mod tenant_isolation_tests {
    use super::*;
    use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};
    use std::sync::Arc;

    const WS_A: &str = "ws-a";
    const WS_B: &str = "ws-b";

    fn ctx(ws: &str, principal: &str) -> WorkspaceCtx {
        WorkspaceCtx {
            id: ws.to_string(),
            principal_id: principal.to_string(),
        }
    }

    fn entity(id: &str, ws: &str) -> Entity {
        Entity {
            id: id.to_string(),
            name: id.to_string(),
            owner: Some(ws.to_string()),
            workspace_id: Some(ws.to_string()),
            status: "Active".to_string(),
            ..Default::default()
        }
    }

    fn gated_task(id: &str, entity_id: &str) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            entity_id: Some(entity_id.to_string()),
            status: TaskStatus::Backlog,
            requires_human_approval: true,
            metadata: Some(serde_json::json!({ crate::task_executor::META_EXECUTABLE: true })),
            ..Task::default()
        }
    }

    fn mission(id: &str, entity_id: &str) -> Mission {
        Mission {
            id: id.to_string(),
            entity_id: entity_id.to_string(),
            title: id.to_string(),
            summary: String::new(),
            status: MissionStatus::Active,
            priority: "medium".to_string(),
            goal: "Ship".to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: Vec::new(),
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        }
    }

    /// Tenant A owns `ent-a` (with a gated task and a mission); tenant B owns
    /// `ent-b`.
    async fn two_tenants() -> ServerState {
        let surreal = SurrealMemoryStore::connect_memory().await.unwrap();
        let heartbeats = crate::heartbeat_store::SurrealHeartbeatStore::new(surreal.db());
        heartbeats.init_schema().await.unwrap();
        let memory: Arc<dyn MemoryStore> = Arc::new(surreal);
        let control = Arc::new(crate::state::ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        control
            .entities
            .write()
            .await
            .extend([entity("ent-a", WS_A), entity("ent-b", WS_B)]);
        control
            .tasks
            .write()
            .await
            .push(gated_task("task-a", "ent-a"));
        control
            .missions
            .write()
            .await
            .push(mission("mission-a", "ent-a"));
        ServerState::new("t".into(), Some("secret".into()), memory)
            .with_control_state(control)
            .with_heartbeat_store(heartbeats)
    }

    async fn task_a(state: &ServerState) -> Task {
        let s = state.control_state.as_ref().unwrap();
        let tasks = s.tasks.read().await;
        tasks.iter().find(|t| t.id == "task-a").cloned().unwrap()
    }

    #[tokio::test]
    async fn another_tenant_cannot_act_through_an_entitys_connections() {
        let state = two_tenants().await;
        let res = perform_entity_action_endpoint(
            State(state.clone()),
            Extension(ctx(WS_B, "user-b")),
            Path("ent-a".to_string()),
            Json(PerformEntityActionRequest {
                action: "send_email".to_string(),
                channel: "email".to_string(),
                payload: None,
                task_id: None,
                agent_id: None,
            }),
        )
        .await;
        assert_eq!(res.err(), Some(StatusCode::NOT_FOUND));

        // Nor link its own action to someone else's task.
        let res = perform_entity_action_endpoint(
            State(state),
            Extension(ctx(WS_B, "user-b")),
            Path("ent-b".to_string()),
            Json(PerformEntityActionRequest {
                action: "send_email".to_string(),
                channel: "email".to_string(),
                payload: None,
                task_id: Some("task-a".to_string()),
                agent_id: None,
            }),
        )
        .await;
        assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn an_approval_cannot_reach_another_tenants_task() {
        let state = two_tenants().await;
        // Filing one against your own entity but someone else's task: refused.
        let res = create_approval(
            State(state.clone()),
            Extension(ctx(WS_B, "user-b")),
            Json(CreateApprovalRequestBody {
                entity_id: "ent-b".to_string(),
                connection_id: None,
                task_id: Some("task-a".to_string()),
                action: "run".to_string(),
                detail: None,
                created_by: None,
            }),
        )
        .await;
        assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));

        // One that already points there (filed before the check existed)
        // resolves, but must not ungate the foreign task.
        let s = state.control_state.as_ref().unwrap();
        s.approvals.write().await.push(ApprovalRequest {
            id: "ap-legacy".to_string(),
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
            entity_id: "ent-b".to_string(),
            connection_id: None,
            task_id: Some("task-a".to_string()),
            action: "run".to_string(),
            status: ApprovalStatus::Pending,
            detail: None,
            created_by: None,
        });
        let resolved = patch_approval(
            State(state.clone()),
            Extension(ctx(WS_B, "user-b")),
            Path("ap-legacy".to_string()),
            Json(PatchApprovalRequestBody {
                status: ApprovalStatus::Approved,
            }),
        )
        .await
        .unwrap();
        assert_eq!(resolved.0.status, ApprovalStatus::Approved);
        let t = task_a(&state).await;
        assert!(t.requires_human_approval, "foreign task stays gated");
        assert_eq!(t.status, TaskStatus::Backlog);
    }

    #[tokio::test]
    async fn another_tenants_mission_engine_reads_as_missing() {
        let state = two_tenants().await;
        let b = || Extension(ctx(WS_B, "user-b"));
        let id = || Path("mission-a".to_string());
        assert_eq!(
            get_mission_engine(State(state.clone()), b(), id())
                .await
                .err(),
            Some(StatusCode::NOT_FOUND)
        );
        assert_eq!(
            beat_mission_engine_now(State(state.clone()), b(), id())
                .await
                .err(),
            Some(StatusCode::NOT_FOUND)
        );
        assert_eq!(
            detach_mission_engine(State(state.clone()), b(), id())
                .await
                .err(),
            Some(StatusCode::NOT_FOUND)
        );
        // The owner gets through to the (absent) engine.
        let status = get_mission_engine(
            State(state),
            Extension(ctx(WS_A, "user-a")),
            Path("mission-a".to_string()),
        )
        .await
        .unwrap();
        assert!(!status.0.attached);
    }

    #[tokio::test]
    async fn a_heartbeat_cannot_target_another_tenants_entity_or_mission() {
        let state = two_tenants().await;
        for (kind, target) in [("entity", "ent-a"), ("mission", "mission-a")] {
            let res = create_heartbeat(
                State(state.clone()),
                Extension(ctx(WS_B, "user-b")),
                Json(CreateHeartbeatBody {
                    name: "spy".to_string(),
                    target_kind: kind.to_string(),
                    target_id: Some(target.to_string()),
                    director_id: "operations".to_string(),
                    cron: None,
                    cadence_secs: Some(3600),
                    workspace_id: None,
                    condition: None,
                    enabled: true,
                }),
            )
            .await;
            assert_eq!(res.err(), Some(StatusCode::NOT_FOUND), "{kind}");
        }
        let own = create_heartbeat(
            State(state),
            Extension(ctx(WS_A, "user-a")),
            Json(CreateHeartbeatBody {
                name: "pulse".to_string(),
                target_kind: "entity".to_string(),
                target_id: Some("ent-a".to_string()),
                director_id: "operations".to_string(),
                cron: None,
                cadence_secs: Some(3600),
                workspace_id: None,
                condition: None,
                enabled: true,
            }),
        )
        .await;
        assert!(own.is_ok());
    }

    #[tokio::test]
    async fn an_out_of_range_snooze_is_a_400_not_a_panic() {
        let state = two_tenants().await;
        let body: MissionActionRequest = serde_json::from_value(serde_json::json!({
            "action": "snooze_review",
            "hours": u32::MAX,
        }))
        .unwrap();
        let res = apply_mission_action(
            State(state.clone()),
            Extension(ctx(WS_A, "user-a")),
            Path("mission-a".to_string()),
            Json(body),
        )
        .await;
        assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));

        let body: MissionActionRequest = serde_json::from_value(serde_json::json!({
            "action": "snooze_review",
            "hours": 48,
        }))
        .unwrap();
        let snoozed = apply_mission_action(
            State(state),
            Extension(ctx(WS_A, "user-a")),
            Path("mission-a".to_string()),
            Json(body),
        )
        .await
        .unwrap();
        assert!(snoozed.0.last_reviewed_at.is_some());
    }

    #[tokio::test]
    async fn an_empty_string_clears_an_entity_narrative_field() {
        let state = two_tenants().await;
        let a = || Extension(ctx(WS_A, "user-a"));
        let set: UpdateEntityRequest =
            serde_json::from_value(serde_json::json!({ "notes": "remember", "goals": "grow" }))
                .unwrap();
        let set = update_entity(State(state.clone()), a(), Path("ent-a".into()), Json(set))
            .await
            .unwrap();
        assert_eq!(set.0.notes.as_deref(), Some("remember"));
        let clear: UpdateEntityRequest =
            serde_json::from_value(serde_json::json!({ "notes": "", "goals": null })).unwrap();
        let e = update_entity(State(state), a(), Path("ent-a".into()), Json(clear))
            .await
            .unwrap()
            .0;
        assert_eq!(e.notes, None, "\"\" clears");
        assert_eq!(e.goals.as_deref(), Some("grow"), "null leaves it alone");
    }
}
