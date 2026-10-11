//! Provider-agnostic streaming types and SSE (server-sent events) parsing
//! helpers, shared by the HTTP-based adapters (Anthropic, Google, OpenAI,
//! Ollama) so each provider's stream can be normalized into one `StreamEvent`
//! sequence for callers.

use futures::Stream;
use std::collections::HashMap;
use std::pin::Pin;

/// A boxed, pinned stream of `StreamEvent`s returned by `LlmProvider::stream_complete`.
pub type TokenStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

/// One normalized event from a streaming completion, regardless of provider.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", content = "payload")]
pub enum StreamEvent {
    Token(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, arguments_chunk: String },
    ToolCallEnd { id: String },
    Done,
    Error(String),
}

/// OpenAI-style SSE delta. Only fields we care about.
#[derive(Debug, Default, serde::Deserialize)]
struct StreamDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<StreamToolCallDelta>>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct StreamToolCallDelta {
    index: Option<u32>,
    id: Option<String>,
    #[serde(rename = "type")]
    #[allow(dead_code)]
    kind: Option<String>,
    function: Option<StreamToolCallFunction>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct StreamToolCallFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct StreamChunk {
    choices: Option<Vec<StreamChoice>>,
}

/// Stateful parser for OpenAI chat completion SSE stream.
/// Emits StreamEvent from each "data: {...}" line.
#[derive(Default)]
pub struct StreamParser {
    /// index -> (id, name, arguments_so_far). When we have id+name we've emitted ToolCallStart.
    tool_calls: HashMap<u32, (String, String, String)>,
    /// indices we've emitted ToolCallStart for
    started: std::collections::HashSet<u32>,
}

impl StreamParser {
    /// Create an empty parser with no in-flight tool calls.
    pub fn new() -> Self {
        Self::default()
    }

    /// Parse one SSE data line (without "data: " prefix). Returns events to emit.
    pub fn feed_line(&mut self, data: &str) -> Vec<StreamEvent> {
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            let mut out = Vec::new();
            for (idx, (id, _, _)) in self.tool_calls.drain() {
                if self.started.contains(&idx) {
                    out.push(StreamEvent::ToolCallEnd { id });
                }
            }
            self.started.clear();
            if !out.is_empty() {
                return out;
            }
            if data == "[DONE]" {
                return vec![StreamEvent::Done];
            }
            return Vec::new();
        }

        let chunk: StreamChunk = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();
        let choices = match chunk.choices {
            Some(c) if !c.is_empty() => c,
            _ => return out,
        };

        for choice in choices {
            if let Some(ref delta) = choice.delta {
                if let Some(ref content) = delta.content
                    && !content.is_empty()
                {
                    out.push(StreamEvent::Token(content.clone()));
                }
                if let Some(ref tcs) = delta.tool_calls {
                    for tc in tcs {
                        let index = tc.index.unwrap_or(0);
                        if let Some(ref id) = tc.id {
                            if let Some(ref func) = tc.function
                                && let Some(ref name) = func.name
                            {
                                let args = func.arguments.as_deref().unwrap_or_default();
                                if !self.started.contains(&index) {
                                    self.started.insert(index);
                                    self.tool_calls.insert(
                                        index,
                                        (id.clone(), name.clone(), args.to_string()),
                                    );
                                    out.push(StreamEvent::ToolCallStart {
                                        id: id.clone(),
                                        name: name.clone(),
                                    });
                                    if !args.is_empty() {
                                        out.push(StreamEvent::ToolCallDelta {
                                            id: id.clone(),
                                            arguments_chunk: args.to_string(),
                                        });
                                    }
                                } else if let Some((_, _, acc)) = self.tool_calls.get_mut(&index)
                                    && let Some(ref a) = func.arguments
                                {
                                    acc.push_str(a);
                                    out.push(StreamEvent::ToolCallDelta {
                                        id: id.clone(),
                                        arguments_chunk: a.clone(),
                                    });
                                }
                            }
                        } else if let Some((id, _, acc)) = self.tool_calls.get_mut(&index)
                            && let Some(ref func) = tc.function
                            && let Some(ref args) = func.arguments
                        {
                            acc.push_str(args);
                            out.push(StreamEvent::ToolCallDelta {
                                id: id.clone(),
                                arguments_chunk: args.clone(),
                            });
                        }
                    }
                }
            }
            if let Some(ref reason) = choice.finish_reason
                && (reason == "tool_calls" || reason == "stop" || reason == "length")
            {
                for (idx, (id, _, _)) in self.tool_calls.drain() {
                    if self.started.contains(&idx) {
                        out.push(StreamEvent::ToolCallEnd { id });
                    }
                }
                self.started.clear();
                if reason != "tool_calls" {
                    out.push(StreamEvent::Done);
                }
            }
        }
        out
    }
}

/// Stateless SSE line parser for content-only streams.
pub struct StreamHandler;

impl StreamHandler {
    /// Parse a single SSE line (full line including "data: " prefix). Stateless; for simple content-only streams.
    /// For OpenAI stream with tool_calls use StreamParser instead.
    pub fn parse_sse_line(line: &str) -> Option<StreamEvent> {
        let line = line.trim();
        let data = line.strip_prefix("data:")?.trim();
        if data.is_empty() {
            return None;
        }
        if data == "[DONE]" {
            return Some(StreamEvent::Done);
        }
        let chunk: StreamChunk = serde_json::from_str(data).ok()?;
        let choice = chunk.choices?.into_iter().next()?;
        let delta = choice.delta?;
        if let Some(content) = delta.content
            && !content.is_empty()
        {
            return Some(StreamEvent::Token(content));
        }
        if choice.finish_reason.as_deref() == Some("stop")
            || choice.finish_reason.as_deref() == Some("length")
        {
            return Some(StreamEvent::Done);
        }
        None
    }
}
