//! Core domain types shared across the workspace: chat messages, sessions,
//! agent config, and tool call/result payloads.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single inbound or outbound chat message tied to a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub session_id: String,
    pub channel: ChannelKind,
    pub sender: Sender,
    pub content: String,
    pub timestamp: DateTime<Utc>,
    // Free-form channel-specific payload (e.g. attachments, reply targets).
    pub metadata: serde_json::Value,
}

/// Identifies who sent a [`Message`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sender {
    pub id: String,
    pub display_name: Option<String>,
}

/// Which channel a message/session came through. Currently only `Web`;
/// more variants are expected as other channels are wired in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ChannelKind {
    Web,
}

impl ChannelKind {
    /// Lowercase identifier for this channel.
    pub fn as_str(&self) -> &'static str {
        match self {
            ChannelKind::Web => "web",
        }
    }
}

impl std::str::FromStr for ChannelKind {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "web" => Ok(ChannelKind::Web),
            _ => Err(format!("unknown channel: {}", s)),
        }
    }
}

/// Whether a session/memory namespace belongs to the human principal or to
/// an entity (e.g. a sub-agent persona) acting on their behalf.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[derive(Default)]
pub enum ScopeType {
    #[default]
    Principal,
    Entity,
}

/// A conversation thread: which agent owns it, which channel/peer it's
/// bound to, and whose scope (principal vs. entity) it belongs to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub agent_id: String,
    pub channel: ChannelKind,
    pub peer_id: String,
    #[serde(default)]
    pub scope_type: ScopeType,
    #[serde(default)]
    pub scope_id: String,
    // Channel-specific data used to re-bind this session (e.g. platform thread id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_binding_metadata: Option<serde_json::Value>,
    // Human-readable note on why this session was resolved/created (debugging aid).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
}

/// Static configuration for one agent instance (id, model, persona, workspace).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub id: String,
    pub name: String,
    pub model: String,
    pub persona_path: Option<String>,
    pub workspace: String,
    pub max_concurrent: usize,
}

/// A tool invocation requested by the LLM, with its raw JSON arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// The outcome of executing a [`ToolCall`], fed back to the LLM as a tool message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: String,
    pub is_error: bool,
}
