//! Local Codex CLI provider.
//!
//! This deliberately uses `codex exec` in read-only, ephemeral mode. It
//! consumes the user's existing Codex/ChatGPT CLI authentication without
//! exposing a ChatGPT session token to the gateway, and it cannot mutate the
//! repository or invoke Codex's own tools. Structured workspace mutations stay
//! in the gateway's tool loop.

use crate::client::{
    ChatMessage, CompletionRequest, CompletionResponse, LlmProvider, Role, TokenUsage,
    ToolDefinition,
};
use async_trait::async_trait;
use consistent_world_common::platform;
use consistent_world_common::types::ToolCall;
use serde::Deserialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct CodexProvider {
    program: PathBuf,
    model: Option<String>,
    timeout: Duration,
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    item: Option<EventItem>,
    usage: Option<EventUsage>,
}
#[derive(Deserialize)]
struct EventItem {
    #[serde(rename = "type")]
    kind: Option<String>,
    text: Option<String>,
}
#[derive(Deserialize)]
struct EventUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

/// Find the `codex` binary — on Windows usually the npm `codex.cmd` shim.
pub fn locate_codex() -> Option<PathBuf> {
    platform::find_executable("codex")
}

impl CodexProvider {
    pub fn new(model: Option<String>) -> Option<Self> {
        Some(Self {
            program: locate_codex()?,
            model,
            timeout: Duration::from_secs(180),
        })
    }
}

fn render(messages: &[ChatMessage], tools: &[ToolDefinition]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = match m.role {
            Role::System => "SYSTEM",
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
            Role::Tool => "TOOL",
        };
        out.push_str(&format!("\n[{role}]\n{}\n", m.content));
    }
    if !tools.is_empty() {
        out.push_str("\nWhen a listed workspace tool is needed, return ONLY JSON in the form {\"tool\":\"name\",\"arguments\":{...}}. Never execute tools yourself.\nTOOLS:\n");
        for t in tools {
            out.push_str(&format!("- {}: {}\n", t.name, t.description));
        }
    }
    out
}

fn parse_tool_call(text: &str) -> Option<(String, serde_json::Value)> {
    let value: serde_json::Value = serde_json::from_str(text.trim()).ok()?;
    let obj = value.as_object()?;
    Some((
        obj.get("tool")?.as_str()?.to_string(),
        obj.get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    ))
}

#[async_trait]
impl LlmProvider for CodexProvider {
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        if request.stream {
            anyhow::bail!("codex provider uses exec JSONL, not native streaming");
        }
        let prompt = render(&request.messages, request.tools.as_deref().unwrap_or(&[]));
        if prompt.trim().is_empty() {
            anyhow::bail!("codex provider: empty prompt");
        }
        let mut cmd = platform::command(&self.program);
        cmd.args([
            "exec",
            "--json",
            "--ephemeral",
            "--sandbox",
            "read-only",
            "--skip-git-repo-check",
        ]);
        // A construction-time pin wins; otherwise honour the model the request
        // asked for (the gateway constructs this provider with `None`, so
        // without this fallback a UI-picked model id was silently dropped).
        // Sentinels meaning "the CLI's own default" are filtered out entirely.
        if let Some(model) = self
            .model
            .as_deref()
            .or(Some(request.model.as_str()))
            .filter(|m| {
                !m.trim().is_empty()
                    && !m.eq_ignore_ascii_case("default")
                    && !m.eq_ignore_ascii_case("codex-default")
                    && !m.eq_ignore_ascii_case("codex")
            })
        {
            cmd.args(["--model", model]);
        }
        // No positional prompt: `codex exec` reads it from stdin. The rendered
        // conversation is multi-line and unbounded, and on Windows `codex` is
        // an npm `.cmd` shim, whose `cmd.exe` hop cannot carry a newline or
        // more than 8,191 characters of command line.
        cmd.kill_on_drop(true);
        let started = Instant::now();
        let output = tokio::time::timeout(
            self.timeout,
            platform::output_with_stdin(cmd, prompt.as_bytes()),
        )
        .await
        .map_err(|_| anyhow::anyhow!("codex provider timed out"))??;
        if !output.status.success() {
            anyhow::bail!(
                "codex CLI failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let mut text = String::new();
        let mut usage = TokenUsage::default();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Ok(event) = serde_json::from_str::<Event>(line) {
                if event.kind == "item.completed"
                    && event.item.as_ref().and_then(|i| i.kind.as_deref()) == Some("agent_message")
                {
                    text = event.item.and_then(|i| i.text).unwrap_or_default();
                }
                if event.kind == "turn.completed"
                    && let Some(u) = event.usage
                {
                    usage.input_tokens = u.input_tokens.unwrap_or(0);
                    usage.output_tokens = u.output_tokens.unwrap_or(0);
                }
            }
        }
        if text.trim().is_empty() {
            anyhow::bail!("codex returned empty content");
        }
        let tools = request.tools.as_deref().unwrap_or(&[]);
        let tool_calls = if tools.is_empty() {
            Vec::new()
        } else {
            parse_tool_call(&text)
                .map(|(name, arguments)| {
                    vec![ToolCall {
                        id: format!("codex-{}", started.elapsed().as_nanos()),
                        name,
                        arguments,
                    }]
                })
                .unwrap_or_default()
        };
        let content = if tool_calls.is_empty() {
            Some(text)
        } else {
            None
        };
        Ok(CompletionResponse {
            content,
            tool_calls,
            usage,
            model: self.model.clone().unwrap_or_else(|| "codex".to_string()),
            latency_ms: started.elapsed().as_millis() as u64,
        })
    }
    fn provider_name(&self) -> &'static str {
        "codex"
    }
}
