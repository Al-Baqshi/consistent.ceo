//! Surreal-backed log of heartbeat outcomes (Phase 9).
//!
//! When the [`crate::heartbeat_store::HeartbeatTicker`] fires a due
//! [`crate::state::Heartbeat`], the configured dispatcher (see
//! [`crate::heartbeat_dispatcher::ChatHeartbeatDispatcher`]) produces a
//! one-paragraph summary for the assigned director and persists a row here.
//!
//! The web chat polls `/api/entities/{id}/heartbeat-results` and renders these
//! rows as distinct "heartbeat" bubbles in the active entity's chat surface.
//! Keeping the results in their own table avoids coupling to the still-fluid
//! `{channel}-{peer_id}` session id scheme — a single entity may be visited
//! through many ad-hoc UUID sessions on the web, but heartbeat results belong
//! to the entity, not to any one of those sessions.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

/// One recorded outcome of a fired heartbeat — the director's summary plus
/// where and when it ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatResult {
    pub id: String,
    pub heartbeat_id: String,
    pub workspace_id: Option<String>,
    pub entity_id: Option<String>,
    pub director_id: String,
    pub fired_at: String,
    pub summary: String,
    pub created_at: String,
}

#[derive(Deserialize)]
struct ResultRow {
    id: surrealdb::RecordId,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}

// Surreal's RecordId prints as `table:id`; strip the table prefix and any
// backtick quoting to get back the bare id used elsewhere.
fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

// Reassemble a plain id string onto the row's fields, then decode as a
// HeartbeatResult, dropping the row silently on decode failure.
fn row_to_result(row: ResultRow) -> Option<HeartbeatResult> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id));
    serde_json::from_value(serde_json::Value::Object(map)).ok()
}

// Write-side projection of HeartbeatResult: same fields, minus `id` (that's
// supplied separately as the record key).
#[derive(Serialize)]
struct UpsertBody {
    heartbeat_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entity_id: Option<String>,
    director_id: String,
    fired_at: String,
    summary: String,
    created_at: String,
}

/// Where fired heartbeat outcomes are logged and read back for chat + reviews.
#[async_trait]
pub trait HeartbeatResultsStore: Send + Sync {
    /// Persist one heartbeat outcome.
    async fn record(&self, result: &HeartbeatResult) -> Result<()>;
    /// Beats for one entity, newest first.
    async fn list_for_entity(&self, entity_id: &str, limit: usize) -> Result<Vec<HeartbeatResult>>;
    /// Newest beats across every entity. The daily review aggregates beats the
    /// operator hasn't opened yet, which is cross-entity by nature; callers
    /// filter to their workspace afterwards.
    async fn list_recent(&self, limit: usize) -> Result<Vec<HeartbeatResult>>;
    /// Drop every beat log for an entity. Part of the entity-delete cascade.
    async fn delete_for_entity(&self, entity_id: &str) -> Result<()>;
}

/// Heartbeat-result persistence, backed by Surreal.
pub struct SurrealHeartbeatResultsStore {
    db: Arc<Surreal<Any>>,
}

impl SurrealHeartbeatResultsStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Create the `heartbeat_results` table and its indexes if they don't exist yet.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE heartbeat_results SCHEMALESS;
                DEFINE FIELD heartbeat_id ON heartbeat_results TYPE string;
                DEFINE FIELD director_id ON heartbeat_results TYPE string;
                DEFINE INDEX heartbeat_results_entity_idx ON heartbeat_results FIELDS entity_id, fired_at;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealHeartbeatResultsStore schema warning: {}", err);
        }
        Ok(())
    }
}

#[async_trait]
impl HeartbeatResultsStore for SurrealHeartbeatResultsStore {
    async fn record(&self, result: &HeartbeatResult) -> Result<()> {
        let body = UpsertBody {
            heartbeat_id: result.heartbeat_id.clone(),
            workspace_id: result.workspace_id.clone(),
            entity_id: result.entity_id.clone(),
            director_id: result.director_id.clone(),
            fired_at: result.fired_at.clone(),
            summary: result.summary.clone(),
            created_at: result.created_at.clone(),
        };
        let _: Option<ResultRow> = self
            .db
            .upsert(("heartbeat_results", result.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert heartbeat_result {}: {}", result.id, e))?;
        Ok(())
    }

    async fn list_for_entity(&self, entity_id: &str, limit: usize) -> Result<Vec<HeartbeatResult>> {
        // SCHEMALESS rows mean `entity_id` may be missing — filter explicitly so
        // global heartbeats don't leak into a per-entity view.
        let mut res = self
            .db
            .query(
                "SELECT * FROM heartbeat_results \
                 WHERE entity_id = $entity_id \
                 ORDER BY fired_at DESC LIMIT $lim",
            )
            .bind(("entity_id", entity_id.to_string()))
            .bind(("lim", limit as i64))
            .await
            .map_err(|e| anyhow::anyhow!("query heartbeat_results: {}", e))?;
        let rows: Vec<ResultRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take heartbeat_results: {}", e))?;
        let mut out: Vec<HeartbeatResult> = rows.into_iter().filter_map(row_to_result).collect();
        // Surreal LIMIT + ORDER BY is sometimes unreliable on SCHEMALESS; resort.
        out.sort_by(|a, b| b.fired_at.cmp(&a.fired_at));
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn list_recent(&self, limit: usize) -> Result<Vec<HeartbeatResult>> {
        let mut res = self
            .db
            .query("SELECT * FROM heartbeat_results ORDER BY fired_at DESC LIMIT $lim")
            .bind(("lim", limit as i64))
            .await
            .map_err(|e| anyhow::anyhow!("query recent heartbeat_results: {}", e))?;
        let rows: Vec<ResultRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take recent heartbeat_results: {}", e))?;
        let mut out: Vec<HeartbeatResult> = rows.into_iter().filter_map(row_to_result).collect();
        // Same caveat as `list_for_entity`: re-sort rather than trust SCHEMALESS
        // ORDER BY + LIMIT.
        out.sort_by(|a, b| b.fired_at.cmp(&a.fired_at));
        if out.len() > limit {
            out.truncate(limit);
        }
        Ok(out)
    }

    async fn delete_for_entity(&self, entity_id: &str) -> Result<()> {
        self.db
            .query("DELETE FROM heartbeat_results WHERE entity_id = $entity_id")
            .bind(("entity_id", entity_id.to_string()))
            .await
            .map_err(|e| anyhow::anyhow!("delete heartbeat_results for {}: {}", entity_id, e))?;
        Ok(())
    }
}

/// Default rendering used by [`crate::heartbeat_dispatcher::ChatHeartbeatDispatcher`]
/// when the canned demo path is taken (no LLM call). Kept here so future code
/// that wants to wrap an LLM-produced summary in the same envelope can reuse
/// the timestamp + framing logic.
pub fn build_canned_summary(
    director_name: &str,
    entity_name: &str,
    pinned_notes: &[String],
    fired_at: DateTime<Utc>,
) -> String {
    let head = format!(
        "Heartbeat — {director_name} director — {time}.",
        time = fired_at.format("%Y-%m-%d %H:%M UTC"),
    );
    let body = if pinned_notes.is_empty() {
        format!(
            "No pinned mission notes for {entity_name} yet. \
             Pin a note on the active mission and the next heartbeat will reference it."
        )
    } else {
        let top: Vec<String> = pinned_notes
            .iter()
            .take(3)
            .map(|n| format!("• {n}"))
            .collect();
        format!("Standing items for {entity_name}:\n{}", top.join("\n"),)
    };
    format!("{head}\n{body}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_and_list_filters_by_entity_and_orders_desc() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealHeartbeatResultsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let mk = |id: &str, entity: Option<&str>, fired_at: &str| HeartbeatResult {
            id: id.to_string(),
            heartbeat_id: "hb_1".to_string(),
            workspace_id: Some("ws_1".to_string()),
            entity_id: entity.map(|s| s.to_string()),
            director_id: "operations".to_string(),
            fired_at: fired_at.to_string(),
            summary: format!("res {id}"),
            created_at: fired_at.to_string(),
        };

        store
            .record(&mk("res_a", Some("ent_qsme"), "2026-05-24T09:00:00Z"))
            .await
            .unwrap();
        store
            .record(&mk("res_b", Some("ent_qsme"), "2026-05-25T09:00:00Z"))
            .await
            .unwrap();
        store
            .record(&mk("res_other", Some("ent_other"), "2026-05-24T09:00:00Z"))
            .await
            .unwrap();
        store
            .record(&mk("res_global", None, "2026-05-24T09:00:00Z"))
            .await
            .unwrap();

        let listed = store.list_for_entity("ent_qsme", 10).await.unwrap();
        let ids: Vec<_> = listed.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["res_b", "res_a"],
            "newest first, only target entity"
        );

        let limited = store.list_for_entity("ent_qsme", 1).await.unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].id, "res_b");

        // list_recent is cross-entity and includes rows with no entity_id.
        let recent = store.list_recent(10).await.unwrap();
        assert_eq!(recent.len(), 4);
        assert_eq!(recent[0].id, "res_b", "newest first");
        assert_eq!(store.list_recent(2).await.unwrap().len(), 2);
    }

    #[test]
    fn canned_summary_uses_pinned_notes_when_present() {
        let fired = "2026-05-24T09:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let s = build_canned_summary(
            "operations",
            "QS Me",
            &[
                "Tile schedule blocking close-out".into(),
                "Variation #12 unsigned".into(),
            ],
            fired,
        );
        assert!(s.contains("Heartbeat — operations director"));
        assert!(s.contains("Tile schedule"));
        assert!(s.contains("Variation #12"));
    }

    #[test]
    fn canned_summary_falls_back_when_no_notes() {
        let fired = "2026-05-24T09:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let s = build_canned_summary("operations", "QS Me", &[], fired);
        assert!(s.contains("No pinned mission notes for QS Me"));
    }
}
