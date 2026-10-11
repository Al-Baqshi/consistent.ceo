//! Deterministic task-output verification — the adversarial gate.
//!
//! After a plain agent task produces output, Consistent grades it *before*
//! accepting a `Completed` outcome. This is the "adversarial validator, not
//! cheerleader" spine: the loop should not mark work done that is empty, a
//! refusal, or full of unfilled placeholders.
//!
//! v1 is rule-based (no LLM), mirroring `capture_routing`: deterministic first,
//! semantic/LLM verification is a later slice. It is intentionally
//! **conservative and non-destructive** — the worst it can do is downgrade a
//! `Completed` result to `NeedsReview`, which flags the task for a human and
//! emits a `VerificationNeedsReview` feed event. It never fails a task, never
//! blocks the continuation loop, and never deletes work.

use std::env;

use serde::Serialize;

/// Minimum trimmed length below which output is treated as effectively empty.
const MIN_MEANINGFUL_LEN: usize = 2;

/// Phrases that signal the agent could not actually do the work. Matched
/// case-insensitively as substrings. Kept high-signal to limit false positives.
const REFUSAL_MARKERS: &[&str] = &[
    "i cannot ",
    "i can't ",
    "i am unable",
    "i'm unable",
    "i do not have access",
    "i don't have access",
    "i do not have the ability",
    "i don't have the ability",
    "i am not able to",
    "i'm not able to",
    "unable to complete",
    "cannot complete",
    "as an ai language model",
];

/// Unfilled-placeholder markers that indicate a stubbed, not-real result.
const PLACEHOLDER_MARKERS: &[&str] = &["lorem ipsum", "<insert", "[insert", "insert your "];

/// Explicit markers an agent can emit to route its own output to review.
const REVIEW_MARKERS: &[&str] = &["needs_human_approval", "needs_review"];

/// Outcome of the deterministic gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    /// Output passed the deterministic checks.
    Pass,
    /// Output looks incomplete/refused/stubbed; a human should review it.
    NeedsReview,
}

/// Full verdict from grading a task's output: pass/review plus why.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationVerdict {
    pub status: VerificationStatus,
    /// Human-readable explanation; empty when `Pass`.
    pub reason: String,
    /// Machine tags for the checks that fired (e.g. `capability_refusal`).
    pub flags: Vec<String>,
}

impl VerificationVerdict {
    /// Construct a passing verdict with no flags or reason.
    pub fn pass() -> Self {
        VerificationVerdict {
            status: VerificationStatus::Pass,
            reason: String::new(),
            flags: Vec::new(),
        }
    }

    /// True when this verdict downgrades the task to `NeedsReview`.
    pub fn needs_review(&self) -> bool {
        self.status == VerificationStatus::NeedsReview
    }

    /// Structured value stored under `task.metadata.verification` for audit.
    pub fn to_metadata_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }

    /// Append a visible reviewer note to the stored result when review is needed.
    pub fn annotate_result(&self, result: String) -> String {
        if self.needs_review() && !self.reason.is_empty() {
            format!(
                "{result}\n\n---\n⚠️ Verification flagged this output for review: {}",
                self.reason
            )
        } else {
            result
        }
    }

    /// Detail string for the operator-feed event (`None` on pass).
    pub fn feed_detail(&self) -> Option<String> {
        if self.needs_review() {
            Some(self.reason.clone())
        } else {
            None
        }
    }
}

/// Whether the task-output verification gate is active. Enabled by default;
/// set `CONSISTENT_WORLD_TASK_VERIFICATION` to `0`/`false`/`off` to disable.
pub fn enabled() -> bool {
    match env::var("CONSISTENT_WORLD_TASK_VERIFICATION") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        ),
        Err(_) => true,
    }
}

/// Grade a completed task's output. Deterministic and side-effect free.
pub fn verify_task_output(result: &str) -> VerificationVerdict {
    let trimmed = result.trim();
    let lower = trimmed.to_ascii_lowercase();
    let mut flags: Vec<String> = Vec::new();
    let mut reasons: Vec<String> = Vec::new();

    if trimmed.chars().count() < MIN_MEANINGFUL_LEN {
        flags.push("empty_output".to_string());
        reasons.push("the output is empty or too short to be a real result".to_string());
    }

    if REFUSAL_MARKERS.iter().any(|m| lower.contains(m)) {
        flags.push("capability_refusal".to_string());
        reasons.push("the agent reported it could not complete the work".to_string());
    }

    if PLACEHOLDER_MARKERS.iter().any(|m| lower.contains(m)) {
        flags.push("unfilled_placeholder".to_string());
        reasons.push("the output contains unfilled placeholders".to_string());
    }

    if REVIEW_MARKERS.iter().any(|m| lower.contains(m)) {
        flags.push("explicit_review_marker".to_string());
        reasons.push("the agent explicitly requested human review".to_string());
    }

    if flags.is_empty() {
        VerificationVerdict::pass()
    } else {
        VerificationVerdict {
            status: VerificationStatus::NeedsReview,
            reason: reasons.join("; "),
            flags,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substantive_output_passes() {
        let v = verify_task_output(
            "Refactored the auth middleware into a shared guard and added 3 tests.",
        );
        assert_eq!(v.status, VerificationStatus::Pass);
        assert!(v.flags.is_empty());
        assert!(v.reason.is_empty());
    }

    #[test]
    fn empty_output_needs_review() {
        let v = verify_task_output("   \n  ");
        assert!(v.needs_review());
        assert!(v.flags.contains(&"empty_output".to_string()));
    }

    #[test]
    fn capability_refusal_needs_review() {
        let v = verify_task_output("I cannot access that repository, so I did nothing.");
        assert!(v.needs_review());
        assert!(v.flags.contains(&"capability_refusal".to_string()));
    }

    #[test]
    fn unfilled_placeholder_needs_review() {
        let v = verify_task_output("Draft copy: Lorem ipsum dolor sit amet.");
        assert!(v.needs_review());
        assert!(v.flags.contains(&"unfilled_placeholder".to_string()));
    }

    #[test]
    fn explicit_review_marker_needs_review() {
        let v = verify_task_output("Done, but NEEDS_HUMAN_APPROVAL before sending.");
        assert!(v.needs_review());
        assert!(v.flags.contains(&"explicit_review_marker".to_string()));
    }

    #[test]
    fn multiple_flags_are_all_recorded() {
        let v = verify_task_output("I am unable to finish. Lorem ipsum placeholder.");
        assert!(v.needs_review());
        assert!(v.flags.contains(&"capability_refusal".to_string()));
        assert!(v.flags.contains(&"unfilled_placeholder".to_string()));
        assert!(v.reason.contains(';'));
    }

    #[test]
    fn annotate_result_appends_only_on_review() {
        let pass = VerificationVerdict::pass();
        assert_eq!(pass.annotate_result("body".to_string()), "body");
        let review = verify_task_output("");
        let annotated = review.annotate_result("body".to_string());
        assert!(annotated.starts_with("body"));
        assert!(annotated.contains("flagged this output for review"));
    }

    #[test]
    fn metadata_value_roundtrips() {
        let v = verify_task_output("I cannot do that.");
        let meta = v.to_metadata_value();
        assert_eq!(meta["status"], "needs_review");
        assert!(
            meta["flags"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f == "capability_refusal")
        );
    }
}
