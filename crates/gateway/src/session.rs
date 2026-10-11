//! Chat sessions: one per `(channel, peer_id)` pair, each pinned to a memory
//! scope (a specific entity, or a fallback scoped to the principal) so a
//! conversation's context stays attached to the right place in the memory
//! store. Distinct from `auth_store`'s login sessions — this is about which
//! entity a message belongs to, not who is authenticated.

use consistent_world_common::types::{ChannelKind, ScopeType, Session};
use consistent_world_memory::store::MemoryStore;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// In-memory cache of chat [`Session`]s, backed by a [`MemoryStore`] for
/// persistence across restarts.
pub struct SessionManager {
    sessions: HashMap<String, Session>,
    memory: Arc<dyn MemoryStore>,
}

/// Which memory scope an incoming message resolves to, and why — the `reason`
/// is kept around mainly for debugging/logging a routing decision after the
/// fact.
#[derive(Debug, Clone)]
pub struct ScopeResolution {
    pub scope_type: ScopeType,
    pub scope_id: String,
    pub channel_binding_metadata: Option<Value>,
    pub resolution_reason: String,
}

impl SessionManager {
    pub fn new(memory: Arc<dyn MemoryStore>) -> Self {
        Self {
            sessions: HashMap::new(),
            memory,
        }
    }

    /// Get existing session or create one. Session id = "{channel}-{peer_id}".
    pub async fn get_or_create(
        &mut self,
        channel: ChannelKind,
        peer_id: &str,
        agent_id: &str,
        content_hint: Option<&str>,
        metadata_hint: Option<&Value>,
    ) -> anyhow::Result<Session> {
        let (scope_type, scope_id, binding_metadata, resolution_reason) =
            resolve_scope(&channel, peer_id, content_hint, metadata_hint, None);
        let id = format!("{}-{}", channel.as_str(), peer_id);
        if let Some(s) = self.sessions.get_mut(&id) {
            s.last_active = chrono::Utc::now();
            s.scope_type = scope_type;
            s.scope_id = scope_id;
            s.channel_binding_metadata = binding_metadata;
            s.resolution_reason = Some(resolution_reason);
            return Ok(s.clone());
        }
        let now = chrono::Utc::now();
        let session = Session {
            id: id.clone(),
            agent_id: agent_id.to_string(),
            channel,
            peer_id: peer_id.to_string(),
            scope_type,
            scope_id,
            channel_binding_metadata: binding_metadata,
            resolution_reason: Some(resolution_reason),
            created_at: now,
            last_active: now,
        };
        self.memory.upsert_session(&session).await?;
        self.sessions.insert(id, session.clone());
        Ok(session)
    }

    pub fn get(&self, session_id: &str) -> Option<&Session> {
        self.sessions.get(session_id)
    }

    /// Persist all in-memory sessions to disk.
    pub async fn persist_all(&self) -> anyhow::Result<()> {
        for session in self.sessions.values() {
            self.memory.upsert_session(session).await?;
        }
        Ok(())
    }

    /// Load persisted sessions from disk into the in-memory map.
    pub async fn load_from_disk(&mut self) -> anyhow::Result<()> {
        self.sessions.clear();
        for session in self.memory.list_sessions().await? {
            self.sessions.insert(session.id.clone(), session);
        }
        Ok(())
    }
}

/// Public entry point for scope resolution outside of [`SessionManager`]
/// (e.g. wherever a message needs a scope decided without touching the
/// session cache itself). Thin wrapper around [`resolve_scope`].
pub fn resolve_scope_for_message(
    channel: &ChannelKind,
    peer_id: &str,
    content_hint: Option<&str>,
    metadata_hint: Option<&Value>,
    chat_bound_entity_id: Option<&str>,
) -> ScopeResolution {
    let (scope_type, scope_id, channel_binding_metadata, resolution_reason) = resolve_scope(
        channel,
        peer_id,
        content_hint,
        metadata_hint,
        chat_bound_entity_id,
    );
    ScopeResolution {
        scope_type,
        scope_id,
        channel_binding_metadata,
        resolution_reason,
    }
}

/// Decide which entity (or principal) a message belongs to. Checked in order,
/// first match wins:
/// 1. an explicit `entity:<id>` / `scope:entity:<id>` / `/entity <id>` marker
///    in the message text itself;
/// 2. an `entity_id` carried in the channel's own metadata;
/// 3. the entity a chat session is already bound to by default;
/// 4. otherwise, fall back to a scope keyed on the channel + peer, so the
///    conversation still has *somewhere* to live.
fn resolve_scope(
    channel: &ChannelKind,
    peer_id: &str,
    content_hint: Option<&str>,
    metadata_hint: Option<&Value>,
    chat_bound_entity_id: Option<&str>,
) -> (ScopeType, String, Option<Value>, String) {
    if let Some(entity_id) = content_hint.and_then(parse_explicit_entity_override) {
        return (
            ScopeType::Entity,
            entity_id,
            metadata_hint.cloned(),
            "explicit-override".to_string(),
        );
    }

    if let Some(meta) = metadata_hint
        && let Some(entity_id) = extract_entity_id(meta)
    {
        return (
            ScopeType::Entity,
            entity_id,
            Some(meta.clone()),
            "metadata-entity-id".to_string(),
        );
    }

    if let Some(entity_id) = chat_bound_entity_id.filter(|s| !s.is_empty()) {
        return (
            ScopeType::Entity,
            entity_id.to_string(),
            metadata_hint.cloned(),
            "default-chat-session-binding".to_string(),
        );
    }

    // No entity signal anywhere: scope to whoever is talking, not to any
    // shared entity.
    (
        ScopeType::Principal,
        format!("{}:{}", channel.as_str(), peer_id),
        metadata_hint.cloned(),
        "principal-fallback".to_string(),
    )
}

/// Recognize an explicit entity marker at the start of a message
/// (`entity:<id>`, `scope:entity:<id>`, or `/entity <id>`), returning the id.
fn parse_explicit_entity_override(content: &str) -> Option<String> {
    let trimmed = content.trim();
    if let Some(rest) = trimmed.strip_prefix("entity:") {
        let id = rest.split_whitespace().next().unwrap_or_default().trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Some(rest) = trimmed.strip_prefix("scope:entity:") {
        let id = rest.split_whitespace().next().unwrap_or_default().trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Some(rest) = trimmed.strip_prefix("/entity ") {
        let id = rest.split_whitespace().next().unwrap_or_default().trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

/// Pull an entity id out of channel metadata, trying the shapes different
/// callers have used: top-level `entity_id`, top-level `entityId`, or a
/// nested `scope.entity_id`.
fn extract_entity_id(metadata: &Value) -> Option<String> {
    if let Some(v) = metadata.get("entity_id").and_then(Value::as_str) {
        let id = v.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Some(v) = metadata.get("entityId").and_then(Value::as_str) {
        let id = v.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    if let Some(v) = metadata
        .get("scope")
        .and_then(|scope| scope.get("entity_id"))
        .and_then(Value::as_str)
    {
        let id = v.trim();
        if !id.is_empty() {
            return Some(id.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_explicit_override_first() {
        let metadata = serde_json::json!({ "entity_id": "entity-from-meta" });
        let (scope_type, scope_id, _, reason) = resolve_scope(
            &ChannelKind::Web,
            "peer-1",
            Some("entity:entity-123 do work"),
            Some(&metadata),
            None,
        );
        assert!(matches!(scope_type, ScopeType::Entity));
        assert_eq!(scope_id, "entity-123");
        assert_eq!(reason, "explicit-override");
    }

    #[test]
    fn resolves_metadata_entity_when_no_override() {
        let metadata = serde_json::json!({ "scope": { "entity_id": "entity-abc" } });
        let (scope_type, scope_id, _, reason) =
            resolve_scope(&ChannelKind::Web, "peer-2", None, Some(&metadata), None);
        assert!(matches!(scope_type, ScopeType::Entity));
        assert_eq!(scope_id, "entity-abc");
        assert_eq!(reason, "metadata-entity-id");
    }

    #[test]
    fn resolves_principal_fallback_deterministically() {
        let (scope_type, scope_id, _, reason) =
            resolve_scope(&ChannelKind::Web, "peer-3", Some("hello"), None, None);
        assert!(matches!(scope_type, ScopeType::Principal));
        assert_eq!(scope_id, "web:peer-3");
        assert_eq!(reason, "principal-fallback");
    }

    #[test]
    fn resolves_default_chat_session_binding() {
        let (scope_type, scope_id, _, reason) = resolve_scope(
            &ChannelKind::Web,
            "peer-x",
            Some("hello"),
            None,
            Some("ent-bound"),
        );
        assert!(matches!(scope_type, ScopeType::Entity));
        assert_eq!(scope_id, "ent-bound");
        assert_eq!(reason, "default-chat-session-binding");
    }
}
