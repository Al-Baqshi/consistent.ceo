//! Core request/response types and the `LlmProvider` trait every adapter
//! (Anthropic, Google, OpenAI, Ollama, the `claude` CLI) implements, so the
//! rest of the gateway can call any provider through one interface.

use async_trait::async_trait;
use consistent_world_common::types::ToolCall;
use serde::{Deserialize, Serialize};

use crate::streaming::TokenStream;

/// One turn of a chat conversation, in the provider-agnostic shape callers build requests with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub tool_calls: Option<Vec<ToolCall>>,
    // Set when this message is a tool result, linking it back to the call that produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// Who sent a `ChatMessage`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A provider-agnostic completion request; each adapter translates this into its own wire format.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub stream: bool,
}

/// A tool the model may call, described as a name, description, and JSON Schema for its arguments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// A provider-agnostic completion result.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub usage: TokenUsage,
    pub model: String,
    /// Request latency in milliseconds (0 if not measured).
    pub latency_ms: u64,
}

/// Token counts for a single completion, used for cost accounting (see `cost::CostCalculator`).
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Common interface every LLM adapter implements, so callers don't need to know which provider they're talking to.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Run one non-streaming completion.
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse>;

    /// Stream completion events. Default returns error "streaming not implemented".
    async fn stream_complete(&self, request: CompletionRequest) -> anyhow::Result<TokenStream> {
        let _ = request;
        Err(anyhow::anyhow!(
            "streaming not implemented for this provider"
        ))
    }

    /// Short identifier for this provider (e.g. "anthropic"), used in logs and routing.
    fn provider_name(&self) -> &'static str;
}
