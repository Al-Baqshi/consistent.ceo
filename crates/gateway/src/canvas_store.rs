//! Surreal-backed persistence for the live canvas (spatial brain map).
//!
//! Canvas nodes are spatial objects owned by an entity space (a business
//! entity or a user's Personal Space). Unlike a drawing tool, nodes can
//! *mirror live records* — a node with `kind = "task"` and a `ref_id` renders
//! the current state of that task, not a snapshot. Edges connect two nodes
//! within the same entity space.
//!
//! Privacy falls out of entity visibility: the API layer resolves the owning
//! entity through `is_entity_visible`, so Personal Space canvases are only
//! ever readable/writable by their owner.
//!
//! Record-id handling follows [`crate::entity_store`]: Surreal v2 returns the
//! primary key as a `RecordId`, so rows are read through a `{ id: RecordId,
//! flatten rest }` projection and the string id is re-injected, and writes go
//! through id-less body structs so the path argument is the single source of
//! the record key.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

pub const NODES_TABLE: &str = "canvas_nodes";
pub const EDGES_TABLE: &str = "canvas_edges";

/// Node kinds that mirror a live record via `ref_id`.
pub const LIVE_KINDS: [&str; 3] = ["task", "mission", "entity"];
/// Free-form node kinds whose content lives on the node itself.
pub const STATIC_KINDS: [&str; 4] = ["text", "note", "link", "shape"];

/// One spatial object on a canvas — either free-form content or a mirror of a
/// live task/mission/entity record (see [`LIVE_KINDS`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanvasNode {
    pub id: String,
    pub workspace_id: String,
    /// Owning space: a business entity id or the viewer's Personal Space id.
    pub entity_id: String,
    /// One of [`LIVE_KINDS`] or [`STATIC_KINDS`].
    pub kind: String,
    /// Id of the live record this node mirrors (task/mission/entity kinds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub x: f64,
    pub y: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub w: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub h: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A connection between two nodes in the same entity space.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CanvasEdge {
    pub id: String,
    pub workspace_id: String,
    pub entity_id: String,
    /// Source node id.
    pub from_node: String,
    /// Target node id.
    pub to_node: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

/// Row projection for reads: Surreal v2 returns the key as a `RecordId`, not
/// a string, so we capture it separately and re-inject it into the flattened
/// body before deserialising into the public structs.
#[derive(Deserialize)]
struct Row {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

// Strips the Surreal `table:` prefix (and any backticks) off a RecordId's
// string form, leaving just the key.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

// Re-injects the string id into the flattened row before deserialising into T.
fn row_to<T: serde::de::DeserializeOwned>(row: Row) -> Result<T> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id));
    Ok(serde_json::from_value::<T>(serde_json::Value::Object(map))?)
}

// Batch version of `row_to`; a malformed row is logged and dropped rather
// than failing the whole list.
fn rows_to<T: serde::de::DeserializeOwned>(rows: Vec<Row>, what: &str) -> Vec<T> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let key = record_id_key(&row.id);
        match row_to::<T>(row) {
            Ok(v) => out.push(v),
            Err(e) => {
                tracing::warn!(id = %key, error = %e, "CanvasStore: dropping malformed {what} row")
            }
        }
    }
    out
}

/// Id-less write body for nodes — the Surreal path argument is the single
/// source of the record key (see module docs).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeBody {
    workspace_id: String,
    entity_id: String,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ref_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    x: f64,
    y: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    w: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    h: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    color: Option<String>,
    created_by: String,
    created_at: String,
    updated_at: String,
}

fn node_body(n: &CanvasNode) -> NodeBody {
    NodeBody {
        workspace_id: n.workspace_id.clone(),
        entity_id: n.entity_id.clone(),
        kind: n.kind.clone(),
        ref_id: n.ref_id.clone(),
        label: n.label.clone(),
        content: n.content.clone(),
        x: n.x,
        y: n.y,
        w: n.w,
        h: n.h,
        color: n.color.clone(),
        created_by: n.created_by.clone(),
        created_at: n.created_at.clone(),
        updated_at: n.updated_at.clone(),
    }
}

// Id-less write body for edges, mirroring `NodeBody`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EdgeBody {
    workspace_id: String,
    entity_id: String,
    from_node: String,
    to_node: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    created_by: String,
    created_at: String,
}

fn edge_body(e: &CanvasEdge) -> EdgeBody {
    EdgeBody {
        workspace_id: e.workspace_id.clone(),
        entity_id: e.entity_id.clone(),
        from_node: e.from_node.clone(),
        to_node: e.to_node.clone(),
        label: e.label.clone(),
        created_by: e.created_by.clone(),
        created_at: e.created_at.clone(),
    }
}

/// Surreal-backed handle for canvas nodes and edges.
pub struct CanvasStore {
    db: Arc<Surreal<Any>>,
}

impl CanvasStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Defines the schemaless tables plus the workspace/entity indexes used by
    /// every scoped query below.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE canvas_nodes SCHEMALESS;
                DEFINE FIELD workspaceId ON canvas_nodes TYPE option<string>;
                DEFINE FIELD entityId ON canvas_nodes TYPE option<string>;
                DEFINE INDEX canvas_nodes_ws_idx ON canvas_nodes FIELDS workspaceId;
                DEFINE INDEX canvas_nodes_entity_idx ON canvas_nodes FIELDS entityId;

                DEFINE TABLE canvas_edges SCHEMALESS;
                DEFINE FIELD workspaceId ON canvas_edges TYPE option<string>;
                DEFINE FIELD entityId ON canvas_edges TYPE option<string>;
                DEFINE INDEX canvas_edges_ws_idx ON canvas_edges FIELDS workspaceId;
                DEFINE INDEX canvas_edges_entity_idx ON canvas_edges FIELDS entityId;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("CanvasStore schema warning: {}", err);
        }
        Ok(())
    }

    /// All nodes and edges in one entity space, scoped by workspace.
    pub async fn list(
        &self,
        workspace_id: &str,
        entity_id: &str,
    ) -> Result<(Vec<CanvasNode>, Vec<CanvasEdge>)> {
        let mut res = self
            .db
            .query("SELECT * FROM canvas_nodes WHERE workspaceId = $ws AND entityId = $ent")
            .query("SELECT * FROM canvas_edges WHERE workspaceId = $ws AND entityId = $ent")
            .bind(("ws", workspace_id.to_string()))
            .bind(("ent", entity_id.to_string()))
            .await?;
        let node_rows: Vec<Row> = res.take(0)?;
        let edge_rows: Vec<Row> = res.take(1)?;
        Ok((
            rows_to::<CanvasNode>(node_rows, "node"),
            rows_to::<CanvasEdge>(edge_rows, "edge"),
        ))
    }

    pub async fn get_node(&self, id: &str) -> Result<Option<CanvasNode>> {
        let row: Option<Row> = self.db.select((NODES_TABLE, id)).await?;
        row.map(row_to::<CanvasNode>).transpose()
    }

    pub async fn upsert_node(&self, node: &CanvasNode) -> Result<()> {
        let _: Option<Row> = self
            .db
            .upsert((NODES_TABLE, node.id.as_str()))
            .content(node_body(node))
            .await?;
        Ok(())
    }

    /// Deletes a node and every edge touching it.
    pub async fn delete_node(&self, id: &str) -> Result<()> {
        let _: Option<Row> = self.db.delete((NODES_TABLE, id)).await?;
        let mut res = self
            .db
            .query("DELETE FROM canvas_edges WHERE fromNode = $node OR toNode = $node")
            .bind(("node", id.to_string()))
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("CanvasStore edge cascade warning: {}", err);
        }
        Ok(())
    }

    pub async fn get_edge(&self, id: &str) -> Result<Option<CanvasEdge>> {
        let row: Option<Row> = self.db.select((EDGES_TABLE, id)).await?;
        row.map(row_to::<CanvasEdge>).transpose()
    }

    pub async fn upsert_edge(&self, edge: &CanvasEdge) -> Result<()> {
        let _: Option<Row> = self
            .db
            .upsert((EDGES_TABLE, edge.id.as_str()))
            .content(edge_body(edge))
            .await?;
        Ok(())
    }

    pub async fn delete_edge(&self, id: &str) -> Result<()> {
        let _: Option<Row> = self.db.delete((EDGES_TABLE, id)).await?;
        Ok(())
    }

    /// Drop an entity's whole canvas (nodes and edges). Part of the
    /// entity-delete cascade.
    pub async fn delete_for_entity(&self, entity_id: &str) -> Result<()> {
        let mut res = self
            .db
            .query("DELETE FROM canvas_nodes WHERE entityId = $ent")
            .query("DELETE FROM canvas_edges WHERE entityId = $ent")
            .bind(("ent", entity_id.to_string()))
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("CanvasStore entity cascade warning: {}", err);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_node(id: &str, entity_id: &str) -> CanvasNode {
        CanvasNode {
            id: id.to_string(),
            workspace_id: "default".to_string(),
            entity_id: entity_id.to_string(),
            kind: "note".to_string(),
            ref_id: None,
            label: Some("hello".to_string()),
            content: None,
            x: 10.0,
            y: 20.0,
            w: None,
            h: None,
            color: None,
            created_by: "user-1".to_string(),
            created_at: "2026-07-11T00:00:00Z".to_string(),
            updated_at: "2026-07-11T00:00:00Z".to_string(),
        }
    }

    async fn memory_store() -> Arc<CanvasStore> {
        let db = surrealdb::engine::any::connect("mem://").await.unwrap();
        db.use_ns("test").use_db("test").await.unwrap();
        let store = CanvasStore::new(Arc::new(db));
        store.init_schema().await.unwrap();
        store
    }

    #[tokio::test]
    async fn node_roundtrip_and_entity_scoping() {
        let store = memory_store().await;
        store
            .upsert_node(&sample_node("n1", "ent-a"))
            .await
            .unwrap();
        store
            .upsert_node(&sample_node("n2", "ent-b"))
            .await
            .unwrap();

        let (nodes, edges) = store.list("default", "ent-a").await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, "n1");
        assert_eq!(nodes[0].label.as_deref(), Some("hello"));
        assert!(edges.is_empty());

        let fetched = store.get_node("n1").await.unwrap().unwrap();
        assert_eq!(fetched.entity_id, "ent-a");
        assert_eq!(fetched.x, 10.0);
    }

    #[tokio::test]
    async fn deleting_node_cascades_edges() {
        let store = memory_store().await;
        store
            .upsert_node(&sample_node("n1", "ent-a"))
            .await
            .unwrap();
        store
            .upsert_node(&sample_node("n2", "ent-a"))
            .await
            .unwrap();
        store
            .upsert_edge(&CanvasEdge {
                id: "e1".to_string(),
                workspace_id: "default".to_string(),
                entity_id: "ent-a".to_string(),
                from_node: "n1".to_string(),
                to_node: "n2".to_string(),
                label: None,
                created_by: "user-1".to_string(),
                created_at: "2026-07-11T00:00:00Z".to_string(),
            })
            .await
            .unwrap();

        let (nodes, edges) = store.list("default", "ent-a").await.unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(edges.len(), 1);

        store.delete_node("n1").await.unwrap();
        let (nodes, edges) = store.list("default", "ent-a").await.unwrap();
        assert_eq!(nodes.len(), 1);
        assert!(
            edges.is_empty(),
            "edges touching a deleted node must go too"
        );
    }
}
