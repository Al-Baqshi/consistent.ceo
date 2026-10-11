//! AUD — append-only audit trail. Every Architect step writes one
//! [`AuditEvent`] regardless of outcome so the trail is uniform and Phase 1
//! shadow runs can be diffed against the legacy chat path turn-by-turn.
//!
//! Schema is created idempotently; failure to init falls back to a no-op
//! writer so a degraded DB never blocks chat.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

/// One row of the audit trail: which step ran, for which turn/entity, and
/// what happened. Written for every step regardless of outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    pub turn_id: String,
    pub trigger: String,
    pub workspace_id: Option<String>,
    pub entity_id: Option<String>,
    pub step: String,
    pub status: String,
    pub payload: serde_json::Value,
    pub occurred_at: String,
}

/// Sink for audit events. Implementations range from a no-op (DB down) to a
/// Surreal-backed store; callers never need to know which is in play.
#[async_trait]
pub trait AuditWriter: Send + Sync {
    async fn append(&self, event: AuditEvent);
    /// Read the most recent `limit` rows, newest first. Used by
    /// `/api/audit/recent` and by tests.
    async fn recent(&self, limit: usize) -> Result<Vec<AuditEvent>>;

    /// Every row for one turn, oldest first — the pipeline in the order it ran.
    ///
    /// `recent` interleaves every concurrent turn, which makes inspecting a
    /// single run impractical once there is any traffic at all. Defaulted to
    /// empty so a writer that cannot index by turn (the no-op) still compiles.
    async fn for_turn(&self, _turn_id: &str) -> Result<Vec<AuditEvent>> {
        Ok(Vec::new())
    }
}

/// Discards every event. Used when the DB is unavailable so the architect
/// can still run without throwing.
#[derive(Default)]
pub struct NoopAuditWriter;

#[async_trait]
impl AuditWriter for NoopAuditWriter {
    async fn append(&self, _event: AuditEvent) {}
    async fn recent(&self, _limit: usize) -> Result<Vec<AuditEvent>> {
        Ok(Vec::new())
    }
}

/// Surreal-backed [`AuditWriter`] — the durable implementation used once the
/// DB is reachable.
pub struct SurrealAuditStore {
    db: Arc<Surreal<Any>>,
}

/// Read shape: includes the Surreal record id that [`AuditUpsertBody`] omits
/// on write.
#[derive(Deserialize)]
struct AuditRow {
    #[allow(dead_code)]
    id: surrealdb::RecordId,
    turn_id: String,
    trigger: String,
    workspace_id: Option<String>,
    entity_id: Option<String>,
    step: String,
    status: String,
    payload: serde_json::Value,
    occurred_at: String,
}

/// Write shape — same fields as [`AuditEvent`] minus the id Surreal assigns.
#[derive(Serialize)]
struct AuditUpsertBody {
    turn_id: String,
    trigger: String,
    workspace_id: Option<String>,
    entity_id: Option<String>,
    step: String,
    status: String,
    payload: serde_json::Value,
    occurred_at: String,
}

impl SurrealAuditStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Creates the `audit_trail` table, fields, and indexes if they don't
    /// already exist; safe to call on every startup.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE audit_trail SCHEMALESS;
                DEFINE FIELD turn_id ON audit_trail TYPE string;
                DEFINE FIELD trigger ON audit_trail TYPE string;
                DEFINE FIELD workspace_id ON audit_trail TYPE option<string>;
                DEFINE FIELD entity_id ON audit_trail TYPE option<string>;
                DEFINE FIELD step ON audit_trail TYPE string;
                DEFINE FIELD status ON audit_trail TYPE string;
                DEFINE FIELD payload ON audit_trail TYPE object;
                DEFINE FIELD occurred_at ON audit_trail TYPE string;
                DEFINE INDEX audit_turn_idx ON audit_trail FIELDS turn_id;
                DEFINE INDEX audit_time_idx ON audit_trail FIELDS occurred_at;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealAuditStore schema warning: {}", err);
        }
        Ok(())
    }
}

#[async_trait]
impl AuditWriter for SurrealAuditStore {
    async fn append(&self, event: AuditEvent) {
        // Append-only: every call gets a fresh row id rather than keying on
        // turn_id, so multiple steps for the same turn all persist.
        let row_id = uuid::Uuid::new_v4().to_string();
        let turn_id_for_log = event.turn_id.clone();
        let step_for_log = event.step.clone();
        let body = AuditUpsertBody {
            turn_id: event.turn_id,
            trigger: event.trigger,
            workspace_id: event.workspace_id,
            entity_id: event.entity_id,
            step: event.step,
            status: event.status,
            payload: event.payload,
            occurred_at: event.occurred_at,
        };
        let res: Result<Option<AuditRow>, _> =
            self.db.upsert(("audit_trail", row_id)).content(body).await;
        // Swallow write failures — a degraded audit DB must never fail the
        // caller's turn, just get logged.
        if let Err(e) = res {
            tracing::warn!(turn_id = %turn_id_for_log, step = %step_for_log, error = %e, "audit append failed");
        }
    }

    async fn for_turn(&self, turn_id: &str) -> Result<Vec<AuditEvent>> {
        // Ascending: a trail is read in the order the stages ran. This is what
        // `audit_turn_idx` was defined for and had no caller until now.
        let mut res = self
            .db
            .query("SELECT * FROM audit_trail WHERE turn_id = $t ORDER BY occurred_at ASC")
            .bind(("t", turn_id.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("audit for_turn query: {}", e))?;
        let rows: Vec<AuditRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take audit rows: {}", e))?;
        Ok(rows
            .into_iter()
            .map(|r| AuditEvent {
                turn_id: r.turn_id,
                trigger: r.trigger,
                workspace_id: r.workspace_id,
                entity_id: r.entity_id,
                step: r.step,
                status: r.status,
                payload: r.payload,
                occurred_at: r.occurred_at,
            })
            .collect())
    }

    async fn recent(&self, limit: usize) -> Result<Vec<AuditEvent>> {
        let mut res = self
            .db
            .query("SELECT * FROM audit_trail ORDER BY occurred_at DESC LIMIT $k")
            .bind(("k", limit as i64))
            .await
            .map_err(|e| anyhow::anyhow!("recent audit query: {}", e))?;
        let rows: Vec<AuditRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take audit rows: {}", e))?;
        Ok(rows
            .into_iter()
            .map(|r| AuditEvent {
                turn_id: r.turn_id,
                trigger: r.trigger,
                workspace_id: r.workspace_id,
                entity_id: r.entity_id,
                step: r.step,
                status: r.status,
                payload: r.payload,
                occurred_at: r.occurred_at,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_writer_round_trip() {
        let w = NoopAuditWriter;
        w.append(AuditEvent {
            turn_id: "t1".into(),
            trigger: "chat".into(),
            workspace_id: None,
            entity_id: None,
            step: "inspect".into(),
            status: "ok".into(),
            payload: serde_json::json!({}),
            occurred_at: "2026-05-24T00:00:00Z".into(),
        })
        .await;
        assert!(w.recent(10).await.expect("ok").is_empty());
    }

    #[tokio::test]
    async fn surreal_writer_appends_and_reads() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealAuditStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        store
            .append(AuditEvent {
                turn_id: "t1".into(),
                trigger: "chat".into(),
                workspace_id: Some("ws".into()),
                entity_id: Some("baqshi".into()),
                step: "inspect".into(),
                status: "ok".into(),
                payload: serde_json::json!({ "mem": 3 }),
                occurred_at: "2026-05-24T00:00:00Z".into(),
            })
            .await;
        let rows = store.recent(10).await.expect("recent");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].turn_id, "t1");
        assert_eq!(rows[0].entity_id.as_deref(), Some("baqshi"));
    }
}
