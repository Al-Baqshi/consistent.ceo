//! The locally-installed `claude` CLI as an LLM provider.
//!
//! ### Why this exists
//!
//! Every other provider in this crate speaks HTTP and needs an API key, which
//! means every director that thinks costs metered tokens. But the operator
//! already pays for a Claude subscription, and the CLI on their machine is
//! already authenticated against it. Wrapping that CLI behind [`LlmProvider`]
//! turns the subscription into a muscle any director can use — no API key, no
//! OAuth dance, no MCP server, and no marginal cost per call.
//!
//! It is deliberately the *shortcut*: the CLI already solved auth, token
//! refresh, and model access. We shell out and read one JSON object back.
//!
//! ### What this is NOT
//!
//! This is a **text-completion** adapter, not an agent. `--max-turns 1` plus an
//! empty tool allowlist mean the CLI answers the prompt and stops — it cannot
//! read or write files here. Agentic work (editing repos, running for hours)
//! goes through `agent_sessions` in the gateway instead, which is a different
//! job with different safety rules. Keeping the two apart is what stops a
//! director's "summarise this" call from quietly rewriting a codebase.
//!
//! ### Trade-offs the caller should know
//!
//! - **Latency**: process spawn + model round trip, so ~1–3s of floor. Fine for
//!   a director's periodic thinking, wrong for every keystroke of chat.
//! - **Rate limits**: shares the operator's subscription window. When it is
//!   spent, calls fail until it resets.
//! - **Tool calling is in-band text, not native.** The CLI's own tools stay
//!   disabled (`--tools ""`, `--max-turns 1`) — those act on the
//!   machine, which this adapter must never do. Instead, when the caller
//!   supplies [`ToolDefinition`]s, they are rendered into the system prompt
//!   with an instruction to answer with a single JSON object
//!   (`{"tool": ..., "arguments": ...}`) when a tool is wanted, and the reply
//!   is parsed back into a [`ToolCall`]. Without this, a chat agent whose brain
//!   is the subscription could *describe* saving a note or creating a task but
//!   never actually do it — the exact failure that motivated the protocol.

use crate::client::{
    ChatMessage, CompletionRequest, CompletionResponse, LlmProvider, Role, TokenUsage,
    ToolDefinition,
};
use async_trait::async_trait;
use consistent_world_common::platform;
use consistent_world_common::types::ToolCall;
use serde::Deserialize;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Whether the system prompt goes to the CLI in a file rather than as an
/// argument. Windows caps a whole command line at 32,767 characters, and a
/// director's system prompt (SOUL, operating contract, entity docs) outgrows
/// that; a `claude.cmd` npm shim runs through `cmd.exe`, which cannot carry its
/// newlines at all. Elsewhere `ARG_MAX` is about a megabyte, so the prompt stays
/// an argument and never touches the disk.
const SYSTEM_PROMPT_VIA_FILE: bool = cfg!(windows);

const DEFAULT_TIMEOUT_SECS: u64 = 180;

/// What the `claude-code` / `default` sentinels resolve to. An alias, not a
/// model id: the CLI maps `opus` to the current generation itself, so this
/// never needs bumping when a new model ships.
pub const DEFAULT_MODEL_ALIAS: &str = "opus";

/// An `LlmProvider` that shells out to the local `claude` CLI for a single, tool-free completion.
pub struct ClaudeCodeProvider {
    program: PathBuf,
    /// Optional `--model` override; `None` resolves to `DEFAULT_MODEL_ALIAS`
    /// (see `build_args` for why it is never left to the CLI).
    model: Option<String>,
    timeout: Duration,
}

/// `--output-format json` returns a single object of this shape.
#[derive(Debug, Deserialize)]
struct CliResult {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    is_error: Option<bool>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    usage: Option<CliUsage>,
}

/// Token counts as reported by the CLI's JSON output.
#[derive(Debug, Deserialize)]
struct CliUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

/// Find the `claude` binary, tolerating a stripped GUI `PATH` (and, on
/// Windows, finding `claude.exe` or an npm `claude.cmd`).
pub fn locate_claude() -> Option<PathBuf> {
    platform::find_executable("claude")
}

impl ClaudeCodeProvider {
    /// `None` when the CLI isn't installed — callers should fall back to an
    /// API provider rather than failing the whole run.
    pub fn new(model: Option<String>) -> Option<Self> {
        Some(Self {
            program: locate_claude()?,
            model: model.filter(|m| !m.trim().is_empty()),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        })
    }

    /// Override the default 180s process timeout; clamped to a 10s floor.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.timeout = Duration::from_secs(secs.max(10));
        self
    }
}

/// Split a chat request into (system text, conversation text).
///
/// The CLI takes one prompt string, so the multi-turn history has to be
/// flattened. Roles are labelled rather than concatenated raw, otherwise a
/// long history reads as one rambling user message and the model loses track
/// of who said what.
pub fn render_prompt(messages: &[ChatMessage]) -> (Option<String>, String) {
    let mut system_parts: Vec<&str> = Vec::new();
    let mut convo: Vec<String> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => system_parts.push(m.content.as_str()),
            Role::User => convo.push(format!("User: {}", m.content)),
            Role::Assistant => convo.push(format!("Assistant: {}", m.content)),
            // Tool results are context, not a speaker.
            Role::Tool => convo.push(format!("Tool result: {}", m.content)),
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, convo.join("\n\n"))
}

/// Render the caller's tool catalog into system-prompt text.
///
/// The CLI has no flag for caller-defined tools, so the definitions ride in the
/// prompt and the call comes back as text. The instruction is strict — ONLY the
/// JSON object, no prose around it — because the parser on the other side
/// treats "mostly JSON" as a normal reply rather than guessing at intent.
pub fn render_tool_protocol(tools: &[ToolDefinition]) -> String {
    let mut s = String::from(
        "\n\n## Workspace tools\n\
         You can call the tools listed below. To call one, reply with ONLY this JSON object and nothing else — no prose before or after, no code fence:\n\
         {\"tool\": \"<name>\", \"arguments\": { ... }}\n\
         After the tool runs you will see `Tool result: ...` and can then answer the user (or call another tool). \
         If no tool is needed, just answer normally in plain language. Never print a tool-call object as part of a normal answer.\n\nAvailable tools:\n",
    );
    for t in tools {
        s.push_str(&format!(
            "- `{}` — {}\n  parameters (JSON Schema): {}\n",
            t.name, t.description, t.parameters
        ));
    }
    s
}

/// The shape the model is instructed to emit for a call.
#[derive(Debug, Deserialize)]
struct TextProtocolCall {
    tool: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

/// Parse a reply as a tool call, or `None` when it is a normal answer.
///
/// Accepts the bare object we asked for, and — because models add fences no
/// matter what the instructions say — the same object inside a ``` fence. A
/// reply with prose around the JSON is treated as a normal answer: guessing at
/// half-formed calls would execute tools the model may only have been quoting.
pub fn parse_tool_call(reply: &str) -> Option<(String, serde_json::Value)> {
    let trimmed = reply.trim();
    let candidate = if trimmed.starts_with("```") {
        trimmed
            .trim_start_matches("```json")
            .trim_start_matches("```")
            .trim_end_matches("```")
            .trim()
    } else {
        trimmed
    };
    if !candidate.starts_with('{') {
        return None;
    }
    let parsed: TextProtocolCall = serde_json::from_str(candidate).ok()?;
    if parsed.tool.trim().is_empty() {
        return None;
    }
    let args = if parsed.arguments.is_null() {
        serde_json::json!({})
    } else {
        parsed.arguments
    };
    Some((parsed.tool, args))
}

/// How the system prompt reaches the CLI; see [`SYSTEM_PROMPT_VIA_FILE`].
#[derive(Debug, Clone, Copy)]
pub enum SystemPrompt<'a> {
    Inline(&'a str),
    File(&'a Path),
}

/// Build the argv for a one-shot, tool-free completion.
///
/// The prompt is not in it: it is written to the CLI's stdin. An argument
/// would hit Windows' command-line limits (see [`SYSTEM_PROMPT_VIA_FILE`]), and
/// with no positional prompt there is nothing for a list-valued flag such as
/// `--tools` to swallow — the failure that once left every call with no input.
pub fn build_args(system: Option<SystemPrompt<'_>>, model: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "-p".to_string(),
        "--output-format".to_string(),
        "json".to_string(),
        // One turn, no tools: this must behave like a completion endpoint, not
        // an agent that can touch the machine.
        //
        // `--tools ""` disables the CLI's ENTIRE built-in set. This used to be
        // a --disallowedTools denylist, but the CLI ships more tools than the
        // list named (TodoWrite, Glob, ...), and the model reaching for any
        // un-denied one counted as the single allowed turn — the run died as
        // `error_max_turns` with exit 1 before producing a word. An allowlist
        // of nothing cannot rot the way a denylist does.
        "--max-turns".to_string(),
        "1".to_string(),
        "--tools".to_string(),
        String::new(),
        // `--tools` only governs BUILT-IN tools. The operator's own MCP servers
        // stayed attached, so a completion could still reach for (say) a
        // deployment or cloud-storage tool — it would burn the single turn and
        // exit 1 with `error_max_turns`, which surfaced as an HTTP 502 on every
        // chat request. `--safe-mode` drops MCP servers along with CLAUDE.md,
        // hooks, skills and plugins, which is what this adapter wants anyway:
        // an answer that depends only on the prompt, not on how the operator
        // happens to have configured their machine.
        "--safe-mode".to_string(),
    ];
    // Always name a model. Left unset, the CLI uses whatever `/model` the
    // operator last picked in an interactive session — a per-user setting
    // that can point at a model the installed CLI cannot serve. That is how
    // the gateway went from "LLM ready" to canned replies within one `/model`
    // command on 2026-09-07 (CLI 2.1.231 asked for a model needing 2.1.251).
    // The sentinels mean "the brain tier", so they resolve to the `opus`
    // alias, which the CLI maps to the current generation on its own.
    let m = model
        .map(str::trim)
        .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("default"))
        .unwrap_or(DEFAULT_MODEL_ALIAS);
    args.push("--model".to_string());
    args.push(m.to_string());
    match system {
        Some(SystemPrompt::Inline(s)) if !s.trim().is_empty() => {
            args.push("--append-system-prompt".to_string());
            args.push(s.to_string());
        }
        Some(SystemPrompt::File(path)) => {
            args.push("--append-system-prompt-file".to_string());
            args.push(path.to_string_lossy().into_owned());
        }
        _ => {}
    }
    args
}

/// Write the system prompt where [`SystemPrompt::File`] can point at it. The
/// file is private to the user (0600 on Unix, `%TEMP%` on Windows) and deleted
/// when the returned handle drops, which the caller holds until the CLI exits.
fn system_prompt_file(system: &str) -> anyhow::Result<tempfile::NamedTempFile> {
    let mut file = tempfile::Builder::new()
        .prefix("consistent-system-")
        .suffix(".md")
        .tempfile()?;
    file.write_all(system.as_bytes())?;
    file.flush()?;
    Ok(file)
}

// Wires the CLI into the shared `LlmProvider` interface; streaming is
// intentionally unsupported (see `stream_complete`'s default in `client.rs`).
#[async_trait]
impl LlmProvider for ClaudeCodeProvider {
    async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        if request.stream {
            anyhow::bail!("claude-code provider does not support streaming completions");
        }
        let (mut system, prompt) = render_prompt(&request.messages);
        if prompt.trim().is_empty() {
            anyhow::bail!("claude-code provider: empty prompt");
        }
        // Caller-defined tools ride in the prompt; see render_tool_protocol.
        let tools_offered = request.tools.as_deref().unwrap_or(&[]);
        if !tools_offered.is_empty() {
            let protocol = render_tool_protocol(tools_offered);
            system = Some(match system {
                Some(s) => format!("{s}{protocol}"),
                None => protocol,
            });
        }
        let model = request
            .model
            .trim()
            .is_empty()
            .then(|| self.model.clone())
            .flatten()
            .or_else(|| Some(request.model.clone()))
            .filter(|m| !m.trim().is_empty() && m != "claude-code");
        let system = system.filter(|s| !s.trim().is_empty());
        // Held until the CLI exits: dropping it deletes the file.
        let system_file = match system.as_deref() {
            Some(s) if SYSTEM_PROMPT_VIA_FILE => Some(system_prompt_file(s)?),
            _ => None,
        };
        let system_arg = match (&system_file, system.as_deref()) {
            (Some(file), _) => Some(SystemPrompt::File(file.path())),
            (None, Some(s)) => Some(SystemPrompt::Inline(s)),
            (None, None) => None,
        };
        let args = build_args(system_arg, model.as_deref());

        let started = Instant::now();
        let mut cmd = platform::command(&self.program);
        cmd.args(&args).kill_on_drop(true);
        let output = tokio::time::timeout(
            self.timeout,
            platform::output_with_stdin(cmd, prompt.as_bytes()),
        )
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "claude-code provider timed out after {}s",
                self.timeout.as_secs()
            )
        })??;
        drop(system_file);

        let latency_ms = started.elapsed().as_millis() as u64;
        if !output.status.success() {
            // stderr is where the CLI explains an expired login or a spent
            // usage window — but API-level failures are printed as a JSON
            // result on *stdout* with exit 1, so on a silent stderr the
            // diagnosis lives there. Surface both rather than reword either.
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if !err.is_empty() {
                err
            } else if !out.is_empty() {
                out.chars().take(600).collect()
            } else {
                "no stderr or stdout".to_string()
            };
            anyhow::bail!("claude-code CLI failed ({}): {}", output.status, detail);
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let parsed: CliResult = serde_json::from_str(stdout.trim())
            .map_err(|e| anyhow::anyhow!("claude-code: could not parse CLI JSON: {e}"))?;

        if parsed.is_error.unwrap_or(false) {
            anyhow::bail!(
                "claude-code returned an error: {}",
                parsed.subtype.unwrap_or_else(|| "unknown".to_string())
            );
        }
        let content = parsed.result.unwrap_or_default();
        if content.trim().is_empty() {
            anyhow::bail!("claude-code returned empty content");
        }

        let usage = parsed
            .usage
            .map(|u| TokenUsage {
                input_tokens: u.input_tokens.unwrap_or(0),
                output_tokens: u.output_tokens.unwrap_or(0),
            })
            .unwrap_or_default();

        // A reply that is exactly the protocol object is a tool call, not an
        // answer. Only honoured when tools were actually offered this turn, so
        // a model *quoting* the protocol in a normal chat cannot trigger a run.
        let tool_calls = if tools_offered.is_empty() {
            Vec::new()
        } else {
            match parse_tool_call(&content) {
                Some((name, arguments)) => vec![ToolCall {
                    id: format!("cc-text-{}", latency_ms),
                    name,
                    arguments,
                }],
                None => Vec::new(),
            }
        };
        let content = if tool_calls.is_empty() {
            Some(content)
        } else {
            None
        };

        Ok(CompletionResponse {
            content,
            tool_calls,
            usage,
            model: self
                .model
                .clone()
                .unwrap_or_else(|| "claude-code".to_string()),
            latency_ms,
        })
    }

    fn provider_name(&self) -> &'static str {
        "claude-code"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> ChatMessage {
        ChatMessage {
            role,
            content: content.to_string(),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    #[test]
    fn a_bare_protocol_object_parses_as_a_tool_call() {
        let (name, args) =
            parse_tool_call(r#"{"tool":"create_entity","arguments":{"name":"Copper Kitchen"}}"#)
                .expect("should parse");
        assert_eq!(name, "create_entity");
        assert_eq!(args["name"], "Copper Kitchen");
    }

    #[test]
    fn a_fenced_protocol_object_still_parses() {
        let reply = "```json\n{\"tool\":\"create_task\",\"arguments\":{\"title\":\"x\"}}\n```";
        let (name, _) = parse_tool_call(reply).expect("should parse");
        assert_eq!(name, "create_task");
    }

    #[test]
    fn prose_around_json_is_a_normal_answer_not_a_call() {
        assert!(parse_tool_call("Sure! {\"tool\":\"create_task\"} done.").is_none());
        assert!(parse_tool_call("Just a normal reply.").is_none());
        assert!(parse_tool_call("{\"not_a_tool\":\"x\"}").is_none());
    }

    #[test]
    fn missing_arguments_default_to_an_empty_object() {
        let (_, args) = parse_tool_call(r#"{"tool":"list_entities"}"#).expect("should parse");
        assert!(args.as_object().is_some_and(|o| o.is_empty()));
    }

    #[test]
    fn system_messages_are_separated_from_the_conversation() {
        let (system, convo) = render_prompt(&[
            msg(Role::System, "You are terse."),
            msg(Role::User, "Hello"),
        ]);
        assert_eq!(system.as_deref(), Some("You are terse."));
        assert_eq!(convo, "User: Hello");
    }

    #[test]
    fn multiple_system_messages_are_joined() {
        let (system, _) = render_prompt(&[
            msg(Role::System, "Rule one."),
            msg(Role::System, "Rule two."),
            msg(Role::User, "Go"),
        ]);
        assert_eq!(system.as_deref(), Some("Rule one.\n\nRule two."));
    }

    #[test]
    fn history_keeps_speaker_labels_so_turns_stay_distinguishable() {
        let (_, convo) = render_prompt(&[
            msg(Role::User, "What is 2+2?"),
            msg(Role::Assistant, "4"),
            msg(Role::User, "And 3+3?"),
        ]);
        assert_eq!(
            convo,
            "User: What is 2+2?\n\nAssistant: 4\n\nUser: And 3+3?"
        );
    }

    #[test]
    fn tool_results_are_labelled_as_context_not_as_a_speaker() {
        let (_, convo) = render_prompt(&[msg(Role::Tool, "{\"ok\":true}")]);
        assert!(convo.starts_with("Tool result:"));
    }

    #[test]
    fn the_completion_is_one_turn_and_cannot_touch_the_machine() {
        let args = build_args(None, None);
        assert!(args.contains(&"--max-turns".to_string()));
        assert!(args.contains(&"1".to_string()));
        // The whole built-in set must be off — an empty allowlist, not a
        // denylist that silently misses newly-shipped tools.
        let tools = args
            .iter()
            .position(|a| a == "--tools")
            .map(|i| args[i + 1].clone())
            .expect("the built-in tool set must be explicitly configured");
        assert!(
            tools.is_empty(),
            "every built-in tool must be disabled for a plain completion, got {tools:?}"
        );
    }

    #[test]
    fn the_system_prompt_is_passed_as_a_flag_not_glued_onto_the_prompt() {
        let args = build_args(Some(SystemPrompt::Inline("be terse")), None);
        let idx = args.iter().position(|a| a == "--append-system-prompt");
        assert!(idx.is_some());
        assert_eq!(args[idx.unwrap() + 1], "be terse");
    }

    #[test]
    fn a_system_prompt_file_is_named_not_inlined() {
        let file = system_prompt_file("line one\nline two").unwrap();
        let args = build_args(Some(SystemPrompt::File(file.path())), None);
        let idx = args
            .iter()
            .position(|a| a == "--append-system-prompt-file")
            .expect("file flag");
        assert_eq!(
            std::fs::read_to_string(&args[idx + 1]).unwrap(),
            "line one\nline two"
        );
        assert!(!args.contains(&"--append-system-prompt".to_string()));
    }

    #[test]
    fn the_operators_mcp_servers_are_not_attached_to_a_completion() {
        // `--tools ""` only covers built-in tools. Without `--safe-mode` the
        // operator's MCP servers stayed live, the model reached for one, and
        // the single allowed turn was spent on a tool call — `error_max_turns`,
        // exit 1, HTTP 502 on every chat turn. It is also the difference
        // between a completion that depends on the prompt and one that depends
        // on the operator's machine.
        let args = build_args(None, None);
        assert!(
            args.contains(&"--safe-mode".to_string()),
            "a completion must not inherit MCP servers, CLAUDE.md, or hooks; got {args:?}"
        );
    }

    #[test]
    fn the_prompt_never_rides_on_argv() {
        // A positional prompt right after `--tools` (which takes a list) was
        // once parsed as one more tool name, and every call died with "Input
        // must be provided either through stdin or as a prompt argument" (CLI
        // 2.1.212) — only for calls without a system prompt, so the mission
        // planner went silent while directors kept working. It is on stdin
        // now, which also keeps it clear of Windows' command-line limits. No
        // argument may be anything but a flag or a flag's value.
        for system in [None, Some(SystemPrompt::Inline("be terse"))] {
            let args = build_args(system, Some("opus"));
            let last = args.last().unwrap();
            let before_last = &args[args.len() - 2];
            assert!(
                before_last.starts_with("--") || last.starts_with("--"),
                "trailing positional in {args:?}"
            );
            assert!(!args.contains(&"--".to_string()), "{args:?}");
        }
    }

    #[test]
    fn a_missing_or_sentinel_model_still_names_one() {
        for model in [None, Some("default"), Some("DEFAULT"), Some("  ")] {
            let args = build_args(None, model);
            let at = args
                .iter()
                .position(|a| a == "--model")
                .unwrap_or_else(|| panic!("--model missing for model={model:?}: {args:?}"));
            assert_eq!(args[at + 1], DEFAULT_MODEL_ALIAS, "model={model:?}");
        }
    }

    /// The whole path against the real CLI: prompt on stdin, a multi-line
    /// system prompt, both the inline and the file form. Needs a signed-in
    /// `claude`, so it only runs on request (`-- --ignored`).
    #[tokio::test]
    #[ignore]
    async fn a_real_completion_reads_its_prompt_from_stdin() {
        let provider = ClaudeCodeProvider::new(Some("haiku".to_string())).expect("claude CLI");
        let request = CompletionRequest {
            model: String::new(),
            messages: vec![
                msg(
                    Role::System,
                    "Answer in UPPERCASE.\nNever add punctuation or other words.",
                ),
                msg(Role::User, "Say the fruit\nbanana"),
            ],
            tools: None,
            max_tokens: None,
            temperature: None,
            stream: false,
        };
        let reply = provider.complete(request).await.expect("completion");
        let text = reply.content.unwrap_or_default();
        assert!(
            text.contains("BANANA"),
            "system prompt ignored? got {text:?}"
        );

        let file = system_prompt_file("Answer in UPPERCASE. No punctuation.").unwrap();
        let mut cmd = platform::command(&provider.program);
        cmd.args(build_args(
            Some(SystemPrompt::File(file.path())),
            Some("haiku"),
        ));
        let out = platform::output_with_stdin(cmd, b"Say the fruit apple")
            .await
            .unwrap();
        let parsed: CliResult = serde_json::from_slice(&out.stdout).expect("json");
        assert!(
            parsed.result.unwrap_or_default().contains("APPLE"),
            "system prompt file ignored"
        );
    }

    #[test]
    fn an_empty_system_prompt_is_omitted() {
        let args = build_args(Some(SystemPrompt::Inline("   ")), None);
        assert!(!args.contains(&"--append-system-prompt".to_string()));
    }
}
