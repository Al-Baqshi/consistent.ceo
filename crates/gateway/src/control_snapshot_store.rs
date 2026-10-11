//! Surreal-backed persistence for the whole [`ControlStateSnapshot`].
//!
//! The control state (tasks — including reminders — approvals, operator feed,
//! agents, director states, workspaces) historically persisted only to
//! `~/.consistent-world/control_state.json`. In containers that file lives on
//! the ephemeral layer, so every redeploy silently wiped the operational
//! state. This store writes the same snapshot into SurrealDB as a single
//! JSON-string record; boot prefers it over the file.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::state::ControlStateSnapshot;

const TABLE: &str = "control_snapshots";
// Fixed id: the whole control state lives in one singleton row, not one row
// per snapshot.
const KEY: &str = "main";

/// Persists the full [`ControlStateSnapshot`] to SurrealDB as one row.
pub struct SurrealControlSnapshotStore {
    db: Arc<Surreal<Any>>,
}

/// Write shape for the singleton row.
#[derive(Serialize)]
struct RowContent {
    /// The snapshot as a JSON string — opaque to Surreal so schema evolution
    /// of the snapshot never fights field definitions.
    data: String,
    saved_at: String,
}

/// Read shape for the singleton row.
#[derive(Deserialize)]
struct Row {
    #[allow(dead_code)]
    id: surrealdb::RecordId,
    data: String,
}

impl SurrealControlSnapshotStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Serialize `snapshot` to JSON and upsert it over the singleton row.
    pub async fn save(&self, snapshot: &ControlStateSnapshot) -> Result<()> {
        let content = RowContent {
            data: serde_json::to_string(snapshot).context("serialize control snapshot")?,
            saved_at: chrono::Utc::now().to_rfc3339(),
        };
        let _: Option<Row> = self
            .db
            .upsert((TABLE, KEY))
            .content(content)
            .await
            .map_err(|e| anyhow::anyhow!("save control snapshot: {}", e))?;
        Ok(())
    }

    /// Load and deserialize the singleton row, if one has been saved yet.
    pub async fn load(&self) -> Result<Option<ControlStateSnapshot>> {
        let row: Option<Row> = self
            .db
            .select((TABLE, KEY))
            .await
            .map_err(|e| anyhow::anyhow!("load control snapshot: {}", e))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let snap: ControlStateSnapshot =
            serde_json::from_str(&row.data).context("parse control snapshot")?;
        Ok(Some(snap))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{ControlState, Task, TaskStatus};

    #[tokio::test]
    async fn snapshot_round_trips_through_surreal() {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealControlSnapshotStore::new(Arc::new(db));

        assert!(store.load().await.expect("empty load").is_none());

        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        {
            let mut tasks = control.tasks.write().await;
            tasks.push(Task {
                id: "rem_persist".to_string(),
                title: "Reminder: survive the redeploy".to_string(),
                task_kind: Some("reminder".to_string()),
                status: TaskStatus::Queued,
                metadata: Some(serde_json::json!({"due_at": "2026-07-18T00:00:00Z"})),
                ..Task::default()
            });
        }
        let snap = control.snapshot().await;
        store.save(&snap).await.expect("save");
        // Second save overwrites, not duplicates.
        store.save(&snap).await.expect("save again");

        let loaded = store.load().await.expect("load").expect("present");
        let restored = ControlState::from_snapshot(loaded);
        let tasks = restored.tasks.read().await;
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "rem_persist");
        assert_eq!(tasks[0].task_kind.as_deref(), Some("reminder"));
        assert!(tasks[0].metadata.is_some(), "reminder due_at survives");
    }
}
