//! The agent loop: turning a chat message into an LLM completion (optionally
//! streamed), running any requested tools, and recalling/saving memory.
//! Also covers routing messages to agents, background sub-agents, and
//! loading an agent's persona (SOUL/USER/AGENTS files).

/// The core `Agent` type and its message-handling loop.
pub mod agent;
/// Loading an agent's persona files into a system prompt.
pub mod persona;
/// Routing inbound messages to the right agent id.
pub mod routing;
/// Spawning and managing background sub-agent tasks.
pub mod subagent;

pub use agent::ToolPolicyProvider;
pub use consistent_world_llm::streaming::StreamEvent;
