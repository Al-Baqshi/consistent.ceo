//! Adapter for a locally-running Ollama server — the on-device model path,
//! used when the operator wants inference without an API key or a network
//! call.

use std::time::{Duration, Instant};

use anyhow::{Context, anyhow};
use async_trait::async_trait;
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::client::{CompletionRequest, CompletionResponse, LlmProvider, Role, TokenUsage};
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
    options: Option<OllamaOptions>,
}

// Ollama servers default `num_ctx` to 2048-4096 and silently drop the
// overflow, so a long assembled system prompt loses its head (SOUL, rules,
// entity goals) before the model ever sees it. Size the window to the actual
// request instead, clamped so small chats keep the KV cache cheap and huge
// prompts can't balloon memory on modest machines.
const MIN_NUM_CTX: i32 = 4096;
const MAX_NUM_CTX: i32 = 32768;

fn estimated_num_ctx(messages: &[OllamaMessage], num_predict: Option<i32>) -> i32 {
    let prompt_chars: usize = messages.iter().map(|m| m.content.len()).sum();
    // ~3 chars per token is deliberately pessimistic for English text, leaving
    // headroom for the chat template and tokenizer variance.
    let prompt_tokens = (prompt_chars / 3) as i32;
    (prompt_tokens + num_predict.unwrap_or(1024) + 512).clamp(MIN_NUM_CTX, MAX_NUM_CTX)
}

fn to_ollama_messages(messages: Vec<crate::client::ChatMessage>) -> Vec<OllamaMessage> {
    messages
        .into_iter()
        .map(|m| OllamaMessage {
            role: match m.role {
                Role::System => "system".to_string(),
                Role::User => "user".to_string(),
                Role::Assistant => "assistant".to_string(),
                Role::Tool => "user".to_string(),
            },
            content: m.content,
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct OllamaMessageResponse {
    content: String,
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
        let messages = to_ollama_messages(request.messages);
        let num_predict = request.max_tokens.map(|v| v as i32);
        let num_ctx = estimated_num_ctx(&messages, num_predict);
        let body = OllamaChatRequest {
            model: request.model.clone(),
            messages,
            stream: false,
            think: false,
            options: Some(OllamaOptions {
                temperature: request.temperature,
                num_predict,
                num_ctx,
            }),
        };

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

        Ok(CompletionResponse {
            content: parsed.message.map(|m| m.content),
            tool_calls: Vec::new(),
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
        let messages = to_ollama_messages(request.messages);
        let num_predict = request.max_tokens.map(|v| v as i32);
        let num_ctx = estimated_num_ctx(&messages, num_predict);
        let body = OllamaChatRequest {
            model: request.model.clone(),
            messages,
            stream: true,
            think: false,
            options: Some(OllamaOptions {
                temperature: request.temperature,
                num_predict,
                num_ctx,
            }),
        };

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
        // objects rather than SSE `data:` lines, and this parse never emits
        // tool-call events (Ollama's chat API doesn't report them here).
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

    fn msg(content: &str) -> OllamaMessage {
        OllamaMessage {
            role: "system".to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn small_chats_keep_the_minimum_context() {
        assert_eq!(estimated_num_ctx(&[msg("hello")], Some(400)), MIN_NUM_CTX);
    }

    #[test]
    fn heavy_director_prompts_grow_past_the_ollama_default() {
        // A ~33k-char assembled prompt (observed live) must request far more
        // than the 2048-token server default that was silently truncating it.
        let heavy = "x".repeat(33_000);
        let ctx = estimated_num_ctx(&[msg(&heavy)], Some(400));
        assert!(ctx > 8_000, "got {ctx}");
        assert!(ctx <= MAX_NUM_CTX);
    }

    #[test]
    fn context_estimate_is_clamped_at_the_ceiling() {
        let enormous = "x".repeat(1_000_000);
        assert_eq!(estimated_num_ctx(&[msg(&enormous)], None), MAX_NUM_CTX);
    }

    #[test]
    fn request_disables_thinking_and_pins_num_ctx() {
        let body = OllamaChatRequest {
            model: "gemma4:e4b".to_string(),
            messages: vec![msg("hi")],
            stream: false,
            think: false,
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
