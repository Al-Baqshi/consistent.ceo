//! Goal-level Definition of Done.
//!
//! Task verification (`verification.rs`) grades a single task's output. This
//! module answers the bigger question the autonomous loop needs: **is the whole
//! mission done?** Today the mission-review loop only ever *proposes the next
//! step* and never concludes — a mission whose work is genuinely finished sits
//! `Active` forever until a 48h inactivity stall. DoD gives the loop a real
//! termination signal.
//!
//! v1 is deterministic (no LLM), mirroring `capture_routing`/`verification`. It
//! does not claim to semantically verify the goal text — it aggregates the
//! *operational* state of the mission's tasks into an honest done/blocked/
//! in-progress signal. Semantic goal-satisfaction (LLM over `mission.goal` +
//! `success_criteria`) is a later slice that composes over this.

use serde::Serialize;

use crate::state::{Mission, MissionStatus, Task, TaskOutcome, TaskStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DodStatus {
    /// The mission has no tasks yet.
    NotStarted,
    /// Work is still proceeding (open tasks remain).
    InProgress,
    /// Something needs attention before it can be done (failed / awaiting user).
    Blocked,
    /// All tasks finished, but some outputs were flagged for human review.
    NeedsReview,
    /// All tasks finished cleanly — the mission's work is done.
    Met,
}

/// Snapshot of a mission's task counts, plus the status they add up to.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DodAssessment {
    pub status: DodStatus,
    /// Human-readable explanation of the status.
    pub reason: String,
    pub total_tasks: usize,
    pub completed: usize,
    /// Backlog / queued / in-progress tasks.
    pub open: usize,
    pub failed: usize,
    /// Completed tasks whose output the verifier flagged (`NeedsReview`).
    pub needs_review: usize,
    /// Non-terminal tasks awaiting human approval.
    pub pending_approval: usize,
}

impl DodAssessment {
    /// Shorthand for `status == DodStatus::Met`.
    pub fn is_met(&self) -> bool {
        self.status == DodStatus::Met
    }
}

/// Assess whether a mission's work is done, from the operational state of its
/// tasks. Deterministic and side-effect free.
pub fn assess_dod(mission: &Mission, tasks: &[&Task]) -> DodAssessment {
    let total_tasks = tasks.len();
    let mut completed = 0usize;
    let mut open = 0usize;
    let mut failed = 0usize;
    let mut needs_review = 0usize;
    let mut pending_approval = 0usize;

    for t in tasks {
        match t.status {
            TaskStatus::Completed => {
                completed += 1;
                if t.outcome == Some(TaskOutcome::NeedsReview) {
                    needs_review += 1;
                }
            }
            TaskStatus::Failed => failed += 1,
            TaskStatus::Backlog | TaskStatus::Queued | TaskStatus::InProgress => {
                open += 1;
                if t.requires_human_approval {
                    pending_approval += 1;
                }
            }
            // Cancelled tasks are intentionally stopped — they neither block DoD
            // nor count as done.
            TaskStatus::Cancelled => {}
        }
    }

    let mission_flagged = matches!(
        mission.status,
        MissionStatus::Blocked | MissionStatus::UserInputNeeded
    );

    let (status, reason) = if total_tasks == 0 {
        (
            DodStatus::NotStarted,
            "No tasks yet — the mission has not been broken down into work.".to_string(),
        )
    } else if failed > 0 || pending_approval > 0 || mission_flagged {
        let mut blockers = Vec::new();
        if failed > 0 {
            blockers.push(format!("{failed} failed task(s)"));
        }
        if pending_approval > 0 {
            blockers.push(format!("{pending_approval} task(s) awaiting approval"));
        }
        if mission_flagged {
            blockers.push("the mission is flagged for user input".to_string());
        }
        (
            DodStatus::Blocked,
            format!("Needs attention: {}.", blockers.join(", ")),
        )
    } else if open > 0 {
        (
            DodStatus::InProgress,
            format!("{completed} of {total_tasks} task(s) complete, {open} still open."),
        )
    } else if needs_review > 0 {
        (
            DodStatus::NeedsReview,
            format!(
                "All {total_tasks} task(s) complete, but {needs_review} output(s) were flagged for review."
            ),
        )
    } else {
        (
            DodStatus::Met,
            format!(
                "All {total_tasks} task(s) complete and verified — the mission's work is done."
            ),
        )
    };

    DodAssessment {
        status,
        reason,
        total_tasks,
        completed,
        open,
        failed,
        needs_review,
        pending_approval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mission() -> Mission {
        Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Ship it".to_string(),
            summary: String::new(),
            status: MissionStatus::Active,
            priority: "medium".to_string(),
            goal: "Ship the feature".to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-07-04T00:00:00Z".to_string(),
            updated_at: "2026-07-04T00:00:00Z".to_string(),
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

    fn task(status: TaskStatus, outcome: Option<TaskOutcome>) -> Task {
        Task {
            status,
            outcome,
            mission_id: Some("m1".to_string()),
            ..Task::default()
        }
    }

    fn assess(m: &Mission, tasks: &[Task]) -> DodAssessment {
        let refs: Vec<&Task> = tasks.iter().collect();
        assess_dod(m, &refs)
    }

    #[test]
    fn no_tasks_is_not_started() {
        let a = assess(&mission(), &[]);
        assert_eq!(a.status, DodStatus::NotStarted);
    }

    #[test]
    fn open_tasks_are_in_progress() {
        let tasks = [
            task(TaskStatus::Completed, Some(TaskOutcome::Completed)),
            task(TaskStatus::InProgress, None),
        ];
        let a = assess(&mission(), &tasks);
        assert_eq!(a.status, DodStatus::InProgress);
        assert_eq!(a.completed, 1);
        assert_eq!(a.open, 1);
    }

    #[test]
    fn failed_task_blocks() {
        let tasks = [
            task(TaskStatus::Completed, Some(TaskOutcome::Completed)),
            task(TaskStatus::Failed, None),
        ];
        let a = assess(&mission(), &tasks);
        assert_eq!(a.status, DodStatus::Blocked);
        assert_eq!(a.failed, 1);
    }

    #[test]
    fn pending_approval_blocks() {
        let mut t = task(TaskStatus::Queued, None);
        t.requires_human_approval = true;
        let a = assess(&mission(), &[t]);
        assert_eq!(a.status, DodStatus::Blocked);
        assert_eq!(a.pending_approval, 1);
    }

    #[test]
    fn all_complete_but_flagged_needs_review() {
        let tasks = [
            task(TaskStatus::Completed, Some(TaskOutcome::Completed)),
            task(TaskStatus::Completed, Some(TaskOutcome::NeedsReview)),
        ];
        let a = assess(&mission(), &tasks);
        assert_eq!(a.status, DodStatus::NeedsReview);
        assert_eq!(a.needs_review, 1);
    }

    #[test]
    fn all_complete_and_clean_is_met() {
        let tasks = [
            task(TaskStatus::Completed, Some(TaskOutcome::Completed)),
            task(TaskStatus::Completed, Some(TaskOutcome::Completed)),
        ];
        let a = assess(&mission(), &tasks);
        assert_eq!(a.status, DodStatus::Met);
        assert!(a.is_met());
    }

    #[test]
    fn mission_flagged_for_user_input_is_blocked_even_if_tasks_done() {
        let mut m = mission();
        m.status = MissionStatus::UserInputNeeded;
        let tasks = [task(TaskStatus::Completed, Some(TaskOutcome::Completed))];
        let a = assess(&m, &tasks);
        assert_eq!(a.status, DodStatus::Blocked);
    }
}
