//! Surreal-backed store for the **entity ledger** (`entity_records` table).
//!
//! An entity record is one sourced fact about an entity's world: a customer
//! row from a CRM, a lead, a deal, a content item, a connected account, a
//! file, a metric snapshot, an event. Import used to end at narrative prose —
//! `description`, `profile`, `goals` — because that was the only place an
//! observed fact could go. This module gives it a structured home instead, so
//! the Architect's prompt assembly and Verify step have real rows to read and
//! resolve claims against, not just a paragraph someone wrote once.
//!
//! Records are always scoped by `entity_id`, exactly like `entity_docs`. What
//! is new here is the upsert key: [`record_key`] hashes `(entity_id, source,
//! kind, source_ref)`, so re-syncing the same source row — the same CRM
//! customer, the same Monday item — overwrites in place instead of piling up
//! duplicates. A caller never invents or tracks that id; it hands over
//! `source` + `source_ref` and gets idempotence for free.

use anyhow::Result;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::state::EntityRecord;

/// Deterministic row key for `(entity_id, source, kind, source_ref)`. Two
/// upserts with the same four values always land on the same row — that is
/// the entire idempotence guarantee bulk sync depends on. Hashed (rather than
/// joining the parts with a separator) so a `source_ref` containing `:` or
/// unicode can never collide with, or be misparsed against, another field.
pub fn record_key(entity_id: &str, source: &str, kind: &str, source_ref: &str) -> String {
    let mut h = Sha256::new();
    h.update(entity_id.as_bytes());
    h.update([0u8]);
    h.update(source.as_bytes());
    h.update([0u8]);
    h.update(kind.as_bytes());
    h.update([0u8]);
    h.update(source_ref.as_bytes());
    hex::encode(h.finalize())
}

/// Handle to the `entity_records` table.
pub struct SurrealEntityRecordsStore {
    db: Arc<Surreal<Any>>,
}

// Read-side shape: same Thing-vs-String workaround as `entity_docs_store::DocRow`.
#[derive(Deserialize)]
struct RecordRow {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

fn row_to_record(row: RecordRow) -> Result<EntityRecord, (String, serde_json::Error)> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    serde_json::from_value::<EntityRecord>(serde_json::Value::Object(map)).map_err(|e| (id, e))
}

fn rows_to_records(rows: Vec<RecordRow>) -> Vec<EntityRecord> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row_to_record(row) {
            Ok(r) => out.push(r),
            Err((id, err)) => {
                tracing::warn!(
                    record_id = %id,
                    error = %err,
                    "SurrealEntityRecordsStore: dropping malformed row"
                );
            }
        }
    }
    out
}

// Owned, id-less projection of `EntityRecord` used as the upsert body — same
// reason `entity_docs_store::DocUpsertBody` exists (Surreal sets `id` from the
// path argument, so a string `id` in the body would collide with it).
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RecordUpsertBody {
    entity_id: String,
    kind: String,
    source: String,
    source_ref: String,
    fields: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
    fetched_at: String,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
}

fn record_to_upsert_body(r: &EntityRecord) -> RecordUpsertBody {
    RecordUpsertBody {
        entity_id: r.entity_id.clone(),
        kind: r.kind.clone(),
        source: r.source.clone(),
        source_ref: r.source_ref.clone(),
        fields: r.fields.clone(),
        evidence: r.evidence.clone(),
        fetched_at: r.fetched_at.clone(),
        created_at: r.created_at.clone(),
        updated_at: r.updated_at.clone(),
        created_by: r.created_by.clone(),
    }
}

/// Compile-time guard: a new `EntityRecord` field forces an update to the
/// upsert body so it isn't silently dropped from persistence.
#[allow(dead_code)]
fn entity_record_field_completeness_guard(r: &EntityRecord) {
    let EntityRecord {
        id: _,
        entity_id: _,
        kind: _,
        source: _,
        source_ref: _,
        fields: _,
        evidence: _,
        fetched_at: _,
        created_at: _,
        updated_at: _,
        created_by: _,
    } = r;
}

impl SurrealEntityRecordsStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE entity_records SCHEMALESS;
                DEFINE FIELD entityId ON entity_records TYPE string;
                DEFINE FIELD kind ON entity_records TYPE string;
                DEFINE FIELD source ON entity_records TYPE string;
                DEFINE FIELD sourceRef ON entity_records TYPE string;
                DEFINE FIELD fetchedAt ON entity_records TYPE string;
                DEFINE FIELD updatedAt ON entity_records TYPE string;
                DEFINE INDEX entity_records_entity_idx ON entity_records FIELDS entityId;
                DEFINE INDEX entity_records_entity_kind_idx ON entity_records FIELDS entityId, kind;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealEntityRecordsStore schema warning: {}", err);
        }
        Ok(())
    }

    /// Create or overwrite one record. `record.id` must already be
    /// [`record_key`]'s output — callers building records from a sync should
    /// go through [`crate::control_api`]'s bulk-upsert handler rather than
    /// calling this directly, so `created_at` is preserved across re-syncs.
    pub async fn upsert_record(&self, record: &EntityRecord) -> Result<()> {
        let body = record_to_upsert_body(record);
        let _: Option<RecordRow> = self
            .db
            .upsert(("entity_records", record.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert entity record {}: {}", record.id, e))?;
        Ok(())
    }

    /// All records for an entity, most recently fetched first.
    pub async fn list_for_entity(&self, entity_id: &str) -> Result<Vec<EntityRecord>> {
        let eid = entity_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM entity_records WHERE entityId = $eid ORDER BY fetchedAt DESC")
            .bind(("eid", eid))
            .await
            .map_err(|e| anyhow::anyhow!("list entity records: {}", e))?;
        let rows: Vec<RecordRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take entity records: {}", e))?;
        Ok(rows_to_records(rows))
    }

    /// Records for an entity narrowed to one `kind`, most recently fetched
    /// first — what the Records tab shows per group and what a targeted
    /// Verify lookup ("how many open deals") reads.
    pub async fn list_for_entity_kind(
        &self,
        entity_id: &str,
        kind: &str,
    ) -> Result<Vec<EntityRecord>> {
        let eid = entity_id.to_string();
        let k = kind.to_string();
        let mut res = self
            .db
            .query(
                "SELECT * FROM entity_records WHERE entityId = $eid AND kind = $kind \
                 ORDER BY fetchedAt DESC",
            )
            .bind(("eid", eid))
            .bind(("kind", k))
            .await
            .map_err(|e| anyhow::anyhow!("list entity records by kind: {}", e))?;
        let rows: Vec<RecordRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take entity records by kind: {}", e))?;
        Ok(rows_to_records(rows))
    }

    pub async fn get(&self, id: &str) -> Result<Option<EntityRecord>> {
        let row: Option<RecordRow> = self
            .db
            .select(("entity_records", id))
            .await
            .map_err(|e| anyhow::anyhow!("get entity record {}: {}", id, e))?;
        Ok(row.and_then(|r| match row_to_record(r) {
            Ok(rec) => Some(rec),
            Err((id, err)) => {
                tracing::warn!(record_id = %id, error = %err, "SurrealEntityRecordsStore: dropping malformed row");
                None
            }
        }))
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        let _: Option<RecordRow> = self
            .db
            .delete(("entity_records", id))
            .await
            .map_err(|e| anyhow::anyhow!("delete entity record {}: {}", id, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record(
        entity: &str,
        kind: &str,
        source: &str,
        source_ref: &str,
        fetched_at: &str,
        fields: serde_json::Value,
    ) -> EntityRecord {
        EntityRecord {
            id: record_key(entity, source, kind, source_ref),
            entity_id: entity.to_string(),
            kind: kind.to_string(),
            source: source.to_string(),
            source_ref: source_ref.to_string(),
            fields,
            evidence: None,
            fetched_at: fetched_at.to_string(),
            created_at: fetched_at.to_string(),
            updated_at: fetched_at.to_string(),
            created_by: None,
        }
    }

    #[test]
    fn record_key_is_stable_and_source_scoped() {
        let a = record_key("ent_qsme", "baqshi_crm", "customer", "cust_1");
        let b = record_key("ent_qsme", "baqshi_crm", "customer", "cust_1");
        assert_eq!(a, b, "same inputs, same key");

        let different_ref = record_key("ent_qsme", "baqshi_crm", "customer", "cust_2");
        assert_ne!(a, different_ref);

        let different_source = record_key("ent_qsme", "monday", "customer", "cust_1");
        assert_ne!(a, different_source);

        // A naive `format!("{a}{b}{c}{d}")` join would let ("ab", "c") collide
        // with ("a", "bc"); the hashed, separator-delimited key must not.
        let joined_ambiguous_a = record_key("ent", "ab", "kind", "c");
        let joined_ambiguous_b = record_key("ent", "a", "kind", "bc");
        assert_ne!(joined_ambiguous_a, joined_ambiguous_b);
    }

    #[test]
    fn upsert_body_strips_id_and_keeps_entity_id() {
        let r = sample_record(
            "ent_qsme",
            "customer",
            "baqshi_crm",
            "cust_1",
            "2026-09-08T00:00:00Z",
            serde_json::json!({"name": "Acme"}),
        );
        let body = record_to_upsert_body(&r);
        let v = serde_json::to_value(&body).expect("serialise");
        let obj = v.as_object().expect("object");
        assert!(!obj.contains_key("id"));
        assert_eq!(
            obj.get("entityId").and_then(|v| v.as_str()),
            Some("ent_qsme")
        );
    }

    #[tokio::test]
    async fn upsert_by_source_ref_overwrites_instead_of_duplicating() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealEntityRecordsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let first = sample_record(
            "ent_qsme",
            "customer",
            "baqshi_crm",
            "cust_1",
            "2026-09-01T00:00:00Z",
            serde_json::json!({"name": "Acme", "stage": "lead"}),
        );
        store.upsert_record(&first).await.expect("first upsert");

        // A re-sync a week later: same source_ref, new fields, later fetchedAt.
        let resynced = sample_record(
            "ent_qsme",
            "customer",
            "baqshi_crm",
            "cust_1",
            "2026-09-08T00:00:00Z",
            serde_json::json!({"name": "Acme", "stage": "customer"}),
        );
        store.upsert_record(&resynced).await.expect("resync");

        let all = store.list_for_entity("ent_qsme").await.expect("list");
        assert_eq!(all.len(), 1, "same source_ref must overwrite, not duplicate");
        assert_eq!(all[0].fields["stage"], "customer");
        assert_eq!(all[0].fetched_at, "2026-09-08T00:00:00Z");
    }

    #[tokio::test]
    async fn list_filters_by_entity_and_kind() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealEntityRecordsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let customer = sample_record(
            "ent_qsme",
            "customer",
            "baqshi_crm",
            "cust_1",
            "2026-09-08T00:00:00Z",
            serde_json::json!({}),
        );
        let deal = sample_record(
            "ent_qsme",
            "deal",
            "baqshi_crm",
            "deal_1",
            "2026-09-07T00:00:00Z",
            serde_json::json!({}),
        );
        let other_entity = sample_record(
            "ent_towme",
            "customer",
            "baqshi_crm",
            "cust_1",
            "2026-09-06T00:00:00Z",
            serde_json::json!({}),
        );
        for r in [&customer, &deal, &other_entity] {
            store.upsert_record(r).await.expect("upsert");
        }

        let all = store.list_for_entity("ent_qsme").await.expect("list");
        assert_eq!(all.len(), 2, "no cross-entity leakage");

        let customers = store
            .list_for_entity_kind("ent_qsme", "customer")
            .await
            .expect("list by kind");
        assert_eq!(customers.len(), 1);
        assert_eq!(customers[0].source_ref, "cust_1");
    }

    #[tokio::test]
    async fn get_and_delete_roundtrip() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealEntityRecordsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let r = sample_record(
            "ent_qsme",
            "customer",
            "baqshi_crm",
            "cust_1",
            "2026-09-08T00:00:00Z",
            serde_json::json!({"name": "Acme"}),
        );
        store.upsert_record(&r).await.expect("upsert");

        let got = store.get(&r.id).await.expect("get").expect("present");
        assert_eq!(got.fields["name"], "Acme");

        store.delete(&r.id).await.expect("delete");
        assert!(store.get(&r.id).await.expect("get2").is_none());
    }
}
