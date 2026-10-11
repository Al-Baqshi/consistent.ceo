//! Entity connections to external integrations.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// `GET` handler: lists an entity's connections.
pub(super) async fn list_entity_connections(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
) -> Result<Json<Vec<EntityConnection>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entities = s.entities.read().await;
    if !entities.iter().any(|e| {
        e.id == entity_id
            && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
    }) {
        return Err(StatusCode::NOT_FOUND);
    }
    drop(entities);
    let out = s
        .entity_connections
        .read()
        .await
        .iter()
        .filter(|c| c.entity_id == entity_id)
        .cloned()
        .collect::<Vec<_>>();
    Ok(Json(out))
}

/// `POST` handler: creates an entity connection, storing any secret via
/// [`store_connection_secret_and_ref`] and logging a creation event.
pub(super) async fn create_entity_connection(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    Json(body): Json<CreateEntityConnectionRequest>,
) -> Result<Json<EntityConnection>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entities = s.entities.read().await;
    if !entities.iter().any(|e| {
        e.id == entity_id
            && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
    }) {
        return Err(StatusCode::NOT_FOUND);
    }
    drop(entities);
    if body.kind.trim().is_empty() || body.provider.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let secret_ref =
        store_connection_secret_and_ref(&state, &id, body.secret_ref, body.secret_value)?;
    let connection = EntityConnection {
        id: id.clone(),
        entity_id: entity_id.clone(),
        kind: body.kind.trim().to_string(),
        provider: body.provider.trim().to_string(),
        label: body
            .label
            .unwrap_or_else(|| format!("{} ({})", body.provider.trim(), body.kind.trim())),
        status: body.status.unwrap_or_else(|| "disconnected".to_string()),
        capability_state: body
            .capability_state
            .unwrap_or_else(|| serde_json::json!({})),
        config: body.config.unwrap_or_else(|| serde_json::json!({})),
        secret_ref,
        external_account_id: body.external_account_id,
        external_handle: body.external_handle,
        scopes: body.scopes.unwrap_or_default(),
        last_checked_at: None,
        last_error: None,
        created_by: body.created_by,
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    s.entity_connections.write().await.push(connection.clone());
    s.append_entity_connection_event(EntityConnectionEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: now,
        entity_id,
        connection_id: id,
        kind: EntityConnectionEventKind::ConnectionCreated,
        detail: Some("source=api".to_string()),
    })
    .await;
    Ok(Json(connection))
}
