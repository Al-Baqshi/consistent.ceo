//! Agent review loop: after an agent run completes, a second (reviewer) agent can
//! check its output. This is the "builder does work, reviewer checks it" spine.
//!
//! Deterministic by construction: policy parsing, the reviewer prompt, and the
//! verdict are all rule-based (the reviewer LLM/CLI is the only non-deterministic
//! part, and its influence is reduced to a single PASS / NEEDS_REVIEW verdict).
//! The state transitions here never fail a run and never delete work — they only
//! attach a review and, when the reviewer flags issues, mark the linked task
//! `NeedsReview` for a human.

use chrono::Utc;
use uuid::Uuid;

use crate::state::{ControlState, ExternalAgentRun, TaskOutcome};

/// How an agent run should be reviewed after it completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewPolicy {
    /// No review.
    None,
    /// Flag the run for a human to review; do not auto-create a reviewer.
    Manual,
    /// Automatically run a reviewer agent over the output.
    Auto,
}

impl ReviewPolicy {
    /// Parse a stored/config policy string. Case-insensitive, trims
    /// whitespace; anything unrecognized (including `None`) is `None`.
    pub fn parse(value: Option<&str>) -> ReviewPolicy {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            Some("manual") => ReviewPolicy::Manual,
            Some("auto") => ReviewPolicy::Auto,
            _ => ReviewPolicy::None,
        }
    }

    /// Inverse of [`Self::parse`], for persisting the policy back out.
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewPolicy::None => "none",
            ReviewPolicy::Manual => "manual",
            ReviewPolicy::Auto => "auto",
        }
    }
}

/// Deterministic reviewer verdict derived from the reviewer's output.
#[derive(Debug, Clone)]
pub struct ReviewVerdict {
    pub needs_review: bool,
    pub reason: String,
}

/// Build the prompt handed to the reviewer agent. It asks for a check (not a
/// redo) and a single explicit verdict line the parser can key on.
pub fn review_prompt(original_prompt: &str, original_output: &str) -> String {
    format!(
        "You are a senior reviewer. Another AI agent was given a task and produced the output below. \
Review it for correctness, completeness, and obvious problems. Do NOT redo the work.\n\n\
End your reply with exactly one line, either:\n\
`VERDICT: PASS` — if the output looks correct and complete, or\n\
`VERDICT: NEEDS_REVIEW <short reason>` — if it has issues a human should check.\n\n\
## Original task\n{original_prompt}\n\n## Agent output\n{original_output}\n"
    )
}

/// Parse a reviewer verdict from output. Explicit `VERDICT:` markers win; absent a
/// clear PASS, the default is cautious (`needs_review`).
pub fn parse_review_verdict(output: &str) -> ReviewVerdict {
    let lower = output.to_ascii_lowercase();
    if lower.contains("verdict: needs_review") || lower.contains("verdict:needs_review") {
        return ReviewVerdict {
            needs_review: true,
            reason: reason_after_marker(output).unwrap_or_else(|| "reviewer flagged issues".into()),
        };
    }
    if lower.contains("verdict: pass") || lower.contains("verdict:pass") {
        return ReviewVerdict {
            needs_review: false,
            reason: String::new(),
        };
    }
    // No explicit verdict. Fall back to the deterministic output checks; if those
    // are clean we still default to needs_review, since the reviewer did not
    // clearly sign off (adversarial-by-default).
    let checked = crate::verification::verify_task_output(output);
    if checked.needs_review() {
        ReviewVerdict {
            needs_review: true,
            reason: checked.reason,
        }
    } else {
        ReviewVerdict {
            needs_review: true,
            reason: "reviewer did not return a clear PASS verdict".into(),
        }
    }
}

// Pull the short reason that follows a `NEEDS_REVIEW` marker: the rest of
// that line, with leading punctuation/spacing stripped and capped at 200
// chars so a verbose reviewer can't blow out the stored reason.
fn reason_after_marker(output: &str) -> Option<String> {
    let lower = output.to_ascii_lowercase();
    let idx = lower.find("needs_review")?;
    let after = output[idx + "needs_review".len()..].trim();
    let reason: String = after
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches([':', '-', ' '])
        .chars()
        .take(200)
        .collect();
    if reason.trim().is_empty() {
        None
    } else {
        Some(reason.trim().to_string())
    }
}

/// Create (but do not execute) a reviewer run for a completed primary run, and
/// link the two. Idempotent: returns `None` if the run is missing or already has
/// a reviewer. The caller executes the returned run and later calls
/// [`finish_review`]. Deterministic state change; no CLI here.
pub async fn start_review(
    control: &ControlState,
    original_run_id: &str,
    reviewer_provider: &str,
) -> Option<ExternalAgentRun> {
    let original = {
        control
            .external_agent_runs
            .read()
            .await
            .iter()
            .find(|r| r.id == original_run_id)
            .cloned()?
    };
    if original.review_run_id.is_some() {
        return None; // review already requested
    }
    let now = Utc::now().to_rfc3339();
    let reviewer = ExternalAgentRun {
        id: Uuid::new_v4().to_string(),
        workspace_id: original.workspace_id.clone(),
        entity_id: original.entity_id.clone(),
        provider: reviewer_provider.to_string(),
        prompt: review_prompt(&original.prompt, &original.output),
        cwd: original.cwd.clone(),
        status: "running".to_string(),
        task_id: original.task_id.clone(),
        output: String::new(),
        error: None,
        started_at: now,
        completed_at: None,
        metadata: serde_json::json!({ "kind": "review", "reviewedRunId": original.id }),
        review_policy: Some("none".to_string()),
        review_status: None,
        review_verdict: None,
        review_run_id: None,
        reviewed_run_id: Some(original.id.clone()),
    };
    {
        let mut runs = control.external_agent_runs.write().await;
        runs.push(reviewer.clone());
        if let Some(orig) = runs.iter_mut().find(|r| r.id == original_run_id) {
            orig.review_run_id = Some(reviewer.id.clone());
            orig.review_status = Some("review_running".to_string());
        }
    }
    Some(reviewer)
}

/// Record a reviewer run's result: store its output, capture it as a raw capture,
/// derive the verdict, attach it to the original run, and — if the reviewer
/// flagged issues — mark the original run's linked task `NeedsReview`.
pub async fn finish_review(
    control: &ControlState,
    reviewer_run_id: &str,
    output: &str,
    status: &str,
) {
    let (reviewed_run_id, entity_id, workspace_id) = {
        let mut runs = control.external_agent_runs.write().await;
        let Some(reviewer) = runs.iter_mut().find(|r| r.id == reviewer_run_id) else {
            return;
        };
        reviewer.output = output.to_string();
        reviewer.status = status.to_string();
        reviewer.completed_at = Some(Utc::now().to_rfc3339());
        (
            reviewer.reviewed_run_id.clone(),
            reviewer.entity_id.clone(),
            reviewer.workspace_id.clone(),
        )
    };
    let Some(original_id) = reviewed_run_id else {
        return;
    };

    // Capture reviewer output into the raw capture inbox (attached to the entity).
    if !output.trim().is_empty() {
        let _ = crate::control_api::internal_create_capture(
            control,
            &workspace_id,
            None,
            crate::control_api::CreateCaptureRequest {
                raw_text: output.to_string(),
                entity_id: Some(entity_id),
                source: Some("agent_review".to_string()),
                source_ref: Some(reviewer_run_id.to_string()),
                session_id: None,
                provider: None,
                summary: Some("Reviewer output".to_string()),
                metadata: Some(serde_json::json!({
                    "kind": "review",
                    "reviewedRunId": original_id,
                    "reviewerRunId": reviewer_run_id,
                })),
            },
        )
        .await;
    }

    let verdict = if status == "completed" {
        parse_review_verdict(output)
    } else {
        ReviewVerdict {
            needs_review: true,
            reason: format!("review run {status}"),
        }
    };
    let (review_status, review_verdict) = if verdict.needs_review {
        ("needs_review", "needs_review")
    } else {
        ("reviewed", "pass")
    };

    let task_id = {
        let mut runs = control.external_agent_runs.write().await;
        if let Some(orig) = runs.iter_mut().find(|r| r.id == original_id) {
            orig.review_status = Some(review_status.to_string());
            orig.review_verdict = Some(review_verdict.to_string());
            orig.task_id.clone()
        } else {
            None
        }
    };

    // If the reviewer flagged issues, mark the linked task for human review.
    // Deterministic and non-destructive: outcome only, status untouched.
    if verdict.needs_review
        && let Some(tid) = task_id
    {
        let mut tasks = control.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == tid) {
            t.outcome = Some(TaskOutcome::NeedsReview);
        }
    }
}

/// Flag a completed run for manual (human) review without spawning a reviewer.
pub async fn mark_pending_review(control: &ControlState, run_id: &str) {
    let mut runs = control.external_agent_runs.write().await;
    if let Some(r) = runs.iter_mut().find(|r| r.id == run_id)
        && r.review_status.is_none()
    {
        r.review_status = Some("pending_review".to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Entity, Task, TaskStatus};

    #[test]
    fn parses_policy() {
        assert_eq!(ReviewPolicy::parse(Some("AUTO")), ReviewPolicy::Auto);
        assert_eq!(ReviewPolicy::parse(Some(" manual ")), ReviewPolicy::Manual);
        assert_eq!(ReviewPolicy::parse(Some("none")), ReviewPolicy::None);
        assert_eq!(ReviewPolicy::parse(None), ReviewPolicy::None);
        assert_eq!(ReviewPolicy::parse(Some("garbage")), ReviewPolicy::None);
    }

    #[test]
    fn review_prompt_includes_task_and_output_and_verdict_instruction() {
        let p = review_prompt("build X", "I built X");
        assert!(p.contains("build X"));
        assert!(p.contains("I built X"));
        assert!(p.contains("VERDICT: PASS"));
        assert!(p.contains("VERDICT: NEEDS_REVIEW"));
    }

    #[test]
    fn verdict_pass_marker() {
        let v = parse_review_verdict("Looks good.\nVERDICT: PASS");
        assert!(!v.needs_review);
    }

    #[test]
    fn verdict_needs_review_marker_with_reason() {
        let v = parse_review_verdict("VERDICT: NEEDS_REVIEW missing error handling");
        assert!(v.needs_review);
        assert!(v.reason.contains("missing error handling"));
    }

    #[test]
    fn verdict_defaults_to_needs_review_without_clear_pass() {
        let v = parse_review_verdict("Some commentary but no verdict line.");
        assert!(v.needs_review);
    }

    fn run(id: &str) -> ExternalAgentRun {
        ExternalAgentRun {
            id: id.to_string(),
            workspace_id: "default".to_string(),
            entity_id: "e1".to_string(),
            provider: "claude".to_string(),
            prompt: "build X".to_string(),
            cwd: "/tmp".to_string(),
            status: "completed".to_string(),
            task_id: Some("task-1".to_string()),
            output: "I built X".to_string(),
            error: None,
            started_at: "2026-07-04T10:00:00+00:00".to_string(),
            completed_at: Some("2026-07-04T10:05:00+00:00".to_string()),
            metadata: serde_json::Value::Null,
            review_policy: Some("auto".to_string()),
            review_status: None,
            review_verdict: None,
            review_run_id: None,
            reviewed_run_id: None,
        }
    }

    #[tokio::test]
    async fn start_review_creates_reviewer_and_links_original() {
        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        control.external_agent_runs.write().await.push(run("run-1"));

        let reviewer = start_review(&control, "run-1", "claude").await.unwrap();
        assert_eq!(reviewer.reviewed_run_id.as_deref(), Some("run-1"));
        assert!(reviewer.prompt.contains("I built X"));

        let runs = control.external_agent_runs.read().await;
        let orig = runs.iter().find(|r| r.id == "run-1").unwrap();
        assert_eq!(orig.review_run_id.as_deref(), Some(reviewer.id.as_str()));
        assert_eq!(orig.review_status.as_deref(), Some("review_running"));

        // Idempotent: a second start returns None.
        drop(runs);
        assert!(start_review(&control, "run-1", "claude").await.is_none());
    }

    #[tokio::test]
    async fn finish_review_flags_task_and_captures_output_when_needs_review() {
        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        control.entities.write().await.push(Entity {
            id: "e1".to_string(),
            name: "E1".to_string(),
            ..Entity::default()
        });
        control.external_agent_runs.write().await.push(run("run-1"));
        control.tasks.write().await.push(Task {
            id: "task-1".to_string(),
            title: "build X".to_string(),
            status: TaskStatus::Completed,
            ..Task::default()
        });

        let reviewer = start_review(&control, "run-1", "claude").await.unwrap();
        finish_review(
            &control,
            &reviewer.id,
            "VERDICT: NEEDS_REVIEW the output is incomplete",
            "completed",
        )
        .await;

        let runs = control.external_agent_runs.read().await;
        let orig = runs.iter().find(|r| r.id == "run-1").unwrap();
        assert_eq!(orig.review_status.as_deref(), Some("needs_review"));
        assert_eq!(orig.review_verdict.as_deref(), Some("needs_review"));

        let task = control.tasks.read().await;
        let t = task.iter().find(|t| t.id == "task-1").unwrap();
        assert_eq!(t.outcome, Some(TaskOutcome::NeedsReview));

        // Reviewer output was captured.
        let caps = control.raw_captures.read().await;
        assert!(caps.iter().any(|c| c.source == "agent_review"));
    }

    #[tokio::test]
    async fn finish_review_marks_reviewed_on_pass() {
        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        control.external_agent_runs.write().await.push(run("run-1"));

        let reviewer = start_review(&control, "run-1", "claude").await.unwrap();
        finish_review(
            &control,
            &reviewer.id,
            "All correct.\nVERDICT: PASS",
            "completed",
        )
        .await;

        let runs = control.external_agent_runs.read().await;
        let orig = runs.iter().find(|r| r.id == "run-1").unwrap();
        assert_eq!(orig.review_status.as_deref(), Some("reviewed"));
        assert_eq!(orig.review_verdict.as_deref(), Some("pass"));
    }
}
