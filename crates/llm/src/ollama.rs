//! Adapter for a locally-running Ollama server — the on-device model path,
//! used when the operator wants inference without an API key or a network
//! call.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use consistent_world_common::types::ToolCall;

use crate::client::{
    CompletionRequest, CompletionResponse, LlmProvider, Role, TokenUsage, ToolDefinition,
};
use crate::streaming::{StreamEvent, TokenStream};

/// `LlmProvider` backed by a local Ollama HTTP server (default `localhost:11434`).
pub struct OllamaProvider {
    pub base_url: String,
    http: Client,
}

impl OllamaProvider {
    /// Point at a local Ollama server; defaults to `http://localhost:11434`.
    pub fn new(base_url: Option<String>) -> Self {
        // Cold-loading a multi-GB local model (e.g. Gemma 4 8B Q4_K_M) can take
        // 30-60s on the first request, plus tens of seconds of generation. The
        // outer run_agent layer enforces its own LLM_CALL_TIMEOUT (120s), so we
        // give the HTTP client enough room to ride out a slow first inference
        // without false-positive transport errors.
        let http = Client::builder()
            .timeout(Duration::from_secs(180))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build reqwest client");
        Self {
            base_url: base_url.unwrap_or_else(|| "http://localhost:11434".to_string()),
            http,
        }
    }
}

#[derive(Debug, Serialize)]
struct OllamaMessage {
    role: String,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
    // Ollama matches tool results to calls by function name, not id.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    function: OllamaToolCallFunction,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolCallFunction {
    name: String,
    // Ollama sends and expects a JSON object here, unlike OpenAI's string.
    #[serde(default)]
    arguments: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OllamaTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OllamaToolFunction,
}

#[derive(Debug, Serialize)]
struct OllamaToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<i32>,
    num_ctx: i32,
}

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    stream: bool,
    // Thinking-capable models (e.g. Gemma 4) otherwise spend the entire
    // `num_predict` budget inside `message.thinking` and return an empty
    // `message.content`. Non-thinking models ignore the flag.
    think: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OllamaTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
}

// Ollama servers default `num_ctx` to 2048-4096 and silently drop the
// overflow, so a long assembled system prompt loses its head (SOUL, rules,
// entity goals) before the model ever sees it. Size the window to the actual
// request instead, clamped so small chats keep the KV cache cheap and huge
// prompts can't balloon memory on modest machines.
const MIN_NUM_CTX: i32 = 4096;
const MAX_NUM_CTX: i32 = 32768;

fn estimated_num_ctx(
    messages: &[OllamaMessage],
    tools: Option<&[OllamaTool]>,
    num_predict: Option<i32>,
) -> i32 {
    let message_chars: usize = messages
        .iter()
        .map(|m| {
            m.content.len()
                + m.tool_calls.as_ref().map_or(0, |calls| {
                    calls
                        .iter()
                        .map(|c| c.function.name.len() + c.function.arguments.to_string().len())
                        .sum()
                })
        })
        .sum();
    // Tool schemas are rendered into the prompt by the chat template.
    let tool_chars: usize = tools.map_or(0, |tools| {
        tools
            .iter()
            .map(|t| {
                t.function.name.len()
                    + t.function.description.len()
                    + t.function.parameters.to_string().len()
            })
            .sum()
    });
    // ~3 chars per token is deliberately pessimistic for English text, leaving
    // headroom for the chat template and tokenizer variance.
    let prompt_tokens = ((message_chars + tool_chars) / 3) as i32;
    (prompt_tokens + num_predict.unwrap_or(1024) + 512).clamp(MIN_NUM_CTX, MAX_NUM_CTX)
}

fn to_ollama_messages(messages: Vec<crate::client::ChatMessage>) -> Vec<OllamaMessage> {
    // Gemma's renderer treats only the first message as the system prompt and
    // renders any later system message as a stray mid-conversation turn, so
    // the gateway's stacked system blocks are joined into one.
    let lead = messages
        .iter()
        .take_while(|m| m.role == Role::System)
        .count();
    let mut rest = messages.into_iter();
    let system: Vec<String> = rest.by_ref().take(lead).map(|m| m.content).collect();
    let mut out: Vec<OllamaMessage> = Vec::new();
    if !system.is_empty() {
        out.push(OllamaMessage {
            role: "system".to_string(),
            content: system.join("\n\n"),
            tool_calls: None,
            tool_name: None,
        });
    }

    let mut call_names: HashMap<String, String> = HashMap::new();
    out.extend(rest.map(|m| {
        match m.role {
            Role::Tool => OllamaMessage {
                role: "tool".to_string(),
                tool_name: m
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| call_names.get(id).cloned()),
                content: m.content,
                tool_calls: None,
            },
            role => {
                let tool_calls = m.tool_calls.filter(|c| !c.is_empty()).map(|calls| {
                    calls
                        .into_iter()
                        .map(|tc| {
                            call_names.insert(tc.id.clone(), tc.name.clone());
                            OllamaToolCall {
                                id: Some(tc.id),
                                function: OllamaToolCallFunction {
                                    name: tc.name,
                                    arguments: arguments_object(tc.arguments),
                                },
                            }
                        })
                        .collect()
                });
                OllamaMessage {
                    role: match role {
                        Role::System => "system",
                        Role::User => "user",
                        _ => "assistant",
                    }
                    .to_string(),
                    content: m.content,
                    tool_calls,
                    tool_name: None,
                }
            }
        }
    }));
    out
}

// Ollama rejects string-encoded arguments, so unwrap any that arrive that way.
fn arguments_object(arguments: serde_json::Value) -> serde_json::Value {
    match arguments {
        serde_json::Value::String(s) => serde_json::from_str::<serde_json::Value>(&s)
            .ok()
            .filter(|v| v.is_object())
            .unwrap_or_else(|| serde_json::json!({})),
        serde_json::Value::Null => serde_json::json!({}),
        other => other,
    }
}

fn to_ollama_tools(tools: Option<Vec<ToolDefinition>>) -> Option<Vec<OllamaTool>> {
    tools.filter(|t| !t.is_empty()).map(|tools| {
        tools
            .into_iter()
            .map(|t| OllamaTool {
                kind: "function",
                function: OllamaToolFunction {
                    name: t.name,
                    description: t.description,
                    parameters: t.parameters,
                },
            })
            .collect()
    })
}

fn into_tool_calls(calls: Vec<OllamaToolCall>) -> Vec<ToolCall> {
    calls
        .into_iter()
        .map(|c| ToolCall {
            // Older Ollama builds omit the id; the agent loop needs one to
            // pair the result with its call.
            id: c
                .id
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4().simple())),
            name: c.function.name,
            arguments: arguments_object(c.function.arguments),
        })
        .collect()
}

fn build_request(request: CompletionRequest, stream: bool) -> OllamaChatRequest {
    let messages = to_ollama_messages(request.messages);
    let tools = to_ollama_tools(request.tools);
    let num_predict = request.max_tokens.map(|v| v as i32);
    let num_ctx = estimated_num_ctx(&messages, tools.as_deref(), num_predict);
    OllamaChatRequest {
        model: request.model,
        messages,
        stream,
        think: false,
        tools,
        options: Some(OllamaOptions {
            temperature: request.temperature,
            num_predict,
            num_ctx,
        }),
    }
}

#[derive(Debug, Deserialize)]
struct OllamaMessageResponse {
    #[serde(default)]
    content: String,
    #[serde(default)]
    tool_calls: Vec<OllamaToolCall>,
}

/// Ollama `/api/chat` response; `prompt_eval_count`/`eval_count` are its input/output token counts.
#[derive(Debug, Deserialize)]
struct OllamaChatResponse {
    model: String,
    #[serde(default)]
    message: Option<OllamaMessageResponse>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        if request.stream {
            // This method never streams; callers wanting a live stream must
            // call `stream_complete` instead.
            return Err(anyhow!(
                "streaming completions are not yet implemented for OllamaProvider"
            ));
        }

        // Strip a trailing "/v1" so an OpenAI-compatible base URL still
        // resolves to Ollama's own `/api/chat` endpoint.
        let url = format!(
            "{}/api/chat",
            self.base_url.trim_end_matches('/').trim_end_matches("/v1")
        );
        let body = build_request(request, false);

        let start = Instant::now();
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call Ollama chat at {url}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let latency_ms = start.elapsed().as_millis() as u64;

        if !status.is_success() {
            return Err(anyhow!("Ollama HTTP error {}: {}", status, text));
        }

        let parsed: OllamaChatResponse = serde_json::from_str(&text)
            .with_context(|| "failed to deserialize Ollama chat response")?;

        let (content, tool_calls) = match parsed.message {
            Some(m) => (Some(m.content), into_tool_calls(m.tool_calls)),
            None => (None, Vec::new()),
        };

        Ok(CompletionResponse {
            content,
            tool_calls,
            usage: TokenUsage {
                input_tokens: parsed.prompt_eval_count.unwrap_or(0),
                output_tokens: parsed.eval_count.unwrap_or(0),
            },
            model: parsed.model,
            latency_ms,
        })
    }

    async fn stream_complete(&self, request: CompletionRequest) -> anyhow::Result<TokenStream> {
        let url = format!(
            "{}/api/chat",
            self.base_url.trim_end_matches('/').trim_end_matches("/v1")
        );
        let body = build_request(request, true);

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call Ollama stream at {url}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Ollama HTTP error {}: {}", status, text));
        }

        let (tx, rx) = mpsc::channel::<StreamEvent>(64);
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        // Unlike the other providers, Ollama streams newline-delimited JSON
        // objects rather than SSE `data:` lines. Tool calls arrive whole in a
        // single chunk, so each is emitted as start/delta/end at once.
        tokio::spawn(async move {
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        buf.extend_from_slice(&bytes);
                        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
                            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1])
                                .trim()
                                .to_string();
                            if line.is_empty() {
                                continue;
                            }
                            let value: serde_json::Value = match serde_json::from_str(&line) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            if let Some(content) = value
                                .get("message")
                                .and_then(|m| m.get("content"))
                                .and_then(|c| c.as_str())
                                && !content.is_empty()
                                && tx
                                    .send(StreamEvent::Token(content.to_string()))
                                    .await
                                    .is_err()
                            {
                                return;
                            }
                            let calls: Vec<OllamaToolCall> = value
                                .get("message")
                                .and_then(|m| m.get("tool_calls"))
                                .and_then(|c| serde_json::from_value(c.clone()).ok())
                                .unwrap_or_default();
                            for call in into_tool_calls(calls) {
                                let events = [
                                    StreamEvent::ToolCallStart {
                                        id: call.id.clone(),
                                        name: call.name,
                                    },
                                    StreamEvent::ToolCallDelta {
                                        id: call.id.clone(),
                                        arguments_chunk: call.arguments.to_string(),
                                    },
                                    StreamEvent::ToolCallEnd { id: call.id },
                                ];
                                for event in events {
                                    if tx.send(event).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            // `done: true` marks the end of the stream.
                            if value.get("done").and_then(|v| v.as_bool()).unwrap_or(false) {
                                let _ = tx.send(StreamEvent::Done).await;
                                return;
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
        "ollama"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::client::ChatMessage;
    use serde_json::json;

    fn msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "system".to_string(),
            content: content.to_string(),
            tool_calls: None,
            tool_name: None,
        }
    }

    fn chat(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn reminder_tool() -> ToolDefinition {
        ToolDefinition {
            name: "set_reminder".into(),
            description: "Schedule a reminder".into(),
            parameters: json!({"type":"object","properties":{"text":{"type":"string"}}}),
        }
    }

    #[test]
    fn small_chats_keep_the_minimum_context() {
        assert_eq!(
            estimated_num_ctx(&[msg("hello")], None, Some(400)),
            MIN_NUM_CTX
        );
    }

    #[test]
    fn tools_are_sent_in_ollama_function_shape() {
        let req = CompletionRequest {
            model: "gemma4:e4b".into(),
            messages: vec![chat(Role::User, "remind me")],
            tools: Some(vec![reminder_tool()]),
            max_tokens: None,
            temperature: None,
            stream: false,
        };
        let json = serde_json::to_value(build_request(req, false)).unwrap();
        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "set_reminder");
    }

    #[test]
    fn empty_tool_list_is_omitted() {
        let req = CompletionRequest {
            model: "m".into(),
            messages: vec![chat(Role::User, "hi")],
            tools: Some(vec![]),
            max_tokens: None,
            temperature: None,
            stream: false,
        };
        let json = serde_json::to_value(build_request(req, false)).unwrap();
        assert!(json.get("tools").is_none(), "got {json}");
    }

    #[test]
    fn tool_round_trip_keeps_call_and_names_the_result() {
        let mut assistant = chat(Role::Assistant, "");
        assistant.tool_calls = Some(vec![ToolCall {
            id: "call_1".into(),
            name: "set_reminder".into(),
            arguments: json!("{\"text\":\"call mom\"}"),
        }]);
        let mut result = chat(Role::Tool, "ok");
        result.tool_call_id = Some("call_1".into());

        let out = to_ollama_messages(vec![chat(Role::User, "hi"), assistant, result]);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(
            json[1]["tool_calls"][0]["function"]["arguments"]["text"],
            "call mom"
        );
        assert_eq!(json[2]["role"], "tool");
        assert_eq!(json[2]["tool_name"], "set_reminder");
    }

    #[test]
    fn leading_system_blocks_merge_into_one_system_prompt() {
        let out = to_ollama_messages(vec![
            chat(Role::System, "persona"),
            chat(Role::System, "canonical"),
            chat(Role::User, "hi"),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].role, "system");
        assert_eq!(out[0].content, "persona\n\ncanonical");
        assert_eq!(out[1].role, "user");
    }

    #[test]
    fn response_tool_calls_are_parsed_and_given_ids() {
        let body = r#"{"model":"gemma4","message":{"role":"assistant","content":"",
            "tool_calls":[{"id":"call_x","function":{"index":0,"name":"set_reminder","arguments":{"text":"a"}}},
                          {"function":{"name":"list_entities","arguments":{}}}]}}"#;
        let parsed: OllamaChatResponse = serde_json::from_str(body).unwrap();
        let calls = into_tool_calls(parsed.message.unwrap().tool_calls);
        assert_eq!(calls[0].id, "call_x");
        assert_eq!(calls[0].arguments["text"], "a");
        assert_eq!(calls[1].name, "list_entities");
        assert!(calls[1].id.starts_with("call_"));
    }

    #[test]
    fn heavy_director_prompts_grow_past_the_ollama_default() {
        // A ~33k-char assembled prompt (observed live) must request far more
        // than the 2048-token server default that was silently truncating it.
        let heavy = "x".repeat(33_000);
        let ctx = estimated_num_ctx(&[msg(&heavy)], None, Some(400));
        assert!(ctx > 8_000, "got {ctx}");
        assert!(ctx <= MAX_NUM_CTX);
    }

    #[test]
    fn context_estimate_is_clamped_at_the_ceiling() {
        let enormous = "x".repeat(1_000_000);
        assert_eq!(
            estimated_num_ctx(&[msg(&enormous)], None, None),
            MAX_NUM_CTX
        );
    }

    #[test]
    fn request_disables_thinking_and_pins_num_ctx() {
        let body = OllamaChatRequest {
            model: "gemma4:e4b".to_string(),
            messages: vec![msg("hi")],
            stream: false,
            think: false,
            tools: None,
            options: Some(OllamaOptions {
                temperature: Some(0.4),
                num_predict: Some(400),
                num_ctx: MIN_NUM_CTX,
            }),
        };
        let json = serde_json::to_string(&body).unwrap();
        // `think` must always be serialized (not skipped): thinking-capable
        // models default it on, which starves `content` of the whole budget.
        assert!(json.contains("\"think\":false"), "got {json}");
        assert!(json.contains("\"num_ctx\":4096"), "got {json}");
    }
}
