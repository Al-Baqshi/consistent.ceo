//! Final mission report generation.
//!
//! When a mission completes (or stops needing user input), the durable loop
//! generates a concise Markdown report: goal, completed / failed / cancelled
//! tasks, flagged outputs, verification results, DoD status, open questions, and
//! the next recommended action. Deterministic and side-effect free.

use crate::mission_dod::{DodStatus, assess_dod};
use crate::state::{Mission, Task, TaskOutcome, TaskStatus, VerificationRun};

// Render task titles as a Markdown bullet list, or a placeholder when empty.
fn bullet_titles(tasks: &[&Task]) -> String {
    if tasks.is_empty() {
        "- (none)\n".to_string()
    } else {
        tasks
            .iter()
            .map(|t| format!("- {}\n", t.title.trim()))
            .collect()
    }
}

/// Build the final report for a mission from its tasks and verifications.
pub fn build_mission_report(
    mission: &Mission,
    tasks: &[&Task],
    verifications: &[&VerificationRun],
) -> String {
    let dod = assess_dod(mission, tasks);

    let completed: Vec<&&Task> = tasks
        .iter()
        .filter(|t| {
            t.status == TaskStatus::Completed && t.outcome != Some(TaskOutcome::NeedsReview)
        })
        .collect();
    let needs_review: Vec<&&Task> = tasks
        .iter()
        .filter(|t| {
            t.status == TaskStatus::Completed && t.outcome == Some(TaskOutcome::NeedsReview)
        })
        .collect();
    let failed: Vec<&&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Failed)
        .collect();
    let cancelled: Vec<&&Task> = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Cancelled)
        .collect();

    // A verification is either a command or a URL check; show whichever is set.
    let verifications_block = if verifications.is_empty() {
        "- (none)\n".to_string()
    } else {
        verifications
            .iter()
            .map(|v| {
                let target = v.command.as_deref().or(v.url.as_deref()).unwrap_or("");
                format!("- [{}] {} — `{}`\n", v.status, v.kind, target.trim())
            })
            .collect()
    };

    // Open questions: failed work and anything awaiting human approval.
    let mut questions: Vec<String> = Vec::new();
    if !failed.is_empty() {
        questions.push(format!(
            "{} task(s) failed — decide whether to retry, fix, or drop them.",
            failed.len()
        ));
    }
    if !needs_review.is_empty() {
        questions.push(format!(
            "{} output(s) flagged for review — confirm or send back.",
            needs_review.len()
        ));
    }
    if tasks
        .iter()
        .any(|t| t.requires_human_approval && t.status != TaskStatus::Completed)
    {
        questions.push("Some work is awaiting your approval.".to_string());
    }
    let questions_block = if questions.is_empty() {
        "- (none)\n".to_string()
    } else {
        questions.iter().map(|q| format!("- {q}\n")).collect()
    };

    let next_action = match dod.status {
        DodStatus::Met => "Mission complete — no further action needed.",
        DodStatus::Blocked => {
            "Resolve the blockers above (failed tasks / approvals), then resume the mission."
        }
        DodStatus::NeedsReview => {
            "Review the flagged outputs; the mission can complete once cleared."
        }
        DodStatus::InProgress => "Let the loop keep executing the open tasks.",
        DodStatus::NotStarted => "Break the goal into concrete tasks to get started.",
    };

    let goal = if mission.goal.trim().is_empty() {
        "(no goal recorded)"
    } else {
        mission.goal.trim()
    };

    format!(
        "# Mission Report — {title}\n\n\
**Goal:** {goal}\n\n\
**Definition of Done:** `{dod_status}` — {dod_reason}\n\n\
## Completed tasks ({n_done})\n\n{done_block}\n\
## Needs review ({n_review})\n\n{review_block}\n\
## Failed tasks ({n_failed})\n\n{failed_block}\n\
## Cancelled tasks ({n_cancelled})\n\n{cancelled_block}\n\
## Verifications ({n_verif})\n\n{verif_block}\n\
## Open questions\n\n{questions_block}\n\
## Next recommended action\n\n{next_action}\n",
        title = mission.title.trim(),
        goal = goal,
        dod_status = dod_status_str(dod.status),
        dod_reason = dod.reason,
        n_done = completed.len(),
        done_block = bullet_titles(&completed.iter().map(|t| **t).collect::<Vec<_>>()),
        n_review = needs_review.len(),
        review_block = bullet_titles(&needs_review.iter().map(|t| **t).collect::<Vec<_>>()),
        n_failed = failed.len(),
        failed_block = bullet_titles(&failed.iter().map(|t| **t).collect::<Vec<_>>()),
        n_cancelled = cancelled.len(),
        cancelled_block = bullet_titles(&cancelled.iter().map(|t| **t).collect::<Vec<_>>()),
        n_verif = verifications.len(),
        verif_block = verifications_block,
        questions_block = questions_block,
        next_action = next_action,
    )
}

fn dod_status_str(s: DodStatus) -> &'static str {
    match s {
        DodStatus::NotStarted => "not_started",
        DodStatus::InProgress => "in_progress",
        DodStatus::Blocked => "blocked",
        DodStatus::NeedsReview => "needs_review",
        DodStatus::Met => "met",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::MissionStatus;

    fn mission() -> Mission {
        Mission {
            id: "m1".into(),
            entity_id: "e1".into(),
            title: "Ship the feature".into(),
            summary: String::new(),
            status: MissionStatus::Completed,
            priority: "high".into(),
            goal: "Ship a small docs feature".into(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-07-04T00:00:00Z".into(),
            updated_at: "2026-07-04T00:00:00Z".into(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        }
    }

    fn task(title: &str, status: TaskStatus, outcome: Option<TaskOutcome>) -> Task {
        Task {
            title: title.into(),
            status,
            outcome,
            mission_id: Some("m1".into()),
            ..Task::default()
        }
    }

    #[test]
    fn report_summarizes_every_section() {
        let tasks = [
            task(
                "build backend",
                TaskStatus::Completed,
                Some(TaskOutcome::Completed),
            ),
            task(
                "write frontend",
                TaskStatus::Completed,
                Some(TaskOutcome::NeedsReview),
            ),
            task("infra step", TaskStatus::Failed, None),
            task("dropped step", TaskStatus::Cancelled, None),
        ];
        let refs: Vec<&Task> = tasks.iter().collect();
        let report = build_mission_report(&mission(), &refs, &[]);

        assert!(report.contains("# Mission Report — Ship the feature"));
        assert!(report.contains("Ship a small docs feature"));
        assert!(report.contains("build backend"));
        assert!(report.contains("write frontend")); // needs review
        assert!(report.contains("infra step")); // failed
        assert!(report.contains("dropped step")); // cancelled
        assert!(report.contains("Open questions"));
        assert!(report.contains("Next recommended action"));
        // A failed task means DoD is blocked.
        assert!(report.contains("`blocked`"));
    }

    #[test]
    fn all_done_reports_met_and_no_questions() {
        let tasks = [task(
            "only task",
            TaskStatus::Completed,
            Some(TaskOutcome::Completed),
        )];
        let refs: Vec<&Task> = tasks.iter().collect();
        let report = build_mission_report(&mission(), &refs, &[]);
        assert!(report.contains("`met`"));
        assert!(report.contains("Mission complete"));
        assert!(report.contains("- (none)")); // empty questions
    }
}
