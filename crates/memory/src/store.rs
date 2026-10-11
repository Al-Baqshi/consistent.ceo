//! SurrealDB-backed memory store: sessions, messages, memories (with vector search).

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use consistent_world_common::types::{ChannelKind, ScopeType, Session};
use serde::Serialize;
use serde::de::IgnoredAny;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use surrealdb::opt::auth::Root;
use uuid::Uuid;

/// Extract the key part from a RecordId (e.g. "session-1" from "sessions:`session-1`").
fn record_id_to_key_string(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    if let Some(rest) = s.split_once(':').map(|x| x.1) {
        rest.trim_matches('`').to_string()
    } else {
        s
    }
}

/// Embedding dimension for HNSW index (e.g. text-embedding-3-small).
pub const EMBEDDING_DIMENSION: u32 = 1536;

/// Async memory store interface (SurrealDB-backed).
#[async_trait]
pub trait MemoryStore: Send + Sync {
    /// Upsert a session (insert or replace by id).
    async fn upsert_session(&self, session: &Session) -> Result<()>;
    /// List all persisted sessions.
    async fn list_sessions(&self) -> Result<Vec<Session>>;
    /// Save a message to a session.
    async fn save_message(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<()>;
    /// Get session message contents only (newest first, then reversed).
    async fn get_session_history(&self, session_id: &str, limit: usize) -> Result<Vec<String>>;
    /// Get session messages as (role, content) pairs (newest first, then reversed).
    async fn get_session_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>>;
    /// Save a memory with optional embedding.
    async fn save_memory(
        &self,
        agent_id: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Result<()>;
    /// Delete all messages for a session.
    async fn delete_session(&self, session_id: &str) -> Result<()>;
    /// Semantic search over memories; returns top-k contents by cosine similarity.
    async fn search_memories_semantic(
        &self,
        agent_id: &str,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<String>>;
    /// Recent memory rows for an agent/namespace id (newest first).
    async fn list_recent_memories(&self, agent_id: &str, limit: usize) -> Result<Vec<String>>;
    /// Recent memory rows with their id and timestamp (newest first).
    ///
    /// Defaulted rather than required: [`list_recent_memories`] already answers
    /// "what does this agent remember", and every store that only needs to feed
    /// a prompt can keep implementing just that one. Implementations backed by a
    /// real table should override this so the rows can be cited.
    ///
    /// [`list_recent_memories`]: MemoryStore::list_recent_memories
    async fn list_recent_memory_records(
        &self,
        agent_id: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        Ok(self
            .list_recent_memories(agent_id, limit)
            .await?
            .into_iter()
            .map(|content| MemoryRecord {
                id: String::new(),
                agent_id: agent_id.to_string(),
                content,
                created_at: String::new(),
            })
            .collect())
    }
    /// Delete every memory row for an agent/namespace id (e.g. an entity's
    /// `entity_mem:{id}:short` namespace when the entity is deleted).
    async fn delete_memories_for_agent(&self, agent_id: &str) -> Result<()>;
}

/// SurrealDB-backed implementation of MemoryStore.
pub struct SurrealMemoryStore {
    db: Arc<Surreal<Any>>,
}

impl fmt::Debug for SurrealMemoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SurrealMemoryStore").finish()
    }
}

impl SurrealMemoryStore {
    /// Connect to a SurrealDB server (ws:// or wss://) and use the given namespace and database.
    ///
    /// When the server is started with root credentials (e.g. Docker
    /// `start --user root --pass root`), pass `Some((user, pass))` so the client can
    /// [`signin`](Surreal::signin) before `USE NS` / `USE DB`. Omit for unauthenticated dev servers.
    pub async fn connect(
        url: &str,
        ns: &str,
        database: &str,
        root_credentials: Option<(&str, &str)>,
    ) -> Result<Self> {
        // `engine::any::connect()` expects `ws://host:8000` and handles the `/rpc`
        // endpoint internally. Some configs/docs use `ws://host:8000/rpc`, so we
        // normalize to avoid double-`/rpc` or handshake failures.
        let normalized_url = url.trim_end_matches('/');
        let connect_url = normalized_url
            .strip_suffix("/rpc")
            .unwrap_or(normalized_url);

        let db = surrealdb::engine::any::connect(connect_url)
            .await
            .with_context(|| format!("Failed to connect to SurrealDB at {connect_url}"))?;
        if let Some((username, password)) = root_credentials {
            db.signin(Root { username, password })
                .await
                .map_err(|e| anyhow::anyhow!("SurrealDB signin (root): {e}"))?;
        }
        db.use_ns(ns)
            .use_db(database)
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB use_ns/use_db: {}", e))?;
        let store = Self { db: Arc::new(db) };
        store.init_schema().await?;
        Ok(store)
    }

    /// Create an in-memory store for tests (no server required).
    pub async fn connect_memory() -> Result<Self> {
        let db = surrealdb::engine::any::connect("memory")
            .await
            .with_context(|| "Failed to create in-memory SurrealDB")?;
        db.use_ns("consistent-world")
            .use_db("test")
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB use_ns/use_db: {}", e))?;
        let store = Self { db: Arc::new(db) };
        store.init_schema().await?;
        Ok(store)
    }

    // Define tables/fields/indexes if they don't already exist. `DEFINE` is
    // idempotent, so this is safe to run on every connect; any errors are
    // logged rather than propagated since they're typically "already defined".
    async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE sessions;
                DEFINE FIELD agent_id ON sessions TYPE string;
                DEFINE FIELD channel ON sessions TYPE string;
                DEFINE FIELD peer_id ON sessions TYPE string;
                DEFINE FIELD created_at ON sessions TYPE string;
                DEFINE FIELD last_active ON sessions TYPE string;

                DEFINE TABLE messages;
                DEFINE FIELD session_id ON messages TYPE string;
                DEFINE FIELD role ON messages TYPE string;
                DEFINE FIELD content ON messages TYPE string;
                DEFINE FIELD timestamp ON messages TYPE string;
                DEFINE INDEX messages_session_idx ON messages FIELDS session_id;

                DEFINE TABLE memories;
                DEFINE FIELD agent_id ON memories TYPE string;
                DEFINE FIELD content ON memories TYPE string;
                DEFINE FIELD embedding ON memories TYPE option<array<float>>;
                DEFINE FIELD created_at ON memories TYPE string;
                DEFINE INDEX hnsw_embed ON memories FIELDS embedding HNSW DIMENSION 1536 DIST COSINE;

                DEFINE TABLE scheduled_tasks;
                DEFINE FIELD cron_expression ON scheduled_tasks TYPE string;
                DEFINE FIELD next_run ON scheduled_tasks TYPE string;
                DEFINE FIELD agent_id ON scheduled_tasks TYPE string;
                DEFINE FIELD task_description ON scheduled_tasks TYPE string;
                "#,
            )
            .await
            .context("SurrealDB schema init")?;
        for (_index, err) in res.take_errors() {
            tracing::warn!("SurrealDB schema warning: {}", err);
        }
        Ok(())
    }

    /// Return the underlying SurrealDB connection for use by ScheduleStore (same ns/db).
    pub fn db(&self) -> Arc<Surreal<Any>> {
        Arc::clone(&self.db)
    }
}

#[async_trait]
impl MemoryStore for SurrealMemoryStore {
    async fn upsert_session(&self, session: &Session) -> Result<()> {
        let channel = session.channel.as_str();
        let created_at = session.created_at.to_rfc3339();
        let last_active = session.last_active.to_rfc3339();
        let row = SessionRowContent {
            id: session.id.clone(),
            agent_id: session.agent_id.clone(),
            channel: channel.to_string(),
            peer_id: session.peer_id.clone(),
            created_at: created_at.clone(),
            last_active: last_active.clone(),
        };
        let _: Option<IgnoredAny> = self
            .db
            .upsert(("sessions", session.id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB upsert session: {}", e))?;
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<Session>> {
        let rows: Vec<SessionRow> = self
            .db
            .select("sessions")
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB select sessions: {}", e))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let channel = ChannelKind::from_str(&r.channel).unwrap_or(ChannelKind::Web);
            // RecordId displays as "table:`key`"; we want only the key for API compatibility.
            let id_str = record_id_to_key_string(&r.id);
            out.push(Session {
                id: id_str,
                agent_id: r.agent_id,
                channel,
                peer_id: r.peer_id,
                scope_type: ScopeType::Principal,
                scope_id: String::new(),
                channel_binding_metadata: None,
                resolution_reason: None,
                created_at: DateTime::parse_from_rfc3339(&r.created_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
                last_active: DateTime::parse_from_rfc3339(&r.last_active)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            });
        }
        Ok(out)
    }

    async fn save_message(
        &self,
        id: &str,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<()> {
        let timestamp = Utc::now().to_rfc3339();
        let row = MessageRowContent {
            id: id.to_string(),
            session_id: session_id.to_string(),
            role: role.to_string(),
            content: content.to_string(),
            timestamp: timestamp.clone(),
        };
        let _: Option<IgnoredAny> = self
            .db
            .upsert(("messages", id))
            .content(row)
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB upsert message: {}", e))?;
        Ok(())
    }

    async fn get_session_history(&self, session_id: &str, limit: usize) -> Result<Vec<String>> {
        let sid = session_id.to_string();
        let mut res = self
            .db
            .query(
                "SELECT content, timestamp FROM messages WHERE session_id = $sid ORDER BY timestamp DESC LIMIT $lim",
            )
            .bind(("sid", sid))
            .bind(("lim", limit))
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB query messages: {}", e))?;
        let rows: Vec<ContentOnly> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("SurrealDB take: {}", e))?;
        let mut out: Vec<String> = rows.into_iter().map(|r| r.content).collect();
        out.reverse();
        Ok(out)
    }

    async fn get_session_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        let sid = session_id.to_string();
        let mut res = self
            .db
            .query(
                "SELECT role, content, timestamp FROM messages WHERE session_id = $sid ORDER BY timestamp DESC LIMIT $lim",
            )
            .bind(("sid", sid))
            .bind(("lim", limit))
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB query messages: {}", e))?;
        let rows: Vec<RoleContent> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("SurrealDB take: {}", e))?;
        let mut out: Vec<(String, String)> =
            rows.into_iter().map(|r| (r.role, r.content)).collect();
        out.reverse();
        Ok(out)
    }

    async fn save_memory(
        &self,
        agent_id: &str,
        content: &str,
        embedding: Option<&[f32]>,
    ) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let created_at = Utc::now().to_rfc3339();
        let embedding_vec: Option<Vec<f32>> = embedding.map(|s| s.to_vec());
        let row = MemoryRowContent {
            id: id.clone(),
            agent_id: agent_id.to_string(),
            content: content.to_string(),
            embedding: embedding_vec,
            created_at: created_at.clone(),
        };
        let _: Option<IgnoredAny> = self
            .db
            .create(("memories", id.as_str()))
            .content(row)
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB create memory: {}", e))?;
        Ok(())
    }

    async fn delete_session(&self, session_id: &str) -> Result<()> {
        let sid = session_id.to_string();
        self.db
            .query("DELETE FROM messages WHERE session_id = $sid")
            .bind(("sid", sid))
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB delete messages: {}", e))?;
        Ok(())
    }

    async fn search_memories_semantic(
        &self,
        agent_id: &str,
        query_embedding: &[f32],
        top_k: usize,
    ) -> Result<Vec<String>> {
        if query_embedding.is_empty() || top_k == 0 {
            return Ok(Vec::new());
        }
        let agent_id_owned = agent_id.to_string();
        let embedding_owned = query_embedding.to_vec();
        // SurrealDB requires K as a literal in <|K,COSINE|>, not a parameter.
        let q = format!(
            "SELECT content, vector::distance::knn() AS distance FROM memories \
             WHERE agent_id = $agent_id AND embedding != NONE \
             AND embedding <|{},COSINE|> $embedding ORDER BY distance",
            top_k
        );
        let mut res = self
            .db
            .query(&q)
            .bind(("agent_id", agent_id_owned))
            .bind(("embedding", embedding_owned))
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB vector search: {}", e))?;
        let rows: Vec<ContentDistance> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("SurrealDB take: {}", e))?;
        Ok(rows.into_iter().map(|r| r.content).collect())
    }

    async fn list_recent_memories(&self, agent_id: &str, limit: usize) -> Result<Vec<String>> {
        let aid = agent_id.to_string();
        let lim = limit as i64;
        let mut res = self
            .db
            .query(
                // `created_at` must appear in the projection: SurrealDB 2.x
                // rejects ordering by a field the selection doesn't carry
                // ("Missing order idiom"). Every caller swallowed that error
                // via unwrap_or_default(), so recall silently returned empty.
                "SELECT content, created_at FROM memories WHERE agent_id = $aid ORDER BY created_at DESC LIMIT $lim",
            )
            .bind(("aid", aid))
            .bind(("lim", lim))
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB list memories: {}", e))?;
        let rows: Vec<ContentOnly> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("SurrealDB take: {}", e))?;
        Ok(rows.into_iter().map(|r| r.content).collect())
    }

    async fn list_recent_memory_records(
        &self,
        agent_id: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>> {
        let aid = agent_id.to_string();
        let lim = limit as i64;
        let mut res = self
            .db
            .query(
                // `created_at` stays in the projection for the same reason as in
                // `list_recent_memories`: SurrealDB 2.x rejects ordering by a
                // field the selection doesn't carry ("Missing order idiom").
                // `embedding` is deliberately absent — these rows travel into
                // the architect's audit payload, and a 1536-float vector per row
                // would dwarf the text it describes.
                "SELECT id, agent_id, content, created_at FROM memories WHERE agent_id = $aid ORDER BY created_at DESC LIMIT $lim",
            )
            .bind(("aid", aid))
            .bind(("lim", lim))
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB list memory records: {}", e))?;
        let rows: Vec<MemoryRecordRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("SurrealDB take: {}", e))?;
        Ok(rows
            .into_iter()
            .map(|r| MemoryRecord {
                id: record_id_to_key_string(&r.id),
                agent_id: r.agent_id,
                content: r.content,
                created_at: r.created_at,
            })
            .collect())
    }

    async fn delete_memories_for_agent(&self, agent_id: &str) -> Result<()> {
        let aid = agent_id.to_string();
        self.db
            .query("DELETE FROM memories WHERE agent_id = $aid")
            .bind(("aid", aid))
            .await
            .map_err(|e| anyhow::anyhow!("SurrealDB delete memories for {}: {}", agent_id, e))?;
        Ok(())
    }
}

// --- Row types below: `*Content` structs are what we serialize when writing
// a row (no id/RecordId needed, or id as a plain string); the corresponding
// `*Row` structs are what we deserialize back, since SurrealDB returns a
// typed RecordId for `id` on select. Some fields are read-only (`#[allow(dead_code)]`)
// but kept so the shape matches the stored row.

/// A memory row with the identity a citation needs. [`MemoryStore::list_recent_memories`]
/// returns bare `String` content, which is enough to put a memory in a prompt but
/// not enough to point at it afterwards — a claim can only resolve to a row that
/// has an id and an instant.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct MemoryRecord {
    /// Bare Surreal key, no `memories:` prefix.
    pub id: String,
    pub agent_id: String,
    pub content: String,
    pub created_at: String,
}

#[derive(serde::Deserialize)]
struct ContentOnly {
    content: String,
}

/// Read shape for [`MemoryStore::list_recent_memory_records`].
#[derive(serde::Deserialize)]
struct MemoryRecordRow {
    id: surrealdb::RecordId,
    agent_id: String,
    content: String,
    created_at: String,
}

#[derive(Serialize, serde::Deserialize)]
struct SessionRowContent {
    id: String,
    agent_id: String,
    channel: String,
    peer_id: String,
    created_at: String,
    last_active: String,
}

#[derive(serde::Deserialize)]
struct SessionRow {
    id: surrealdb::RecordId,
    agent_id: String,
    channel: String,
    peer_id: String,
    created_at: String,
    last_active: String,
}

#[derive(Serialize)]
struct MessageRowContent {
    id: String,
    session_id: String,
    role: String,
    content: String,
    timestamp: String,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct MessageRow {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    session_id: String,
    #[allow(dead_code)]
    role: String,
    content: String,
    #[allow(dead_code)]
    timestamp: String,
}

#[derive(serde::Deserialize)]
struct RoleContent {
    role: String,
    content: String,
    #[allow(dead_code)]
    timestamp: String,
}

#[derive(Serialize)]
struct MemoryRowContent {
    id: String,
    agent_id: String,
    content: String,
    embedding: Option<Vec<f32>>,
    created_at: String,
}

#[derive(serde::Deserialize)]
#[allow(dead_code)]
struct MemoryRow {
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    agent_id: String,
    content: String,
    #[allow(dead_code)]
    created_at: String,
}

#[derive(serde::Deserialize)]
struct ContentDistance {
    content: String,
    #[allow(dead_code)]
    distance: f64,
}
