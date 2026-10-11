//! The core agent loop: turn an inbound [`Message`] into an LLM completion,
//! executing any requested tool calls and looping until the model produces a
//! plain-text reply (or a loop cap is hit). Offers both a buffered
//! (`handle_message`) and a streaming (`handle_message_stream`) variant, and
//! wires in memory recall/persistence plus optional cost tracking.

use std::sync::Arc;

use consistent_world_common::telemetry::CostTracker;
use consistent_world_common::types::{Message, ToolCall, ToolResult};
use consistent_world_llm::client::{
    ChatMessage, CompletionRequest, LlmProvider, Role, ToolDefinition,
};
use consistent_world_llm::cost::CostCalculator;
use consistent_world_llm::streaming::{StreamEvent, TokenStream};
use consistent_world_memory::embedding::EmbeddingProvider;
use consistent_world_memory::store::MemoryStore;
use consistent_world_tools::ToolRegistry;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

// How many semantically-recalled memories to pull into the system prompt per message.
const SEMANTIC_RECALL_TOP_K: usize = 5;

/// Hook for injecting extra context/policy into tool arguments before execution
/// (e.g. scoping a filesystem tool to the calling agent's workspace).
pub trait ToolPolicyProvider: Send + Sync {
    fn enrich_tool_arguments(
        &self,
        message: &Message,
        tool_name: &str,
        arguments: &Value,
        agent_workspace: &str,
    ) -> Value;
}

/// A single configured agent: model, persona, workspace, plus the memory,
/// LLM, and tool dependencies it needs to run the message loop. Built via
/// `new` and the `with_*` builder methods.
pub struct Agent {
    pub id: String,
    pub model: String,
    pub persona: Option<String>,
    pub workspace: String,
    memory: Arc<dyn MemoryStore>,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    cost_tracker: Option<Arc<std::sync::Mutex<CostTracker>>>,
    cost_calculator: Option<Arc<CostCalculator>>,
    tool_policy_provider: Option<Arc<dyn ToolPolicyProvider>>,
}

impl Agent {
    /// Construct an agent with no persona, embedding provider, cost tracking,
    /// or tool policy provider; chain the `with_*` methods to add those.
    pub fn new(
        id: String,
        model: String,
        workspace: String,
        memory: Arc<dyn MemoryStore>,
        llm: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
    ) -> Self {
        Self {
            id,
            model,
            persona: None,
            workspace,
            memory,
            llm,
            tools,
            embedding_provider: None,
            cost_tracker: None,
            cost_calculator: None,
            tool_policy_provider: None,
        }
    }

    /// Enable semantic memory recall by attaching an embedding provider.
    pub fn with_embedding_provider(mut self, provider: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedding_provider = Some(provider);
        self
    }

    /// Enable per-call cost/latency recording into a shared [`CostTracker`].
    pub fn with_cost_tracking(
        mut self,
        tracker: Arc<std::sync::Mutex<CostTracker>>,
        calculator: Arc<CostCalculator>,
    ) -> Self {
        self.cost_tracker = Some(tracker);
        self.cost_calculator = Some(calculator);
        self
    }

    /// Attach a [`ToolPolicyProvider`] to enrich tool arguments before execution.
    pub fn with_tool_policy_provider(mut self, provider: Arc<dyn ToolPolicyProvider>) -> Self {
        self.tool_policy_provider = Some(provider);
        self
    }

    /// Clear session history (e.g. for /reset command).
    pub async fn reset_session(&self, session_id: &str) -> anyhow::Result<()> {
        self.memory.delete_session(session_id).await
    }

    /// Run the full agent loop for one message (buffered, non-streaming):
    /// recall memory, call the LLM, execute any tool calls, and repeat until
    /// a text reply comes back. Persists the user message and final reply.
    pub async fn handle_message(&self, message: &Message) -> anyhow::Result<String> {
        let recalled = self.recall_memories(&message.content).await?;

        let history = self
            .memory
            .get_session_messages(&message.session_id, 50)
            .await?
            .into_iter()
            .map(|(role, content)| ChatMessage {
                role: match role.as_str() {
                    "system" => Role::System,
                    "assistant" => Role::Assistant,
                    "tool" => Role::Tool,
                    _ => Role::User,
                },
                content,
                tool_calls: None,
                tool_call_id: None,
            })
            .collect::<Vec<_>>();

        let mut messages = Vec::with_capacity(history.len() + 3);
        push_system_with_memory(&mut messages, &self.persona, &recalled);
        messages.extend(history);
        messages.push(ChatMessage {
            role: Role::User,
            content: message.content.clone(),
            tool_calls: None,
            tool_call_id: None,
        });

        // Registry gives us raw JSON tool defs; convert to typed ToolDefinition
        // and use None (not an empty Vec) when there are no tools, since some
        // providers treat an empty tools array differently from omitting it.
        let tools = self.tools.list_definitions();
        let tool_defs = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .into_iter()
                    .map(|t| ToolDefinition {
                        name: t["name"].as_str().unwrap_or_default().to_string(),
                        description: t["description"].as_str().unwrap_or_default().to_string(),
                        parameters: t["parameters"].clone(),
                    })
                    .collect(),
            )
        };

        let mut loop_count = 0usize;
        let mut request = CompletionRequest {
            model: self.model.clone(),
            messages,
            tools: tool_defs,
            max_tokens: Some(512),
            temperature: Some(0.2),
            stream: false,
        };

        loop {
            loop_count += 1;
            // Safety cap so a model stuck calling tools can't loop forever.
            if loop_count > 10 {
                return Ok("Tool loop cap reached".to_string());
            }

            let response = self.llm.complete(request.clone()).await?;

            if let (Some(tracker), Some(calc)) = (&self.cost_tracker, &self.cost_calculator) {
                let cost_usd = calc.calculate(
                    &response.model,
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                );
                if let Ok(mut t) = tracker.lock() {
                    t.record(
                        &self.id,
                        &message.session_id,
                        &response.model,
                        response.usage.input_tokens,
                        response.usage.output_tokens,
                        cost_usd,
                        response.latency_ms,
                    );
                }
            }

            // Save the incoming user message once (first iteration).
            if loop_count == 1 {
                self.memory
                    .save_message(
                        &Uuid::new_v4().to_string(),
                        &message.session_id,
                        "user",
                        &message.content,
                    )
                    .await?;
            }

            if response.tool_calls.is_empty() {
                let reply = response.content.unwrap_or_default();
                self.memory
                    .save_message(
                        &Uuid::new_v4().to_string(),
                        &message.session_id,
                        "assistant",
                        &reply,
                    )
                    .await?;
                return Ok(reply);
            }

            // Tool calls: execute each tool and append results as tool messages, then loop.
            let tool_calls = response.tool_calls;
            request.messages.push(ChatMessage {
                role: Role::Assistant,
                content: response.content.unwrap_or_default(),
                tool_calls: Some(tool_calls.clone()),
                tool_call_id: None,
            });
            for tc in tool_calls {
                let result = self.execute_tool_for_message(&tc, message).await?;
                request.messages.push(ChatMessage {
                    role: Role::Tool,
                    content: result.output,
                    tool_calls: None,
                    tool_call_id: Some(result.call_id.clone()),
                });
            }
        }
    }

    /// Stream completion events (tokens and Done). Tool calls are buffered and executed
    /// internally; only content tokens and Done are yielded.
    pub async fn handle_message_stream(&self, message: &Message) -> anyhow::Result<TokenStream> {
        let (tx, rx) = mpsc::channel(64);
        let message = message.clone();
        let agent = self.clone_for_stream();
        let session_id = message.session_id.clone();
        let user_content = message.content.clone();

        tokio::spawn(async move {
            let _ = agent
                .run_stream_loop(message, session_id, user_content, tx)
                .await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    // Embed the query and pull the top-k semantically similar memories; returns
    // empty (no error) when no embedding provider is configured.
    async fn recall_memories(&self, query: &str) -> anyhow::Result<Vec<String>> {
        let Some(emb) = &self.embedding_provider else {
            return Ok(Vec::new());
        };
        let vec = emb.embed(query).await?;
        self.memory
            .search_memories_semantic(&self.id, &vec, SEMANTIC_RECALL_TOP_K)
            .await
    }

    // Build a plain-data handle carrying just what the spawned streaming task
    // needs, avoiding a circular dependency on the full Agent type.
    fn clone_for_stream(&self) -> AgentStreamHandle {
        AgentStreamHandle {
            id: self.id.clone(),
            model: self.model.clone(),
            persona: self.persona.clone(),
            memory: self.memory.clone(),
            llm: self.llm.clone(),
            tools: self.tools.clone(),
            embedding_provider: self.embedding_provider.clone(),
            cost_tracker: self.cost_tracker.clone(),
            cost_calculator: self.cost_calculator.clone(),
        }
    }

    // Look up the requested tool, let the policy provider (if any) enrich its
    // arguments, run it, and stamp the result with the originating call id.
    async fn execute_tool_for_message(
        &self,
        tool_call: &ToolCall,
        message: &Message,
    ) -> anyhow::Result<ToolResult> {
        let tool = self
            .tools
            .get(&tool_call.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", tool_call.name))?;

        let enriched_args = if let Some(provider) = &self.tool_policy_provider {
            provider.enrich_tool_arguments(
                message,
                &tool_call.name,
                &tool_call.arguments,
                &self.workspace,
            )
        } else {
            tool_call.arguments.clone()
        };

        let mut result = tool.execute(enriched_args).await?;
        result.call_id = tool_call.id.clone();
        Ok(result)
    }
}

// Prepend recalled-memory and persona system messages to the request, in that
// order, so memory context comes before persona instructions. Shared by both
// the buffered and streaming request builders.
fn push_system_with_memory(
    messages: &mut Vec<ChatMessage>,
    persona: &Option<String>,
    recalled: &[String],
) {
    if !recalled.is_empty() {
        let memory_section = format!("[Memory]\n{}", recalled.join("\n"));
        messages.push(ChatMessage {
            role: Role::System,
            content: memory_section,
            tool_calls: None,
            tool_call_id: None,
        });
    }
    if let Some(persona) = persona {
        messages.push(ChatMessage {
            role: Role::System,
            content: persona.clone(),
            tool_calls: None,
            tool_call_id: None,
        });
    }
}

/// Minimal agent handle for use in spawned streaming task (no full Agent to avoid circular deps).
#[allow(dead_code)] // cost_tracker/cost_calculator reserved for stream cost recording later
struct AgentStreamHandle {
    id: String,
    model: String,
    persona: Option<String>,
    memory: Arc<dyn MemoryStore>,
    llm: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    cost_tracker: Option<Arc<std::sync::Mutex<CostTracker>>>,
    cost_calculator: Option<Arc<CostCalculator>>,
}

impl AgentStreamHandle {
    // Drives the streaming tool-call loop: forward content tokens to `tx` as
    // they arrive, buffer tool-call deltas until each call completes, execute
    // finished calls, and loop again until the model stops calling tools.
    async fn run_stream_loop(
        &self,
        message: Message,
        session_id: String,
        user_content: String,
        tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        let recalled = if let Some(emb) = &self.embedding_provider {
            let vec = emb.embed(&message.content).await?;
            self.memory
                .search_memories_semantic(&self.id, &vec, SEMANTIC_RECALL_TOP_K)
                .await?
        } else {
            Vec::new()
        };

        let mut loop_count = 0usize;
        let mut request = match self.build_request(&message, &recalled).await {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                return Err(e);
            }
        };

        loop {
            loop_count += 1;
            // Same safety cap as the buffered loop, reported as a stream error instead.
            if loop_count > 10 {
                tx.send(StreamEvent::Error("Tool loop cap reached".to_string()))
                    .await
                    .map_err(|_| anyhow::anyhow!("stream closed"))?;
                return Ok(());
            }

            let mut stream = self.llm.stream_complete(request.clone()).await?;
            let mut content = String::new();
            let mut tool_calls_buf: std::collections::HashMap<String, (String, String)> =
                std::collections::HashMap::new(); // id -> (name, arguments)
            let mut finished_tool_ids = Vec::new();

            if loop_count == 1 {
                self.memory
                    .save_message(
                        &Uuid::new_v4().to_string(),
                        &session_id,
                        "user",
                        &user_content,
                    )
                    .await?;
            }

            while let Some(event) = stream.next().await {
                match event {
                    StreamEvent::Token(t) => {
                        content.push_str(&t);
                        tx.send(StreamEvent::Token(t))
                            .await
                            .map_err(|_| anyhow::anyhow!("stream closed"))?;
                    }
                    StreamEvent::ToolCallStart { id, name } => {
                        tool_calls_buf.insert(id.clone(), (name, String::new()));
                    }
                    StreamEvent::ToolCallDelta {
                        id,
                        arguments_chunk,
                    } => {
                        if let Some((_, args)) = tool_calls_buf.get_mut(&id) {
                            args.push_str(&arguments_chunk);
                        }
                    }
                    StreamEvent::ToolCallEnd { id } => {
                        finished_tool_ids.push(id);
                    }
                    StreamEvent::Done => break,
                    StreamEvent::Error(e) => {
                        let _ = tx.send(StreamEvent::Error(e.clone())).await;
                        return Err(anyhow::anyhow!("stream error: {}", e));
                    }
                }
            }

            let mut executed_any = false;
            for id in &finished_tool_ids {
                if let Some((name, args_str)) = tool_calls_buf.remove(id) {
                    // Buffered arguments should be JSON; if streaming produced
                    // something malformed, fall back to a raw string value.
                    let arguments = serde_json::from_str(&args_str)
                        .unwrap_or(serde_json::Value::String(args_str));
                    let tc = ToolCall {
                        id: id.clone(),
                        name,
                        arguments,
                    };
                    let result = match self.execute_tool(&tc).await {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                            return Err(e);
                        }
                    };
                    request.messages.push(ChatMessage {
                        role: Role::Tool,
                        content: result.output,
                        tool_calls: None,
                        tool_call_id: None,
                    });
                    executed_any = true;
                }
            }

            // No tool calls this round means the model gave a final answer:
            // persist it and signal Done. Otherwise loop again with the tool
            // results appended so the model can continue.
            if !executed_any {
                if let Err(e) = self
                    .memory
                    .save_message(
                        &Uuid::new_v4().to_string(),
                        &session_id,
                        "assistant",
                        &content,
                    )
                    .await
                {
                    let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                    return Err(e);
                }
                tx.send(StreamEvent::Done)
                    .await
                    .map_err(|_| anyhow::anyhow!("stream closed"))?;
                return Ok(());
            }
        }
    }

    // Assemble a streaming CompletionRequest: history + memory/persona system
    // messages + the new user message, plus the registry's tool definitions.
    async fn build_request(
        &self,
        message: &Message,
        recalled: &[String],
    ) -> anyhow::Result<CompletionRequest> {
        let history = self
            .memory
            .get_session_messages(&message.session_id, 50)
            .await?
            .into_iter()
            .map(|(role, content)| ChatMessage {
                role: match role.as_str() {
                    "system" => Role::System,
                    "assistant" => Role::Assistant,
                    "tool" => Role::Tool,
                    _ => Role::User,
                },
                content,
                tool_calls: None,
                tool_call_id: None,
            })
            .collect::<Vec<_>>();

        let mut messages = Vec::with_capacity(history.len() + 3);
        push_system_with_memory(&mut messages, &self.persona, recalled);
        messages.extend(history);
        messages.push(ChatMessage {
            role: Role::User,
            content: message.content.clone(),
            tool_calls: None,
            tool_call_id: None,
        });

        // Same raw-JSON-to-typed conversion as the buffered path in `Agent::handle_message`.
        let tools = self.tools.list_definitions();
        let tool_defs = if tools.is_empty() {
            None
        } else {
            Some(
                tools
                    .into_iter()
                    .map(|t| ToolDefinition {
                        name: t["name"].as_str().unwrap_or_default().to_string(),
                        description: t["description"].as_str().unwrap_or_default().to_string(),
                        parameters: t["parameters"].clone(),
                    })
                    .collect(),
            )
        };

        Ok(CompletionRequest {
            model: self.model.clone(),
            messages,
            tools: tool_defs,
            max_tokens: Some(512),
            temperature: Some(0.2),
            stream: true,
        })
    }

    // Same as Agent::execute_tool_for_message but without the ToolPolicyProvider
    // hook (the stream handle doesn't carry one; see `clone_for_stream`).
    async fn execute_tool(&self, tool_call: &ToolCall) -> anyhow::Result<ToolResult> {
        let tool = self
            .tools
            .get(&tool_call.name)
            .ok_or_else(|| anyhow::anyhow!("unknown tool: {}", tool_call.name))?;

        let mut result = tool.execute(tool_call.arguments.clone()).await?;
        result.call_id = tool_call.id.clone();
        Ok(result)
    }
}
