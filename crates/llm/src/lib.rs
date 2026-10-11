//! LLM provider abstraction for the gateway: a shared `LlmProvider` trait
//! (see `client`) with adapters for Anthropic, the local `claude` CLI,
//! OpenAI, Google/Gemini, and Ollama, plus shared streaming types and cost
//! accounting.

/// Anthropic Messages API adapter (native HTTP, API key auth).
pub mod anthropic;
/// Local `claude` CLI adapter — routes through the operator's Claude subscription.
pub mod claude_code;
/// Shared request/response types and the `LlmProvider` trait every adapter implements.
pub mod client;
pub mod codex;
/// Per-model token pricing and cost calculation.
pub mod cost;
/// Google Gemini API adapter.
pub mod google;
/// Local Ollama adapter for on-device models.
pub mod ollama;
/// OpenAI Chat Completions API adapter.
pub mod openai;
/// Shared streaming event types and SSE parsing helpers.
pub mod streaming;
