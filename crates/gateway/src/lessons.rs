//! Production writer for director lessons.
//!
//! `record_lesson` persists immediately with `embedding: None` so the calling
//! turn never waits on an embedding round-trip, then backfills the embedding
//! in a spawned task (append is upsert-by-id, so the second write is safe).
//! Recall-side callers use `embed_query` to build a query vector with the
//! same dimension guard.

use std::sync::Arc;

use anyhow::Result;
use consistent_world_memory::embedding::EmbeddingProvider;

use crate::director_lessons_store::{LESSON_EMBEDDING_DIMENSION, SurrealDirectorLessonsStore};
use crate::state::DirectorLesson;

/// Providers whose dimension doesn't match the HNSW index (e.g. a 384-dim
/// local model) are ignored rather than corrupting the 1536-dim index.
fn embeddable(provider: &Arc<dyn EmbeddingProvider>) -> bool {
    provider.dimensions() == LESSON_EMBEDDING_DIMENSION
}

/// Embed `text` for lesson recall. Returns `None` (with a warning) on
/// provider absence, dimension mismatch, or embed failure — callers fall
/// back to recency recall.
pub async fn embed_query(
    provider: Option<&Arc<dyn EmbeddingProvider>>,
    text: &str,
) -> Option<Vec<f32>> {
    let provider = provider?;
    if !embeddable(provider) {
        return None;
    }
    match provider.embed(text).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(error = %e, "lesson query embedding failed; using recency recall");
            None
        }
    }
}

/// Persist a lesson and kick off async embedding backfill.
#[allow(clippy::too_many_arguments)]
pub async fn record_lesson(
    store: Arc<SurrealDirectorLessonsStore>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    director_id: &str,
    scope: &str,
    lesson_text: &str,
    source: &str,
    severity: &str,
    created_by: Option<&str>,
) -> Result<DirectorLesson> {
    let lesson = DirectorLesson {
        id: format!("les_{}", uuid::Uuid::new_v4().simple()),
        director_id: director_id.to_string(),
        scope: scope.to_string(),
        lesson: lesson_text.trim().to_string(),
        source: source.to_string(),
        severity: severity.to_string(),
        embedding: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        created_by: created_by.map(|s| s.to_string()),
    };
    store.append_lesson(&lesson).await?;

    if let Some(provider) = embedder.filter(embeddable) {
        let mut pending = lesson.clone();
        tokio::spawn(async move {
            match provider.embed(&pending.lesson).await {
                Ok(embedding) => {
                    pending.embedding = Some(embedding);
                    if let Err(e) = store.append_lesson(&pending).await {
                        tracing::warn!(lesson_id = %pending.id, error = %e, "lesson embedding backfill write failed");
                    }
                }
                Err(e) => {
                    tracing::warn!(lesson_id = %pending.id, error = %e, "lesson embedding failed; recency recall only");
                }
            }
        });
    }

    Ok(lesson)
}

/// How often [`spawn_embedding_backfill_sweep`]'s loop retries lessons still
/// missing an embedding.
const EMBEDDING_SWEEP_INTERVAL_SECS: u64 = 600;

/// One pass: embed every lesson still missing one, using whatever provider
/// is on hand. Split out from [`spawn_embedding_backfill_sweep`] so tests can
/// drive it directly instead of waiting on the real interval.
async fn run_embedding_backfill_pass(
    store: &SurrealDirectorLessonsStore,
    provider: &Arc<dyn EmbeddingProvider>,
) {
    match store.list_missing_embeddings().await {
        Ok(pending) if !pending.is_empty() => {
            tracing::info!(count = pending.len(), "backfilling stale lesson embeddings");
            for mut lesson in pending {
                match provider.embed(&lesson.lesson).await {
                    Ok(embedding) => {
                        lesson.embedding = Some(embedding);
                        if let Err(e) = store.append_lesson(&lesson).await {
                            tracing::warn!(lesson_id = %lesson.id, error = %e, "lesson embedding sweep write failed");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(lesson_id = %lesson.id, error = %e, "lesson embedding sweep failed; will retry next pass");
                    }
                }
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "lesson embedding sweep listing failed"),
    }
}

/// Periodically retry embedding lessons that were written (or whose one-shot
/// backfill failed) while no usable embedding provider was available — e.g.
/// during a provider outage. Without this, such lessons stay `embedding:
/// None` forever even after the provider recovers: they're excluded from
/// semantic top-k, and recency recall is a per-director top-N window that an
/// old lesson eventually ages out of, so recall silently degrades to "never"
/// rather than "recency fallback".
pub fn spawn_embedding_backfill_sweep(
    store: Arc<SurrealDirectorLessonsStore>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
) {
    let Some(provider) = embedder.filter(embeddable) else {
        return;
    };
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(
                EMBEDDING_SWEEP_INTERVAL_SECS,
            ))
            .await;
            run_embedding_backfill_pass(&store, &provider).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;
    use surrealdb::engine::any::connect;

    struct MockEmbedding {
        dim: usize,
    }

    #[async_trait]
    impl EmbeddingProvider for MockEmbedding {
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![0.5; self.dim])
        }
        fn dimensions(&self) -> usize {
            self.dim
        }
    }

    async fn mem_store() -> Arc<SurrealDirectorLessonsStore> {
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealDirectorLessonsStore::new(Arc::new(db));
        store.init_schema().await.expect("schema");
        store
    }

    #[tokio::test]
    async fn record_lesson_persists_and_backfills_embedding() {
        let store = mem_store().await;
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbedding { dim: 1536 });

        let lesson = record_lesson(
            store.clone(),
            Some(provider),
            "operations",
            "entity:ent_x",
            "always confirm units before pricing",
            "correction",
            "hard",
            Some("user_1"),
        )
        .await
        .expect("record");
        assert!(lesson.embedding.is_none(), "insert returns before embed");

        // Poll for the spawned backfill.
        let mut embedded = false;
        for _ in 0..50 {
            let rows = store.list_for_director("operations").await.expect("list");
            assert_eq!(rows.len(), 1);
            if rows[0].embedding.is_some() {
                assert_eq!(rows[0].embedding.as_ref().unwrap().len(), 1536);
                embedded = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(embedded, "embedding backfilled by spawned task");
    }

    #[tokio::test]
    async fn record_lesson_without_provider_stays_unembedded() {
        let store = mem_store().await;
        record_lesson(
            store.clone(),
            None,
            "operations",
            "global",
            "prefer terse summaries",
            "observation",
            "soft",
            None,
        )
        .await
        .expect("record");
        let rows = store.list_for_director("operations").await.expect("list");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].embedding.is_none());
    }

    #[tokio::test]
    async fn record_lesson_skips_mismatched_dimension_provider() {
        let store = mem_store().await;
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbedding { dim: 384 });
        record_lesson(
            store.clone(),
            Some(provider.clone()),
            "operations",
            "global",
            "local model must not hit the 1536 index",
            "observation",
            "soft",
            None,
        )
        .await
        .expect("record");
        // Give any (incorrect) spawn a chance to run, then assert None stuck.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let rows = store.list_for_director("operations").await.expect("list");
        assert!(rows[0].embedding.is_none());

        assert!(embed_query(Some(&provider), "query").await.is_none());
    }

    #[tokio::test]
    async fn backfill_pass_embeds_lessons_stuck_without_a_provider() {
        let store = mem_store().await;
        // Simulate a lesson written during a provider outage.
        record_lesson(
            store.clone(),
            None,
            "operations",
            "global",
            "written while embeddings were down",
            "observation",
            "soft",
            None,
        )
        .await
        .expect("record");
        let rows = store.list_for_director("operations").await.expect("list");
        assert!(rows[0].embedding.is_none());

        // Provider "recovers"; the sweep should pick up the stuck lesson.
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(MockEmbedding { dim: 1536 });
        run_embedding_backfill_pass(&store, &provider).await;

        let rows = store.list_for_director("operations").await.expect("list");
        assert_eq!(rows[0].embedding.as_ref().map(|e| e.len()), Some(1536));

        let pending = store.list_missing_embeddings().await.expect("list missing");
        assert!(
            pending.is_empty(),
            "sweep should clear the missing-embedding set"
        );
    }
}
