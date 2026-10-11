//! Surreal-backed per-row persistence for [`crate::state::Task`].
//!
//! The control-state snapshot is a whole-blob write, which loses races when
//! two gateway instances overlap during a zero-downtime redeploy: the new
//! instance boots from the pre-deploy snapshot and its next periodic save
//! clobbers whatever the old instance flushed on SIGTERM. Tasks (including
//! reminders) are the writes users actually feel losing, so they get their
//! own row-per-task table with:
//!
//!   - **write-through on creation** — the row exists before any redeploy
//!     can start, so a booting instance always sees it;
//!   - **merge hydration at boot** — rows unknown to the in-memory Vec are
//!     added, never discarded;
//!   - **upsert-only periodic sync** — no pruning, so an instance that
//!     doesn't know a task yet can't delete it. Task lifecycle is status
//!     transitions, not row deletion, so nothing leaks.
//!   - **tombstones for the one real deletion** — deleting an entity deletes
//!     its tasks. [`SurrealTaskStore::delete`] tombstones the id, and every
//!     upsert (write-through or periodic) skips a tombstoned id for the life
//!     of the process. Without that, the persist loop's clone of the task Vec
//!     — taken a moment before the cascade — wrote the deleted tasks straight
//!     back, and merge hydration resurrected them on the next boot. Writes and
//!     deletes are serialized per row so a delete cannot land between an
//!     upsert's tombstone check and its write.
//!
//! Rows store the task as an opaque JSON string (like the control snapshot)
//! so the Task struct can evolve without fighting Surreal field types.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::entity_store::SyncLedger;
use crate::state::Task;

const TABLE: &str = "control_tasks";

// Thin wrapper around the `control_tasks` Surreal table handle.
pub struct SurrealTaskStore {
    db: Arc<Surreal<Any>>,
    ledger: SyncLedger,
}

#[derive(Serialize)]
struct RowContent {
    data: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct Row {
    #[allow(dead_code)]
    id: surrealdb::RecordId,
    data: String,
}

impl SurrealTaskStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self {
            db,
            ledger: SyncLedger::default(),
        })
    }

    /// Write-through a single task row (used on creation, so a booting instance
    /// always sees it — see module header). A task deleted in this process is
    /// skipped, so a stale copy cannot write it back.
    pub async fn upsert(&self, task: &Task) -> Result<()> {
        // Held per row, not per batch: a periodic `upsert_all` must not stall
        // every write-through behind it.
        let _serial = self.ledger.serialize().await;
        if self.ledger.is_tombstoned(&task.id) {
            return Ok(());
        }
        let content = RowContent {
            data: serde_json::to_string(task).context("serialize task")?,
            updated_at: chrono::Utc::now().to_rfc3339(),
        };
        let _: Option<Row> = self
            .db
            .upsert((TABLE, task.id.as_str()))
            .content(content)
            .await
            .map_err(|e| anyhow::anyhow!("upsert task {}: {}", task.id, e))?;
        Ok(())
    }

    /// Periodic sync: upsert-only, so an instance that doesn't know a task yet
    /// can't delete it. Tombstoned ids are skipped (see [`Self::upsert`]); the
    /// only deletes are retries of [`Self::delete`] calls whose row delete
    /// failed.
    pub async fn upsert_all(&self, tasks: &[Task]) -> Result<()> {
        for task in tasks {
            self.upsert(task).await?;
        }
        for id in self.ledger.pending_deletes() {
            let _serial = self.ledger.serialize().await;
            self.delete_row(&id).await?;
            self.ledger.delete_landed(&id);
        }
        Ok(())
    }

    /// Load all rows, used for merge hydration at boot.
    pub async fn list_all(&self) -> Result<Vec<Task>> {
        let rows: Vec<Row> = self
            .db
            .select(TABLE)
            .await
            .map_err(|e| anyhow::anyhow!("list tasks: {}", e))?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            match serde_json::from_str::<Task>(&row.data) {
                Ok(t) => out.push(t),
                Err(e) => {
                    tracing::warn!(row_id = %row.id, error = %e, "SurrealTaskStore: dropping malformed task row");
                }
            }
        }
        Ok(out)
    }

    /// Delete a task row. The one lifecycle event that is a real row deletion,
    /// not a status transition: entity deletion — without it, merge hydration
    /// resurrects the deleted entity's tasks on the next boot.
    ///
    /// Tombstones the id first, so it also covers a task that never reached
    /// the table: whatever copy a sync or write-through still holds is skipped.
    pub async fn delete(&self, id: &str) -> Result<()> {
        self.ledger.tombstone(id);
        let _serial = self.ledger.serialize().await;
        self.delete_row(id).await?;
        self.ledger.delete_landed(id);
        Ok(())
    }

    // Callers hold the ledger's write lock.
    async fn delete_row(&self, id: &str) -> Result<()> {
        let _: Option<Row> = self
            .db
            .delete((TABLE, id))
            .await
            .map_err(|e| anyhow::anyhow!("delete task {}: {}", id, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TaskStatus;

    #[tokio::test]
    async fn task_rows_round_trip_and_upsert_overwrites() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealTaskStore::new(Arc::new(db));

        let mut task = Task {
            id: "rem_1".to_string(),
            title: "Reminder: check the oven".to_string(),
            task_kind: Some("reminder".to_string()),
            status: TaskStatus::Queued,
            metadata: Some(serde_json::json!({"due_at": "2026-07-18T09:00:00Z"})),
            ..Task::default()
        };
        store.upsert(&task).await.expect("insert");
        task.status = TaskStatus::Completed;
        store.upsert(&task).await.expect("overwrite");

        let all = store.list_all().await.expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "rem_1");
        assert_eq!(all[0].status, TaskStatus::Completed);
        assert!(all[0].metadata.is_some());
    }

    #[tokio::test]
    async fn a_deleted_task_is_not_resurrected_by_a_stale_sync() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealTaskStore::new(Arc::new(db));

        let keep = Task {
            id: "t_keep".to_string(),
            ..Task::default()
        };
        let doomed = Task {
            id: "t_doomed".to_string(),
            entity_id: Some("ent_gone".to_string()),
            ..Task::default()
        };
        // The persist loop clones the Vec before the entity cascade runs...
        let stale_clone = vec![keep.clone(), doomed.clone()];
        store.upsert_all(&stale_clone).await.expect("first sync");

        // ...the cascade deletes the entity's tasks...
        store.delete("t_doomed").await.expect("cascade delete");
        // ...and a task that never reached the table is tombstoned too.
        store
            .delete("t_never_persisted")
            .await
            .expect("tombstone only");

        // ...then the stale clone is synced, and a late write-through lands.
        store.upsert_all(&stale_clone).await.expect("stale sync");
        store.upsert(&doomed).await.expect("late write-through");
        store
            .upsert(&Task {
                id: "t_never_persisted".to_string(),
                ..Task::default()
            })
            .await
            .expect("late write-through of an unpersisted task");

        let ids: Vec<String> = store
            .list_all()
            .await
            .expect("list")
            .into_iter()
            .map(|t| t.id)
            .collect();
        assert_eq!(ids, vec!["t_keep".to_string()]);
    }
}
