//! HTTP API for the live canvas (spatial brain map).
//!
//! Every canvas belongs to an entity space. Requests default to the viewer's
//! Personal Space when no `entity_id` is given, and every access resolves the
//! owning entity through [`crate::control_api::is_entity_visible`], so
//! privacy is inherited from entity visibility rather than re-implemented
//! here. Nodes with a live kind (`task` / `mission` / `entity`) are hydrated
//! on read with the current title and status of the record they mirror —
//! the canvas shows real state, not a sketch of it.

use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{get, patch, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::canvas_store::{CanvasEdge, CanvasNode, LIVE_KINDS, STATIC_KINDS};
use crate::control_api::WorkspaceCtx;
use crate::state::{MissionStatus, ServerState, TaskStatus, personal_space_id};

/// Mounts the canvas CRUD routes; workspace context is resolved by the
/// middleware layered on below.
pub fn canvas_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/canvas", get(get_canvas))
        .route("/api/canvas/nodes", post(create_node))
        .route(
            "/api/canvas/nodes/{id}",
            patch(update_node).delete(delete_node),
        )
        .route("/api/canvas/edges", post(create_edge))
        .route("/api/canvas/edges/{id}", axum::routing::delete(delete_edge))
        .layer(middleware::from_fn(
            crate::control_api::workspace_context_middleware,
        ))
        .with_state(state)
}

/// Resolves the target entity space for a canvas request and enforces
/// visibility. `None` → the viewer's Personal Space (auto-provisioned when
/// missing, so a fresh account always has a canvas to land on).
async fn resolve_space(
    state: &ServerState,
    workspace: &WorkspaceCtx,
    entity_id: Option<&str>,
) -> Result<String, StatusCode> {
    let control = state
        .control_state
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let personal_id = personal_space_id(&workspace.id, &workspace.principal_id);
    let target = entity_id
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(&personal_id)
        .to_string();
    if target == personal_id {
        control
            .ensure_personal_space(&workspace.principal_id, &workspace.id)
            .await;
    }
    let entities = control.entities.read().await;
    let visible = entities.iter().any(|e| {
        e.id == target
            && crate::control_api::is_entity_visible(
                e,
                &workspace.id,
                Some(&workspace.principal_id),
            )
    });
    if visible {
        Ok(target)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

fn canvas_store(
    state: &ServerState,
) -> Result<&std::sync::Arc<crate::canvas_store::CanvasStore>, StatusCode> {
    state
        .canvas_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// Current state of the record a live node mirrors.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveRef {
    title: String,
    status: String,
    exists: bool,
}

// A stored node plus its hydrated live state, as sent to clients.
#[derive(Debug, Serialize)]
struct NodeOut {
    #[serde(flatten)]
    node: CanvasNode,
    #[serde(skip_serializing_if = "Option::is_none")]
    live: Option<LiveRef>,
}

// Serializes to the enum's serde string repr; falls back to `{:?}` if that ever fails.
fn task_status_label(status: &TaskStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{status:?}"))
}

// Same fallback pattern as `task_status_label`, for the mission status enum.
fn mission_status_label(status: &MissionStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{status:?}"))
}

/// Joins each live node against the current control-state record it mirrors.
///
/// Lock order (rule 2 on `ControlState`): never two guards at once. Each
/// collection is read, the referenced rows cloned, and the guard dropped
/// before the next is taken. This used to hold `tasks`, `missions` and
/// `entities` read guards together, which — against a handler holding
/// `missions.write()` while it waited on `entities` — deadlocked as soon as a
/// writer queued between them.
///
/// Tasks and missions are only shown when their entity is visible to the
/// viewer: a node's `ref_id` is client-supplied, so without the check a
/// canvas could be used to read another tenant's task and mission titles.
async fn hydrate(
    state: &ServerState,
    workspace: &WorkspaceCtx,
    nodes: Vec<CanvasNode>,
) -> Vec<NodeOut> {
    let Some(control) = state.control_state.as_ref() else {
        return nodes
            .into_iter()
            .map(|node| NodeOut { node, live: None })
            .collect();
    };
    let wanted = |kind: &str| -> std::collections::HashSet<String> {
        nodes
            .iter()
            .filter(|n| n.kind == kind)
            .filter_map(|n| n.ref_id.clone())
            .collect()
    };
    let (task_ids, mission_ids) = (wanted("task"), wanted("mission"));

    // Entities first: the visible set gates tasks and missions too.
    let (visible_entity_ids, entity_live) = {
        let entities = control.entities.read().await;
        let mut ids = std::collections::HashSet::new();
        let mut live = std::collections::HashMap::new();
        for e in entities.iter().filter(|e| {
            crate::control_api::is_entity_visible(e, &workspace.id, Some(&workspace.principal_id))
        }) {
            ids.insert(e.id.clone());
            live.insert(
                e.id.clone(),
                LiveRef {
                    title: e.name.clone(),
                    status: e.status.clone(),
                    exists: true,
                },
            );
        }
        (ids, live)
    };
    // A task with no entity belongs to the default workspace only (mirrors
    // `control_api::task_visible_in_workspace`).
    let entity_visible = |entity_id: Option<&str>| match entity_id {
        Some(eid) => visible_entity_ids.contains(eid),
        None => workspace.id == "default",
    };
    let task_live: std::collections::HashMap<String, LiveRef> = {
        let tasks = control.tasks.read().await;
        tasks
            .iter()
            .filter(|t| task_ids.contains(&t.id) && entity_visible(t.entity_id.as_deref()))
            .map(|t| {
                (
                    t.id.clone(),
                    LiveRef {
                        title: t.title.clone(),
                        status: task_status_label(&t.status),
                        exists: true,
                    },
                )
            })
            .collect()
    };
    let mission_live: std::collections::HashMap<String, LiveRef> = {
        let missions = control.missions.read().await;
        missions
            .iter()
            .filter(|m| mission_ids.contains(&m.id) && entity_visible(Some(&m.entity_id)))
            .map(|m| {
                (
                    m.id.clone(),
                    LiveRef {
                        title: m.title.clone(),
                        status: mission_status_label(&m.status),
                        exists: true,
                    },
                )
            })
            .collect()
    };

    let missing = || LiveRef {
        title: String::new(),
        status: "deleted".to_string(),
        exists: false,
    };
    let lookup = |map: &std::collections::HashMap<String, LiveRef>, rid: &str| {
        map.get(rid).cloned().unwrap_or_else(missing)
    };
    nodes
        .into_iter()
        .map(|node| {
            let live = match (node.kind.as_str(), node.ref_id.as_deref()) {
                ("task", Some(rid)) => Some(lookup(&task_live, rid)),
                ("mission", Some(rid)) => Some(lookup(&mission_live, rid)),
                ("entity", Some(rid)) => Some(lookup(&entity_live, rid)),
                _ => None,
            };
            NodeOut { node, live }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct CanvasQuery {
    entity_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CanvasResponse {
    entity_id: String,
    nodes: Vec<NodeOut>,
    edges: Vec<CanvasEdge>,
}

async fn get_canvas(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<CanvasQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let entity_id = resolve_space(&state, &workspace, q.entity_id.as_deref()).await?;
    let store = canvas_store(&state)?;
    let (nodes, edges) = store
        .list(&workspace.id, &entity_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let nodes = hydrate(&state, &workspace, nodes).await;
    Ok(Json(CanvasResponse {
        entity_id,
        nodes,
        edges,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateNodeRequest {
    entity_id: Option<String>,
    kind: String,
    ref_id: Option<String>,
    label: Option<String>,
    content: Option<String>,
    x: f64,
    y: f64,
    w: Option<f64>,
    h: Option<f64>,
    color: Option<String>,
}

// Live-kind nodes must name the record they mirror; static-kind nodes never do.
async fn create_node(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateNodeRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let kind = body.kind.trim().to_lowercase();
    if !LIVE_KINDS.contains(&kind.as_str()) && !STATIC_KINDS.contains(&kind.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if LIVE_KINDS.contains(&kind.as_str())
        && body
            .ref_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .is_none()
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let entity_id = resolve_space(&state, &workspace, body.entity_id.as_deref()).await?;
    let now = chrono::Utc::now().to_rfc3339();
    let node = CanvasNode {
        id: format!("cnode_{}", Uuid::new_v4().simple()),
        workspace_id: workspace.id.clone(),
        entity_id,
        kind,
        ref_id: body.ref_id,
        label: body.label,
        content: body.content,
        x: body.x,
        y: body.y,
        w: body.w,
        h: body.h,
        color: body.color,
        created_by: workspace.principal_id.clone(),
        created_at: now.clone(),
        updated_at: now,
    };
    let store = canvas_store(&state)?;
    store
        .upsert_node(&node)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut out = hydrate(&state, &workspace, vec![node]).await;
    Ok((StatusCode::CREATED, Json(out.remove(0))))
}

/// Loads a node and proves the viewer can see its entity space; the same
/// gate protects update and delete.
async fn load_visible_node(
    state: &ServerState,
    workspace: &WorkspaceCtx,
    id: &str,
) -> Result<CanvasNode, StatusCode> {
    let store = canvas_store(state)?;
    let node = store
        .get_node(id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if node.workspace_id != workspace.id {
        return Err(StatusCode::NOT_FOUND);
    }
    resolve_space(state, workspace, Some(&node.entity_id)).await?;
    Ok(node)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateNodeRequest {
    label: Option<String>,
    content: Option<String>,
    x: Option<f64>,
    y: Option<f64>,
    w: Option<f64>,
    h: Option<f64>,
    color: Option<String>,
}

async fn update_node(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<UpdateNodeRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let mut node = load_visible_node(&state, &workspace, &id).await?;
    if let Some(v) = body.label {
        node.label = Some(v);
    }
    if let Some(v) = body.content {
        node.content = Some(v);
    }
    if let Some(v) = body.x {
        node.x = v;
    }
    if let Some(v) = body.y {
        node.y = v;
    }
    if let Some(v) = body.w {
        node.w = Some(v);
    }
    if let Some(v) = body.h {
        node.h = Some(v);
    }
    if let Some(v) = body.color {
        node.color = Some(v);
    }
    node.updated_at = chrono::Utc::now().to_rfc3339();
    let store = canvas_store(&state)?;
    store
        .upsert_node(&node)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut out = hydrate(&state, &workspace, vec![node]).await;
    Ok(Json(out.remove(0)))
}

async fn delete_node(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    load_visible_node(&state, &workspace, &id).await?;
    let store = canvas_store(&state)?;
    store
        .delete_node(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateEdgeRequest {
    from_node: String,
    to_node: String,
    label: Option<String>,
}

async fn create_edge(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateEdgeRequest>,
) -> Result<impl IntoResponse, StatusCode> {
    let from = load_visible_node(&state, &workspace, &body.from_node).await?;
    let to = load_visible_node(&state, &workspace, &body.to_node).await?;
    if from.entity_id != to.entity_id {
        // Edges never cross entity spaces — that would leak one space's
        // structure into another.
        return Err(StatusCode::BAD_REQUEST);
    }
    let edge = CanvasEdge {
        id: format!("cedge_{}", Uuid::new_v4().simple()),
        workspace_id: workspace.id.clone(),
        entity_id: from.entity_id.clone(),
        from_node: from.id,
        to_node: to.id,
        label: body.label,
        created_by: workspace.principal_id.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    let store = canvas_store(&state)?;
    store
        .upsert_edge(&edge)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((StatusCode::CREATED, Json(edge)))
}

async fn delete_edge(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let store = canvas_store(&state)?;
    let edge = store
        .get_edge(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if edge.workspace_id != workspace.id {
        return Err(StatusCode::NOT_FOUND);
    }
    resolve_space(&state, &workspace, Some(&edge.entity_id)).await?;
    store
        .delete_edge(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}
