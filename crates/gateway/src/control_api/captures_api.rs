//! Captures: the inbox of loose thoughts, and the paths that promote one into real work.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// `GET` handler: lists raw captures visible in the workspace, optionally
/// decorated with a routing suggestion per item.
pub(super) async fn list_captures(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<CaptureQuery>,
) -> Result<Json<PaginatedResponse<RawCaptureWithRoute>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let status_filter = q.status.as_deref().map(raw_capture_status_from_str);
    // Entity ids visible to THIS user (excludes other users' Personal Spaces).
    let visible_entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let captures = s.raw_captures.read().await;
    let mut filtered: Vec<RawCapture> = captures
        .iter()
        .filter(|c| c.workspace_id == workspace_id)
        // Drop captures attached to an entity the caller can't see (e.g. a
        // teammate's Personal Space). Unattached inbox captures stay visible.
        .filter(|c| {
            c.entity_id
                .as_deref()
                .is_none_or(|eid| visible_entity_ids.contains(eid))
        })
        .filter(|c| match q.entity_id.as_deref() {
            Some(eid) => c.entity_id.as_deref() == Some(eid),
            None => true,
        })
        .filter(|c| match status_filter {
            Some(parsed) => parsed == Some(c.status),
            None => true,
        })
        .cloned()
        .collect();
    // Newest first so the review panel surfaces fresh captures at the top.
    filtered.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let total = filtered.len();
    let pag = apply_pagination(
        filtered,
        total,
        &PaginationQuery {
            limit: q.limit,
            offset: q.offset,
        },
    );

    // Only compute routing for the page that will be returned, and only when
    // requested. Suggestions are advisory; nothing is applied here.
    let routes: Vec<Option<crate::capture_routing::RouteSuggestion>> = if q.with_route {
        let entities: Vec<Entity> = s
            .entities
            .read()
            .await
            .iter()
            .filter(|e| is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str())))
            .cloned()
            .collect();
        pag.items
            .iter()
            .map(|c| {
                Some(crate::capture_routing::suggest_route(
                    c,
                    &entities,
                    q.selected_entity_id.as_deref(),
                ))
            })
            .collect()
    } else {
        vec![None; pag.items.len()]
    };

    let items = pag
        .items
        .into_iter()
        .zip(routes)
        .map(|(capture, route)| RawCaptureWithRoute { capture, route })
        .collect();
    Ok(Json(PaginatedResponse {
        items,
        total: pag.total,
        limit: pag.limit,
        offset: pag.offset,
        has_more: pag.has_more,
    }))
}

/// `POST` handler: creates a raw capture (see [`internal_create_capture`]).
pub(super) async fn create_capture(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateCaptureRequest>,
) -> Result<Json<RawCapture>, StatusCode> {
    let s = control(&state)?;
    let capture = internal_create_capture(
        s,
        &workspace.id,
        Some(workspace.principal_id.as_str()),
        body,
    )
    .await?;
    Ok(Json(capture))
}

/// `PATCH` handler: updates a capture's summary/status/metadata and
/// re-mirrors it to the local vault.
pub(super) async fn update_capture(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCaptureRequest>,
) -> Result<Json<RawCapture>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let updated = {
        let mut captures = s.raw_captures.write().await;
        let capture = captures
            .iter_mut()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        if let Some(summary) = body.summary {
            let trimmed = summary.trim().to_string();
            capture.summary = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            };
        }
        if let Some(status) = body.status {
            capture.status = status;
        }
        if let Some(metadata) = body.metadata {
            capture.metadata = metadata;
        }
        capture.updated_at = chrono::Utc::now().to_rfc3339();
        capture.clone()
    };
    mirror_capture_change(s, &updated).await;
    Ok(Json(updated))
}

/// Handler: marks a capture dismissed and re-mirrors it to the local vault.
pub(super) async fn dismiss_capture(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<RawCapture>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let updated = {
        let mut captures = s.raw_captures.write().await;
        let capture = captures
            .iter_mut()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        capture.status = RawCaptureStatus::Dismissed;
        capture.updated_at = chrono::Utc::now().to_rfc3339();
        capture.clone()
    };
    mirror_capture_change(s, &updated).await;
    Ok(Json(updated))
}

/// Handler: promotes a capture into a task, linking the two.
pub(super) async fn promote_capture_task(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<PromoteCaptureTaskRequest>,
) -> Result<Json<PromoteCaptureResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let capture = {
        let captures = s.raw_captures.read().await;
        captures
            .iter()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    // Idempotent: if already promoted and the task still exists, return it.
    if let Some(ref task_id) = capture.promoted_task_id
        && let Some(task) = s
            .tasks
            .read()
            .await
            .iter()
            .find(|t| t.id == *task_id)
            .cloned()
    {
        return Ok(Json(PromoteCaptureResponse { capture, task }));
    }
    if let Some(priority) = body.priority.as_deref()
        && !is_valid_priority(priority)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let title = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .unwrap_or_else(|| capture_title(&capture));
    let task = internal_create_task(
        s,
        &workspace_id,
        None,
        CreateTaskRequest {
            title,
            dept: body.dept,
            roles: None,
            entity_id: capture.entity_id.clone(),
            mission_id: None,
            priority: Some(body.priority.unwrap_or_else(|| "medium".to_string())),
            requires_human_approval: body.requires_human_approval,
            task_kind: Some(
                body.task_kind
                    .unwrap_or_else(|| "capture-promotion".to_string()),
            ),
            content: Some(capture.raw_text.clone()),
            director_id: body.director_id,
            agent_id: None,
            connection_id: None,
            external_source: Some("capture".to_string()),
            external_id: Some(capture.id.clone()),
            metadata: Some(serde_json::json!({
                "captureId": capture.id,
                "source": capture.source,
                "provider": capture.provider,
            })),
            due_at: None,
        },
    )
    .await?;
    let updated = {
        let mut captures = s.raw_captures.write().await;
        let c = captures
            .iter_mut()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        c.status = RawCaptureStatus::PromotedToTask;
        c.promoted_task_id = Some(task.id.clone());
        c.updated_at = chrono::Utc::now().to_rfc3339();
        c.clone()
    };
    mirror_capture_change(s, &updated).await;
    Ok(Json(PromoteCaptureResponse {
        capture: updated,
        task,
    }))
}

/// Handler: promotes a capture into a narrative note appended to an entity's
/// local vault, marking the capture `NarrativeUpdated`.
pub(super) async fn update_capture_narrative(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCaptureNarrativeRequest>,
) -> Result<Json<CaptureReviewResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let capture = {
        let captures = s.raw_captures.read().await;
        captures
            .iter()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    // A narrative update always targets an entity: from the request, else the
    // one already on the capture.
    let entity_id = body
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .or_else(|| capture.entity_id.clone())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let entity = {
        let entities = s.entities.read().await;
        let entity = entities
            .iter()
            .find(|e| e.id == entity_id)
            .ok_or(StatusCode::BAD_REQUEST)?;
        if !is_entity_visible(entity, &workspace_id, Some(workspace.principal_id.as_str())) {
            return Err(StatusCode::FORBIDDEN);
        }
        entity.clone()
    };
    let note = body
        .note
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .or_else(|| capture.summary.clone())
        .unwrap_or_else(|| capture.raw_text.clone());
    let note = note.trim().to_string();
    if note.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = crate::local_vault::append_entity_narrative_note(
        &entity,
        &workspace_id,
        &note,
        &capture.id,
    )
    .await
    .map_err(|err| {
        tracing::warn!(capture_id = %id, error = %err, "failed to append narrative note");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let updated = {
        let mut captures = s.raw_captures.write().await;
        let c = captures
            .iter_mut()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        c.entity_id = Some(entity_id);
        c.status = RawCaptureStatus::NarrativeUpdated;
        c.updated_at = chrono::Utc::now().to_rfc3339();
        c.clone()
    };
    mirror_capture_change(s, &updated).await;
    Ok(Json(CaptureReviewResponse {
        capture: updated,
        note_path: path.to_string_lossy().to_string(),
    }))
}

/// Handler: promotes a capture into a stored memory note (canonical or
/// lessons), entity-scoped or workspace-inbox depending on the request.
pub(super) async fn promote_capture_memory(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<PromoteCaptureMemoryRequest>,
) -> Result<Json<CaptureReviewResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let capture = {
        let captures = s.raw_captures.read().await;
        captures
            .iter()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    // Memory notes may be entity-scoped or land in the workspace inbox.
    let entity_id = body
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .or_else(|| capture.entity_id.clone());
    let entity = if let Some(ref eid) = entity_id {
        let entities = s.entities.read().await;
        let entity = entities
            .iter()
            .find(|e| e.id == *eid)
            .ok_or(StatusCode::BAD_REQUEST)?;
        if !is_entity_visible(entity, &workspace_id, Some(workspace.principal_id.as_str())) {
            return Err(StatusCode::FORBIDDEN);
        }
        Some(entity.clone())
    } else {
        None
    };
    let kind = match body.kind.as_deref().map(|k| k.trim().to_ascii_lowercase()) {
        Some(ref k) if k == "lessons" || k == "lesson" => "lessons",
        _ => "canonical",
    };
    let title = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .unwrap_or_else(|| capture_title(&capture));
    let note_body = body
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string())
        .unwrap_or_else(|| capture.raw_text.clone());
    let path = crate::local_vault::write_memory_note(
        entity.as_ref(),
        &workspace_id,
        kind,
        &title,
        &note_body,
        &capture,
    )
    .await
    .map_err(|err| {
        tracing::warn!(capture_id = %id, error = %err, "failed to write memory note");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let updated = {
        let mut captures = s.raw_captures.write().await;
        let c = captures
            .iter_mut()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        if let Some(eid) = entity_id {
            c.entity_id = Some(eid);
        }
        c.status = RawCaptureStatus::PromotedToMemory;
        c.updated_at = chrono::Utc::now().to_rfc3339();
        c.clone()
    };
    mirror_capture_change(s, &updated).await;
    Ok(Json(CaptureReviewResponse {
        capture: updated,
        note_path: path.to_string_lossy().to_string(),
    }))
}

/// Handler: computes a routing suggestion for a capture and, when
/// `auto_apply` is set and confidence clears the threshold, attaches it.
pub(super) async fn route_capture(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<RouteCaptureRequest>,
) -> Result<Json<RouteCaptureResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let capture = {
        let captures = s.raw_captures.read().await;
        captures
            .iter()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    let entities: Vec<Entity> = s
        .entities
        .read()
        .await
        .iter()
        .filter(|e| is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str())))
        .cloned()
        .collect();
    let suggestion = crate::capture_routing::suggest_route(
        &capture,
        &entities,
        body.selected_entity_id.as_deref(),
    );

    // Only mutate on explicit opt-in and sufficient confidence, and never when
    // the capture is already attached to the suggested entity.
    let should_apply = body.auto_apply
        && suggestion.confidence >= crate::capture_routing::AUTO_APPLY_THRESHOLD
        && suggestion
            .entity_id
            .as_deref()
            .is_some_and(|eid| capture.entity_id.as_deref() != Some(eid));

    if !should_apply {
        return Ok(Json(RouteCaptureResponse {
            capture,
            suggestion,
            applied: false,
        }));
    }

    let target = suggestion.entity_id.clone().unwrap();
    let updated = {
        let mut captures = s.raw_captures.write().await;
        let c = captures
            .iter_mut()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        c.entity_id = Some(target);
        c.updated_at = chrono::Utc::now().to_rfc3339();
        c.clone()
    };
    mirror_capture_change(s, &updated).await;
    Ok(Json(RouteCaptureResponse {
        capture: updated,
        suggestion,
        applied: true,
    }))
}

/// Handler: manually attaches a capture to an entity without changing its
/// review status.
pub(super) async fn attach_capture(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<AttachCaptureRequest>,
) -> Result<Json<RawCapture>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_id = body.entity_id.trim().to_string();
    if entity_id.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // The target entity must be visible in the caller's workspace.
    {
        let entities = s.entities.read().await;
        let entity = entities
            .iter()
            .find(|e| e.id == entity_id)
            .ok_or(StatusCode::BAD_REQUEST)?;
        if !is_entity_visible(entity, &workspace_id, Some(workspace.principal_id.as_str())) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    // Manual attach only reroutes the capture; it preserves the review status.
    let updated = {
        let mut captures = s.raw_captures.write().await;
        let c = captures
            .iter_mut()
            .find(|c| c.id == id && c.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        c.entity_id = Some(entity_id);
        c.updated_at = chrono::Utc::now().to_rfc3339();
        c.clone()
    };
    mirror_capture_change(s, &updated).await;
    Ok(Json(updated))
}

/// Handler: LLM-interprets a capture into a suggested routing/action, without
/// committing anything (see `crate::capture::interpret_capture`).
pub(super) async fn capture_interpret(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::capture::CaptureInterpretRequest>,
) -> Result<Json<crate::capture::CaptureInterpretResponse>, StatusCode> {
    let s = control(&state)?;
    let cfg = state
        .app_config
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let out = crate::capture::interpret_capture(&state, s, cfg, &workspace.id, body).await?;
    Ok(Json(out))
}

/// Handler: commits a previously interpreted capture action (see
/// `crate::capture::commit_capture`).
pub(super) async fn capture_commit(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::capture::CaptureCommitRequest>,
) -> Result<Json<crate::capture::CaptureCommitResponse>, StatusCode> {
    let s = control(&state)?;
    let out = crate::capture::commit_capture(
        &state,
        s,
        &workspace.id,
        Some(workspace.principal_id.as_str()),
        body,
    )
    .await?;
    Ok(Json(out))
}
