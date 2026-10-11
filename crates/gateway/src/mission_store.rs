//! Surreal-backed persistence for [`crate::state::Mission`].
//!
//! Missions used to live only inside `ControlState::missions: RwLock<Vec<Mission>>`
//! and never survived restart. This store mirrors `entity_store.rs`: the in-memory
//! Vec stays as the live working copy; we hydrate on cold start with `list_all`
//! and sync periodically with `sync_all`, which upserts every row and deletes
//! only ids removed through [`SurrealMissionStore::delete`]. Inferring deletes
//! from "missing in the Vec" destroyed rows that failed to deserialize, rows
//! newer than a fallback snapshot, and rows a concurrent sync had just written;
//! see the `entity_store` module docs and [`crate::entity_store::SyncLedger`].
//!
//! ### SurrealDB v2 record-id handling
//! Same gotcha as `entity_store.rs`: Surreal stores the row id as a `Thing` /
//! `RecordId`, not a string. We can't `select::<Vec<Mission>>(...)` directly
//! and we can't `.content(mission.clone())` directly. So we round-trip
//! through a `MissionRow` (RecordId on read) and a `MissionUpsertBody` (no
//! `id` field on write). The `mission_field_completeness_guard` below pins
//! the projection so a future Mission field can't be silently dropped from
//! persistence.

use anyhow::Result;
use serde::Deserialize;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::entity_store::SyncLedger;
use crate::state::{Mission, MissionStatus};

/// Mission persistence, backed by Surreal, with an in-memory `Vec` as the live copy.
pub struct SurrealMissionStore {
    db: Arc<Surreal<Any>>,
    ledger: SyncLedger,
}

#[derive(Deserialize)]
struct MissionRow {
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

// Reassemble a plain id string onto the row's fields, then decode as a Mission.
fn row_to_mission(row: MissionRow) -> Result<Mission, (String, serde_json::Error)> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    serde_json::from_value::<Mission>(serde_json::Value::Object(map)).map_err(|e| (id, e))
}

// Write-side projection of Mission: same fields, minus `id` (that's supplied
// separately as the record key).
#[derive(serde::Serialize)]
struct MissionUpsertBody {
    entity_id: String,
    title: String,
    summary: String,
    status: MissionStatus,
    priority: String,
    goal: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_reviewed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    review_cadence_hours: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    narrative: Option<String>,
    canonical_notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assigned_director_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_report: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loop_config: Option<crate::state::MissionLoopConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loop_state: Option<crate::state::MissionLoopState>,
}

fn mission_to_upsert_body(m: &Mission) -> MissionUpsertBody {
    MissionUpsertBody {
        entity_id: m.entity_id.clone(),
        title: m.title.clone(),
        summary: m.summary.clone(),
        status: m.status,
        priority: m.priority.clone(),
        goal: m.goal.clone(),
        owner: m.owner.clone(),
        created_by: m.created_by.clone(),
        source: m.source.clone(),
        created_at: m.created_at.clone(),
        updated_at: m.updated_at.clone(),
        last_reviewed_at: m.last_reviewed_at.clone(),
        review_cadence_hours: m.review_cadence_hours,
        narrative: m.narrative.clone(),
        canonical_notes: m.canonical_notes.clone(),
        assigned_director_id: m.assigned_director_id.clone(),
        final_report: m.final_report.clone(),
        loop_config: m.loop_config.clone(),
        loop_state: m.loop_state.clone(),
    }
}

/// Compile-time guard: destructuring fails if a new field is added to `Mission`,
/// forcing the dev to update `MissionUpsertBody` so it isn't silently dropped
/// from persistence.
#[allow(dead_code)]
fn mission_field_completeness_guard(m: &Mission) {
    let Mission {
        id: _,
        entity_id: _,
        title: _,
        summary: _,
        status: _,
        priority: _,
        goal: _,
        owner: _,
        created_by: _,
        source: _,
        created_at: _,
        updated_at: _,
        last_reviewed_at: _,
        review_cadence_hours: _,
        narrative: _,
        canonical_notes: _,
        assigned_director_id: _,
        final_report: _,
        loop_config: _,
        loop_state: _,
    } = m;
}

impl SurrealMissionStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self {
            db,
            ledger: SyncLedger::default(),
        })
    }

    /// Create the `missions` table and its indexes if they don't exist yet.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE missions SCHEMALESS;
                DEFINE FIELD entity_id ON missions TYPE string;
                DEFINE FIELD status ON missions TYPE string;
                DEFINE INDEX missions_entity_idx ON missions FIELDS entity_id;
                DEFINE INDEX missions_status_idx ON missions FIELDS status;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealMissionStore schema warning: {}", err);
        }
        Ok(())
    }

    /// All missions, in whatever order Surreal returns them. This is the boot
    /// hydration call, so its outcome is recorded: while it last failed,
    /// `sync_all` deletes nothing.
    pub async fn list_all(&self) -> Result<Vec<Mission>> {
        let rows: Vec<MissionRow> = match self.db.select("missions").await {
            Ok(rows) => rows,
            Err(e) => {
                self.ledger.record_full_listing(false);
                return Err(anyhow::anyhow!("select missions: {}", e));
            }
        };
        self.ledger.record_full_listing(true);
        Ok(self.rows_to_missions(rows))
    }

    /// All missions for one entity.
    pub async fn list_by_entity(&self, entity_id: &str) -> Result<Vec<Mission>> {
        let eid = entity_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM missions WHERE entity_id = $eid")
            .bind(("eid", eid))
            .await
            .map_err(|e| anyhow::anyhow!("query missions by entity: {}", e))?;
        let rows: Vec<MissionRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take missions: {}", e))?;
        Ok(self.rows_to_missions(rows))
    }

    /// Insert or replace one mission by id. A mission deleted in this process
    /// is skipped: a write-through carrying a stale copy must not resurrect it.
    pub async fn upsert(&self, mission: &Mission) -> Result<()> {
        let _serial = self.ledger.serialize().await;
        if self.ledger.is_tombstoned(&mission.id) {
            return Ok(());
        }
        self.upsert_row(mission).await
    }

    /// Remove one mission by id, explicitly. The id is tombstoned first so no
    /// later upsert or sync writes it back; a failed row delete is retried by
    /// the next `sync_all`.
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.ledger.tombstone(id);
        let _serial = self.ledger.serialize().await;
        self.delete_row(id).await?;
        self.ledger.delete_landed(id);
        Ok(())
    }

    // Callers hold the ledger's write lock.
    async fn upsert_row(&self, mission: &Mission) -> Result<()> {
        let body = mission_to_upsert_body(mission);
        let _: Option<MissionRow> = self
            .db
            .upsert(("missions", mission.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert mission {}: {}", mission.id, e))?;
        Ok(())
    }

    // Callers hold the ledger's write lock.
    async fn delete_row(&self, id: &str) -> Result<()> {
        let _: Option<MissionRow> = self
            .db
            .delete(("missions", id))
            .await
            .map_err(|e| anyhow::anyhow!("delete mission {}: {}", id, e))?;
        Ok(())
    }

    /// Append `note` to `canonical_notes` if not already present. Returns the
    /// updated mission. Used by the prompt-assembly pipeline to surface pinned
    /// context that should never be compacted out.
    pub async fn pin_note(&self, mission_id: &str, note: &str) -> Result<Option<Mission>> {
        let Some(mut m) = self.get(mission_id).await? else {
            return Ok(None);
        };
        if !m.canonical_notes.iter().any(|n| n == note) {
            m.canonical_notes.push(note.to_string());
            m.updated_at = chrono::Utc::now().to_rfc3339();
            self.upsert(&m).await?;
        }
        Ok(Some(m))
    }

    /// Remove every occurrence of `note` from `canonical_notes`. Returns the
    /// updated mission.
    pub async fn unpin_note(&self, mission_id: &str, note: &str) -> Result<Option<Mission>> {
        let Some(mut m) = self.get(mission_id).await? else {
            return Ok(None);
        };
        let before = m.canonical_notes.len();
        m.canonical_notes.retain(|n| n != note);
        if m.canonical_notes.len() != before {
            m.updated_at = chrono::Utc::now().to_rfc3339();
            self.upsert(&m).await?;
        }
        Ok(Some(m))
    }

    /// Replace the mission narrative wholesale. `None` clears it.
    pub async fn set_narrative(
        &self,
        mission_id: &str,
        narrative: Option<String>,
    ) -> Result<Option<Mission>> {
        let Some(mut m) = self.get(mission_id).await? else {
            return Ok(None);
        };
        m.narrative = narrative;
        m.updated_at = chrono::Utc::now().to_rfc3339();
        self.upsert(&m).await?;
        Ok(Some(m))
    }

    /// Load one mission by id. `Ok(None)` if it doesn't exist.
    pub async fn get(&self, id: &str) -> Result<Option<Mission>> {
        let row: Option<MissionRow> = self
            .db
            .select(("missions", id))
            .await
            .map_err(|e| anyhow::anyhow!("select mission {}: {}", id, e))?;
        Ok(row.and_then(|r| row_to_mission(r).ok()))
    }

    /// Missions in `Active` status for the given entity, in arbitrary order.
    /// Drives prompt-assembly: only active missions feed the canonical block.
    pub async fn list_active_for_entity(&self, entity_id: &str) -> Result<Vec<Mission>> {
        let eid = entity_id.to_string();
        let mut res = self
            .db
            .query("SELECT * FROM missions WHERE entity_id = $eid AND status = 'active'")
            .bind(("eid", eid))
            .await
            .map_err(|e| anyhow::anyhow!("query active missions: {}", e))?;
        let rows: Vec<MissionRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take active missions: {}", e))?;
        Ok(self.rows_to_missions(rows))
    }

    /// Idempotent bulk sync: upserts every mission in `missions` that has not
    /// been deleted. Rows missing from `missions` are left alone — it may be a
    /// stale clone or a fallback snapshot. The only deletes are retries of
    /// failed [`Self::delete`] calls, and none while the last hydration is
    /// known to have failed. Same shape as `SurrealEntityStore::sync_all`.
    pub async fn sync_all(&self, missions: &[Mission]) -> Result<()> {
        let _serial = self.ledger.serialize().await;
        for mission in missions {
            if self.ledger.is_tombstoned(&mission.id) {
                continue;
            }
            self.upsert_row(mission).await?;
        }
        if self.ledger.hydration_failed() {
            return Ok(());
        }
        for id in self.ledger.pending_deletes() {
            self.delete_row(&id).await?;
            self.ledger.delete_landed(&id);
        }
        Ok(())
    }

    // Decode every row, skipping (and logging and remembering) any that fail
    // rather than failing the whole batch. Remembered rows are never deleted
    // by a sync.
    fn rows_to_missions(&self, rows: Vec<MissionRow>) -> Vec<Mission> {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            match row_to_mission(row) {
                Ok(m) => out.push(m),
                Err((id, err)) => {
                    tracing::warn!(
                        mission_id = %id,
                        error = %err,
                        "SurrealMissionStore: skipping malformed row (kept in the table)"
                    );
                    self.ledger.record_unreadable(&id);
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_mission(id: &str) -> Mission {
        Mission {
            id: id.to_string(),
            entity_id: "ent_1".to_string(),
            title: "Title".to_string(),
            summary: "Summary".to_string(),
            status: MissionStatus::Active,
            priority: "normal".to_string(),
            goal: "Ship".to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-05-24T00:00:00Z".to_string(),
            updated_at: "2026-05-24T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: Some("Started building.".to_string()),
            canonical_notes: vec!["always ship".to_string()],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        }
    }

    #[test]
    fn upsert_body_strips_id_field() {
        let m = sample_mission("mis_1");
        let body = mission_to_upsert_body(&m);
        let serialised = serde_json::to_value(&body).expect("serialise");
        let obj = serialised.as_object().expect("object");
        assert!(
            !obj.contains_key("id"),
            "id must be stripped from upsert body"
        );
        assert_eq!(obj.get("entity_id").and_then(|v| v.as_str()), Some("ent_1"));
        assert_eq!(
            obj.get("canonical_notes")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(1)
        );
    }

    #[tokio::test]
    async fn pin_unpin_narrative_and_active_filter() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealMissionStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let mut active = sample_mission("mis_active");
        active.canonical_notes.clear();
        active.narrative = None;
        let mut completed = sample_mission("mis_done");
        completed.status = MissionStatus::Completed;
        store.upsert(&active).await.expect("upsert active");
        store.upsert(&completed).await.expect("upsert completed");

        // pin twice → only one entry; unpin removes it.
        store
            .pin_note("mis_active", "north star")
            .await
            .expect("pin");
        store
            .pin_note("mis_active", "north star")
            .await
            .expect("pin again");
        let after = store.get("mis_active").await.unwrap().unwrap();
        assert_eq!(after.canonical_notes, vec!["north star".to_string()]);
        store
            .unpin_note("mis_active", "north star")
            .await
            .expect("unpin");
        let cleared = store.get("mis_active").await.unwrap().unwrap();
        assert!(cleared.canonical_notes.is_empty());

        store
            .set_narrative("mis_active", Some("now we go".to_string()))
            .await
            .expect("narrative");
        let narrated = store.get("mis_active").await.unwrap().unwrap();
        assert_eq!(narrated.narrative.as_deref(), Some("now we go"));

        // list_active_for_entity excludes Completed.
        let active_list = store
            .list_active_for_entity("ent_1")
            .await
            .expect("active list");
        assert_eq!(active_list.len(), 1);
        assert_eq!(active_list[0].id, "mis_active");
    }

    #[tokio::test]
    async fn end_to_end_in_memory_surreal() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealMissionStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let m1 = sample_mission("mis_a");
        let m2 = sample_mission("mis_b");
        store.upsert(&m1).await.expect("upsert m1");
        store.upsert(&m2).await.expect("upsert m2");

        let all = store.list_all().await.expect("list_all");
        assert_eq!(all.len(), 2);

        let by_entity = store.list_by_entity("ent_1").await.expect("by entity");
        assert_eq!(by_entity.len(), 2);

        // A sync whose input lacks a row no longer deletes it: the input may
        // be a stale clone. Only an explicit delete removes a row.
        store
            .sync_all(std::slice::from_ref(&m1))
            .await
            .expect("sync without m2");
        assert_eq!(store.list_all().await.expect("list").len(), 2);

        store.delete("mis_b").await.expect("delete m2");
        // A stale clone still carrying m2 cannot write it back.
        store
            .sync_all(&[m1.clone(), m2.clone()])
            .await
            .expect("stale sync");
        store.upsert(&m2).await.expect("stale write-through");
        let pruned = store.list_all().await.expect("list after delete");
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].id, "mis_a");
    }

    async fn raw_keys(store: &SurrealMissionStore) -> Vec<String> {
        let rows: Vec<MissionRow> = store.db.select("missions").await.expect("select");
        let mut keys: Vec<String> = rows.iter().map(|r| record_id_key(&r.id)).collect();
        keys.sort();
        keys
    }

    #[tokio::test]
    async fn a_malformed_mission_row_survives_a_sync() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let db = Arc::new(db);
        let store = SurrealMissionStore::new(db.clone());
        store.init_schema().await.expect("init");
        store.upsert(&sample_mission("mis_ok")).await.expect("seed");
        // No title/summary/goal: cannot become a `Mission`.
        db.query("CREATE missions:mis_broken CONTENT { entity_id: 'ent_1', status: 'active' }")
            .await
            .expect("insert malformed row");

        let hydrated = store.list_all().await.expect("hydrate");
        assert_eq!(hydrated.len(), 1);
        store.sync_all(&hydrated).await.expect("sync");
        assert_eq!(raw_keys(&store).await, vec!["mis_broken", "mis_ok"]);
    }

    #[tokio::test]
    async fn a_mission_sync_after_failed_hydration_prunes_nothing() {
        use surrealdb::engine::any::connect;
        // No namespace yet, so hydration fails.
        let db = Arc::new(connect("memory").await.expect("connect"));
        let store = SurrealMissionStore::new(db.clone());
        assert!(store.list_all().await.is_err());

        db.use_ns("test").use_db("test").await.expect("use");
        store.init_schema().await.expect("init");
        store
            .upsert(&sample_mission("mis_newer"))
            .await
            .expect("row the fallback snapshot lacks");
        store
            .sync_all(&[sample_mission("mis_snapshot")])
            .await
            .expect("sync");
        assert_eq!(raw_keys(&store).await, vec!["mis_newer", "mis_snapshot"]);
    }
}
