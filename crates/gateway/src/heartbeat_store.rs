//! Surreal-backed persistence + tick loop for [`crate::state::Heartbeat`].
//!
//! Heartbeats are pillar 1: each entity / mission / "global" target can declare
//! a cadence (cron or `cadence_secs`) and a director to run on that cadence.
//! The tick scans `heartbeats WHERE enabled AND next_fire_at <= now()`, hands
//! each due row to a [`HeartbeatDispatcher`], then advances `last_fired_at` /
//! `next_fire_at` and upserts.
//!
//! Orchestrator wiring (turning the dispatched event into an actual queued
//! task on the canonical `tasks` flow) lives outside this file — it's the
//! Phase 9 demo. Phase 4 only guarantees that *something* is invoked with the
//! right director + target context.
//!
//! ### Surreal v2 record-id handling
//! Same round-trip pattern as `entity_store` / `mission_store` / `director_store`:
//! read via `HeartbeatRow { id: RecordId, ..flatten }`, write via
//! `HeartbeatUpsertBody` (no `id`). The `heartbeat_field_completeness_guard`
//! below pins the projection so a new Heartbeat field can't be silently
//! dropped from persistence.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cron::Schedule;
use serde::Deserialize;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::state::Heartbeat;

/// Heartbeat persistence, backed by Surreal.
pub struct SurrealHeartbeatStore {
    db: Arc<Surreal<Any>>,
}

#[derive(Deserialize)]
struct HeartbeatRow {
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

// Reassemble a plain id string onto the row's fields, then decode as a Heartbeat.
fn row_to_heartbeat(row: HeartbeatRow) -> Result<Heartbeat, (String, serde_json::Error)> {
    let id = record_id_key(&row.id);
    let mut map = row.rest;
    map.insert("id".to_string(), serde_json::Value::String(id.clone()));
    serde_json::from_value::<Heartbeat>(serde_json::Value::Object(map)).map_err(|e| (id, e))
}

// Write-side projection of Heartbeat: same fields, minus `id` (that's supplied
// separately as the record key).
#[derive(serde::Serialize)]
struct HeartbeatUpsertBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<String>,
    name: String,
    enabled: bool,
    target_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_id: Option<String>,
    director_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cron: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cadence_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_fired_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_fire_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lease_token: Option<String>,
    created_at: String,
    updated_at: String,
}

fn heartbeat_to_upsert_body(h: &Heartbeat) -> HeartbeatUpsertBody {
    HeartbeatUpsertBody {
        workspace_id: h.workspace_id.clone(),
        name: h.name.clone(),
        enabled: h.enabled,
        target_kind: h.target_kind.clone(),
        target_id: h.target_id.clone(),
        director_id: h.director_id.clone(),
        cron: h.cron.clone(),
        cadence_secs: h.cadence_secs,
        condition: h.condition.clone(),
        last_fired_at: h.last_fired_at.clone(),
        next_fire_at: h.next_fire_at.clone(),
        lease_until: h.lease_until.clone(),
        lease_token: h.lease_token.clone(),
        created_at: h.created_at.clone(),
        updated_at: h.updated_at.clone(),
    }
}

/// Compile-time guard: destructuring fails if a new field is added to
/// `Heartbeat`, forcing the dev to update `HeartbeatUpsertBody` so it isn't
/// silently dropped from persistence.
#[allow(dead_code)]
fn heartbeat_field_completeness_guard(h: &Heartbeat) {
    let Heartbeat {
        id: _,
        workspace_id: _,
        name: _,
        enabled: _,
        target_kind: _,
        target_id: _,
        director_id: _,
        cron: _,
        cadence_secs: _,
        condition: _,
        last_fired_at: _,
        next_fire_at: _,
        lease_until: _,
        lease_token: _,
        created_at: _,
        updated_at: _,
    } = h;
}

/// Dispatcher called when a heartbeat is due. Implementors typically enqueue
/// a task into the orchestrator with the right director + entity/mission
/// context attached. Phase 4 only ships the contract; the real orchestrator
/// wire-up lands in Phase 9.
pub trait HeartbeatDispatcher: Send + Sync {
    fn dispatch(&self, heartbeat: Heartbeat) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// Persistence + due-row query contract for heartbeats.
#[async_trait]
pub trait HeartbeatStore: Send + Sync {
    async fn upsert(&self, heartbeat: &Heartbeat) -> Result<()>;
    async fn get(&self, id: &str) -> Result<Option<Heartbeat>>;
    async fn list_all(&self) -> Result<Vec<Heartbeat>>;
    /// Heartbeats whose schedule (and lease) says they're ready to fire at `now`.
    async fn list_due(&self, now: DateTime<Utc>) -> Result<Vec<Heartbeat>>;
    /// Atomically claim a due heartbeat so only one instance dispatches it;
    /// `false` if another instance already holds the lease or won the race.
    async fn try_claim(
        &self,
        heartbeat: &Heartbeat,
        token: &str,
        until: DateTime<Utc>,
    ) -> Result<bool>;
    async fn delete(&self, id: &str) -> Result<()>;
}

impl SurrealHeartbeatStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Create the `heartbeats` table and its indexes if they don't exist yet.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE heartbeats SCHEMALESS;
                DEFINE FIELD enabled ON heartbeats TYPE bool;
                DEFINE FIELD target_kind ON heartbeats TYPE string;
                DEFINE FIELD director_id ON heartbeats TYPE string;
                DEFINE INDEX heartbeats_next_fire_idx ON heartbeats FIELDS next_fire_at;
                DEFINE INDEX heartbeats_target_idx ON heartbeats FIELDS target_kind, target_id;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("SurrealHeartbeatStore schema warning: {}", err);
        }
        Ok(())
    }
}

#[async_trait]
impl HeartbeatStore for SurrealHeartbeatStore {
    async fn upsert(&self, heartbeat: &Heartbeat) -> Result<()> {
        let body = heartbeat_to_upsert_body(heartbeat);
        let _: Option<HeartbeatRow> = self
            .db
            .upsert(("heartbeats", heartbeat.id.as_str()))
            .content(body)
            .await
            .map_err(|e| anyhow::anyhow!("upsert heartbeat {}: {}", heartbeat.id, e))?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Heartbeat>> {
        let row: Option<HeartbeatRow> = self
            .db
            .select(("heartbeats", id))
            .await
            .map_err(|e| anyhow::anyhow!("select heartbeat {}: {}", id, e))?;
        Ok(row.and_then(|r| row_to_heartbeat(r).ok()))
    }

    async fn list_all(&self) -> Result<Vec<Heartbeat>> {
        let rows: Vec<HeartbeatRow> = self
            .db
            .select("heartbeats")
            .await
            .map_err(|e| anyhow::anyhow!("select heartbeats: {}", e))?;
        Ok(rows_to_heartbeats(rows))
    }

    async fn list_due(&self, now: DateTime<Utc>) -> Result<Vec<Heartbeat>> {
        // Surreal can't compare RFC3339 strings reliably across all timezones,
        // so we fetch enabled rows and filter in-process. Volume is tiny
        // (heartbeats per workspace, not per event) so this is fine.
        let mut res = self
            .db
            .query("SELECT * FROM heartbeats WHERE enabled = true")
            .await
            .map_err(|e| anyhow::anyhow!("query enabled heartbeats: {}", e))?;
        let rows: Vec<HeartbeatRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("take enabled heartbeats: {}", e))?;
        let all = rows_to_heartbeats(rows);
        Ok(all.into_iter().filter(|h| is_due(h, now)).collect())
    }

    async fn try_claim(
        &self,
        heartbeat: &Heartbeat,
        token: &str,
        until: DateTime<Utc>,
    ) -> Result<bool> {
        // The predicate is evaluated by Surreal as part of the UPDATE, so two
        // gateway instances racing on the same due row cannot both acquire it.
        // The schedule cursor is included to prevent a stale reader from
        // claiming a slot that another worker has already advanced.
        let (schedule_predicate, schedule_bind) = match heartbeat.next_fire_at.as_deref() {
            Some(_) => (
                "next_fire_at = $next_fire_at",
                heartbeat.next_fire_at.clone(),
            ),
            None => ("next_fire_at IS NONE", None),
        };
        let query = format!(
            "UPDATE type::thing('heartbeats', $id) SET lease_until = $lease_until, lease_token = $lease_token WHERE enabled = true AND (lease_until IS NONE OR lease_until < $now) AND {schedule_predicate} RETURN AFTER"
        );
        let mut response = self
            .db
            .query(query)
            .bind(("id", heartbeat.id.clone()))
            .bind(("lease_until", until.to_rfc3339()))
            .bind(("lease_token", token.to_string()))
            .bind(("now", Utc::now().to_rfc3339()))
            .bind(("next_fire_at", schedule_bind))
            .await
            .map_err(|e| anyhow::anyhow!("claim heartbeat {}: {}", heartbeat.id, e))?;
        let rows: Vec<HeartbeatRow> = response
            .take(0)
            .map_err(|e| anyhow::anyhow!("read heartbeat claim {}: {}", heartbeat.id, e))?;
        Ok(!rows.is_empty())
    }

    async fn delete(&self, id: &str) -> Result<()> {
        let _: Option<HeartbeatRow> = self
            .db
            .delete(("heartbeats", id))
            .await
            .map_err(|e| anyhow::anyhow!("delete heartbeat {}: {}", id, e))?;
        Ok(())
    }
}

// Decode every row, dropping (and logging) any that fail rather than failing
// the whole batch.
fn rows_to_heartbeats(rows: Vec<HeartbeatRow>) -> Vec<Heartbeat> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row_to_heartbeat(row) {
            Ok(h) => out.push(h),
            Err((id, err)) => {
                tracing::warn!(
                    heartbeat_id = %id,
                    error = %err,
                    "SurrealHeartbeatStore: dropping malformed row"
                );
            }
        }
    }
    out
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

// Due means: not currently leased by another instance, and either never
// scheduled yet or its next_fire_at has passed.
fn is_due(h: &Heartbeat, now: DateTime<Utc>) -> bool {
    if h.lease_until
        .as_deref()
        .and_then(parse_rfc3339)
        .is_some_and(|held| held > now)
    {
        return false;
    }
    match h.next_fire_at.as_deref().and_then(parse_rfc3339) {
        Some(next) => next <= now,
        // Never fired and no schedule computed yet → due immediately.
        None => true,
    }
}

/// Compute the next fire timestamp from a heartbeat's cadence. Cron wins over
/// `cadence_secs` if both are set. Returns `None` if neither is configured or
/// the cron expression is invalid (caller decides whether to disable the row).
pub fn next_fire_at(h: &Heartbeat, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if let Some(expr) = h.cron.as_deref()
        && let Ok(schedule) = Schedule::from_str(expr)
        && let Some(next) = schedule.after(&after).next()
    {
        return Some(next);
    }
    if let Some(secs) = h.cadence_secs
        && secs > 0
    {
        return Some(after + chrono::Duration::seconds(secs as i64));
    }
    None
}

/// Periodic loop: every `poll_interval_secs`, ask the store for due heartbeats,
/// dispatch each, then bump `last_fired_at` / `next_fire_at`.
///
/// Default poll interval is 30s per the Phase 4 spec ("scheduler tick fires
/// every ~30s").
pub struct HeartbeatTicker {
    store: Arc<dyn HeartbeatStore>,
    dispatcher: Arc<dyn HeartbeatDispatcher>,
    poll_interval_secs: u64,
    /// Prevents an interval tick and an on-demand trigger from dispatching the
    /// same heartbeat concurrently within this process. Durable leasing is
    /// added below for multi-instance deployments.
    in_flight: Arc<Mutex<HashSet<String>>>,
}

impl HeartbeatTicker {
    pub fn new(
        store: Arc<dyn HeartbeatStore>,
        dispatcher: Arc<dyn HeartbeatDispatcher>,
        poll_interval_secs: u64,
    ) -> Self {
        Self {
            store,
            dispatcher,
            poll_interval_secs: poll_interval_secs.max(1),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Run one tick. Visible for tests + on-demand triggers.
    pub async fn run_one_tick(&self) -> Result<usize> {
        let now = Utc::now();
        let due = self.store.list_due(now).await?;
        let claimed = {
            let mut in_flight = self.in_flight.lock().await;
            due.into_iter()
                .filter(|hb| in_flight.insert(hb.id.clone()))
                .collect::<Vec<_>>()
        };
        let count = claimed.len();
        for hb in claimed {
            let token = Uuid::new_v4().to_string();
            // The lease must outlive the dispatch it protects: a director beat
            // is one full LLM completion, and a slow provider outliving a short
            // lease means a second instance re-fires the same beat.
            let lease_until = now + chrono::Duration::minutes(10);
            if !self.store.try_claim(&hb, &token, lease_until).await? {
                self.in_flight.lock().await.remove(&hb.id);
                continue;
            }
            let next = next_fire_at(&hb, now);
            let Some(next) = next else {
                tracing::warn!(heartbeat_id = %hb.id, "disabling heartbeat with no valid schedule");
                let heartbeat_id = hb.id.clone();
                let updated = Heartbeat {
                    enabled: false,
                    next_fire_at: None,
                    lease_until: None,
                    lease_token: None,
                    updated_at: now.to_rfc3339(),
                    ..hb
                };
                let result = self.store.upsert(&updated).await;
                self.in_flight.lock().await.remove(&heartbeat_id);
                result?;
                continue;
            };
            let updated = Heartbeat {
                last_fired_at: Some(now.to_rfc3339()),
                next_fire_at: Some(next.to_rfc3339()),
                lease_until: None,
                lease_token: None,
                updated_at: now.to_rfc3339(),
                ..hb.clone()
            };
            // Persist the schedule bump *before* dispatching: a dispatch that
            // outlives its lease must not be re-fired by another instance, and
            // a crash mid-dispatch skips one beat rather than double-running
            // it. A failed write releases the claim and lets the next tick
            // refire instead of dispatching un-bumped.
            if let Err(e) = self.store.upsert(&updated).await {
                tracing::warn!(
                    heartbeat_id = %updated.id,
                    error = %e,
                    "heartbeat tick: failed to persist next_fire_at; will refire next tick"
                );
                self.in_flight.lock().await.remove(&updated.id);
                continue;
            }
            // Dispatch concurrently: each beat is a full LLM completion, and
            // awaiting them serially would let one slow provider starve every
            // other entity's beat this tick.
            let dispatcher = self.dispatcher.clone();
            let in_flight = self.in_flight.clone();
            tokio::spawn(async move {
                let id = hb.id.clone();
                dispatcher.dispatch(hb).await;
                in_flight.lock().await.remove(&id);
            });
        }
        Ok(count)
    }

    /// Poll forever at `poll_interval_secs`, running `run_one_tick` each time.
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(self.poll_interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if let Err(e) = self.run_one_tick().await {
                tracing::warn!("heartbeat tick error: {:?}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn sample_heartbeat(id: &str) -> Heartbeat {
        Heartbeat {
            id: id.to_string(),
            workspace_id: Some("ws_1".to_string()),
            name: "daily standup".to_string(),
            enabled: true,
            target_kind: "entity".to_string(),
            target_id: Some("ent_qsme".to_string()),
            director_id: "operations".to_string(),
            cron: None,
            cadence_secs: Some(60),
            condition: None,
            last_fired_at: None,
            next_fire_at: None,
            lease_until: None,
            lease_token: None,
            created_at: "2026-05-24T00:00:00Z".to_string(),
            updated_at: "2026-05-24T00:00:00Z".to_string(),
        }
    }

    #[derive(Default)]
    struct CountingDispatcher {
        seen: Mutex<Vec<Heartbeat>>,
    }

    impl HeartbeatDispatcher for CountingDispatcher {
        fn dispatch(&self, heartbeat: Heartbeat) -> Pin<Box<dyn Future<Output = ()> + Send>> {
            self.seen.lock().unwrap().push(heartbeat);
            Box::pin(async {})
        }
    }

    #[test]
    fn upsert_body_strips_id_field() {
        let h = sample_heartbeat("hb_1");
        let body = heartbeat_to_upsert_body(&h);
        let v = serde_json::to_value(&body).expect("serialise");
        let obj = v.as_object().expect("object");
        assert!(!obj.contains_key("id"));
        assert_eq!(
            obj.get("director_id").and_then(|v| v.as_str()),
            Some("operations")
        );
    }

    #[test]
    fn next_fire_at_cadence_secs() {
        let h = sample_heartbeat("hb_1");
        let base = "2026-05-24T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next = next_fire_at(&h, base).expect("cadence next");
        assert_eq!(next - base, chrono::Duration::seconds(60));
    }

    #[test]
    fn next_fire_at_cron_wins() {
        let mut h = sample_heartbeat("hb_1");
        // every minute
        h.cron = Some("0 * * * * *".to_string());
        h.cadence_secs = Some(3_600); // would be wrong if cadence won
        let base = "2026-05-24T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let next = next_fire_at(&h, base).expect("cron next");
        assert!(next - base <= chrono::Duration::seconds(60));
    }

    #[tokio::test]
    async fn tick_fires_due_and_bumps_next_fire_at() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealHeartbeatStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        // Due immediately (no next_fire_at set).
        let h = sample_heartbeat("hb_due");
        store.upsert(&h).await.expect("upsert");

        // Disabled row must not fire.
        let mut off = sample_heartbeat("hb_off");
        off.enabled = false;
        store.upsert(&off).await.expect("upsert off");

        let dispatcher = Arc::new(CountingDispatcher::default());
        let ticker = HeartbeatTicker::new(store.clone(), dispatcher.clone(), 30);

        let fired = ticker.run_one_tick().await.expect("tick");
        assert_eq!(fired, 1);
        // Dispatch is spawned, not awaited inline; give the spawned task a
        // chance to run before asserting on it.
        for _ in 0..50 {
            if !dispatcher.seen.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        {
            let seen = dispatcher.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].id, "hb_due");
            assert_eq!(seen[0].director_id, "operations");
        }

        let after = store.get("hb_due").await.unwrap().unwrap();
        assert!(after.last_fired_at.is_some(), "last_fired_at set");
        assert!(
            after.next_fire_at.is_some(),
            "next_fire_at computed from cadence"
        );

        // Second tick immediately after — next_fire_at is in the future, nothing
        // should fire.
        let fired_again = ticker.run_one_tick().await.expect("tick 2");
        assert_eq!(fired_again, 0);
    }

    #[tokio::test]
    async fn list_due_includes_past_next_fire_at_only() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealHeartbeatStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let mut past = sample_heartbeat("hb_past");
        past.next_fire_at = Some("2020-01-01T00:00:00Z".to_string());
        store.upsert(&past).await.expect("upsert past");

        let mut future = sample_heartbeat("hb_future");
        future.next_fire_at = Some("2099-01-01T00:00:00Z".to_string());
        store.upsert(&future).await.expect("upsert future");

        let due = store.list_due(Utc::now()).await.expect("list due");
        let ids: Vec<_> = due.iter().map(|h| h.id.as_str()).collect();
        assert_eq!(ids, vec!["hb_past"]);
    }
}
