//! Adapter for Google's Gemini API: request/response mapping between this
//! crate's provider-agnostic types and Gemini's `contents`/`parts` wire
//! format, plus SSE streaming with tool-call (function-call) deltas.

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

/// `LlmProvider` for Google's Gemini API (native HTTP, API-key auth).
pub struct GoogleProvider {
    pub api_key: String,
    pub base_url: String,
    http: Client,
}

impl GoogleProvider {
    /// `base_url` defaults to `https://generativelanguage.googleapis.com/v1beta`.
    pub fn new(api_key: String, base_url: Option<String>) -> Self {
        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("failed to build reqwest client");

        Self {
            api_key,
            base_url: base_url
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".to_string()),
            http,
        }
    }
}

// Outbound (request) wire types below; a "part" carries exactly one of
// text, a function call, or a function response, per Gemini's schema.
#[derive(Debug, Serialize)]
struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    function_call: Option<GeminiFunctionCall>,
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    function_response: Option<GeminiFunctionResponse>,
}

#[derive(Debug, Serialize, Clone)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

/// One turn of the conversation; `role` is Gemini's own vocabulary ("user" or "model").
#[derive(Debug, Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize)]
struct GeminiTool {
    #[serde(rename = "functionDeclarations")]
    function_declarations: Vec<GeminiFunctionDecl>,
}

#[derive(Debug, Serialize)]
struct GeminiFunctionDecl {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize, Default)]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(rename = "maxOutputTokens", skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

/// The full outbound `generateContent`/`streamGenerateContent` request body.
#[derive(Debug, Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
    #[serde(rename = "generationConfig", skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
}

// Inbound (response) wire types below; the "In" suffix distinguishes them
// from the outbound types above with the same conceptual shape.
#[derive(Debug, Deserialize)]
struct GeminiFunctionCallIn {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

#[derive(Debug, Deserialize, Default)]
struct GeminiPartIn {
    #[serde(default)]
    text: Option<String>,
    #[serde(default, rename = "functionCall")]
    function_call: Option<GeminiFunctionCallIn>,
}

#[derive(Debug, Deserialize, Default)]
struct GeminiContentIn {
    #[serde(default)]
    parts: Vec<GeminiPartIn>,
}

/// One generated response option; Gemini can return several, but only the first is used here.
#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    #[serde(default)]
    content: Option<GeminiContentIn>,
}

#[derive(Debug, Deserialize, Default)]
struct GeminiUsage {
    #[serde(default, rename = "promptTokenCount")]
    prompt_token_count: u64,
    #[serde(default, rename = "candidatesTokenCount")]
    candidates_token_count: u64,
}

/// The full `generateContent` response body.
#[derive(Debug, Deserialize)]
struct GeminiResponse {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default, rename = "usageMetadata")]
    usage_metadata: Option<GeminiUsage>,
    #[serde(default, rename = "modelVersion")]
    model_version: Option<String>,
}

// Translate the provider-agnostic request into Gemini's contents/parts shape.
fn build_body(request: &CompletionRequest) -> GeminiRequest {
    let mut system_parts: Vec<GeminiPart> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::new();

    for msg in &request.messages {
        match msg.role {
            Role::System => {
                system_parts.push(GeminiPart {
                    text: Some(msg.content.clone()),
                    function_call: None,
                    function_response: None,
                });
            }
            Role::User => {
                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts: vec![GeminiPart {
                        text: Some(msg.content.clone()),
                        function_call: None,
                        function_response: None,
                    }],
                });
            }
            Role::Assistant => {
                let mut parts: Vec<GeminiPart> = Vec::new();
                if !msg.content.is_empty() {
                    parts.push(GeminiPart {
                        text: Some(msg.content.clone()),
                        function_call: None,
                        function_response: None,
                    });
                }
                if let Some(ref calls) = msg.tool_calls {
                    for call in calls {
                        parts.push(GeminiPart {
                            text: None,
                            function_call: Some(GeminiFunctionCall {
                                name: call.name.clone(),
                                args: call.arguments.clone(),
                            }),
                            function_response: None,
                        });
                    }
                }
                if !parts.is_empty() {
                    contents.push(GeminiContent {
                        role: "model".to_string(),
                        parts,
                    });
                }
            }
            // Gemini has no "tool" role; a tool result rides on a "user"
            // content entry as a functionResponse part.
            Role::Tool => {
                let response_value: serde_json::Value = serde_json::from_str(&msg.content)
                    .unwrap_or_else(|_| serde_json::json!({ "result": msg.content }));
                let name = msg
                    .tool_call_id
                    .clone()
                    .unwrap_or_else(|| "function".to_string());
                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts: vec![GeminiPart {
                        text: None,
                        function_call: None,
                        function_response: Some(GeminiFunctionResponse {
                            name,
                            response: response_value,
                        }),
                    }],
                });
            }
        }
    }

    let system_instruction = if system_parts.is_empty() {
        None
    } else {
        Some(GeminiSystemInstruction {
            parts: system_parts,
        })
    };

    let tools = request.tools.as_ref().map(|t| {
        vec![GeminiTool {
            function_declarations: t
                .iter()
                .map(|def: &ToolDefinition| GeminiFunctionDecl {
                    name: def.name.clone(),
                    description: def.description.clone(),
                    parameters: def.parameters.clone(),
                })
                .collect(),
        }]
    });

    let generation_config = if request.temperature.is_some() || request.max_tokens.is_some() {
        Some(GeminiGenerationConfig {
            temperature: request.temperature,
            max_output_tokens: request.max_tokens,
        })
    } else {
        None
    };

    GeminiRequest {
        contents,
        system_instruction,
        tools,
        generation_config,
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

// Flatten one candidate's parts into accumulated text plus any tool calls;
// Gemini gives function calls no id, so one is generated here for callers
// that key on it (e.g. to pair a later tool result back to the call).
fn parse_candidate(cand: GeminiCandidate) -> (Option<String>, Vec<ToolCall>) {
    let mut text_acc = String::new();
    let mut calls = Vec::new();
    if let Some(content) = cand.content {
        for part in content.parts {
            if let Some(t) = part.text {
                text_acc.push_str(&t);
            }
            if let Some(fc) = part.function_call {
                calls.push(ToolCall {
                    id: format!("call_{}", uuid::Uuid::new_v4()),
                    name: fc.name,
                    arguments: fc.args,
                });
            }
        }
    }
    let content = if text_acc.is_empty() {
        None
    } else {
        Some(text_acc)
    };
    (content, calls)
}

#[async_trait]
impl LlmProvider for GoogleProvider {
    /// Non-streaming call to `generateContent`.
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        if request.stream {
            return Err(anyhow!(
                "streaming completions are not yet implemented for google provider; use stream_complete"
            ));
        }

        let model = request.model.clone();
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            self.base_url.trim_end_matches('/'),
            model,
            self.api_key
        );

        let body = build_body(&request);

        let start = Instant::now();
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call Gemini generateContent for model {model}"))?;

        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let latency_ms = start.elapsed().as_millis() as u64;

        if !status.is_success() {
            return Err(map_http_error(status, &text));
        }

        let parsed: GeminiResponse = serde_json::from_str(&text)
            .with_context(|| "failed to deserialize Gemini generateContent response")?;

        let candidate = parsed
            .candidates
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Gemini response contained no candidates"))?;

        let (content, tool_calls) = parse_candidate(candidate);

        let usage = parsed
            .usage_metadata
            .map(|u| TokenUsage {
                input_tokens: u.prompt_token_count,
                output_tokens: u.candidates_token_count,
            })
            .unwrap_or_default();

        Ok(CompletionResponse {
            content,
            tool_calls,
            usage,
            model: parsed.model_version.unwrap_or(model),
            latency_ms,
        })
    }

    /// Streaming call to `streamGenerateContent` (requested as SSE via `alt=sse`).
    async fn stream_complete(&self, request: CompletionRequest) -> anyhow::Result<TokenStream> {
        let model = request.model.clone();
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse&key={}",
            self.base_url.trim_end_matches('/'),
            model,
            self.api_key
        );

        let body = build_body(&request);

        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("failed to call Gemini streamGenerateContent for {model}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(map_http_error(status, &text));
        }

        let (tx, rx) = mpsc::channel::<StreamEvent>(64);
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        tokio::spawn(async move {
            let mut open_tool_calls: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();

            // Parse one SSE "data:" line into events; returns false once the
            // receiver has gone away, so the caller can stop reading.
            // Gemini sends a whole function call in a single chunk (no
            // incremental args), so each call gets an id here and is
            // reported as Start+Delta+End together.
            async fn flush_line(
                line: &str,
                tx: &mpsc::Sender<StreamEvent>,
                open: &mut std::collections::HashMap<String, String>,
            ) -> bool {
                let data = match line.strip_prefix("data:") {
                    Some(d) => d.trim(),
                    None => return true,
                };
                if data.is_empty() {
                    return true;
                }
                let parsed: GeminiResponse = match serde_json::from_str(data) {
                    Ok(p) => p,
                    Err(_) => return true,
                };
                for cand in parsed.candidates {
                    if let Some(content) = cand.content {
                        for part in content.parts {
                            if let Some(t) = part.text
                                && !t.is_empty()
                                && tx.send(StreamEvent::Token(t)).await.is_err()
                            {
                                return false;
                            }
                            if let Some(fc) = part.function_call {
                                let id = format!("call_{}", uuid::Uuid::new_v4());
                                if tx
                                    .send(StreamEvent::ToolCallStart {
                                        id: id.clone(),
                                        name: fc.name.clone(),
                                    })
                                    .await
                                    .is_err()
                                {
                                    return false;
                                }
                                let args_str = serde_json::to_string(&fc.args).unwrap_or_default();
                                if !args_str.is_empty()
                                    && args_str != "null"
                                    && tx
                                        .send(StreamEvent::ToolCallDelta {
                                            id: id.clone(),
                                            arguments_chunk: args_str,
                                        })
                                        .await
                                        .is_err()
                                {
                                    return false;
                                }
                                open.insert(id.clone(), fc.name);
                                if tx.send(StreamEvent::ToolCallEnd { id }).await.is_err() {
                                    return false;
                                }
                            }
                        }
                    }
                }
                true
            }

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
                            if !flush_line(&line, &tx, &mut open_tool_calls).await {
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
            if !buf.is_empty() {
                let line = String::from_utf8_lossy(&buf).trim().to_string();
                if !line.is_empty() {
                    let _ = flush_line(&line, &tx, &mut open_tool_calls).await;
                }
            }
            let _ = tx.send(StreamEvent::Done).await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }

    fn provider_name(&self) -> &'static str {
        "google"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::ChatMessage;

    fn req(messages: Vec<ChatMessage>, tools: Option<Vec<ToolDefinition>>) -> CompletionRequest {
        CompletionRequest {
            model: "gemini-2.0-flash".into(),
            messages,
            tools,
            max_tokens: Some(1024),
            temperature: Some(0.2),
            stream: false,
        }
    }

    #[test]
    fn body_maps_system_message_to_system_instruction() {
        let r = req(
            vec![
                ChatMessage {
                    role: Role::System,
                    content: "You are Consistent.".into(),
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
        assert!(body.system_instruction.is_some());
        assert_eq!(body.contents.len(), 1);
        assert_eq!(body.contents[0].role, "user");
    }

    #[test]
    fn body_maps_assistant_tool_calls_to_function_call_parts() {
        let r = req(
            vec![ChatMessage {
                role: Role::Assistant,
                content: "".into(),
                tool_calls: Some(vec![consistent_world_common::types::ToolCall {
                    id: "id_1".into(),
                    name: "set_lights".into(),
                    arguments: serde_json::json!({"level": 3}),
                }]),
                tool_call_id: None,
            }],
            None,
        );
        let body = build_body(&r);
        assert_eq!(body.contents.len(), 1);
        assert_eq!(body.contents[0].role, "model");
        assert!(body.contents[0].parts[0].function_call.is_some());
    }

    #[test]
    fn body_maps_tool_role_to_user_with_function_response() {
        let r = req(
            vec![ChatMessage {
                role: Role::Tool,
                content: "{\"result\":\"ok\"}".into(),
                tool_calls: None,
                tool_call_id: Some("set_lights".into()),
            }],
            None,
        );
        let body = build_body(&r);
        assert_eq!(body.contents[0].role, "user");
        assert!(body.contents[0].parts[0].function_response.is_some());
    }

    #[test]
    fn body_includes_generation_config_when_set() {
        let r = req(
            vec![ChatMessage {
                role: Role::User,
                content: "hi".into(),
                tool_calls: None,
                tool_call_id: None,
            }],
            None,
        );
        let body = build_body(&r);
        let gc = body.generation_config.unwrap();
        assert_eq!(gc.temperature, Some(0.2));
        assert_eq!(gc.max_output_tokens, Some(1024));
    }

    #[test]
    fn body_wraps_tools_in_function_declarations() {
        let tools = vec![ToolDefinition {
            name: "search".into(),
            description: "search the web".into(),
            parameters: serde_json::json!({"type":"object"}),
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
        assert_eq!(t[0].function_declarations.len(), 1);
        assert_eq!(t[0].function_declarations[0].name, "search");
    }
}
