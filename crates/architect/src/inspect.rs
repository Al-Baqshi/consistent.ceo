//! ① Inspect — read entity state, recent memories, recent lessons, active
//! missions, and open tasks BEFORE any LLM call. Returns a [`Snapshot`] that
//! is the only allowed input to Forward / Generate / Verify in later phases.
//!
//! The crate stays decoupled from the gateway by reading through narrow
//! `*Reader` traits. The gateway implements these via thin adapters over its
//! Surreal stores.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Bundle of reader handles. All optional so missing stores degrade
/// gracefully (the snapshot just omits that slice).
#[derive(Clone, Default)]
pub struct SnapshotSources {
    pub entities: Option<Arc<dyn EntityReader>>,
    pub memories: Option<Arc<dyn MemoryReader>>,
    pub lessons: Option<Arc<dyn LessonsReader>>,
    pub missions: Option<Arc<dyn MissionsReader>>,
    pub docs: Option<Arc<dyn DocsReader>>,
}

/// Looks up a single entity by id for the snapshot.
#[async_trait]
pub trait EntityReader: Send + Sync {
    /// Resolve an entity to a JSON view. `workspace_id` is advisory — adapters
    /// may use it to scope multi-tenant reads.
    async fn read_entity(
        &self,
        workspace_id: Option<&str>,
        entity_id: &str,
    ) -> Result<Option<serde_json::Value>>;
}

/// Fetches an entity's most recent memories, newest first.
#[async_trait]
pub trait MemoryReader: Send + Sync {
    async fn recent_for_entity(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>>;
}

/// Fetches an entity's most recent lessons, newest first.
#[async_trait]
pub trait LessonsReader: Send + Sync {
    async fn recent_for_entity(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>>;
}

/// Fetches an entity's enabled knowledge documents.
///
/// These are the most citable rows in the system — an imported source is the
/// one thing an answer can point at that the model did not write itself — so
/// Verify wants them in the snapshot.
#[async_trait]
pub trait DocsReader: Send + Sync {
    async fn enabled_for_entity(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>>;
}

/// Fetches an entity's active missions and open tasks.
#[async_trait]
pub trait MissionsReader: Send + Sync {
    /// Missions currently in progress for this entity.
    async fn active_for_entity(&self, entity_id: &str) -> Result<Vec<serde_json::Value>>;
    /// Tasks not yet completed, most relevant first.
    async fn open_tasks_for_entity(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>>;
}

/// What Inspect produces. JSON-shaped on purpose: keeps the architect crate
/// independent of gateway types and makes audit-log diffing trivial.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub workspace_id: Option<String>,
    pub entity_id: Option<String>,
    pub entity: Option<serde_json::Value>,
    pub recent_memories: Vec<serde_json::Value>,
    pub recent_lessons: Vec<serde_json::Value>,
    pub active_missions: Vec<serde_json::Value>,
    pub open_tasks: Vec<serde_json::Value>,
    /// Enabled knowledge documents. `#[serde(default)]` so audit rows written
    /// before this field existed still deserialize.
    #[serde(default)]
    pub entity_docs: Vec<serde_json::Value>,
    pub inspected_at: String,
}

impl Snapshot {
    /// One-line human note for the audit log so reviewers don't have to expand
    /// every payload to see what Inspect found.
    pub fn summary_note(&self) -> String {
        let ent = self.entity.is_some();
        format!(
            "entity={} mem={} lessons={} missions={} tasks={} docs={}",
            if ent { "yes" } else { "no" },
            self.recent_memories.len(),
            self.recent_lessons.len(),
            self.active_missions.len(),
            self.open_tasks.len(),
            self.entity_docs.len(),
        )
    }
}

/// How many knowledge documents Inspect pulls into a snapshot. Bounded because
/// the snapshot is written whole into `audit_trail.payload`.
pub const DOCS_IN_SNAPSHOT: usize = 12;

/// Read everything the future Architect steps need, in parallel where
/// possible. Missing readers yield empty slices, not errors — Phase 1 is
/// shadow mode and must not surface failures to the user.
pub async fn inspect(
    sources: &SnapshotSources,
    workspace_id: Option<&str>,
    entity_id: Option<&str>,
) -> Result<Snapshot> {
    let inspected_at = crate::now_iso();
    let mut snap = Snapshot {
        workspace_id: workspace_id.map(|s| s.to_string()),
        entity_id: entity_id.map(|s| s.to_string()),
        entity: None,
        recent_memories: Vec::new(),
        recent_lessons: Vec::new(),
        active_missions: Vec::new(),
        open_tasks: Vec::new(),
        entity_docs: Vec::new(),
        inspected_at,
    };

    // No entity to inspect — return the shell snapshot (workspace-only context).
    let Some(eid) = entity_id else {
        return Ok(snap);
    };

    if let Some(reader) = &sources.entities {
        match reader.read_entity(workspace_id, eid).await {
            Ok(v) => snap.entity = v,
            Err(e) => tracing::warn!(error = %e, entity_id = %eid, "inspect: entity read failed"),
        }
    }
    if let Some(reader) = &sources.memories {
        match reader.recent_for_entity(eid, 10).await {
            Ok(v) => snap.recent_memories = v,
            Err(e) => tracing::warn!(error = %e, entity_id = %eid, "inspect: memories read failed"),
        }
    }
    if let Some(reader) = &sources.lessons {
        match reader.recent_for_entity(eid, 10).await {
            Ok(v) => snap.recent_lessons = v,
            Err(e) => tracing::warn!(error = %e, entity_id = %eid, "inspect: lessons read failed"),
        }
    }
    if let Some(reader) = &sources.docs {
        match reader.enabled_for_entity(eid, DOCS_IN_SNAPSHOT).await {
            Ok(v) => snap.entity_docs = v,
            Err(e) => tracing::warn!(error = %e, entity_id = %eid, "inspect: docs read failed"),
        }
    }
    if let Some(reader) = &sources.missions {
        match reader.active_for_entity(eid).await {
            Ok(v) => snap.active_missions = v,
            Err(e) => tracing::warn!(error = %e, entity_id = %eid, "inspect: missions read failed"),
        }
        match reader.open_tasks_for_entity(eid, 20).await {
            Ok(v) => snap.open_tasks = v,
            Err(e) => tracing::warn!(error = %e, entity_id = %eid, "inspect: tasks read failed"),
        }
    }

    Ok(snap)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CountingEntityReader;
    #[async_trait]
    impl EntityReader for CountingEntityReader {
        async fn read_entity(
            &self,
            _workspace_id: Option<&str>,
            entity_id: &str,
        ) -> Result<Option<serde_json::Value>> {
            Ok(Some(serde_json::json!({ "id": entity_id, "name": "test" })))
        }
    }

    #[tokio::test]
    async fn inspect_with_no_entity_returns_empty_snapshot() {
        let snap = inspect(&SnapshotSources::default(), Some("ws"), None)
            .await
            .expect("ok");
        assert!(snap.entity.is_none());
        assert!(snap.recent_memories.is_empty());
    }

    #[tokio::test]
    async fn inspect_reads_entity_when_reader_present() {
        let sources = SnapshotSources {
            entities: Some(Arc::new(CountingEntityReader)),
            ..Default::default()
        };
        let snap = inspect(&sources, Some("ws"), Some("baqshi"))
            .await
            .expect("ok");
        assert!(snap.entity.is_some());
        assert_eq!(
            snap.summary_note(),
            "entity=yes mem=0 lessons=0 missions=0 tasks=0 docs=0"
        );
    }
}
