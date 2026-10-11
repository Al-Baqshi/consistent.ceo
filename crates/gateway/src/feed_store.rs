//! Surreal-backed persistence for operator feed and action events.

use anyhow::Result;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;

use crate::state::{ActionEvent, OperatorFeedEvent};

/// Retained operator-feed rows. This is the activity timeline's depth, not the
/// system's memory: durable history (task completions, streaks) lives on
/// permanent `control_tasks` rows, so this bound only decides how far back the
/// UI can scroll.
const FEED_MAX: usize = 2000;

/// Slack above [`FEED_MAX`] before a prune pass runs. Pruning selects the whole
/// table, so doing it on every append made each write O(table).
const FEED_PRUNE_SLACK: usize = 100;

/// Surreal-backed store for the operator feed and action-event tables.
pub struct FeedStore {
    db: Arc<Surreal<Any>>,
}

impl FeedStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self { db })
    }

    /// Define the `operator_feed` and `action_events` tables and their
    /// workspace-id indexes. Safe to call repeatedly (idempotent DDL).
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE operator_feed SCHEMALESS;
                DEFINE FIELD workspace_id ON operator_feed TYPE option<string>;
                DEFINE INDEX operator_feed_ws_idx ON operator_feed FIELDS workspace_id;

                DEFINE TABLE action_events SCHEMALESS;
                DEFINE FIELD workspace_id ON action_events TYPE option<string>;
                DEFINE INDEX action_events_ws_idx ON action_events FIELDS workspace_id;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("FeedStore schema warning: {}", err);
        }
        Ok(())
    }

    /// Insert one operator-feed row, then prune if the table has drifted too
    /// far past [`FEED_MAX`].
    pub async fn append_operator_feed_event(
        &self,
        event: &OperatorFeedEvent,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        let _res: Vec<OperatorFeedEvent> = self
            .db
            .insert("operator_feed")
            .content(serde_json::json!({
                "id": event.id,
                "timestamp": event.timestamp,
                "kind": event.kind,
                "task_id": event.task_id,
                "mission_id": event.mission_id,
                "entity_id": event.entity_id,
                "connection_id": event.connection_id,
                "title": event.title,
                "detail": event.detail,
                "approval_id": event.approval_id,
                "workspace_id": workspace_id,
            }))
            .await?;

        self.prune_if_needed().await
    }

    /// Trim the feed back to [`FEED_MAX`], oldest first, once it has drifted
    /// [`FEED_PRUNE_SLACK`] rows past the bound.
    async fn prune_if_needed(&self) -> Result<()> {
        let mut all: Vec<OperatorFeedEvent> = self.db.select("operator_feed").await?;
        if all.len() <= FEED_MAX + FEED_PRUNE_SLACK {
            return Ok(());
        }
        // `select` gives no ordering guarantee, so sort before deciding what is
        // oldest — otherwise a prune can drop the newest rows.
        all.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        let to_delete = all.len() - FEED_MAX;
        for old in all.into_iter().take(to_delete) {
            let _: Option<OperatorFeedEvent> =
                self.db.delete(("operator_feed", old.id.as_str())).await?;
        }
        Ok(())
    }

    /// Most recent feed events, newest first, optionally scoped to one entity.
    pub async fn list_operator_feed(
        &self,
        workspace_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<OperatorFeedEvent>> {
        let mut res = self
            .db
            .query("SELECT * FROM operator_feed ORDER BY timestamp DESC LIMIT $lim")
            .bind(("lim", limit as i64))
            .await?;
        let rows: Vec<OperatorFeedEvent> = res.take(0)?;
        // Scoping is applied here on entity_id, after the query returns.
        let filtered: Vec<_> = if let Some(ws) = workspace_id {
            rows.into_iter()
                .filter(|e| e.entity_id.as_deref() == Some(ws))
                .collect()
        } else {
            rows
        };
        Ok(filtered)
    }

    /// Insert one action-event row (capability invocations, approvals, etc.).
    /// Unlike the operator feed, this table is not pruned.
    pub async fn append_action_event(
        &self,
        event: &ActionEvent,
        workspace_id: Option<&str>,
    ) -> Result<()> {
        let _res: Vec<ActionEvent> = self
            .db
            .insert("action_events")
            .content(serde_json::json!({
                "id": event.id,
                "timestamp": event.timestamp,
                "scope_type": event.scope_type,
                "scope_id": event.scope_id,
                "entity_id": event.entity_id,
                "task_id": event.task_id,
                "agent_id": event.agent_id,
                "capability": event.capability,
                "execution_level": event.execution_level,
                "target": event.target,
                "outcome": event.outcome,
                "reason": event.reason,
                "workspace_id": workspace_id,
            }))
            .await?;
        Ok(())
    }

    /// Most recent action events, newest first, optionally scoped to one entity.
    pub async fn list_action_events(
        &self,
        workspace_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ActionEvent>> {
        let mut res = self
            .db
            .query("SELECT * FROM action_events ORDER BY timestamp DESC LIMIT $lim")
            .bind(("lim", limit as i64))
            .await?;
        let rows: Vec<ActionEvent> = res.take(0)?;
        // Scoping is applied here on entity_id, after the query returns.
        let filtered: Vec<_> = if let Some(ws) = workspace_id {
            rows.into_iter()
                .filter(|e| e.entity_id.as_deref() == Some(ws))
                .collect()
        } else {
            rows
        };
        Ok(filtered)
    }

    /// Drop every feed and action row for an entity. Part of the entity-delete
    /// cascade — mirrors the in-memory `feed_references_entity` removal so the
    /// rows don't linger in Surreal after the entity is gone.
    pub async fn delete_for_entity(&self, entity_id: &str) -> Result<()> {
        let mut res = self
            .db
            .query("DELETE FROM operator_feed WHERE entity_id = $ent")
            .query("DELETE FROM action_events WHERE entity_id = $ent")
            .bind(("ent", entity_id.to_string()))
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("FeedStore entity cascade warning: {}", err);
        }
        Ok(())
    }
}
