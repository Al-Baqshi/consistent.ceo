//! Adapter for Anthropic's native Messages API: request/response mapping
//! between this crate's provider-agnostic types and Anthropic's
//! content-block wire format, plus SSE streaming with tool-call deltas.

use std::time::{Duration, Instant};

use crate::client::{
    CompletionRequest, CompletionResponse, LlmProvider, Role, TokenUsage, ToolDefinition,
};
use crate::streaming::{StreamEvent, TokenStream};
use anyhow::{Context, anyhow};
use async_trait::async_trait;
use consistent_world_common::types::ToolCall;
use futures::StreamExt;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const ANTHROPIC_VERSION: &str = "2023-06-01";

/// `LlmProvider` for Anthropic's native Messages API (native HTTP, API-key auth).
pub struct AnthropicProvider {
    pub api_key: String,
    pub base_url: String,
    http: Client,
}

impl AnthropicProvider {
    /// `base_url` defaults to `https://api.anthropic.com/v1`.
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("failed to build reqwest client");

        Self {
            api_key,
            base_url: base_url.unwrap_or_else(|| "https://api.anthropic.com/v1".to_string()),
            http,
        }
    }
}

/// Mirrors Anthropic's content-block union for outbound messages (text, a tool call, or a tool result).
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

// `content` is untyped JSON because it can be either a plain string or a
// serialized Vec<AnthropicContentBlock>, depending on the message role.
#[derive(Debug, Serialize)]
struct AnthropicMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

/// The full outbound `/messages` request body.
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

/// One block of the response's content array; unrecognized block types
/// (e.g. thinking blocks) fall into `Other` and are silently skipped.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum AnthropicResponseBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize, Default)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
}

/// The full `/messages` response body.
#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    #[serde(default)]
    content: Vec<AnthropicResponseBlock>,
    model: Option<String>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

/// Models that reject sampling parameters (`temperature`, `top_p`, `top_k`)
/// with a 400: Opus 4.7+, Sonnet 5, and the Fable/Mythos 5 family. For these,
/// the parameter must be omitted entirely — prompting is the steering lever.
fn model_rejects_sampling_params(model: &str) -> bool {
    model.starts_with("claude-opus-4-7")
        || model.starts_with("claude-opus-4-8")
        || model.starts_with("claude-sonnet-5")
        || model.starts_with("claude-fable-5")
        || model.starts_with("claude-mythos")
}

// Translate the provider-agnostic request into Anthropic's message/content-block shape.
fn build_body(request: &CompletionRequest) -> AnthropicRequest {
    let mut system_buf: Vec<String> = Vec::new();
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    for msg in &request.messages {
        match msg.role {
            Role::System => {
                system_buf.push(msg.content.clone());
            }
            Role::User => {
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::Value::String(msg.content.clone()),
                });
            }
            Role::Assistant => {
                let mut blocks: Vec<AnthropicContentBlock> = Vec::new();
                if !msg.content.is_empty() {
                    blocks.push(AnthropicContentBlock::Text {
                        text: msg.content.clone(),
                    });
                }
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        blocks.push(AnthropicContentBlock::ToolUse {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            input: call.arguments.clone(),
                        });
                    }
                }
                let content = if blocks.is_empty() {
                    serde_json::Value::String(String::new())
                } else {
                    serde_json::to_value(&blocks).unwrap_or(serde_json::Value::Null)
                };
                messages.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content,
                });
            }
            // Anthropic has no "tool" role; a tool result rides on a "user"
            // message as a tool_result block instead.
            Role::Tool => {
                let tool_use_id = msg
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string());
                let blocks = vec![AnthropicContentBlock::ToolResult {
                    tool_use_id,
                    content: msg.content.clone(),
                }];
                messages.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::to_value(&blocks).unwrap_or(serde_json::Value::Null),
                });
            }
        }
    }

    let system = if system_buf.is_empty() {
        None
    } else {
        Some(system_buf.join("\n\n"))
    };

    let tools = request.tools.as_ref().map(|t| {
        t.iter()
            .map(|def: &ToolDefinition| AnthropicTool {
                name: def.name.clone(),
                description: def.description.clone(),
                input_schema: def.parameters.clone(),
            })
            .collect()
    });

    AnthropicRequest {
        model: request.model.clone(),
        max_tokens: request.max_tokens.unwrap_or(2048),
        messages,
        system,
        tools,
        temperature: if model_rejects_sampling_params(&request.model) {
            None
        } else {
            request.temperature
        },
        stream: request.stream,
    }
}

// Turn a non-2xx HTTP response into a descriptive error.
fn map_http_error(status: StatusCode, text: &str) -> anyhow::Error {
    match status {
        StatusCode::UNAUTHORIZED => anyhow!("LLM error 401 Unauthorized: {text}"),
        StatusCode::TOO_MANY_REQUESTS => anyhow!("LLM error 429 Too Many Requests: {text}"),
        s if s.is_server_error() => anyhow!("LLM error {} from provider: {text}", s),
        _ => anyhow!("LLM HTTP error {}: {text}", status),
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    /// Non-streaming call to `/messages`.
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        if request.stream {
            return Err(anyhow!(
                "streaming completions are not yet implemented for anthropic provider; use stream_complete"
            ));
        }

        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));
        let mut body = build_body(&request);
        body.stream = false;

        let start = Instant::now();
        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call Anthropic messages at {url}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let latency_ms = start.elapsed().as_millis() as u64;

        if !status.is_success() {
            return Err(map_http_error(status, &text));
        }

        let parsed: AnthropicResponse = serde_json::from_str(&text)
            .with_context(|| "failed to deserialize Anthropic messages response")?;

        let mut text_acc = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        for block in parsed.content {
            match block {
                AnthropicResponseBlock::Text { text } => text_acc.push_str(&text),
                AnthropicResponseBlock::ToolUse { id, name, input } => {
                    tool_calls.push(ToolCall {
                        id,
                        name,
                        arguments: input,
                    });
                }
                AnthropicResponseBlock::Other => {}
            }
        }

        let content = if text_acc.is_empty() {
            None
        } else {
            Some(text_acc)
        };

        let usage = parsed
            .usage
            .map(|u| TokenUsage {
                input_tokens: u.input_tokens,
                output_tokens: u.output_tokens,
            })
            .unwrap_or_default();

        Ok(CompletionResponse {
            content,
            tool_calls,
            usage,
            model: parsed.model.unwrap_or(request.model),
            latency_ms,
        })
    }

    /// Streaming call to `/messages`; parses Anthropic's SSE event types into `StreamEvent`s.
    async fn stream_complete(&self, request: CompletionRequest) -> anyhow::Result<TokenStream> {
        let url = format!("{}/messages", self.base_url.trim_end_matches('/'));
        let mut body = build_body(&request);
        body.stream = true;

        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call Anthropic stream at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(map_http_error(status, &text));
        }

        let (tx, rx) = mpsc::channel::<StreamEvent>(64);
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        tokio::spawn(async move {
            // index -> (id, name) for open tool_use blocks
            let mut tool_block_ids: std::collections::HashMap<u32, String> =
                std::collections::HashMap::new();
            let mut current_event: Option<String> = None;

            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.extend_from_slice(&bytes);
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                                .trim_end_matches('\r')
                                .to_string();

                            if line.is_empty() {
                                current_event = None;
                                continue;
                            }
                            if let Some(rest) = line.strip_prefix("event:") {
                                current_event = Some(rest.trim().to_string());
                                continue;
                            }
                            let data = match line.strip_prefix("data:") {
                                Some(d) => d.trim(),
                                None => continue,
                            };
                            if data.is_empty() {
                                continue;
                            }

                            let parsed_value: serde_json::Value = match serde_json::from_str(data) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };

                            // Anthropic names an event either via a preceding
                            // "event:" line or a "type" field in the JSON
                            // data; fall back to the JSON field otherwise.
                            let event_type = current_event
                                .clone()
                                .or_else(|| {
                                    parsed_value
                                        .get("type")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string())
                                })
                                .unwrap_or_default();

                            match event_type.as_str() {
                                "content_block_start" => {
                                    let index = parsed_value
                                        .get("index")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0)
                                        as u32;
                                    if let Some(block) = parsed_value.get("content_block")
                                        && block.get("type").and_then(|v| v.as_str())
                                            == Some("tool_use")
                                    {
                                        let id = block
                                            .get("id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or_default()
                                            .to_string();
                                        let name = block
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or_default()
                                            .to_string();
                                        tool_block_ids.insert(index, id.clone());
                                        if tx
                                            .send(StreamEvent::ToolCallStart { id, name })
                                            .await
                                            .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }
                                "content_block_delta" => {
                                    let index = parsed_value
                                        .get("index")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0)
                                        as u32;
                                    if let Some(delta) = parsed_value.get("delta") {
                                        let dtype = delta
                                            .get("type")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or_default();
                                        if dtype == "text_delta"
                                            && let Some(text) =
                                                delta.get("text").and_then(|v| v.as_str())
                                            && !text.is_empty()
                                            && tx
                                                .send(StreamEvent::Token(text.to_string()))
                                                .await
                                                .is_err()
                                        {
                                            return;
                                        }
                                        if dtype == "input_json_delta"
                                            && let Some(partial) =
                                                delta.get("partial_json").and_then(|v| v.as_str())
                                            && let Some(id) = tool_block_ids.get(&index)
                                            && tx
                                                .send(StreamEvent::ToolCallDelta {
                                                    id: id.clone(),
                                                    arguments_chunk: partial.to_string(),
                                                })
                                                .await
                                                .is_err()
                                        {
                                            return;
                                        }
                                    }
                                }
                                "content_block_stop" => {
                                    let index = parsed_value
                                        .get("index")
                                        .and_then(|v| v.as_u64())
                                        .unwrap_or(0)
                                        as u32;
                                    if let Some(id) = tool_block_ids.remove(&index)
                                        && tx.send(StreamEvent::ToolCallEnd { id }).await.is_err()
                                    {
                                        return;
                                    }
                                }
                                // Terminal event: the response is complete.
                                "message_stop" => {
                                    let _ = tx.send(StreamEvent::Done).await;
                                    return;
                                }
                                // Terminal event: surface the provider's error and stop.
                                "error" => {
                                    let msg = parsed_value
                                        .get("error")
                                        .and_then(|e| e.get("message"))
                                        .and_then(|m| m.as_str())
                                        .unwrap_or("unknown error")
                                        .to_string();
                                    let _ = tx.send(StreamEvent::Error(msg)).await;
                                    return;
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(StreamEvent::Error(e.to_string())).await;
                        return;
                    }
                }
            }
            let _ = tx.send(StreamEvent::Done).await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn provider_name(&self) -> &'static str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ChatMessage;

    fn req(messages: Vec<ChatMessage>, tools: Option<Vec<ToolDefinition>>) -> CompletionRequest {
        CompletionRequest {
            model: "claude-opus-4-7".into(),
            messages,
            tools,
            max_tokens: Some(1024),
            temperature: Some(0.2),
            stream: false,
        }
    }

    #[test]
    fn temperature_dropped_on_models_that_reject_sampling_params() {
        let user = ChatMessage {
            role: Role::User,
            content: "hi".into(),
            tool_calls: None,
            tool_call_id: None,
        };
        // Opus 4.7+ / Sonnet 5 / Fable: temperature must be omitted (API 400s on it).
        for model in ["claude-opus-4-8", "claude-opus-4-7", "claude-sonnet-5"] {
            let mut r = req(vec![user.clone()], None);
            r.model = model.into();
            assert_eq!(build_body(&r).temperature, None, "model {model}");
        }
        // Older models keep the caller-provided temperature.
        for model in ["claude-sonnet-4-5", "claude-haiku-4-5", "claude-opus-4-6"] {
            let mut r = req(vec![user.clone()], None);
            r.model = model.into();
            assert_eq!(build_body(&r).temperature, Some(0.2), "model {model}");
        }
    }

    #[test]
    fn body_joins_system_messages() {
        let r = req(
            vec![
                ChatMessage {
                    role: Role::System,
                    content: "You are Consistent.".into(),
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::System,
                    content: "Be direct.".into(),
                    tool_calls: None,
                    tool_call_id: None,
                },
                ChatMessage {
                    role: Role::User,
                    content: "hi".into(),
                    tool_calls: None,
                    tool_call_id: None,
                },
            ],
            None,
        );
        let body = build_body(&r);
        assert_eq!(
            body.system.as_deref(),
            Some("You are Consistent.\n\nBe direct.")
        );
        assert_eq!(body.messages.len(), 1);
    }

    #[test]
    fn body_maps_assistant_tool_call_to_tool_use_block() {
        let r = req(
            vec![ChatMessage {
                role: Role::Assistant,
                content: "".into(),
                tool_calls: Some(vec![consistent_world_common::types::ToolCall {
                    id: "toolu_1".into(),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({"city": "Tokyo"}),
                }]),
                tool_call_id: None,
            }],
            None,
        );
        let body = build_body(&r);
        let content = &body.messages[0].content;
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["id"], "toolu_1");
        assert_eq!(content[0]["name"], "get_weather");
    }

    #[test]
    fn body_maps_tool_role_to_tool_result_on_user() {
        let r = req(
            vec![ChatMessage {
                role: Role::Tool,
                content: "259.75 USD".into(),
                tool_calls: None,
                tool_call_id: Some("toolu_1".into()),
            }],
            None,
        );
        let body = build_body(&r);
        assert_eq!(body.messages[0].role, "user");
        let content = &body.messages[0].content;
        assert_eq!(content[0]["type"], "tool_result");
        assert_eq!(content[0]["tool_use_id"], "toolu_1");
        assert_eq!(content[0]["content"], "259.75 USD");
    }

    #[test]
    fn body_wraps_tools_with_input_schema_key() {
        let tools = vec![ToolDefinition {
            name: "search".into(),
            description: "search the web".into(),
            parameters: serde_json::json!({"type": "object"}),
        }];
        let r = req(
            vec![ChatMessage {
                role: Role::User,
                content: "hi".into(),
                tool_calls: None,
                tool_call_id: None,
            }],
            Some(tools),
        );
        let body = build_body(&r);
        let t = body.tools.unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].name, "search");
    }

    #[test]
    fn max_tokens_defaults_when_unset() {
        let mut r = req(
            vec![ChatMessage {
                role: Role::User,
                content: "hi".into(),
                tool_calls: None,
                tool_call_id: None,
            }],
            None,
        );
        r.max_tokens = None;
        let body = build_body(&r);
        assert_eq!(body.max_tokens, 2048);
    }
}
