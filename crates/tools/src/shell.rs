//! The `shell` tool: runs a command via `sh -lc` on the host (PowerShell on Windows; see
//! [`platform::shell_command`]), gated by an allow/deny command
//! list and an optional [`ShellPolicy`] (capability + risk-pattern + cwd checks). Part of the
//! executor's "unattended for code, gated outward" posture — this module is what actually
//! spawns the process, so its checks are the last line of defense before anything runs.

use crate::Tool;
use async_trait::async_trait;
use consistent_world_common::capabilities::CapabilityId;
use consistent_world_common::platform;
use consistent_world_common::types::ToolResult;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tokio::time;

/// Executes shell commands in `working_dir` (or a per-call `cwd`), subject to the static
/// allow/deny lists and, if set, `policy`.
pub struct ShellTool {
    pub allowed_commands: Option<Vec<String>>,
    pub denied_commands: Vec<String>,
    pub working_dir: String,
    pub policy: Option<ShellPolicy>,
}

/// Per-call `_policy` override, deserialized from the `_policy` argument the task-execution path
/// injects. Any field left unset keeps the tool's own configured value.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellPolicyOverride {
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    allowed_capabilities: Vec<CapabilityId>,
    #[serde(default)]
    approval_required_patterns: Vec<String>,
    #[serde(default)]
    hard_deny_patterns: Vec<String>,
    #[serde(default)]
    allowed_cwd_roots: Vec<String>,
}

/// Access-control policy for the shell tool: which capabilities are granted, which command
/// substrings are hard-denied or need human approval, and which cwd roots are allowed.
#[derive(Debug, Clone)]
pub struct ShellPolicy {
    pub deny_by_default: bool,
    pub allowed_capabilities: Vec<CapabilityId>,
    pub approval_required_patterns: Vec<String>,
    pub hard_deny_patterns: Vec<String>,
    pub allowed_cwd_roots: Vec<String>,
}

/// Commands that pause for a human before they run, as the gateway configures its shell tools.
///
/// Unix and Windows spell privilege and ownership changes differently, so each platform gets its
/// own words. These are substring checks — a speed bump for a well-meaning model, not a sandbox.
pub fn default_approval_required_patterns() -> Vec<String> {
    let patterns: &[&str] = if cfg!(windows) {
        &[
            "-Verb RunAs",
            "runas ",
            "icacls ",
            "takeown ",
            "Set-ExecutionPolicy",
            "reg add",
            "reg delete",
        ]
    } else {
        &["sudo", "chmod 777", "chown "]
    };
    patterns.iter().map(|p| p.to_string()).collect()
}

/// Commands the gateway's shell tools refuse outright; see [`default_approval_required_patterns`].
pub fn default_hard_deny_patterns() -> Vec<String> {
    let patterns: &[&str] = if cfg!(windows) {
        &[
            "Format-Volume",
            "Clear-Disk",
            "diskpart",
            "format c:",
            "rd /s /q c:\\",
            "Remove-Item -Recurse -Force C:\\",
            "rm -rf /",
        ]
    } else {
        &["rm -rf /"]
    };
    patterns.iter().map(|p| p.to_string()).collect()
}

// Does `command` contain `pattern`? PowerShell and cmd ignore case, so on Windows the match does
// too — otherwise `format-volume` would sail past `Format-Volume`.
fn command_matches(command: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    if cfg!(windows) {
        command.to_lowercase().contains(&pattern.to_lowercase())
    } else {
        command.contains(pattern)
    }
}

impl ShellTool {
    // The working directory a call runs in: the `_policy.workingDir` override when present,
    // else the tool's own configured `working_dir`.
    fn effective_working_dir(&self, arguments: &serde_json::Value) -> String {
        arguments
            .get("_policy")
            .cloned()
            .and_then(|v| serde_json::from_value::<ShellPolicyOverride>(v).ok())
            .and_then(|p| p.working_dir)
            .unwrap_or_else(|| self.working_dir.clone())
    }

    // Merge the `_policy` override onto the tool's configured policy: any list the override
    // sets replaces the corresponding list wholesale, not just adds to it.
    fn effective_policy(&self, arguments: &serde_json::Value) -> Option<ShellPolicy> {
        let mut policy = self.policy.clone()?;
        let Some(override_policy) = arguments
            .get("_policy")
            .cloned()
            .and_then(|v| serde_json::from_value::<ShellPolicyOverride>(v).ok())
        else {
            return Some(policy);
        };
        if !override_policy.allowed_capabilities.is_empty() {
            policy.allowed_capabilities = override_policy.allowed_capabilities;
        }
        if !override_policy.approval_required_patterns.is_empty() {
            policy.approval_required_patterns = override_policy.approval_required_patterns;
        }
        if !override_policy.hard_deny_patterns.is_empty() {
            policy.hard_deny_patterns = override_policy.hard_deny_patterns;
        }
        if !override_policy.allowed_cwd_roots.is_empty() {
            policy.allowed_cwd_roots = override_policy.allowed_cwd_roots;
        }
        Some(policy)
    }
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &'static str {
        "shell"
    }

    fn description(&self) -> &'static str {
        // The model writes the command, so it has to know which language it is writing.
        if cfg!(windows) {
            "Execute shell commands on the host system. The host is Windows: commands run in \
             PowerShell, so use PowerShell syntax (Get-ChildItem, $env:NAME, `;` between commands)."
        } else {
            "Execute shell commands on the host system"
        }
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The shell command to execute" },
                "timeout_secs": { "type": "integer", "description": "Timeout in seconds", "default": 30 },
                "cwd": { "type": "string", "description": "Optional working directory override" },
                "capability": { "type": "string", "description": "Requested shell capability", "default": "shell.exec.safe" }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, arguments: serde_json::Value) -> anyhow::Result<ToolResult> {
        let command = arguments
            .get("command")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("shell tool missing required `command` string"))?
            .trim()
            .to_string();
        let requested_capability = arguments
            .get("capability")
            .and_then(|v| v.as_str())
            .unwrap_or("shell.exec.safe")
            .to_string();
        let requested_capability = match CapabilityId::from_str(&requested_capability) {
            Ok(c) => c,
            Err(e) => {
                return Ok(policy_error("SHELL_CAPABILITY_INVALID", e));
            }
        };
        let cwd_override = arguments
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let timeout_secs = arguments
            .get("timeout_secs")
            .and_then(|v| v.as_u64())
            .unwrap_or(30)
            .min(300);

        if command.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: "Empty command".to_string(),
                is_error: true,
            });
        }

        // Static deny list: a plain substring match against the raw command, checked before the
        // richer `ShellPolicy` below even runs.
        if self
            .denied_commands
            .iter()
            .any(|denied| command_matches(&command, denied))
        {
            return Ok(ToolResult {
                call_id: String::new(),
                output: "Command denied by policy".to_string(),
                is_error: true,
            });
        }

        // Static allow list, if configured: the command must equal or start with one of the
        // configured prefixes.
        if let Some(allow) = &self.allowed_commands {
            let ok = allow.iter().any(|a| {
                let a = a.trim();
                !a.is_empty() && (command == a || command.starts_with(a))
            });
            if !ok {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "Command not in allow list".to_string(),
                    is_error: true,
                });
            }
        }

        let effective_cwd = cwd_override.unwrap_or_else(|| self.effective_working_dir(&arguments));
        let resolved_cwd = match canonicalize_path_for_cwd(&effective_cwd) {
            Ok(p) => p,
            Err(e) => {
                return Ok(policy_error("SHELL_CWD_INVALID", e.to_string()));
            }
        };

        let effective_policy = self.effective_policy(&arguments);
        if let Some(policy) = &effective_policy
            && let Err(denial) =
                check_policy(policy, &requested_capability, &command, &resolved_cwd)
        {
            return Ok(denial);
        }

        let (program, args) = platform::shell_command(&command, true);
        let mut child = platform::command(program);
        child
            .args(args)
            .current_dir(platform::process_path(&resolved_cwd));
        child.kill_on_drop(true);

        let output =
            match time::timeout(time::Duration::from_secs(timeout_secs), child.output()).await {
                Ok(res) => res?,
                Err(_) => {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Command timed out after {timeout_secs}s"),
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

// Resolve a possibly-relative cwd against the process's current directory, then canonicalize —
// so policy root checks compare real paths, not `..`-laden strings.
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

// Run one command through the full `ShellPolicy`: capability check, then hard-deny patterns,
// then approval-required patterns, then the allowed cwd roots — first match wins.
fn check_policy(
    policy: &ShellPolicy,
    requested_capability: &CapabilityId,
    command: &str,
    cwd: &Path,
) -> Result<(), ToolResult> {
    // An empty capability list under deny_by_default means "nothing allowed",
    // not "everything allowed" — callers must opt capabilities in explicitly.
    let capability_allowed = if policy.allowed_capabilities.is_empty() {
        !policy.deny_by_default
    } else {
        policy
            .allowed_capabilities
            .iter()
            .any(|c| c == requested_capability)
    };
    if !capability_allowed {
        return Err(policy_error(
            "SHELL_CAPABILITY_DENIED",
            format!(
                "capability '{}' is not allowed for this shell context",
                requested_capability
            ),
        ));
    }

    if policy
        .hard_deny_patterns
        .iter()
        .any(|p| command_matches(command, p))
    {
        return Err(policy_error(
            "SHELL_RISK_HARD_DENY",
            "command matches a hard-deny shell risk pattern",
        ));
    }

    if policy
        .approval_required_patterns
        .iter()
        .any(|p| command_matches(command, p))
    {
        return Err(policy_error(
            "SHELL_NEEDS_HUMAN_APPROVAL",
            "command matches approval-required shell risk pattern",
        ));
    }

    if !policy.allowed_cwd_roots.is_empty() {
        let mut allowed = false;
        for root in &policy.allowed_cwd_roots {
            let root_path = match Path::new(root).canonicalize() {
                Ok(p) => p,
                Err(_) => {
                    return Err(policy_error(
                        "SHELL_POLICY_ROOT_INVALID",
                        format!("configured cwd root is invalid: {}", root),
                    ));
                }
            };
            if cwd.starts_with(&root_path) {
                allowed = true;
                break;
            }
        }
        if !allowed && policy.deny_by_default {
            return Err(policy_error(
                "SHELL_CWD_NOT_ALLOWED",
                format!("cwd '{}' is outside allowed roots", cwd.display()),
            ));
        }
    }

    Ok(())
}
