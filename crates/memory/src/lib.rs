//! Long-term memory for agents: session/message persistence, embeddings,
//! semantic (vector) search, and compaction of old context into summaries.

/// Trimming/summarizing message history to fit within a context window.
pub mod compaction;
/// Embedding providers (OpenAI, Gemini, local) used for semantic recall.
pub mod embedding;
/// The SurrealDB-backed [`store::MemoryStore`]: sessions, messages, memories.
pub mod store;
/// Thin compatibility layer over the store's built-in vector search.
pub mod vector;

// Re-export so integration tests and rust-analyzer see MemoryStore in scope.
pub use store::{EMBEDDING_DIMENSION, MemoryRecord, MemoryStore, SurrealMemoryStore};
