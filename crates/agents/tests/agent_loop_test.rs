//! Integration tests for `Agent::handle_message`: a plain-text reply path
//! and the tool-call loop cap, driven by fake `LlmProvider` implementations.

use std::sync::Arc;

use async_trait::async_trait;
use consistent_world_agents::agent::Agent;
use consistent_world_common::types::{ChannelKind, Message, Sender, ToolCall};
use consistent_world_llm::client::{
    CompletionRequest, CompletionResponse, LlmProvider, TokenUsage,
};
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};
use consistent_world_tools::ToolRegistry;
use uuid::Uuid;

// Always replies with text and no tool calls.
struct FakeLlmText;

#[async_trait]
impl LlmProvider for FakeLlmText {
    async fn complete(&self, _request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            content: Some("hello from fake".to_string()),
            tool_calls: Vec::new(),
            usage: TokenUsage::default(),
            model: "fake".to_string(),
            latency_ms: 0,
        })
    }

    fn provider_name(&self) -> &'static str {
        "fake"
    }
}

// Always requests the same tool call, never a text reply — used to drive
// the agent loop into its cap.
struct FakeLlmToolLoop;

#[async_trait]
impl LlmProvider for FakeLlmToolLoop {
    async fn complete(&self, _request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        Ok(CompletionResponse {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                arguments: serde_json::json!({ "command": "echo ok", "timeout_secs": 5 }),
            }],
            usage: TokenUsage::default(),
            model: "fake".to_string(),
            latency_ms: 0,
        })
    }

    fn provider_name(&self) -> &'static str {
        "fake"
    }
}

#[tokio::test]
async fn test_agent_loop_text_reply() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let llm: Arc<dyn LlmProvider> = Arc::new(FakeLlmText);
    let tools = Arc::new(ToolRegistry::new());

    let agent = Agent::new(
        "a1".to_string(),
        "fake-model".to_string(),
        ".".to_string(),
        memory.clone(),
        llm,
        tools,
    );

    let msg = Message {
        id: Uuid::new_v4(),
        session_id: "s1".to_string(),
        channel: ChannelKind::Web,
        sender: Sender {
            id: "u1".to_string(),
            display_name: None,
        },
        content: "hi".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: serde_json::json!({}),
    };

    let reply = agent.handle_message(&msg).await.unwrap();
    assert_eq!(reply, "hello from fake");

    let history = memory.get_session_messages("s1", 10).await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].0, "user");
    assert_eq!(history[1].0, "assistant");
}

#[tokio::test]
async fn test_tool_call_loop_cap() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let llm: Arc<dyn LlmProvider> = Arc::new(FakeLlmToolLoop);

    // Empty registry so tool execution fails; loop cap should be hit only if tools succeed.
    // For the cap test, provide a dummy shell tool that always returns ok.
    let mut reg = ToolRegistry::new();
    reg.register(Box::new(consistent_world_tools::shell::ShellTool {
        allowed_commands: Some(vec!["echo".to_string()]),
        denied_commands: vec![],
        working_dir: ".".to_string(),
        policy: None,
    }));
    let tools = Arc::new(reg);

    let agent = Agent::new(
        "a1".to_string(),
        "fake-model".to_string(),
        ".".to_string(),
        memory,
        llm,
        tools,
    );

    let msg = Message {
        id: Uuid::new_v4(),
        session_id: "s2".to_string(),
        channel: ChannelKind::Web,
        sender: Sender {
            id: "u1".to_string(),
            display_name: None,
        },
        content: "hi".to_string(),
        timestamp: chrono::Utc::now(),
        metadata: serde_json::json!({}),
    };

    let reply = agent.handle_message(&msg).await.unwrap();
    assert_eq!(reply, "Tool loop cap reached");
}
