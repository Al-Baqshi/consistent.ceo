//! Adapters from the gateway's concrete stores onto the narrow `*Reader`
//! traits the `consistent-world-architect` crate uses for Inspect.
//!
//! The architect crate stays gateway-independent; these adapters are the
//! only place gateway types meet architect traits.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use consistent_world_architect::{EntityReader, LessonsReader, MemoryReader, MissionsReader};

use crate::state::ControlState;

/// Reads entities from the in-memory `ControlState` snapshot (which is itself
/// hydrated from SurrealDB at startup and re-synced every 60s). Cheaper than
/// round-tripping to Surreal for every chat turn.
pub struct ControlStateEntityReader {
    pub control: Arc<ControlState>,
}

#[async_trait]
impl EntityReader for ControlStateEntityReader {
    async fn read_entity(
        &self,
        _workspace_id: Option<&str>,
        entity_id: &str,
    ) -> Result<Option<serde_json::Value>> {
        let entities = self.control.entities.read().await;
        let found = entities.iter().find(|e| e.id == entity_id);
        Ok(found.and_then(|e| serde_json::to_value(e).ok()))
    }
}

/// Active-missions + open-tasks adapter. Missions come from the Surreal
/// mission store; tasks come from the in-memory ControlState (where they
/// live today).
pub struct ControlStateMissionsReader {
    pub control: Arc<ControlState>,
    pub mission_store: Option<Arc<crate::mission_store::SurrealMissionStore>>,
}

#[async_trait]
impl MissionsReader for ControlStateMissionsReader {
    async fn active_for_entity(&self, entity_id: &str) -> Result<Vec<serde_json::Value>> {
        // Prefer the durable Surreal mission store; fall back to the
        // in-memory snapshot when none is wired.
        if let Some(store) = &self.mission_store {
            let missions = store.list_active_for_entity(entity_id).await?;
            return Ok(missions
                .into_iter()
                .filter_map(|m| serde_json::to_value(m).ok())
                .collect());
        }
        let missions = self.control.missions.read().await;
        Ok(missions
            .iter()
            .filter(|m| m.entity_id == entity_id)
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect())
    }

    async fn open_tasks_for_entity(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let tasks = self.control.tasks.read().await;
        let mut out: Vec<_> = tasks
            .iter()
            .filter(|t| t.entity_id.as_deref() == Some(entity_id))
            // Match the variants directly. This used to test the lowercased
            // Debug string for "completed"/"failed", which silently reported
            // every *cancelled* task as still open — and every downstream
            // "no open tasks" rule inherited that.
            .filter(|t| {
                !matches!(
                    t.status,
                    crate::state::TaskStatus::Completed
                        | crate::state::TaskStatus::Failed
                        | crate::state::TaskStatus::Cancelled
                )
            })
            .filter_map(|t| serde_json::to_value(t).ok())
            .collect();
        out.truncate(limit);
        Ok(out)
    }
}

/// Entity-scoped memory recall.
///
/// `MemoryStore` is keyed by `agent_id`, and `entity_memory` already namespaces
/// an entity's notes under the synthetic ids `entity_mem:{id}:short` and
/// `:long` — six production call sites read and write through it. The comment
/// this replaced claimed the entity-scoped view still needed building; it did
/// not. What was missing was only the adapter.
pub struct EntityMemoryReader {
    pub memory: Arc<dyn consistent_world_memory::MemoryStore>,
}

#[async_trait]
impl MemoryReader for EntityMemoryReader {
    async fn recent_for_entity(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        // Long-heavy, matching the split the beat prompt already ships
        // (`director_runner::ENTITY_MEMORY_{LONG,SHORT}_IN_PROMPT`, 10 and 5):
        // durable facts earn more of a bounded budget than fresh chatter.
        let long_n = (limit * 2 / 3).max(1);
        let short_n = limit.saturating_sub(long_n);

        let long = self
            .memory
            .list_recent_memory_records(&crate::entity_memory::long_key(entity_id), long_n)
            .await
            .unwrap_or_default();
        let short = if short_n == 0 {
            Vec::new()
        } else {
            self.memory
                .list_recent_memory_records(&crate::entity_memory::short_key(entity_id), short_n)
                .await
                .unwrap_or_default()
        };

        let mut out: Vec<serde_json::Value> = Vec::with_capacity(long.len() + short.len());
        let mut seen: Vec<String> = Vec::with_capacity(long.len());
        for r in long {
            seen.push(r.content.trim().to_string());
            out.push(serde_json::json!({
                "id": r.id,
                "kind": "long",
                "content": r.content,
                "created_at": r.created_at,
            }));
        }
        for r in short {
            // `promote_repeated_short_to_long` copies a line into both
            // namespaces, so without this the same note appears twice.
            if seen.iter().any(|c| c == r.content.trim()) {
                continue;
            }
            out.push(serde_json::json!({
                "id": r.id,
                "kind": "short",
                "content": r.content,
                "created_at": r.created_at,
            }));
        }
        Ok(out)
    }
}

/// Entity-scoped lessons recall.
///
/// `director_lessons.scope` is not the free-form string the comment this
/// replaced claimed: there is exactly one production writer
/// (`chat_tools.rs`), and it writes either `global` or `entity:{id}`. This
/// reader asks for precisely those two scopes, so it matches what
/// `recall_chat_lessons` filters for in the live chat prompt — the two paths
/// cannot drift without a test noticing.
pub struct DirectorLessonsReader {
    pub store: Arc<crate::director_lessons_store::SurrealDirectorLessonsStore>,
    /// The chat path hardcodes `operations`; mirror it rather than inventing a
    /// second convention.
    pub director_id: String,
}

#[async_trait]
impl LessonsReader for DirectorLessonsReader {
    async fn recent_for_entity(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let scopes = vec!["global".to_string(), format!("entity:{entity_id}")];
        let lessons = self
            .store
            .recall_recent_for_scopes(&self.director_id, &scopes, limit)
            .await
            .unwrap_or_default();
        Ok(lessons
            .into_iter()
            .map(|l| {
                serde_json::json!({
                    "id": l.id,
                    "scope": l.scope,
                    "lesson": l.lesson,
                    "source": l.source,
                    "severity": l.severity,
                    "created_at": l.created_at,
                })
            })
            .collect())
    }
}

/// Entity knowledge documents for the snapshot.
///
/// These are the most citable rows the system holds: an imported source is the
/// one thing an answer can point at that the model did not write itself.
/// `content` is truncated before it reaches the snapshot, because the snapshot
/// is serialised whole into `audit_trail.payload`.
pub struct EntityDocsReader {
    pub store: Arc<crate::entity_docs_store::SurrealEntityDocsStore>,
}

/// Upper bound on a document body carried into a snapshot, in characters.
const DOC_CONTENT_CHARS: usize = 4000;

#[async_trait]
impl consistent_world_architect::DocsReader for EntityDocsReader {
    async fn enabled_for_entity(
        &self,
        entity_id: &str,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let docs = self
            .store
            .list_enabled_for_entity(entity_id)
            .await
            .unwrap_or_default();
        Ok(docs
            .into_iter()
            .take(limit)
            .map(|d| {
                serde_json::json!({
                    "id": d.id,
                    "title": d.title,
                    "source": d.source,
                    "url": d.url,
                    "content": d.content.chars().take(DOC_CONTENT_CHARS).collect::<String>(),
                    "created_at": d.created_at,
                })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use consistent_world_memory::{MemoryStore, SurrealMemoryStore};

    #[tokio::test]
    async fn entity_memory_reader_prefers_long_term_and_dedupes() {
        let store = SurrealMemoryStore::connect_memory().await.expect("connect");
        let memory: Arc<dyn MemoryStore> = Arc::new(store);

        // `promote_repeated_short_to_long` copies a line into both namespaces,
        // so the shared line is the realistic case, not a contrived one.
        let shared = "Invoices go out on the 1st";
        memory
            .save_memory(&crate::entity_memory::long_key("e1"), shared, None)
            .await
            .expect("long");
        memory
            .save_memory(&crate::entity_memory::short_key("e1"), shared, None)
            .await
            .expect("short dup");
        for line in ["Asked about pricing", "Wants a Friday demo"] {
            memory
                .save_memory(&crate::entity_memory::short_key("e1"), line, None)
                .await
                .expect("short");
        }
        // A second entity must never appear.
        memory
            .save_memory(&crate::entity_memory::long_key("e2"), "Other entity", None)
            .await
            .expect("e2");

        let reader = EntityMemoryReader {
            memory: memory.clone(),
        };
        let got = reader.recent_for_entity("e1", 6).await.expect("read");

        let contents: Vec<String> = got
            .iter()
            .map(|v| v["content"].as_str().unwrap_or_default().to_string())
            .collect();
        assert_eq!(
            contents.iter().filter(|c| c.as_str() == shared).count(),
            1,
            "a promoted line must not appear twice: {contents:?}"
        );
        assert_eq!(
            got[0]["kind"], "long",
            "durable facts lead the bounded slice"
        );
        assert!(
            contents.iter().all(|c| c != "Other entity"),
            "another entity's memory leaked: {contents:?}"
        );
        for v in &got {
            assert!(
                !v["id"].as_str().unwrap_or_default().is_empty(),
                "a citable row needs an id"
            );
        }
    }

    // The reader and the live chat prompt must select the same lessons. If this
    // drifts, the audit trail describes a world the model never saw.
    #[tokio::test]
    async fn lessons_reader_matches_the_chat_path_filter() {
        use crate::director_lessons_store::SurrealDirectorLessonsStore;
        use crate::state::DirectorLesson;
        use surrealdb::engine::any::connect;

        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealDirectorLessonsStore::new(Arc::new(db));
        store.init_schema().await.expect("init");

        let mk = |id: &str, scope: &str, at: &str| DirectorLesson {
            id: id.to_string(),
            director_id: "operations".to_string(),
            scope: scope.to_string(),
            lesson: format!("lesson {id}"),
            source: "correction".to_string(),
            severity: "soft".to_string(),
            embedding: None,
            created_at: at.to_string(),
            created_by: None,
        };
        for l in [
            mk("g", "global", "2026-05-20T00:00:00Z"),
            mk("mine", "entity:e1", "2026-05-21T00:00:00Z"),
            mk("theirs", "entity:e2", "2026-05-22T00:00:00Z"),
        ] {
            store.append_lesson(&l).await.expect("append");
        }

        let reader = DirectorLessonsReader {
            store: store.clone(),
            director_id: "operations".to_string(),
        };
        let got = reader.recent_for_entity("e1", 10).await.expect("read");
        let ids: Vec<&str> = got
            .iter()
            .map(|v| v["id"].as_str().unwrap_or_default())
            .collect();

        // Exactly what `chat_agent::recall_chat_lessons` keeps for e1:
        // `scope == "global" || scope == "entity:e1"`, newest first.
        assert_eq!(ids, vec!["mine", "g"]);
        assert!(
            got.iter().all(|v| v.get("embedding").is_none()),
            "embedding must never reach the audit payload"
        );
    }
}
