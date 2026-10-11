//! The `coding_agent` tool: delegates a coding task to an external coding-agent CLI (opencode,
//! claude, cursor-agent, codex), spawned as a subprocess in a project directory and run to
//! completion. Runs unattended within `allowed_cwd_roots`, same "code work is gated inward, not
//! outward" posture as the other local-execution tools in this crate.

use crate::Tool;
use async_trait::async_trait;
use consistent_world_common::platform;
use consistent_world_common::types::ToolResult;
use std::path::{Path, PathBuf};
use tokio::time;

/// Spawns one of the configured coding-agent CLIs with a prompt and waits for it to exit.
pub struct CodingAgentTool {
    pub working_dir: String,
    pub allowed_cwd_roots: Vec<String>,
    pub allowed_kinds: Vec<String>,
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
}

impl Default for CodingAgentTool {
    fn default() -> Self {
        Self {
            working_dir: ".".to_string(),
            allowed_cwd_roots: Vec::new(),
            allowed_kinds: vec![
                "opencode".to_string(),
                "claude".to_string(),
                "cursor-agent".to_string(),
                "codex".to_string(),
            ],
            default_timeout_secs: 600,
            max_timeout_secs: 1800,
        }
    }
}

#[async_trait]
impl Tool for CodingAgentTool {
    fn name(&self) -> &'static str {
        "coding_agent"
    }

    fn description(&self) -> &'static str {
        "Delegate a coding task to an external coding agent CLI (opencode, claude, cursor-agent) \
         running in a project directory. Returns the agent's final output."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["opencode", "claude", "cursor-agent"],
                    "description": "Which coding agent CLI to invoke"
                },
                "prompt": {
                    "type": "string",
                    "description": "The instruction to give the coding agent"
                },
                "cwd": {
                    "type": "string",
                    "description": "Project directory to run in. Defaults to the tool's working_dir."
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Timeout in seconds (default 600, max 1800)"
                }
            },
            "required": ["kind", "prompt"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let kind = arguments
            .get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("coding_agent missing required `kind`"))?
            .to_string();
        let prompt = arguments
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("coding_agent missing required `prompt`"))?
            .to_string();

        if !self.allowed_kinds.iter().any(|k| k == &kind) {
            return Ok(policy_error(
                "CODING_AGENT_KIND_NOT_ALLOWED",
                format!("kind '{}' is not allowed", kind),
            ));
        }
        if prompt.trim().is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: "Empty prompt".to_string(),
                is_error: true,
            });
        }

        let cwd_input = arguments
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| self.working_dir.clone());
        let resolved_cwd = match canonicalize_path_for_cwd(&cwd_input) {
            Ok(p) => p,
            Err(e) => return Ok(policy_error("CODING_AGENT_CWD_INVALID", e.to_string())),
        };

        if !self.allowed_cwd_roots.is_empty() {
            let mut allowed = false;
            for root in &self.allowed_cwd_roots {
                if let Ok(root_path) = Path::new(root).canonicalize()
                    && resolved_cwd.starts_with(&root_path)
                {
                    allowed = true;
                    break;
                }
            }
            if !allowed {
                return Ok(policy_error(
                    "CODING_AGENT_CWD_NOT_ALLOWED",
                    format!("cwd '{}' is outside allowed roots", resolved_cwd.display()),
                ));
            }
        }

        let timeout_secs = arguments
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(self.default_timeout_secs)
            .min(self.max_timeout_secs);

        let invocation = build_invocation(&kind, &prompt);

        let mut child = platform::command(&invocation.program);
        child
            .args(&invocation.args)
            .current_dir(platform::process_path(&resolved_cwd));
        child.kill_on_drop(true);
        let stdin = invocation.stdin.unwrap_or_default();

        let output = match time::timeout(
            time::Duration::from_secs(timeout_secs),
            platform::output_with_stdin(child, stdin.as_bytes()),
        )
        .await
        {
            Ok(Ok(out)) => out,
            Ok(Err(e)) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Failed to spawn '{}': {}", invocation.program, e),
                    is_error: true,
                });
            }
            Err(_) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Coding agent timed out after {timeout_secs}s"),
                    is_error: true,
                });
            }
        };

        let mut combined = String::new();
        combined.push_str(&String::from_utf8_lossy(&output.stdout));
        if !output.stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: combined.trim_end().to_string(),
            is_error: !output.status.success(),
        })
    }
}

/// A resolved coding-agent command line.
struct Invocation {
    program: String,
    args: Vec<String>,
    /// The prompt, for CLIs that read it from stdin. Keeping it off argv is
    /// what lets a long, multi-line prompt survive Windows, where these CLIs
    /// are often `.cmd` shims and `cmd.exe` takes neither newlines nor more
    /// than 8,191 characters.
    stdin: Option<String>,
}

// Map a `kind` to the program to run and the CLI flags that make it execute `prompt`
// non-interactively and, where the CLI has such a flag, without pausing for approval.
fn build_invocation(kind: &str, prompt: &str) -> Invocation {
    let on_argv = |program: &str, mut args: Vec<String>| {
        args.push(prompt.to_string());
        Invocation {
            program: resolve_program(program),
            args,
            stdin: None,
        }
    };
    let on_stdin = |program: &str, args: Vec<String>| Invocation {
        program: resolve_program(program),
        args,
        stdin: Some(prompt.to_string()),
    };
    match kind {
        // `claude -p` and `codex exec` both read a missing prompt from stdin.
        "claude" => on_stdin(
            "claude",
            vec![
                "-p".to_string(),
                "--dangerously-skip-permissions".to_string(),
            ],
        ),
        "codex" => on_stdin(
            "codex",
            vec![
                "exec".to_string(),
                "--sandbox".to_string(),
                "workspace-write".to_string(),
                "--approve-for-me".to_string(),
            ],
        ),
        "opencode" => on_argv("opencode", vec!["run".to_string()]),
        "cursor-agent" => on_argv("cursor-agent", vec!["-p".to_string()]),
        other => on_argv(other, Vec::new()),
    }
}

/// Find a CLI on disk: `$PATH` first, then the install locations these agents
/// commonly land in but which a GUI-launched process often doesn't inherit
/// (the desktop shell spawns the gateway with a login-shell-less environment).
/// See [`platform::find_executable`], which also handles Windows' `.exe`/`.cmd`.
///
/// `None` means genuinely not installed — callers that need to *report*
/// availability (onboarding, the settings integrations card) depend on that
/// distinction, which is why this returns an Option and
/// [`resolve_program`] does the lossy fallback separately.
pub fn locate_program(name: &str) -> Option<PathBuf> {
    platform::find_executable(name)
}

/// Absolute path when the binary is found, else the bare name so the OS gets a
/// chance (and the resulting spawn error names the missing program).
fn resolve_program(name: &str) -> String {
    locate_program(name)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| name.to_string())
}

// Resolve a possibly-relative cwd against the process's current directory, then canonicalize —
// so the `allowed_cwd_roots` check compares real paths, not `..`-laden strings.
fn canonicalize_path_for_cwd(path: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(path);
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    Ok(absolute.canonicalize()?)
}

// Build the JSON-shaped error result the model sees for any policy refusal, tagged with a
// stable `code` so a caller can branch on the reason rather than parsing prose.
fn policy_error(code: &'static str, message: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        output: serde_json::json!({
            "type": "policy_denied",
            "code": code,
            "message": message.into(),
        })
        .to_string(),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_and_codex_take_the_prompt_on_stdin() {
        let prompt = "fix the build\nthen run the tests";
        for kind in ["claude", "codex"] {
            let inv = build_invocation(kind, prompt);
            assert_eq!(inv.stdin.as_deref(), Some(prompt), "{kind}");
            assert!(
                !inv.args.iter().any(|a| a == prompt),
                "{kind}: {:?}",
                inv.args
            );
        }
        let inv = build_invocation("opencode", prompt);
        assert_eq!(inv.args.last().map(String::as_str), Some(prompt));
        assert!(inv.stdin.is_none());
    }
}
