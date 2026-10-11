//! Tasks: the canonical work item — list, create, read, update, cancel, requeue.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// `GET` handler: lists tasks visible in the workspace, filtered by status,
/// entity, and mission, and paginated.
pub(super) async fn list_tasks(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<TaskQuery>,
) -> Result<Json<PaginatedResponse<Task>>, StatusCode> {
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
    let tasks = s.tasks.read().await;
    let filtered: Vec<Task> = tasks
        .iter()
        .filter(|t| task_visible_in_workspace(t, &entity_ids, &workspace_id))
        .filter(|t| {
            if let Some(ref status) = q.status {
                t.status == task_status_from_str(status)
            } else {
                true
            }
        })
        .filter(|t| {
            if let Some(ref eid) = q.entity_id {
                t.entity_id.as_deref() == Some(eid)
            } else {
                true
            }
        })
        .filter(|t| {
            if let Some(ref mid) = q.mission_id {
                t.mission_id.as_deref() == Some(mid)
            } else {
                true
            }
        })
        .cloned()
        .collect();
    let total = filtered.len();
    let pag = apply_pagination(
        filtered,
        total,
        &PaginationQuery {
            limit: q.limit,
            offset: q.offset,
        },
    );
    Ok(Json(pag))
}

/// `POST` handler: creates a task (see [`internal_create_task`]).
pub(super) async fn create_task(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateTaskRequest>,
) -> Result<Json<Task>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let task = internal_create_task(
        s,
        &workspace_id,
        Some(workspace.principal_id.as_str()),
        body,
    )
    .await?;
    Ok(Json(task))
}

/// `GET` handler: fetches a single task by id, scoped to the workspace.
pub(super) async fn get_task(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<Task>, StatusCode> {
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
    let tasks = s.tasks.read().await;
    tasks
        .iter()
        .find(|t| t.id == id && task_visible_in_workspace(t, &entity_ids, &workspace_id))
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// `PATCH` handler: updates a task's status/progress/result/due date, pushing
/// a completed/blocked feed event on a status transition.
pub(super) async fn update_task(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTaskRequest>,
) -> Result<Json<Task>, StatusCode> {
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
    let mut tasks = s.tasks.write().await;
    let t = tasks
        .iter_mut()
        .find(|t| t.id == id && task_visible_in_workspace(t, &entity_ids, &workspace_id))
        .ok_or(StatusCode::NOT_FOUND)?;
    let old_status = t.status;
    if let Some(status) = body.status {
        crate::state::set_task_status_by_human(t, status);
        if status == TaskStatus::Completed || status == TaskStatus::Failed {
            t.progress = 100;
        }
    }
    if let Some(due_at) = body.due_at.clone() {
        t.due_at = Some(due_at);
    }
    if let Some(progress) = body.progress {
        t.progress = progress;
    }
    if let Some(result) = body.result {
        t.result = Some(result);
    }
    let out = t.clone();
    drop(tasks);
    if out.status == TaskStatus::Completed && old_status != TaskStatus::Completed {
        s.push_operator_feed(OperatorFeedKind::TaskCompleted, &out, None)
            .await;
    } else if out.status == TaskStatus::Failed && old_status != TaskStatus::Failed {
        s.push_operator_feed(
            OperatorFeedKind::TaskBlocked,
            &out,
            out.result
                .clone()
                .or_else(|| Some("Task failed".to_string())),
        )
        .await;
    }
    Ok(Json(out))
}

/// Handler: cancels a task, rejecting an already-terminal one with 409.
pub(super) async fn cancel_task(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<Task>, StatusCode> {
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
    let updated = {
        let mut tasks = s.tasks.write().await;
        let t = tasks
            .iter_mut()
            .find(|t| t.id == id && task_visible_in_workspace(t, &entity_ids, &workspace_id))
            .ok_or(StatusCode::NOT_FOUND)?;
        if matches!(t.status, TaskStatus::Completed | TaskStatus::Cancelled) {
            return Err(StatusCode::CONFLICT);
        }
        let was_running = t.status == TaskStatus::InProgress;
        crate::state::set_task_status_by_human(t, TaskStatus::Cancelled);
        t.progress = 100;
        crate::state::task_meta_clear(t, "nextRetryAt");
        t.assigned_director_id = None;
        t.assigned_agent_id = None;
        t.result = Some(if was_running {
            "Cancelled by user. The in-flight run cannot be killed; its result is ignored."
                .to_string()
        } else {
            "Cancelled by user.".to_string()
        });
        t.clone()
    };
    s.push_operator_feed(
        OperatorFeedKind::TaskCancelled,
        &updated,
        updated.result.clone(),
    )
    .await;
    Ok(Json(updated))
}

/// Handler: cancels a coding-agent run — kills the live process if this
/// gateway owns it, then marks the run and its linked task cancelled.
pub(super) async fn cancel_entity_agent_run(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, run_id)): Path<(String, String)>,
) -> Result<Json<EntityAgentRunResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;

    // Authorize before acting: the run must belong to an entity the caller can
    // see, in the caller's workspace, and still be live. Killing the process
    // first would let anyone who learned a run id stop another tenant's agent
    // and only then get a 404.
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    {
        let runs = s.external_agent_runs.read().await;
        let run = runs
            .iter()
            .find(|r| r.id == run_id && r.entity_id == entity_id && r.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        if matches!(
            run.status.as_str(),
            "completed" | "failed" | "cancelled" | "stale"
        ) {
            return Err(StatusCode::CONFLICT);
        }
    }

    // Then kill the child and record the outcome. Sessions launched by this
    // gateway have a live cancel channel; anything else (a run inherited from a
    // previous life of the process) can only be marked, which is what the
    // wording below reflects.
    let killed = s.agent_processes.cancel(&run_id).await;

    let updated = {
        let mut runs = s.external_agent_runs.write().await;
        let run = runs
            .iter_mut()
            .find(|r| r.id == run_id && r.entity_id == entity_id && r.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        if matches!(
            run.status.as_str(),
            "completed" | "failed" | "cancelled" | "stale"
        ) {
            return Err(StatusCode::CONFLICT);
        }
        run.status = "cancelled".to_string();
        run.completed_at = Some(chrono::Utc::now().to_rfc3339());
        run.error = Some(if killed {
            "Cancelled by user; the agent process was stopped.".to_string()
        } else {
            "Cancelled by user. No live process was owned by this gateway; a late result is ignored."
                .to_string()
        });
        run.clone()
    };
    // Cancel the linked task so the loop does not act on a late result.
    if let Some(ref tid) = updated.task_id {
        let cancelled = {
            let mut tasks = s.tasks.write().await;
            tasks.iter_mut().find(|t| t.id == *tid).and_then(|t| {
                if matches!(t.status, TaskStatus::Completed | TaskStatus::Cancelled) {
                    None
                } else {
                    crate::state::set_task_status_by_human(t, TaskStatus::Cancelled);
                    t.progress = 100;
                    crate::state::task_meta_clear(t, "nextRetryAt");
                    t.result = Some("Cancelled with its external agent run.".to_string());
                    Some(t.clone())
                }
            })
        };
        if let Some(t) = cancelled {
            s.push_operator_feed(OperatorFeedKind::TaskCancelled, &t, t.result.clone())
                .await;
        }
    }
    Ok(Json(entity_agent_run_response(updated)))
}

/// Handler: resets a task back to `Queued` for a fresh run, clearing the
/// executor's attempt/backoff metadata so the veto matrix doesn't keep
/// refusing it, and clearing any stale outcome/result.
pub(super) async fn requeue_task(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<Task>, StatusCode> {
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
    let requeued = {
        let mut tasks = s.tasks.write().await;
        let t = tasks
            .iter_mut()
            .find(|t| t.id == id && task_visible_in_workspace(t, &entity_ids, &workspace_id))
            .ok_or(StatusCode::NOT_FOUND)?;
        crate::state::set_task_status_by_human(t, TaskStatus::Queued);
        t.progress = 0;
        t.assigned_director_id = None;
        t.assigned_agent_id = None;
        // A human requeue is a fresh start: without clearing the attempt
        // markers the executor's veto matrix keeps refusing the task and the
        // "retry" button is a no-op. A stale failure summary is cleared too so
        // it can't leak into the next run's prompt as task details.
        t.outcome = None;
        t.result = None;
        crate::task_executor::write_meta(
            t,
            &[
                (crate::task_executor::META_AUTO_RUN_ATTEMPTED, ""),
                (crate::task_executor::META_ATTEMPT_COUNT, ""),
                (crate::task_executor::META_NEXT_ATTEMPT_AT, ""),
            ],
        );
        t.clone()
    };
    s.persist_task_row(&requeued).await;
    Ok(Json(requeued))
}
