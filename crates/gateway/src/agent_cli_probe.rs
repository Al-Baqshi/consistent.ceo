//! Detect and verify the coding-agent CLIs installed on the operator's machine.
//!
//! The product promise is that Claude Code works **out of the box**: if the
//! operator has already installed and logged into the `claude` CLI, Consistent
//! CEO should just use it — no MCP server to configure, no keys to paste.
//! Making good on that starts with being able to *say* whether it's there.
//!
//! Before this module, a missing CLI was indistinguishable from a failed run:
//! [`consistent_world_tools::coding_agent::CodingAgentTool`] falls back to the
//! bare program name when it can't find the binary, so "claude is not
//! installed" surfaced as a spawn error in the middle of a task, minutes after
//! the operator asked for something.
//!
//! Two tiers, deliberately separated by cost:
//!
//! - [`probe`] — free and instant. Is the binary on disk, what version, and do
//!   credentials appear to exist? Safe to call on every settings page load.
//! - [`verify`] — spends a token or two. Actually runs the CLI headless and
//!   reads its answer, which is the only way to tell "installed" from "logged
//!   in". Behind an explicit button, mirroring [`crate::wizard::test_llm`].

use consistent_world_llm::claude_code::DEFAULT_MODEL_ALIAS;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use consistent_world_tools::coding_agent::locate_program;

/// The CLIs the coding-agent tool knows how to drive.
pub const KNOWN_KINDS: [&str; 4] = ["claude", "codex", "opencode", "cursor-agent"];

/// `--version` should answer immediately; anything slower is a broken install.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
/// A one-turn round trip to the model, including cold start.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(90);
/// Kept short so a chatty CLI can't balloon an API response.
const OUTPUT_CAP: usize = 2_000;

/// Result of the cheap, disk-only [`probe`] for one CLI kind.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliStatus {
    /// `"claude"` | `"opencode"` | `"cursor-agent"`.
    pub kind: String,
    /// Binary found on disk.
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whatever `--version` printed, trimmed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Credentials *appear* to be present on disk. A heuristic, not proof —
    /// only [`verify`] can actually confirm the CLI is logged in.
    pub credentials_likely: bool,
    /// Why the CLI is unusable, when it is. Surfaced verbatim to the operator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Result of actually running the CLI, from [`verify`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliVerification {
    pub kind: String,
    /// The CLI ran and answered. This is the real "it works" signal.
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// Char-safe truncation (not byte-indexed) so a multi-byte UTF-8 char is
// never split, plus a trailing ellipsis when truncated.
fn truncate(s: &str, max: usize) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Does this CLI look logged in?
///
/// Each vendor stores credentials differently and none of it is a stable
/// contract, so this only ever downgrades the UI from "ready" to "may need
/// sign-in" — it never blocks a run. `verify` is the authority.
fn credentials_likely(kind: &str) -> bool {
    let Some(home) = consistent_world_common::platform::home_dir() else {
        return false;
    };
    match kind {
        // The CLI keeps `~/.claude.json` alongside a `~/.claude/` state dir;
        // either one existing means it has been set up at least once.
        "claude" => home.join(".claude.json").exists() || home.join(".claude").is_dir(),
        // `codex login` writes `~/.codex/auth.json`; the bare state dir also
        // appears after any run, so check the auth file first.
        "codex" => home.join(".codex/auth.json").exists() || home.join(".codex").is_dir(),
        "opencode" => {
            home.join(".config/opencode").is_dir() || home.join(".local/share/opencode").is_dir()
        }
        "cursor-agent" => home.join(".cursor").is_dir(),
        _ => false,
    }
}

// Runs `<program> --version`; `None` on timeout, spawn failure, non-zero
// exit, or empty output — any of which just means "no version to report".
async fn read_version(program: &str) -> Option<String> {
    let output = tokio::time::timeout(
        VERSION_TIMEOUT,
        consistent_world_common::platform::command(program)
            .arg("--version")
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Cheap check: on disk? what version? credentials present?
pub async fn probe(kind: &str) -> CliStatus {
    let Some(path) = locate_program(kind) else {
        return CliStatus {
            kind: kind.to_string(),
            installed: false,
            path: None,
            version: None,
            credentials_likely: false,
            detail: Some(format!(
                "`{kind}` was not found on PATH or in the usual install locations."
            )),
        };
    };

    let program = path.to_string_lossy().to_string();
    let version = read_version(&program).await;
    // A binary that won't answer `--version` is present but broken — worth
    // saying so, since the operator will otherwise see it as "installed" and
    // wonder why runs fail.
    let detail = version
        .is_none()
        .then(|| format!("`{kind}` is installed but did not respond to --version."));

    CliStatus {
        kind: kind.to_string(),
        installed: true,
        path: Some(program),
        version,
        credentials_likely: credentials_likely(kind),
        detail,
    }
}

/// Probe every CLI the tool layer knows about, concurrently.
pub async fn probe_all() -> Vec<CliStatus> {
    futures::future::join_all(KNOWN_KINDS.iter().map(|k| probe(k))).await
}

/// The headless invocation that proves a CLI is installed *and* authenticated.
///
/// Deliberately the smallest possible real request: one turn, a two-token
/// answer. Anything less doesn't touch the auth path; anything more spends the
/// operator's money to tell us what this already tells us.
fn verify_invocation(kind: &str) -> Option<Vec<String>> {
    let prompt = "Reply with only: OK".to_string();
    match kind {
        // `--model` is not optional: without it the CLI uses the operator's
        // interactive `/model` choice, which an older CLI may not be able to
        // serve — and that failure would read here as "not signed in". Same
        // alias the completion adapter pins.
        "claude" => Some(vec![
            "-p".to_string(),
            "--max-turns".to_string(),
            "1".to_string(),
            "--model".to_string(),
            DEFAULT_MODEL_ALIAS.to_string(),
            prompt,
        ]),
        "opencode" => Some(vec!["run".to_string(), prompt]),
        "cursor-agent" => Some(vec!["-p".to_string(), prompt]),
        "codex" => Some(vec![
            "exec".to_string(),
            "--json".to_string(),
            "--ephemeral".to_string(),
            "--sandbox".to_string(),
            "read-only".to_string(),
            "--skip-git-repo-check".to_string(),
            prompt,
        ]),
        _ => None,
    }
}

/// Run the CLI for real. This is what distinguishes "installed" from "usable":
/// an unauthenticated CLI exits non-zero and explains itself on stderr, and
/// that explanation is passed through untouched rather than reworded — the
/// vendor's own "run `claude login`" is more useful than anything we'd write.
pub async fn verify(kind: &str) -> CliVerification {
    let status = probe(kind).await;
    if !status.installed {
        return CliVerification {
            kind: kind.to_string(),
            ok: false,
            output: None,
            detail: status.detail,
        };
    }
    let Some(args) = verify_invocation(kind) else {
        return CliVerification {
            kind: kind.to_string(),
            ok: false,
            output: None,
            detail: Some(format!("Don't know how to verify `{kind}`.")),
        };
    };
    let program = status.path.unwrap_or_else(|| kind.to_string());

    let result = tokio::time::timeout(
        VERIFY_TIMEOUT,
        consistent_world_common::platform::command(&program)
            .args(&args)
            .output(),
    )
    .await;

    match result {
        Err(_) => CliVerification {
            kind: kind.to_string(),
            ok: false,
            output: None,
            detail: Some(format!(
                "`{kind}` did not answer within {}s.",
                VERIFY_TIMEOUT.as_secs()
            )),
        },
        Ok(Err(e)) => CliVerification {
            kind: kind.to_string(),
            ok: false,
            output: None,
            detail: Some(format!("Failed to run `{kind}`: {e}")),
        },
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if output.status.success() {
                CliVerification {
                    kind: kind.to_string(),
                    ok: true,
                    output: Some(truncate(&stdout, OUTPUT_CAP)),
                    detail: None,
                }
            } else {
                // stderr carries the sign-in instruction when there is one.
                let reason = if stderr.trim().is_empty() {
                    truncate(&stdout, OUTPUT_CAP)
                } else {
                    truncate(&stderr, OUTPUT_CAP)
                };
                CliVerification {
                    kind: kind.to_string(),
                    ok: false,
                    output: None,
                    detail: Some(if reason.is_empty() {
                        format!("`{kind}` exited with {}.", output.status)
                    } else {
                        reason
                    }),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_kinds_have_no_verify_invocation() {
        assert!(verify_invocation("definitely-not-an-agent").is_none());
    }

    #[test]
    fn claude_verification_is_one_turn_and_headless() {
        let args = verify_invocation("claude").expect("claude is a known kind");
        assert!(args.contains(&"-p".to_string()), "must run headless");
        assert!(args.contains(&"--max-turns".to_string()));
        let at = args
            .iter()
            .position(|a| a == "--model")
            .expect("must pin a model rather than ride the CLI's /model default");
        assert_eq!(args[at + 1], DEFAULT_MODEL_ALIAS);
        assert!(
            args.contains(&"1".to_string()),
            "one turn keeps the check cheap"
        );
        // The check must never be able to touch the operator's files.
        assert!(!args.iter().any(|a| a.contains("dangerously")));
    }

    #[tokio::test]
    async fn a_missing_cli_probes_as_not_installed_with_a_reason() {
        let status = probe("definitely-not-an-agent-binary").await;
        assert!(!status.installed);
        assert!(status.path.is_none());
        assert!(status.detail.is_some_and(|d| d.contains("not found")));
    }

    #[tokio::test]
    async fn verifying_a_missing_cli_fails_without_spawning() {
        let v = verify("definitely-not-an-agent-binary").await;
        assert!(!v.ok);
        assert!(v.detail.is_some());
    }

    #[tokio::test]
    async fn probe_all_covers_every_known_kind() {
        let all = probe_all().await;
        assert_eq!(all.len(), KNOWN_KINDS.len());
        for kind in KNOWN_KINDS {
            assert!(all.iter().any(|s| s.kind == kind));
        }
    }

    #[test]
    fn truncate_is_char_safe() {
        let s = "é".repeat(50);
        assert_eq!(truncate(&s, 10).chars().count(), 10);
    }
}
