//! Multi-tenant bootstrap store.
//!
//! On first-seen `principal_id` (a user id minted by the own-auth store, or a
//! scoped-bearer principal in dev), creates a row in `users` + a row in
//! `workspaces`, returning `first_signin = true` so the caller can seed trial
//! credits exactly once. Subsequent requests short-circuit via an in-process
//! dedup cache to skip the DB round-trip.
//!
//! Tables are SCHEMALESS so additional profile fields can be added later without a
//! migration; `id` is always the primary key (the user id / workspace id).

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tokio::sync::Mutex;
use tracing::debug;

/// Handle to the `users` + `workspaces` tables. `seen` is the in-process dedup
/// cache mentioned in the module doc — it only ever grows, so a principal is
/// re-checked against the DB at most once per gateway process.
pub struct TenantStore {
    db: Arc<Surreal<Any>>,
    seen: Mutex<HashSet<String>>,
}

#[derive(Deserialize)]
struct IdOnly {
    #[allow(dead_code)]
    id: surrealdb::RecordId,
}

/// A workspace as read back for listing/display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRow {
    pub id: String,
    pub name: String,
    pub status: String,
    pub primary_user_id: Option<String>,
    pub created_at: String,
}

#[derive(Deserialize)]
struct WorkspaceDbRow {
    id: surrealdb::RecordId,
    name: Option<String>,
    status: Option<String>,
    primary_user_id: Option<String>,
    created_at: Option<String>,
}

/// Id-less write bodies. The Surreal SDK serialiser rejects
/// `serde_json::Value` (externally-tagged enum) and a string `id` in the body
/// collides with the record key from the path argument — same pitfalls
/// documented in [`crate::entity_store`]. Typed structs avoid both.
#[derive(Serialize)]
struct UserBody {
    primary_workspace_id: String,
    created_at: String,
    last_seen: String,
}

#[derive(Serialize)]
struct WorkspaceBody {
    name: String,
    status: String,
    primary_user_id: Option<String>,
    created_at: String,
}

/// Refusal from [`TenantStore::create_workspace`]: that id already belongs to
/// someone else (or to no provable owner), so "creating" it would rename and
/// take over another tenant's workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkspaceTaken;

impl std::fmt::Display for WorkspaceTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("workspace id already belongs to another owner")
    }
}

impl std::error::Error for WorkspaceTaken {}

impl TenantStore {
    pub fn new(db: Arc<Surreal<Any>>) -> Arc<Self> {
        Arc::new(Self {
            db,
            seen: Mutex::new(HashSet::new()),
        })
    }

    /// Define the `workspaces` and `users` tables. Idempotent, safe on every
    /// startup.
    pub async fn init_schema(&self) -> Result<()> {
        let mut res = self
            .db
            .query(
                r#"
                DEFINE TABLE workspaces SCHEMALESS;
                DEFINE FIELD primary_user_id ON workspaces TYPE string;
                DEFINE FIELD created_at ON workspaces TYPE string;

                DEFINE TABLE users SCHEMALESS;
                DEFINE FIELD primary_workspace_id ON users TYPE string;
                DEFINE FIELD created_at ON users TYPE string;
                DEFINE FIELD last_seen ON users TYPE string;
                "#,
            )
            .await?;
        for (_idx, err) in res.take_errors() {
            tracing::warn!("TenantStore schema warning: {}", err);
        }
        Ok(())
    }

    /// Idempotent. Returns `true` if the user row was newly created (i.e. first sign-in
    /// from this principal_id). Callers can seed trial credits on that signal.
    pub async fn ensure_tenant(&self, principal_id: &str, workspace_id: &str) -> Result<bool> {
        {
            let seen = self.seen.lock().await;
            if seen.contains(principal_id) {
                return Ok(false);
            }
        }

        let existing: Option<IdOnly> = self
            .db
            .select(("users", principal_id))
            .await
            .map_err(|e| anyhow::anyhow!("select user: {}", e))?;
        let first_signin = existing.is_none();

        let now = Utc::now().to_rfc3339();

        if first_signin {
            let _: Option<IdOnly> = self
                .db
                .create(("users", principal_id))
                .content(UserBody {
                    primary_workspace_id: workspace_id.to_string(),
                    created_at: now.clone(),
                    last_seen: now.clone(),
                })
                .await
                .map_err(|e| anyhow::anyhow!("create user: {}", e))?;

            let _: Option<IdOnly> = self
                .db
                .upsert(("workspaces", workspace_id))
                .content(WorkspaceBody {
                    name: workspace_name_from_id(workspace_id),
                    status: "active".to_string(),
                    primary_user_id: Some(principal_id.to_string()),
                    created_at: now,
                })
                .await
                .map_err(|e| anyhow::anyhow!("upsert workspace: {}", e))?;

            debug!(
                "tenant bootstrap: created user={} workspace={}",
                principal_id, workspace_id
            );
        } else {
            // Touch last_seen without blowing away created_at.
            let _ = self
                .db
                .query("UPDATE type::thing('users', $pid) SET last_seen = $now")
                .bind(("pid", principal_id.to_string()))
                .bind(("now", now))
                .await;
        }

        self.seen.lock().await.insert(principal_id.to_string());
        Ok(first_signin)
    }

    /// One workspace row, if it exists.
    pub async fn get_workspace(&self, workspace_id: &str) -> Result<Option<WorkspaceRow>> {
        let row: Option<WorkspaceDbRow> = self
            .db
            .select(("workspaces", workspace_id))
            .await
            .map_err(|e| anyhow::anyhow!("select workspace: {}", e))?;
        Ok(row.map(|row| {
            let id = record_id_key(&row.id);
            WorkspaceRow {
                name: row.name.unwrap_or_else(|| workspace_name_from_id(&id)),
                status: row.status.unwrap_or_else(|| "active".to_string()),
                primary_user_id: row.primary_user_id,
                created_at: row.created_at.unwrap_or_default(),
                id,
            }
        }))
    }

    /// Create a workspace explicitly, as opposed to the implicit one
    /// `ensure_tenant` creates on first sign-in — or rename one the caller
    /// already owns.
    ///
    /// This used to be a blind upsert, and a registered user's workspace id is
    /// their user id (listed by `/api/workspaces`), so any tenant could rename
    /// any other's. An existing id is now only rewritten by its recorded
    /// primary user; anyone else — including a caller with no principal to
    /// prove ownership — gets [`WorkspaceTaken`].
    pub async fn create_workspace(
        &self,
        workspace_id: &str,
        name: &str,
        principal_id: Option<&str>,
    ) -> Result<WorkspaceRow> {
        let existing = self.get_workspace(workspace_id).await?;
        if let Some(ref existing) = existing {
            let owned = matches!(
                (principal_id, existing.primary_user_id.as_deref()),
                (Some(caller), Some(owner)) if caller == owner
            );
            if !owned {
                return Err(WorkspaceTaken.into());
            }
        }
        let now = Utc::now().to_rfc3339();
        let row = WorkspaceRow {
            id: workspace_id.to_string(),
            name: name.trim().to_string(),
            status: existing
                .as_ref()
                .map(|e| e.status.clone())
                .unwrap_or_else(|| "active".to_string()),
            primary_user_id: principal_id.map(str::to_string),
            created_at: existing.map(|e| e.created_at).unwrap_or(now),
        };
        let _: Option<IdOnly> = self
            .db
            .upsert(("workspaces", workspace_id))
            .content(WorkspaceBody {
                name: row.name.clone(),
                status: row.status.clone(),
                primary_user_id: row.primary_user_id.clone(),
                created_at: row.created_at.clone(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("upsert workspace: {}", e))?;
        Ok(row)
    }

    /// Every workspace, ordered by id. `name`/`status` fall back to sensible
    /// defaults for rows written before those fields existed.
    pub async fn list_workspaces(&self) -> Result<Vec<WorkspaceRow>> {
        let mut res = self
            .db
            .query(
                "SELECT id, name, status, primary_user_id, created_at FROM workspaces ORDER BY id",
            )
            .await
            .map_err(|e| anyhow::anyhow!("query workspaces: {}", e))?;
        let rows: Vec<WorkspaceDbRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("decode workspaces: {}", e))?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let id = record_id_key(&row.id);
                WorkspaceRow {
                    name: row.name.unwrap_or_else(|| workspace_name_from_id(&id)),
                    status: row.status.unwrap_or_else(|| "active".to_string()),
                    primary_user_id: row.primary_user_id,
                    created_at: row.created_at.unwrap_or_default(),
                    id,
                }
            })
            .collect())
    }
}

/// A chat message as stored, with the id and timestamp that
/// `MemoryStore::get_session_messages` reads but throws away.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    /// RFC 3339, as written by `MemoryStore::save_message`.
    pub timestamp: String,
}

#[derive(Deserialize)]
struct StoredMessageRow {
    id: surrealdb::RecordId,
    role: String,
    content: String,
    #[serde(default)]
    timestamp: String,
}

impl TenantStore {
    /// The newest `limit` messages of one chat session, oldest first, with
    /// their stored ids and timestamps.
    ///
    /// This reads the memory crate's `messages` table (same database, same
    /// connection) because the `MemoryStore` trait only returns
    /// `(role, content)` pairs, and the agent-group chat needs real ids to key
    /// its list and real timestamps to order it. It belongs on `MemoryStore`;
    /// it lives here only because that crate is out of this change's reach.
    pub async fn session_messages(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<StoredMessage>> {
        let mut res = self
            .db
            .query(
                "SELECT id, role, content, timestamp FROM messages \
                 WHERE session_id = $sid ORDER BY timestamp DESC LIMIT $lim",
            )
            .bind(("sid", session_id.to_string()))
            .bind(("lim", limit))
            .await
            .map_err(|e| anyhow::anyhow!("query session messages: {}", e))?;
        let rows: Vec<StoredMessageRow> = res
            .take(0)
            .map_err(|e| anyhow::anyhow!("decode session messages: {}", e))?;
        let mut out: Vec<StoredMessage> = rows
            .into_iter()
            .map(|r| StoredMessage {
                id: record_id_key(&r.id),
                role: r.role,
                content: r.content,
                timestamp: r.timestamp,
            })
            .collect();
        out.reverse();
        Ok(out)
    }
}

fn record_id_key(rid: &surrealdb::RecordId) -> String {
    let s = rid.to_string();
    s.split_once(':')
        .map(|x| x.1)
        .map(|rest| rest.trim_matches('`').to_string())
        .unwrap_or(s)
}

// Title-cases a `-`/`_`-separated id into a display name, e.g. "my-team" ->
// "My Team", so a workspace created without an explicit name still has a
// presentable one.
fn workspace_name_from_id(workspace_id: &str) -> String {
    if workspace_id == "default" {
        return "Default".to_string();
    }
    workspace_id
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Arc<TenantStore> {
        let db = surrealdb::engine::any::connect("memory").await.unwrap();
        db.use_ns("test").use_db("test").await.unwrap();
        let store = TenantStore::new(Arc::new(db));
        store.init_schema().await.unwrap();
        store
    }

    fn taken(err: &anyhow::Error) -> bool {
        err.downcast_ref::<WorkspaceTaken>().is_some()
    }

    #[tokio::test]
    async fn a_workspace_cannot_be_taken_over_by_another_tenant() {
        let store = store().await;
        let row = store
            .create_workspace("acme", "Acme", Some("alice"))
            .await
            .unwrap();
        assert_eq!(row.primary_user_id.as_deref(), Some("alice"));

        // Bob cannot rename (or re-own) Alice's workspace…
        let err = store
            .create_workspace("acme", "Pwned", Some("bob"))
            .await
            .unwrap_err();
        assert!(taken(&err));
        // …and neither can a caller with no principal to prove ownership.
        let err = store.create_workspace("acme", "Pwned", None).await.unwrap_err();
        assert!(taken(&err));
        assert_eq!(store.get_workspace("acme").await.unwrap().unwrap().name, "Acme");

        // The owner can still rename their own.
        let renamed = store
            .create_workspace("acme", "Acme Inc", Some("alice"))
            .await
            .unwrap();
        assert_eq!(renamed.name, "Acme Inc");
        assert_eq!(renamed.created_at, row.created_at);
    }

    #[tokio::test]
    async fn session_messages_come_back_with_their_stored_ids_in_order() {
        use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};
        let memory = SurrealMemoryStore::connect_memory().await.unwrap();
        let store = TenantStore::new(memory.db());
        memory.save_message("m-1", "web-ops-alice", "user", "ok").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        memory.save_message("m-2", "web-ops-alice", "assistant", "hi").await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        memory.save_message("m-3", "web-ops-alice", "user", "ok").await.unwrap();
        memory.save_message("m-x", "web-ops-bob", "user", "not alice's").await.unwrap();

        let rows = store.session_messages("web-ops-alice", 50).await.unwrap();
        let ids: Vec<_> = rows.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["m-1", "m-2", "m-3"], "real ids, oldest first, one session");
        assert!(rows.iter().all(|m| !m.timestamp.is_empty()));
    }

    #[tokio::test]
    async fn a_signed_in_users_own_workspace_is_theirs_alone() {
        let store = store().await;
        store.ensure_tenant("user-1", "user-1").await.unwrap();
        let err = store
            .create_workspace("user-1", "Mine now", Some("user-2"))
            .await
            .unwrap_err();
        assert!(taken(&err));
    }
}
