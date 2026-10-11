//! Tests for `SurrealMemoryStore` (sessions, messages, semantic vector
//! search) and `Compactor` (history-trimming threshold/behavior).

use chrono::Utc;
use consistent_world_common::types::{ChannelKind, ScopeType, Session};
use consistent_world_memory::compaction::{CompactionMode, Compactor};
use consistent_world_memory::store::{EMBEDDING_DIMENSION, MemoryStore, SurrealMemoryStore};

/// Build a 1536-d vector from first 3 components (rest zeros) so SurrealDB HNSW index accepts it.
fn emb(v: [f32; 3]) -> Vec<f32> {
    let mut out = vec![0.0f32; EMBEDDING_DIMENSION as usize];
    out[0] = v[0];
    out[1] = v[1];
    out[2] = v[2];
    out
}

#[tokio::test]
async fn test_memory_store_init() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    store
        .save_message("1", "session-1", "user", "hello")
        .await
        .unwrap();
}

#[tokio::test]
async fn test_save_and_retrieve_message() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    store
        .save_message("1", "session-1", "user", "hello")
        .await
        .unwrap();
    store
        .save_message("2", "session-1", "assistant", "hi")
        .await
        .unwrap();
    store
        .save_message("3", "session-1", "user", "how are you?")
        .await
        .unwrap();

    let history = store.get_session_history("session-1", 10).await.unwrap();
    assert_eq!(history, vec!["hello", "hi", "how are you?"]);
}

#[tokio::test]
async fn test_get_session_history_limit() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    for i in 0..20 {
        let id = format!("msg-{i}");
        let content = format!("message-{i}");
        store
            .save_message(&id, "session-2", "user", &content)
            .await
            .unwrap();
    }

    let history = store.get_session_history("session-2", 5).await.unwrap();
    assert_eq!(history.len(), 5);
    assert_eq!(history[0], "message-15");
    assert_eq!(history[4], "message-19");
}

#[test]
fn test_compaction_threshold() {
    let compactor = Compactor::new(CompactionMode::Safeguard, 100);
    assert!(!compactor.should_compact(50));
    assert!(compactor.should_compact(150));

    let messages: Vec<String> = (0..15).map(|i| format!("msg-{i}")).collect();
    let compacted = compactor.compact(&messages).unwrap();
    assert_eq!(compacted.len(), 11);
    assert!(compacted[0].starts_with("[summary]"));
    assert_eq!(compacted.last().unwrap(), "msg-14");
}

#[tokio::test]
async fn test_session_persists() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    let session = Session {
        id: "web-user1".to_string(),
        agent_id: "default".to_string(),
        channel: ChannelKind::Web,
        peer_id: "user1".to_string(),
        scope_type: ScopeType::Principal,
        scope_id: "web:user1".to_string(),
        channel_binding_metadata: None,
        resolution_reason: Some("test".to_string()),
        created_at: Utc::now(),
        last_active: Utc::now(),
    };
    store.upsert_session(&session).await.unwrap();

    let sessions = store.list_sessions().await.unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "web-user1");
    assert_eq!(sessions[0].agent_id, "default");
    assert!(matches!(sessions[0].channel, ChannelKind::Web));
    assert_eq!(sessions[0].peer_id, "user1");
}

// Vector search: SurrealDB schema uses HNSW DIMENSION 1536. Tests use 3-d vectors; SurrealDB may reject or use brute-force.
// Skip semantic vector test if dimension mismatch; or use 1536-d stub vectors. For simplicity we test with 3-d and hope
// the in-memory engine accepts (some engines allow brute-force without index match).
#[tokio::test]
async fn test_vector_search() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    let agent_id = "agent1";
    store
        .save_memory(agent_id, "first", Some(&emb([1.0, 0.0, 0.0])))
        .await
        .unwrap();
    store
        .save_memory(agent_id, "second", Some(&emb([0.9, 0.1, 0.0])))
        .await
        .unwrap();
    store
        .save_memory(agent_id, "third", Some(&emb([0.8, 0.2, 0.0])))
        .await
        .unwrap();
    store
        .save_memory(agent_id, "fourth", Some(&emb([0.0, 1.0, 0.0])))
        .await
        .unwrap();
    store
        .save_memory(agent_id, "fifth", Some(&emb([0.0, 0.0, 1.0])))
        .await
        .unwrap();

    let top = store
        .search_memories_semantic(agent_id, &emb([1.0, 0.0, 0.0]), 3)
        .await
        .unwrap();
    assert_eq!(top.len(), 3);
    assert_eq!(top[0], "first");
    assert_eq!(top[1], "second");
    assert_eq!(top[2], "third");

    let top1 = store
        .search_memories_semantic(agent_id, &emb([0.0, 0.0, 1.0]), 1)
        .await
        .unwrap();
    assert_eq!(top1.len(), 1);
    assert_eq!(top1[0], "fifth");
}

// Regression: SurrealDB 2.x rejects ORDER BY on a field missing from the
// projection ("Missing order idiom"), and every recall caller swallowed the
// error via unwrap_or_default() — so recency recall silently returned empty
// for months. This test fails loudly if the projection regresses.
#[tokio::test]
async fn test_list_recent_memories_returns_rows_newest_first() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    let agent_id = "agent-recency";
    for line in ["oldest", "middle", "newest"] {
        store.save_memory(agent_id, line, None).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let recent = store.list_recent_memories(agent_id, 2).await.unwrap();
    assert_eq!(recent.len(), 2, "recall must not be silently empty");
    assert_eq!(recent[0], "newest");
    assert_eq!(recent[1], "middle");

    let none = store.list_recent_memories("someone-else", 5).await.unwrap();
    assert!(none.is_empty(), "recall must stay agent-scoped");
}

#[tokio::test]
async fn test_semantic_recall_in_agent() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    let agent_id = "agent1";
    store
        .save_memory(agent_id, "User loves coffee", Some(&emb([0.95, 0.05, 0.0])))
        .await
        .unwrap();
    store
        .save_memory(agent_id, "Weather is sunny", Some(&emb([0.0, 0.0, 1.0])))
        .await
        .unwrap();

    let query_embedding = emb([0.92, 0.08, 0.0]);
    let recalled = store
        .search_memories_semantic(agent_id, &query_embedding, 2)
        .await
        .unwrap();
    assert!(!recalled.is_empty());
    assert_eq!(recalled[0], "User loves coffee");
}

// Semantic recall must stay inside one agent's namespace. `test_semantic_recall_in_agent`
// only ever writes as one agent, so nothing pinned cross-agent leakage until now.
// This is also the permanent lock on the knn trap documented in
// `director_lessons_store::recall_lessons`: if anyone ever indexes
// `memories.agent_id`, an equality condition can short-circuit the knn operator
// to zero rows — and this test fails loudly instead of recall going quietly empty.
#[tokio::test]
async fn semantic_recall_does_not_leak_across_agents() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    store
        .save_memory("agent-a", "Ships on Fridays", Some(&emb([0.95, 0.05, 0.0])))
        .await
        .unwrap();
    store
        .save_memory("agent-a", "Prefers dark roast", Some(&emb([0.0, 0.0, 1.0])))
        .await
        .unwrap();
    store
        .save_memory(
            "agent-b",
            "Ships on Mondays",
            Some(&emb([0.94, 0.06, 0.0])),
        )
        .await
        .unwrap();

    let recalled = store
        .search_memories_semantic("agent-a", &emb([0.92, 0.08, 0.0]), 2)
        .await
        .unwrap();

    assert!(!recalled.is_empty(), "recall must not be silently empty");
    assert!(
        recalled.iter().all(|c| c != "Ships on Mondays"),
        "agent-b's memory leaked into agent-a's recall: {recalled:?}"
    );
}

// `list_recent_memories` returns bare strings, which is enough to build a prompt
// but not enough to cite: a claim can only resolve to a row that has an id and an
// instant. This pins that the record form carries both.
#[tokio::test]
async fn recent_memory_records_carry_id_and_timestamp() {
    let store = SurrealMemoryStore::connect_memory().await.unwrap();
    let agent_id = "agent-records";
    for line in ["oldest", "middle", "newest"] {
        store.save_memory(agent_id, line, None).await.unwrap();
        // Ordering is by RFC3339 string, so the writes need distinct instants.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }

    let rows = store
        .list_recent_memory_records(agent_id, 2)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2, "limit must truncate");
    assert_eq!(rows[0].content, "newest");
    assert_eq!(rows[1].content, "middle");
    for r in &rows {
        assert_eq!(r.agent_id, agent_id);
        assert!(!r.id.is_empty(), "a citable row needs an id");
        assert!(
            !r.id.contains(':'),
            "id must be the bare key, not `memories:xxx`: {}",
            r.id
        );
        assert!(!r.created_at.is_empty(), "a citable row needs an instant");
    }
}
