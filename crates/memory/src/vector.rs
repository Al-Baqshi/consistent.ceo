//! Vector search over memories. Semantic search is implemented in MemoryStore::search_memories_semantic
//! using SurrealDB's HNSW index on the memories.embedding field. This module provides a thin
//! compatibility layer; callers typically use MemoryStore::search_memories_semantic directly.

use crate::store::MemoryStore;
use std::sync::Arc;

/// Result of a vector search (id, similarity score, content).
#[derive(Debug)]
pub struct SearchResult {
    pub id: String,
    pub score: f32,
    pub content: String,
}

/// Thin wrapper around MemoryStore for semantic search. All vector search is performed by
/// SurrealDB (HNSW index on memories.embedding). Use MemoryStore::search_memories_semantic
/// for async semantic recall; this struct is retained for API compatibility.
pub struct VectorSearch {
    #[allow(dead_code)]
    store: Arc<dyn MemoryStore>,
}

impl VectorSearch {
    // `_db_path` is unused; kept for API compatibility with older call sites.
    pub fn new(store: Arc<dyn MemoryStore>, _db_path: &str) -> Self {
        Self { store }
    }

    /// No-op; schema and index are managed by SurrealMemoryStore at connect time.
    pub fn init(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Insert is not used; embeddings are stored via MemoryStore::save_memory(..., Some(embedding)).
    pub fn insert(&self, _id: &str, _embedding: &[f32], _metadata: &str) -> anyhow::Result<()> {
        Ok(())
    }

    /// Semantic search is done via MemoryStore::search_memories_semantic (SurrealDB).
    /// This stub returns empty; use the store's async method directly for recall.
    pub fn search(
        &self,
        _agent_id: &str,
        _query_embedding: &[f32],
        _top_k: usize,
    ) -> anyhow::Result<Vec<SearchResult>> {
        Ok(Vec::new())
    }
}
