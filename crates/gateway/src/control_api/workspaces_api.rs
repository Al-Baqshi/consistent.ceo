//! Workspace (tenant) listing, creation and membership.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Summary row for the workspace picker.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceSummary {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) status: String,
    pub(super) entity_count: usize,
}

/// Body for creating a workspace.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceRequest {
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
}

/// Response after creating a workspace.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateWorkspaceResponse {
    pub id: String,
    pub name: String,
    pub status: String,
    pub local_path: String,
}

/// How many audit rows a caller sees from `/api/audit/recent`.
const RECENT_AUDIT_ROWS: usize = 50;
/// How far back `/api/audit/recent` reads before filtering to the caller's
/// workspace, so a busy tenant does not push everyone else's rows out of the
/// window entirely.
const RECENT_AUDIT_SCAN: usize = 1000;

/// Whether an `audit_trail` row belongs to the caller. Rows carry the
/// workspace (and entity) the turn ran in; a row tied to an entity also needs
/// that entity to be visible (a teammate's Personal Space is not). Rows with no
/// workspace predate it and read as the default workspace's.
pub(super) fn audit_event_visible(
    event: &consistent_world_architect::AuditEvent,
    workspace_id: &str,
    entity_ids: &HashSet<String>,
) -> bool {
    let event_ws = event
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|w| !w.is_empty())
        .unwrap_or(DEFAULT_WORKSPACE_ID);
    if event_ws != workspace_id {
        return false;
    }
    match event.entity_id.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
        Some(eid) => entity_ids.contains(eid),
        None => true,
    }
}

/// Entities the caller can see, for audit filtering.
async fn visible_entity_ids(state: &ServerState, workspace: &WorkspaceCtx) -> HashSet<String> {
    match control(state) {
        Ok(s) => {
            let entities = s.entities.read().await;
            entity_ids_for_workspace(
                &entities,
                &workspace.id,
                Some(workspace.principal_id.as_str()),
            )
        }
        Err(_) => HashSet::new(),
    }
}

/// Phase 1 (Build 3) — surface the last N rows from `audit_trail` so we can
/// eyeball what the Architect's shadow run produced against real entities.
/// Only the caller's workspace: the trail carries every tenant's turns.
pub(super) async fn list_recent_audit(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<Vec<consistent_world_architect::AuditEvent>>, StatusCode> {
    let Some(store) = state.audit_store.as_ref() else {
        return Ok(Json(Vec::new()));
    };
    use consistent_world_architect::AuditWriter;
    match store.recent(RECENT_AUDIT_SCAN).await {
        Ok(rows) => {
            let entity_ids = visible_entity_ids(&state, &workspace).await;
            let mut rows: Vec<_> = rows
                .into_iter()
                .filter(|e| audit_event_visible(e, &workspace.id, &entity_ids))
                .collect();
            rows.truncate(RECENT_AUDIT_ROWS);
            Ok(Json(rows))
        }
        Err(e) => {
            tracing::warn!(error = %e, "audit_trail recent query failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// One turn's Architect trail, oldest step first.
///
/// `/api/audit/recent` interleaves every concurrent turn, so it cannot be used
/// to read a single pipeline run once there is any traffic. This is the view
/// that answers "what did Verify decide about *that* answer". Rows from another
/// workspace's turn are dropped, so a guessed turn id reads as empty.
pub(super) async fn list_turn_audit(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(turn_id): Path<String>,
) -> Result<Json<Vec<consistent_world_architect::AuditEvent>>, StatusCode> {
    let Some(store) = state.audit_store.as_ref() else {
        return Ok(Json(Vec::new()));
    };
    use consistent_world_architect::AuditWriter;
    match store.for_turn(&turn_id).await {
        Ok(rows) => {
            let entity_ids = visible_entity_ids(&state, &workspace).await;
            Ok(Json(
                rows.into_iter()
                    .filter(|e| audit_event_visible(e, &workspace.id, &entity_ids))
                    .collect(),
            ))
        }
        Err(e) => {
            tracing::warn!(error = %e, turn_id = %turn_id, "audit_trail turn query failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `GET` handler: lists workspaces with their (non-personal) entity counts,
/// merging the tenant store's rows over the in-memory entity tally.
/// Whether a workspace summary is still wearing its id as its name — the
/// shape a self-registered user's workspace has until something names it.
pub(super) fn workspace_needs_owner_name(id: &str, name: &str) -> bool {
    id != "default" && (name == id || name == "Untitled workspace")
}

pub(super) async fn list_workspaces(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<Vec<WorkspaceSummary>>, StatusCode> {
    let s = control(&state)?;
    // Every registered user's workspace id is their user id, and the names
    // below are their usernames — so the full list is a user directory. Only
    // the instance admin gets it; anyone else sees their own workspace plus
    // any they created.
    let admin =
        crate::server::is_instance_admin(&auth, crate::server::gateway_is_self_hosted(&state));
    let entities = s.entities.read().await;
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for entity in entities.iter() {
        // Personal Spaces are per-user and private; they don't count toward a
        // workspace's entity total.
        if entity.is_personal {
            continue;
        }
        let wid = entity_workspace_id(entity);
        *counts.entry(wid.to_string()).or_insert(0) += 1;
    }
    // Always include "default"
    counts.entry("default".to_string()).or_insert(0);
    let mut by_id: std::collections::HashMap<String, WorkspaceSummary> = counts
        .into_iter()
        .map(|(id, count)| {
            let name = if id == "default" {
                "Default".to_string()
            } else {
                id.clone()
            };
            WorkspaceSummary {
                id: id.clone(),
                name,
                status: "active".to_string(),
                entity_count: count,
            }
        })
        .map(|summary| (summary.id.clone(), summary))
        .collect();
    let mut owned: HashSet<String> = HashSet::new();
    owned.insert(auth.workspace_id.clone());
    if let Some(store) = state.tenant_store.as_ref() {
        match store.list_workspaces().await {
            Ok(rows) => {
                for row in rows {
                    if row.primary_user_id.as_deref() == Some(auth.principal_id.as_str()) {
                        owned.insert(row.id.clone());
                    }
                    let count = by_id.get(&row.id).map(|s| s.entity_count).unwrap_or(0);
                    by_id.insert(
                        row.id.clone(),
                        WorkspaceSummary {
                            id: row.id,
                            name: if row.name.trim().is_empty() {
                                "Untitled workspace".to_string()
                            } else {
                                row.name
                            },
                            status: row.status,
                            entity_count: count,
                        },
                    );
                }
            }
            Err(err) => tracing::warn!(error = %err, "list workspaces from tenant store failed"),
        }
    }
    if !admin {
        by_id.retain(|id, _| owned.contains(id));
        // The caller's own workspace is listed even before anything is in it.
        by_id
            .entry(auth.workspace_id.clone())
            .or_insert_with(|| WorkspaceSummary {
                id: auth.workspace_id.clone(),
                name: auth.workspace_id.clone(),
                status: "active".to_string(),
                entity_count: 0,
            });
    }
    // A registered user's workspace is keyed by their user id and nothing
    // names it (`AuthStore::register` creates no tenant row), so the switcher
    // showed a 32-character hex string as the tenant. Name it after its
    // owner — the person who signed in to it in the first place. Explicit
    // tenant-store names above still win; only the id-as-name fallback goes.
    if let Some(auth) = state.auth_store.as_ref() {
        for summary in by_id.values_mut() {
            if !workspace_needs_owner_name(&summary.id, &summary.name) {
                continue;
            }
            if let Some(user) = auth.get_auth_user(&summary.id).await {
                summary.name = user.username;
            }
        }
    }
    let mut summaries: Vec<WorkspaceSummary> = by_id.into_values().collect();
    summaries.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Json(summaries))
}

/// Core workspace-creation logic: persists the tenant row and lays out its
/// local vault folder.
pub(crate) async fn internal_create_workspace(
    state: &ServerState,
    workspace_id: &str,
    name: &str,
    principal_id: Option<&str>,
) -> Result<CreateWorkspaceResponse, StatusCode> {
    let Some(store) = state.tenant_store.as_ref() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };
    let clean_name = name.trim();
    if clean_name.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let workspace_id = safe_workspace_id(workspace_id);
    // `default` is the operator's workspace; only the machine owner may name it.
    if workspace_id == DEFAULT_WORKSPACE_ID
        && !matches!(principal_id, Some("bearer") | Some("anonymous"))
    {
        return Err(StatusCode::CONFLICT);
    }
    let row = store
        .create_workspace(&workspace_id, clean_name, principal_id)
        .await
        .map_err(|err| {
            if err
                .downcast_ref::<crate::tenant_store::WorkspaceTaken>()
                .is_some()
            {
                tracing::info!(workspace_id = %workspace_id, "refused to take over an existing workspace");
                return StatusCode::CONFLICT;
            }
            tracing::warn!(error = %err, workspace_id = %workspace_id, "workspace creation failed");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    crate::local_vault::ensure_workspace_layout(&row.id).await;
    let local_path = crate::local_vault::vault_root()
        .join("workspaces")
        .join(&row.id)
        .to_string_lossy()
        .to_string();
    Ok(CreateWorkspaceResponse {
        id: row.id,
        name: row.name,
        status: row.status,
        local_path,
    })
}

/// `POST` handler: creates a workspace. Reads the raw request body itself
/// (rather than a `Json<T>` extractor) so it can also pull the `AuthContext`
/// out of the request extensions before consuming the body.
pub(super) async fn create_workspace(
    State(state): State<ServerState>,
    req: Request<Body>,
) -> Result<Json<CreateWorkspaceResponse>, StatusCode> {
    let ctx = req.extensions().get::<AuthContext>().cloned();
    let bytes = axum::body::to_bytes(req.into_body(), 1024 * 16)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let body: CreateWorkspaceRequest =
        serde_json::from_slice(&bytes).map_err(|_| StatusCode::BAD_REQUEST)?;
    let id = body
        .id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(safe_workspace_id)
        .unwrap_or_else(|| safe_workspace_id(&body.name));
    let out = internal_create_workspace(
        &state,
        &id,
        &body.name,
        ctx.as_ref().map(|c| c.principal_id.as_str()),
    )
    .await?;
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(ws: Option<&str>, entity: Option<&str>) -> consistent_world_architect::AuditEvent {
        consistent_world_architect::AuditEvent {
            turn_id: "t".into(),
            trigger: "chat".into(),
            workspace_id: ws.map(str::to_string),
            entity_id: entity.map(str::to_string),
            step: "inspect".into(),
            status: "ok".into(),
            payload: serde_json::Value::Null,
            occurred_at: String::new(),
        }
    }

    #[test]
    fn audit_rows_are_scoped_to_the_callers_workspace() {
        let mine: HashSet<String> = ["ent-a".to_string()].into_iter().collect();
        assert!(audit_event_visible(&event(Some("ws-a"), None), "ws-a", &mine));
        assert!(audit_event_visible(&event(Some("ws-a"), Some("ent-a")), "ws-a", &mine));
        assert!(!audit_event_visible(&event(Some("ws-b"), None), "ws-a", &mine));
        // A teammate's Personal Space in the same workspace stays private.
        assert!(!audit_event_visible(&event(Some("ws-a"), Some("ent-x")), "ws-a", &mine));
        // Workspace-less rows predate tenancy: the default workspace's.
        assert!(!audit_event_visible(&event(None, None), "ws-a", &mine));
        assert!(audit_event_visible(&event(None, None), "default", &mine));
    }
}
