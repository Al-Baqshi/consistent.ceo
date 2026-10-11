//! Adapter for OpenAI's Chat Completions API, including SSE streaming with
//! tool-call deltas via `StreamParser`.

use std::time::{Duration, Instant};

use crate::client::{
    CompletionRequest, CompletionResponse, LlmProvider, Role, TokenUsage, ToolDefinition,
};
use crate::streaming::{StreamEvent, StreamParser, TokenStream};
use anyhow::{Context, anyhow};
use async_trait::async_trait;
use consistent_world_common::types::ToolCall;
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// `LlmProvider` for the OpenAI Chat Completions API (and OpenAI-compatible endpoints via `base_url`).
pub struct OpenAiProvider {
    pub api_key: String,
    pub base_url: String,
    http: Client,
}

impl OpenAiProvider {
    /// `base_url` defaults to `https://api.openai.com/v1`; override to point at a compatible endpoint.
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("failed to build reqwest client");

        Self {
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            http,
        }
    }
}

// Outbound (request) wire types below; the "Outbound" suffix on tool-call
// types distinguishes them from the deserialized response shapes further down.
#[derive(Debug, Serialize)]
struct OpenAiToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    kind: String,
    function: OpenAiToolFunction,
}

/// `content` is optional because an assistant message carrying tool calls sends `null` content, not "".
#[derive(Debug, Serialize)]
struct OpenAiMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OpenAiToolCallOutbound>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct OpenAiToolCallFunctionOutbound {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct OpenAiToolCallOutbound {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: OpenAiToolCallFunctionOutbound,
}

/// The full outbound `/chat/completions` request body.
#[derive(Debug, Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

// Inbound (response) wire types below.
#[derive(Debug, Deserialize)]
struct OpenAiToolCallFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: OpenAiToolCallFunction,
}

#[derive(Debug, Deserialize)]
struct OpenAiChatMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<OpenAiToolCall>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    message: OpenAiChatMessage,
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// The full `/chat/completions` response body; only the first choice is used.
#[derive(Debug, Deserialize)]
struct OpenAiChatResponse {
    model: String,
    choices: Vec<OpenAiChoice>,
    #[serde(default)]
    usage: Option<OpenAiUsage>,
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        if request.stream {
            // Streaming will be implemented in a later phase. For now, reject explicitly so callers
            // know this path is not yet available.
            return Err(anyhow!("streaming completions are not yet implemented"));
        }

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let body = OpenAiChatRequest {
            model: request.model.clone(),
            messages: request
                .messages
                .into_iter()
                .map(|m| OpenAiMessage {
                    role: match m.role {
                        Role::System => "system".to_string(),
                        Role::User => "user".to_string(),
                        Role::Assistant => "assistant".to_string(),
                        Role::Tool => "tool".to_string(),
                    },
                    content: if m.role == Role::Assistant
                        && m.tool_calls.is_some()
                        && m.content.is_empty()
                    {
                        None
                    } else {
                        Some(m.content)
                    },
                    tool_calls: m.tool_calls.map(|calls| {
                        calls
                            .into_iter()
                            .map(|tc| OpenAiToolCallOutbound {
                                id: tc.id,
                                kind: "function".to_string(),
                                function: OpenAiToolCallFunctionOutbound {
                                    name: tc.name,
                                    arguments: tc.arguments.to_string(),
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: m.tool_call_id,
                })
                .collect(),
            tools: request.tools.map(|tools| {
                tools
                    .into_iter()
                    .map(|t: ToolDefinition| OpenAiTool {
                        kind: "function".to_string(),
                        function: OpenAiToolFunction {
                            name: t.name,
                            description: t.description,
                            parameters: t.parameters,
                        },
                    })
                    .collect()
            }),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream: false,
        };

        let start = Instant::now();
        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call OpenAI chat completions at {url}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let latency_ms = start.elapsed().as_millis() as u64;

        if !status.is_success() {
            let err = match status {
                StatusCode::UNAUTHORIZED => anyhow!("LLM error 401 Unauthorized: {text}"),
                StatusCode::TOO_MANY_REQUESTS => anyhow!("LLM error 429 Too Many Requests: {text}"),
                s if s.is_server_error() => anyhow!("LLM error {} from provider: {text}", s),
                _ => anyhow!("LLM HTTP error {}: {text}", status),
            };
            return Err(err);
        }

        let parsed: OpenAiChatResponse = serde_json::from_str(&text)
            .with_context(|| "failed to deserialize OpenAI chat completion response")?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("LLM response contained no choices"))?;

        let message = choice.message;
        let content = message.content;
        let tool_calls = message
            .tool_calls
            .into_iter()
            .filter(|tc| tc.kind == "function")
            .map(|tc| ToolCall {
                id: tc.id,
                name: tc.function.name,
                arguments: serde_json::from_str(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone())),
            })
            .collect();

        let usage = if let Some(u) = parsed.usage {
            TokenUsage {
                input_tokens: u.prompt_tokens,
                output_tokens: u.completion_tokens,
            }
        } else {
            TokenUsage::default()
        };

        Ok(CompletionResponse {
            content,
            tool_calls,
            usage,
            model: parsed.model,
            latency_ms,
        })
    }

    async fn stream_complete(&self, request: CompletionRequest) -> anyhow::Result<TokenStream> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = OpenAiChatRequest {
            model: request.model.clone(),
            messages: request
                .messages
                .into_iter()
                .map(|m| OpenAiMessage {
                    role: match m.role {
                        Role::System => "system".to_string(),
                        Role::User => "user".to_string(),
                        Role::Assistant => "assistant".to_string(),
                        Role::Tool => "tool".to_string(),
                    },
                    content: if m.role == Role::Assistant
                        && m.tool_calls.is_some()
                        && m.content.is_empty()
                    {
                        None
                    } else {
                        Some(m.content)
                    },
                    tool_calls: m.tool_calls.map(|calls| {
                        calls
                            .into_iter()
                            .map(|tc| OpenAiToolCallOutbound {
                                id: tc.id,
                                kind: "function".to_string(),
                                function: OpenAiToolCallFunctionOutbound {
                                    name: tc.name,
                                    arguments: tc.arguments.to_string(),
                                },
                            })
                            .collect()
                    }),
                    tool_call_id: m.tool_call_id,
                })
                .collect(),
            tools: request.tools.map(|tools| {
                tools
                    .into_iter()
                    .map(|t: ToolDefinition| OpenAiTool {
                        kind: "function".to_string(),
                        function: OpenAiToolFunction {
                            name: t.name,
                            description: t.description,
                            parameters: t.parameters,
                        },
                    })
                    .collect()
            }),
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stream: true,
        };

        let resp = self
            .http
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call OpenAI stream at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(match status {
                StatusCode::UNAUTHORIZED => anyhow!("LLM error 401 Unauthorized: {text}"),
                StatusCode::TOO_MANY_REQUESTS => anyhow!("LLM error 429 Too Many Requests: {text}"),
                s if s.is_server_error() => anyhow!("LLM error {} from provider: {text}", s),
                _ => anyhow!("LLM HTTP error {}: {text}", status),
            });
        }

        let (tx, rx) = mpsc::channel(64);
        let mut stream = resp.bytes_stream();
        let mut parser = StreamParser::new();
        let mut buf = Vec::new();

        // Buffer bytes into newline-delimited SSE lines; the actual
        // token/tool-call event shape is handled by the shared StreamParser.
        tokio::spawn(async move {
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.extend_from_slice(&bytes);
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let line = String::from_utf8_lossy(&buf[..pos]).into_owned();
                            buf.drain(..=pos);
                            let data = line.strip_prefix("data:").map(str::trim).unwrap_or("");
                            for event in parser.feed_line(data) {
                                if tx.send(event).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                        break;
                    }
                }
            }
            if !buf.is_empty() {
                let line = String::from_utf8_lossy(&buf).into_owned();
                let data = line.strip_prefix("data:").map(str::trim).unwrap_or("");
                for event in parser.feed_line(data) {
                    let _ = tx.send(event).await;
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn provider_name(&self) -> &'static str {
        "openai"
    }
}
