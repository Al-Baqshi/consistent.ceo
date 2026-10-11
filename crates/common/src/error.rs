//! The workspace-wide error enum, plus a `Result` alias built on it.

use thiserror::Error;

/// Catch-all error type covering the failure modes crates in this workspace
/// need to distinguish (config, channels, LLM calls, memory, tools, etc).
#[derive(Error, Debug)]
pub enum ConsistentWorldError {
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Channel error ({channel}): {message}")]
    Channel { channel: String, message: String },

    #[error("LLM error ({provider}): {message}")]
    Llm { provider: String, message: String },

    #[error("Memory error: {0}")]
    Memory(String),

    #[error("Tool execution error ({tool}): {message}")]
    Tool { tool: String, message: String },

    #[error("Plugin error: {0}")]
    Plugin(String),

    #[error("Credential error: {0}")]
    Credential(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    // `#[from]` lets `?` convert std::io::Error automatically.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    // `#[from]` lets `?` convert serde_json::Error automatically.
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
}

/// Shorthand for `Result<T, ConsistentWorldError>`.
pub type Result<T> = std::result::Result<T, ConsistentWorldError>;
