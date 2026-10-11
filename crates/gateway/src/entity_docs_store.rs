//! Surreal-backed store for **entity docs** (`entity_docs` table).
//!
//! An entity doc is a Cursor-style reference document attached to an entity:
//! stack notes, architecture, conventions, an API's docs snapshot, etc. Each
//! entity owns its own stack/architecture, so docs are entity-scoped. Enabled
//! docs are eagerly spliced into the canonical system prompt (between the entity
//! block and the mission block) so the model always sees the entity's ground
//! truth instead of guessing — see [`crate::prompt_assembly`].
//!
//! Recall is always filtered by `entity_id` so one entity's docs can never leak
//! into another's prompt. The `embedding` field is persisted for a future
//! semantic `@`-reference path (dim 1536 cosine, matches `memories` /
//! `director_lessons`) but the v1 injection path is purely the eager
//! enabled-docs list.

use anyhow::Result;
use serde::Deserialize;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::state::EntityDoc;

/// Vector length for `embedding`, matching the `memories` / `director_lessons`
/// tables so a future semantic recall path can share tooling.
pub const DOC_EMBEDDING_DIMENSION: usize = 1536;

/// Handle to the `entity_docs` table.
pub struct SurrealEntityDocsStore {
    db: Arc<Surreal<Any>>,
}

// Read-side shape: same Thing-vs-String workaround as `entity_store::EntityRow`.
#[derive(Deserialize)]
struct DocRow {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

/// Extract the key portion of a Surreal `RecordId`, e.g. `entity_docs:doc_1`
/// -> `doc_1`.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

/// Convert a Surreal-shaped row into an `EntityDoc` by re-injecting the
/// record-id key as the `id` string.
fn row_to_doc(row: DocRow) -> Result<EntityDoc, (String, serde_json::Error)> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    // `state.rs` renames fields camelCase; the row was written camelCase too, so
    // this round-trips cleanly.
    serde_json::from_value::<EntityDoc>(serde_json::Value::Object(map)).map_err(|e| (id, e))
}

// Owned, id-less projection of `EntityDoc` used as the upsert body, for the
// same reason `entity_store::EntityUpsertBody` exists (Surreal sets `id` from
// the path argument, so a string `id` in the body would collide with it).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocUpsertBody {
    entity_id: String,
    title: String,
    content: String,
    source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    embedding: Option<Vec<f32>>,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
}

fn doc_to_upsert_body(d: &EntityDoc) -> DocUpsertBody {
    DocUpsertBody {
        entity_id: d.entity_id.clone(),
        title: d.title.clone(),
        content: d.content.clone(),
        source: d.source.clone(),
        url: d.url.clone(),
        enabled: d.enabled,
        embedding: d.embedding.clone(),
        created_at: d.created_at.clone(),
        updated_at: d.updated_at.clone(),
        created_by: d.created_by.clone(),
    }
}

/// Compile-time guard: a new `EntityDoc` field forces an update to the upsert
/// body so it isn't silently dropped from persistence.
#[allow(dead_code)]
fn entity_doc_field_completeness_guard(d: &EntityDoc) {
    let EntityDoc {
        id: _,
        entity_id: _,
        title: _,
        content: _,
        source: _,
        url: _,
        enabled: _,
        embedding: _,
        created_at: _,
        updated_at: _,
        created_by: _,
    } = d;
}

impl SurrealEntityDocsStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    // Defines the entity-scoped index plus the HNSW vector index that will
    // back the future semantic `@`-reference recall path.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE entity_docs SCHEMALESS;
                DEFINE FIELD entityId ON entity_docs TYPE string;
                DEFINE FIELD enabled ON entity_docs TYPE bool;
                DEFINE FIELD embedding ON entity_docs TYPE option<array<float>>;
                DEFINE FIELD createdAt ON entity_docs TYPE string;
                DEFINE INDEX entity_docs_entity_idx ON entity_docs FIELDS entityId;
                DEFINE INDEX entity_docs_embed_idx ON entity_docs FIELDS embedding HNSW DIMENSION 1536 DIST COSINE;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealEntityDocsStore schema warning: {}", err);
        }
        Ok(())
    }

    /// Create or update a doc. The `id` on `doc` is the row key — caller
    /// generates it (uuid, etc.). Upsert semantics so retries are safe.
    pub async fn upsert_doc(&self, doc: &EntityDoc) -> Result<()> {
        let body = doc_to_upsert_body(doc);
        let _: Option<DocRow> = self
            .db
            .upsert(("entity_docs", doc.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert entity doc {}: {}", doc.id, e))?;
        Ok(())
    }

    /// All docs for an entity, newest first.
    pub async fn list_for_entity(&self, entity_id: &str) -> Result<Vec<EntityDoc>> {
        let eid = entity_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM entity_docs WHERE entityId = $eid ORDER BY createdAt DESC")
            .bind(("eid", eid))
            .await
            .map_err(|e| anyhow::anyhow!("list entity docs: {}", e))?;
        let rows: Vec<DocRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take entity docs: {}", e))?;
        Ok(rows_to_docs(rows))
    }

    /// Only the **enabled** docs for an entity, newest first — exactly what the
    /// prompt-assembly pipeline eagerly injects.
    pub async fn list_enabled_for_entity(&self, entity_id: &str) -> Result<Vec<EntityDoc>> {
        let eid = entity_id.to_string();
        let mut res = self
            .db
            .query(
                "SELECT * FROM entity_docs WHERE entityId = $eid AND enabled = true \
                 ORDER BY createdAt DESC",
            )
            .bind(("eid", eid))
            .await
            .map_err(|e| anyhow::anyhow!("list enabled entity docs: {}", e))?;
        let rows: Vec<DocRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take enabled entity docs: {}", e))?;
        Ok(rows_to_docs(rows))
    }

    pub async fn get(&self, id: &str) -> Result<Option<EntityDoc>> {
        let row: Option<DocRow> = self
            .db
            .select(("entity_docs", id))
            .await
            .map_err(|e| anyhow::anyhow!("get entity doc {}: {}", id, e))?;
        Ok(row.and_then(|r| match row_to_doc(r) {
            Ok(d) => Some(d),
            Err((id, err)) => {
                tracing::warn!(doc_id = %id, error = %err, "SurrealEntityDocsStore: dropping malformed row");
                None
            }
        }))
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let _: Option<DocRow> = self
            .db
            .delete(("entity_docs", id))
            .await
            .map_err(|e| anyhow::anyhow!("delete entity doc {}: {}", id, e))?;
        Ok(())
    }
}

fn rows_to_docs(rows: Vec<DocRow>) -> Vec<EntityDoc> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row_to_doc(row) {
            Ok(d) => out.push(d),
            Err((id, err)) => {
                tracing::warn!(
                    doc_id = %id,
                    error = %err,
                    "SurrealEntityDocsStore: dropping malformed row"
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_doc(
        id: &str,
        entity: &str,
        title: &str,
        created_at: &str,
        enabled: bool,
    ) -> EntityDoc {
        EntityDoc {
            id: id.to_string(),
            entity_id: entity.to_string(),
            title: title.to_string(),
            content: format!("body of {title}"),
            source: "manual".to_string(),
            url: None,
            enabled,
            embedding: None,
            created_at: created_at.to_string(),
            updated_at: created_at.to_string(),
            created_by: Some("carlos".to_string()),
        }
    }

    #[test]
    fn upsert_body_strips_id_and_keeps_entity_id() {
        let d = sample_doc("doc_1", "ent_qsme", "Stack", "2026-06-07T00:00:00Z", true);
        let body = doc_to_upsert_body(&d);
        let v = serde_json::to_value(&body).expect("serialise");
        let obj = v.as_object().expect("object");
        assert!(!obj.contains_key("id"));
        assert_eq!(
            obj.get("entityId").and_then(|v| v.as_str()),
            Some("ent_qsme")
        );
    }

    #[tokio::test]
    async fn list_filters_by_entity_and_enabled() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealEntityDocsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");
        store.init_schema().await.expect("idempotent");

        let d_old = sample_doc("doc_old", "ent_qsme", "old", "2026-06-01T00:00:00Z", true);
        let d_new = sample_doc("doc_new", "ent_qsme", "new", "2026-06-05T00:00:00Z", true);
        let d_off = sample_doc(
            "doc_off",
            "ent_qsme",
            "disabled",
            "2026-06-06T00:00:00Z",
            false,
        );
        let d_other = sample_doc(
            "doc_other",
            "ent_towme",
            "other",
            "2026-06-07T00:00:00Z",
            true,
        );
        for d in [&d_old, &d_new, &d_off, &d_other] {
            store.upsert_doc(d).await.expect("upsert");
        }

        let all = store.list_for_entity("ent_qsme").await.expect("list");
        let ids: Vec<_> = all.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["doc_off", "doc_new", "doc_old"],
            "newest first, no other-entity leakage"
        );

        let enabled = store
            .list_enabled_for_entity("ent_qsme")
            .await
            .expect("enabled");
        let enabled_ids: Vec<_> = enabled.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            enabled_ids,
            vec!["doc_new", "doc_old"],
            "disabled doc excluded"
        );

        // No cross-entity leakage.
        for d in &all {
            assert_ne!(d.entity_id, "ent_towme");
        }
    }

    #[tokio::test]
    async fn get_and_delete_roundtrip() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealEntityDocsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let d = sample_doc("doc_1", "ent_qsme", "Stack", "2026-06-07T00:00:00Z", true);
        store.upsert_doc(&d).await.expect("upsert");
        let got = store.get("doc_1").await.expect("get").expect("present");
        assert_eq!(got.title, "Stack");
        assert_eq!(got.entity_id, "ent_qsme");

        store.delete("doc_1").await.expect("delete");
        assert!(store.get("doc_1").await.expect("get2").is_none());
    }
}
