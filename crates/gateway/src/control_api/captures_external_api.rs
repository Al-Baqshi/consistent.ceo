//! External capture ingress: the batch and single-item paths other systems post to.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Merge the external request's structured location hints and idempotency key
/// into a single metadata object. User-supplied metadata (if an object) is kept
/// and augmented; known keys are set without clobbering unrelated fields.
pub(super) fn build_external_metadata(body: &ExternalCaptureRequest) -> serde_json::Value {
    let mut obj = match &body.metadata {
        Some(serde_json::Value::Object(map)) => map.clone(),
        _ => serde_json::Map::new(),
    };
    let mut set = |key: &str, value: Option<&str>| {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            obj.insert(key.to_string(), serde_json::Value::String(v.to_string()));
        }
    };
    set("externalId", body.external_id.as_deref());
    set("title", body.title.as_deref());
    set("cwd", body.cwd.as_deref());
    set("repoUrl", body.repo_url.as_deref());
    set("url", body.url.as_deref());
    serde_json::Value::Object(obj)
}

/// Look up an existing capture by `(workspace, source, external_id)` for
/// idempotent ingestion. Returns a clone when found.
pub(super) async fn find_external_capture(
    s: &crate::state::ControlState,
    workspace_id: &str,
    source: &str,
    external_id: &str,
) -> Option<RawCapture> {
    s.raw_captures
        .read()
        .await
        .iter()
        .find(|c| {
            c.workspace_id == workspace_id
                && c.source == source
                && c.metadata.get("externalId").and_then(|v| v.as_str()) == Some(external_id)
        })
        .cloned()
}

/// Core external ingestion: idempotent create, mirror, and optional high-confidence
/// routing. Never promotes. Shared by the single and batch endpoints.
pub(super) async fn ingest_external_capture(
    s: &crate::state::ControlState,
    workspace_id: &str,
    body: ExternalCaptureRequest,
) -> Result<ExternalCaptureResponse, StatusCode> {
    let source = body.source.trim().to_string();
    if source.is_empty() || body.raw_text.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let selected_entity_id = body.selected_entity_id.clone();
    let auto_route = body.auto_route;

    // Idempotency: return the existing capture untouched when the same
    // (source, external_id) was already ingested in this workspace.
    if let Some(external_id) = body
        .external_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        && let Some(existing) = find_external_capture(s, workspace_id, &source, external_id).await
    {
        let entities = visible_entities(s, workspace_id).await;
        let suggestion = crate::capture_routing::suggest_route(
            &existing,
            &entities,
            selected_entity_id.as_deref(),
        );
        return Ok(ExternalCaptureResponse {
            capture: existing,
            suggestion,
            applied: false,
            existing: true,
        });
    }

    let metadata = build_external_metadata(&body);
    let created = internal_create_capture(
        s,
        workspace_id,
        None,
        CreateCaptureRequest {
            raw_text: body.raw_text,
            entity_id: body.entity_id.clone(),
            source: Some(source),
            source_ref: body.source_ref.clone(),
            session_id: body.session_id.clone(),
            provider: body.provider.clone(),
            summary: body.summary.clone(),
            metadata: Some(metadata),
        },
    )
    .await?;

    let entities = visible_entities(s, workspace_id).await;
    let suggestion =
        crate::capture_routing::suggest_route(&created, &entities, selected_entity_id.as_deref());

    // Attach only on explicit opt-in and sufficient confidence, and never when the
    // capture is already attached to the suggested entity. Never promote.
    let should_apply = auto_route
        && suggestion.confidence >= crate::capture_routing::AUTO_APPLY_THRESHOLD
        && suggestion
            .entity_id
            .as_deref()
            .is_some_and(|eid| created.entity_id.as_deref() != Some(eid));

    if !should_apply {
        return Ok(ExternalCaptureResponse {
            capture: created,
            suggestion,
            applied: false,
            existing: false,
        });
    }

    let target = suggestion.entity_id.clone().unwrap();
    let updated = {
        let mut captures = s.raw_captures.write().await;
        let c = captures
            .iter_mut()
            .find(|c| c.id == created.id && c.workspace_id == workspace_id)
            .ok_or(StatusCode::NOT_FOUND)?;
        c.entity_id = Some(target);
        c.updated_at = chrono::Utc::now().to_rfc3339();
        c.clone()
    };
    mirror_capture_change(s, &updated).await;
    Ok(ExternalCaptureResponse {
        capture: updated,
        suggestion,
        applied: true,
        existing: false,
    })
}

/// Snapshot of business entities visible in a workspace, for routing. Personal
/// spaces are intentionally excluded (`viewer = None`): captures are never
/// auto-routed into a user's private space.
pub(super) async fn visible_entities(s: &crate::state::ControlState, workspace_id: &str) -> Vec<Entity> {
    s.entities
        .read()
        .await
        .iter()
        .filter(|e| is_entity_visible(e, workspace_id, None))
        .cloned()
        .collect()
}

/// Handler: ingests a single external capture (see [`ingest_external_capture`]).
pub(super) async fn create_external_capture(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<ExternalCaptureRequest>,
) -> Result<Json<ExternalCaptureResponse>, StatusCode> {
    let s = control(&state)?;
    let out = ingest_external_capture(s, &workspace.id, body).await?;
    Ok(Json(out))
}

/// Handler: ingests a batch of external captures, one at a time so a single
/// bad item fails only itself rather than the whole batch.
pub(super) async fn create_external_captures_batch(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<ExternalCaptureBatchRequest>,
) -> Result<Json<ExternalCaptureBatchResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let mut results = Vec::with_capacity(body.items.len());
    let (mut created, mut existing, mut failed) = (0usize, 0usize, 0usize);
    // First version: sequential, per-item isolation so one bad item does not
    // fail the batch. Errors surface as an HTTP-style status on that item.
    for item in body.items {
        match ingest_external_capture(s, &workspace_id, item).await {
            Ok(out) => {
                if out.existing {
                    existing += 1;
                } else {
                    created += 1;
                }
                results.push(ExternalCaptureBatchItemResult {
                    ok: true,
                    capture: Some(out.capture),
                    suggestion: Some(out.suggestion),
                    applied: out.applied,
                    existing: out.existing,
                    error_status: None,
                });
            }
            Err(status) => {
                failed += 1;
                results.push(ExternalCaptureBatchItemResult {
                    ok: false,
                    capture: None,
                    suggestion: None,
                    applied: false,
                    existing: false,
                    error_status: Some(status.as_u16()),
                });
            }
        }
    }
    Ok(Json(ExternalCaptureBatchResponse {
        results,
        created,
        existing,
        failed,
    }))
}
