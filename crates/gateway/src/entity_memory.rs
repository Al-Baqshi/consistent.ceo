//! Entity-scoped memory namespaces on top of `MemoryStore` (`agent_id` = namespace key).

use std::sync::Arc;

use consistent_world_memory::store::MemoryStore;

// `MemoryStore` is keyed by `agent_id`; these namespace an entity's short-term
// and long-term memories under distinct synthetic agent ids.
pub(crate) fn short_key(entity_id: &str) -> String {
    format!("entity_mem:{entity_id}:short")
}

pub(crate) fn long_key(entity_id: &str) -> String {
    format!("entity_mem:{entity_id}:long")
}

/// Recent short-term memories for planner / heartbeat context.
pub async fn recall_entity_short(
    memory: Arc<dyn MemoryStore>,
    entity_id: &str,
    limit: usize,
) -> Vec<String> {
    memory
        .list_recent_memories(&short_key(entity_id), limit)
        .await
        .unwrap_or_default()
}

/// Long-term memories for an entity — the subset promoted from short-term by
/// [`promote_repeated_short_to_long`].
pub async fn recall_entity_long(
    memory: Arc<dyn MemoryStore>,
    entity_id: &str,
    limit: usize,
) -> Vec<String> {
    memory
        .list_recent_memories(&long_key(entity_id), limit)
        .await
        .unwrap_or_default()
}

/// If the same short-term line appears `min_hits` times, append once to long-term (simple promotion).
/// Append one short-term line for an entity (e.g. after an approval event).
pub async fn append_entity_short_line(
    memory: Arc<dyn MemoryStore>,
    entity_id: &str,
    line: &str,
) -> anyhow::Result<()> {
    if line.trim().is_empty() {
        return Ok(());
    }
    memory.save_memory(&short_key(entity_id), line, None).await
}

/// Delete both memory namespaces for an entity. Part of the entity-delete
/// cascade.
pub async fn forget_entity(memory: Arc<dyn MemoryStore>, entity_id: &str) -> anyhow::Result<()> {
    memory
        .delete_memories_for_agent(&short_key(entity_id))
        .await?;
    memory.delete_memories_for_agent(&long_key(entity_id)).await
}

/// Promote `line` to long-term storage once it has recurred `min_hits` times
/// in recent short-term memory, skipping it if it's already there.
pub async fn promote_repeated_short_to_long(
    memory: Arc<dyn MemoryStore>,
    entity_id: &str,
    line: &str,
    min_hits: usize,
) -> anyhow::Result<()> {
    if line.trim().is_empty() || min_hits <= 1 {
        return Ok(());
    }
    let recent = recall_entity_short(memory.clone(), entity_id, 64).await;
    let hits = recent.iter().filter(|s| s.trim() == line.trim()).count();
    if hits + 1 >= min_hits {
        let long = recall_entity_long(memory.clone(), entity_id, 200).await;
        if !long.iter().any(|s| s.trim() == line.trim()) {
            memory.save_memory(&long_key(entity_id), line, None).await?;
        }
    }
    Ok(())
}
