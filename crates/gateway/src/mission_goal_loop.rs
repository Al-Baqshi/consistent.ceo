//! Autonomous mission goal loop (MVP).
//!
//! Each orchestrator tick, every active mission is evaluated and moved one step
//! along `plan → execute → verify → review → continue / ask / complete`. This
//! module is the explicit decision layer: it maps a mission's Definition-of-Done
//! state (`mission_dod`) to a single conservative decision, performs the safe
//! transitions itself (complete / ask-user), and records every decision in the
//! operator feed so the whole loop is visible in the Mission Control Timeline.
//!
//! It does NOT duplicate work already owned elsewhere: task *execution* is done
//! by the orchestrator's task pickup, and next-task *proposal* is done by
//! `report_mission_gaps` (which respects automation modes and cooldowns). The
//! goal loop only decides, records, and takes the two safe terminal/blocking
//! transitions. It performs no destructive actions.

use std::collections::HashMap;

use chrono::Utc;

use crate::mission_dod::{DodStatus, assess_dod};
use crate::state::{ControlState, Mission, MissionStatus, OperatorFeedKind, Task, TaskStatus};

/// The single next move for a mission this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalDecision {
    /// DoD met — mark the mission complete.
    Complete,
    /// Blocked (failed task / awaiting approval / user input) — ask the user.
    AskUser,
    /// All tasks done but some outputs flagged — needs verify/review.
    Verify,
    /// A ready task exists — let the orchestrator run it.
    Execute,
    /// No tasks yet / DoD not satisfied and nothing running — propose next task.
    Continue,
    /// Tasks are in flight; nothing to do this tick.
    Waiting,
}

impl GoalDecision {
    /// Wire form of the decision, used in feed entries and `last_decision` dedup.
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalDecision::Complete => "complete",
            GoalDecision::AskUser => "ask_user",
            GoalDecision::Verify => "verify",
            GoalDecision::Execute => "execute",
            GoalDecision::Continue => "continue",
            GoalDecision::Waiting => "waiting",
        }
    }
}

/// One mission's outcome from a single `decide` call.
#[derive(Debug, Clone)]
pub struct GoalLoopDecision {
    pub mission_id: String,
    pub decision: GoalDecision,
    pub reason: String,
}

// A task the orchestrator can pick up right now: not yet started, and not
// waiting on a human to approve it first.
fn has_ready_task(tasks: &[&Task]) -> bool {
    tasks.iter().any(|t| {
        matches!(t.status, TaskStatus::Backlog | TaskStatus::Queued) && !t.requires_human_approval
    })
}

/// Decide the mission's next loop move from its status + tasks + DoD. Pure.
pub fn decide(mission: &Mission, tasks: &[&Task]) -> GoalLoopDecision {
    let dod = assess_dod(mission, tasks);
    let decision = match dod.status {
        DodStatus::Met => GoalDecision::Complete,
        DodStatus::Blocked => GoalDecision::AskUser,
        DodStatus::NeedsReview => GoalDecision::Verify,
        DodStatus::NotStarted => GoalDecision::Continue,
        DodStatus::InProgress => {
            if has_ready_task(tasks) {
                GoalDecision::Execute
            } else {
                GoalDecision::Waiting
            }
        }
    };
    GoalLoopDecision {
        mission_id: mission.id.clone(),
        decision,
        reason: dod.reason,
    }
}

/// Run one goal-loop tick over all active missions. Performs the safe transitions
/// (complete / ask-user) and records every *changed* decision to the operator
/// feed. `last_decision` dedups so an unchanged decision is not re-logged every
/// tick. Returns the decisions made (for tests / callers).
pub async fn tick(
    control: &ControlState,
    last_decision: &mut HashMap<String, String>,
) -> Vec<GoalLoopDecision> {
    let missions: Vec<Mission> = control
        .missions
        .read()
        .await
        .iter()
        .filter(|m| {
            matches!(
                m.status,
                MissionStatus::Active | MissionStatus::UserInputNeeded
            )
        })
        .cloned()
        .collect();
    if missions.is_empty() {
        last_decision.clear();
        return Vec::new();
    }
    let tasks = control.tasks.read().await.clone();

    let mut out = Vec::with_capacity(missions.len());
    let mut active_ids = Vec::with_capacity(missions.len());
    for mission in &missions {
        active_ids.push(mission.id.clone());
        let mission_tasks: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.mission_id.as_deref() == Some(mission.id.as_str()))
            .collect();
        let d = decide(mission, &mission_tasks);
        // A loop-enabled mission's terminal transitions belong to its Drive
        // beat, which completes only after the critic has passed the work.
        // Letting this loop auto-complete on task counts alone would retire a
        // mission while its reviewer is still running.
        if mission.loop_config.is_some() {
            last_decision.insert(mission.id.clone(), d.decision.as_str().to_string());
            out.push(d);
            continue;
        }
        let changed = last_decision
            .get(&mission.id)
            .map(|s| s != d.decision.as_str())
            .unwrap_or(true);
        last_decision.insert(mission.id.clone(), d.decision.as_str().to_string());

        match d.decision {
            // Mission's work is done: complete it and generate the final report.
            GoalDecision::Complete if mission.status == MissionStatus::Active => {
                let completed_at = Utc::now();
                let now = completed_at.to_rfc3339();
                {
                    let mut missions_mut = control.missions.write().await;
                    if let Some(m) = missions_mut.iter_mut().find(|m| m.id == mission.id) {
                        m.status = MissionStatus::Completed;
                        m.updated_at = now.clone();
                        m.last_reviewed_at = Some(now);
                    }
                }
                // Only fires once per workspace, ever: the activation moment.
                if let Some(elapsed) = control
                    .record_time_to_first_completed_mission(mission, completed_at)
                    .await
                {
                    control
                        .push_mission_feed(
                            OperatorFeedKind::MissionFirstCompleted,
                            mission,
                            Some(format!(
                                "time_to_first_completed_mission_seconds={elapsed}"
                            )),
                        )
                        .await;
                }
                control
                    .push_mission_feed(
                        OperatorFeedKind::MissionCompleted,
                        mission,
                        Some(format!("reason=definition_of_done_met {}", d.reason)),
                    )
                    .await;
                crate::control_api::generate_mission_report(control, &mission.id).await;
            }
            // Mission is blocked: flag it and generate a report so the operator has context.
            GoalDecision::AskUser if mission.status == MissionStatus::Active => {
                {
                    let mut missions_mut = control.missions.write().await;
                    if let Some(m) = missions_mut.iter_mut().find(|m| m.id == mission.id) {
                        m.status = MissionStatus::UserInputNeeded;
                        m.updated_at = Utc::now().to_rfc3339();
                    }
                }
                control
                    .push_mission_feed(
                        OperatorFeedKind::MissionUserInputNeeded,
                        mission,
                        Some(format!("reason=goal_loop_blocked {}", d.reason)),
                    )
                    .await;
                crate::control_api::generate_mission_report(control, &mission.id).await;
            }
            _ => {
                // Execute / Verify / Continue / Waiting, or a repeat AskUser on an
                // already-UserInputNeeded mission: record the decision when it
                // changed so the loop stays visible without spamming the feed.
                if changed {
                    control
                        .push_mission_feed(
                            OperatorFeedKind::GoalLoopDecision,
                            mission,
                            Some(format!("decision={} {}", d.decision.as_str(), d.reason)),
                        )
                        .await;
                }
            }
        }
        out.push(d);
    }
    last_decision.retain(|mission_id, _| active_ids.iter().any(|id| id == mission_id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TaskOutcome;

    fn mission(status: MissionStatus) -> Mission {
        Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Ship it".to_string(),
            summary: String::new(),
            status,
            priority: "medium".to_string(),
            goal: "ship the feature".to_string(),
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

    fn decide_v(m: &Mission, tasks: &[Task]) -> GoalDecision {
        let refs: Vec<&Task> = tasks.iter().collect();
        decide(m, &refs).decision
    }

    #[test]
    fn no_tasks_is_continue() {
        assert_eq!(
            decide_v(&mission(MissionStatus::Active), &[]),
            GoalDecision::Continue
        );
    }

    #[test]
    fn ready_task_is_execute() {
        let tasks = [task(TaskStatus::Queued, None)];
        assert_eq!(
            decide_v(&mission(MissionStatus::Active), &tasks),
            GoalDecision::Execute
        );
    }

    #[test]
    fn only_in_progress_is_waiting() {
        let tasks = [task(TaskStatus::InProgress, None)];
        assert_eq!(
            decide_v(&mission(MissionStatus::Active), &tasks),
            GoalDecision::Waiting
        );
    }

    #[test]
    fn failed_task_asks_user() {
        let tasks = [task(TaskStatus::Failed, None)];
        assert_eq!(
            decide_v(&mission(MissionStatus::Active), &tasks),
            GoalDecision::AskUser
        );
    }

    #[test]
    fn awaiting_approval_asks_user() {
        let mut t = task(TaskStatus::Queued, None);
        t.requires_human_approval = true;
        assert_eq!(
            decide_v(&mission(MissionStatus::Active), &[t]),
            GoalDecision::AskUser
        );
    }

    #[test]
    fn all_complete_clean_is_complete() {
        let tasks = [
            task(TaskStatus::Completed, Some(TaskOutcome::Completed)),
            task(TaskStatus::Completed, Some(TaskOutcome::Completed)),
        ];
        assert_eq!(
            decide_v(&mission(MissionStatus::Active), &tasks),
            GoalDecision::Complete
        );
    }

    #[test]
    fn all_complete_but_flagged_is_verify() {
        let tasks = [
            task(TaskStatus::Completed, Some(TaskOutcome::Completed)),
            task(TaskStatus::Completed, Some(TaskOutcome::NeedsReview)),
        ];
        assert_eq!(
            decide_v(&mission(MissionStatus::Active), &tasks),
            GoalDecision::Verify
        );
    }

    #[tokio::test]
    async fn tick_progresses_mission_from_execute_to_complete() {
        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        control
            .missions
            .write()
            .await
            .push(mission(MissionStatus::Active));
        control.tasks.write().await.push(Task {
            id: "t1".to_string(),
            status: TaskStatus::Queued,
            mission_id: Some("m1".to_string()),
            ..Task::default()
        });
        let mut last = HashMap::new();

        // A ready task → execute (mission stays Active).
        let d1 = tick(&control, &mut last).await;
        assert_eq!(d1[0].decision, GoalDecision::Execute);
        assert_eq!(
            control.missions.read().await[0].status,
            MissionStatus::Active
        );

        // The task completes cleanly → next tick completes the mission.
        {
            let mut tasks = control.tasks.write().await;
            let t = tasks.iter_mut().find(|t| t.id == "t1").unwrap();
            t.status = TaskStatus::Completed;
            t.outcome = Some(TaskOutcome::Completed);
        }
        let d2 = tick(&control, &mut last).await;
        assert_eq!(d2[0].decision, GoalDecision::Complete);
        assert_eq!(
            control.missions.read().await[0].status,
            MissionStatus::Completed
        );

        // Time-to-first-completed-mission was recorded, measured from the
        // mission's created_at (2026-07-04T00:00:00Z) to completion.
        let metrics = control.mission_metrics.read().await;
        assert!(metrics.contains_key(crate::state::TIME_TO_FIRST_COMPLETED_MISSION_METRIC));
        drop(metrics);

        // A completed mission is no longer processed.
        let d3 = tick(&control, &mut last).await;
        assert!(d3.is_empty());
    }

    #[tokio::test]
    async fn time_to_first_completed_mission_is_recorded_once() {
        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        let m1 = mission(MissionStatus::Completed);
        let completed_at = "2026-07-04T00:05:00Z".parse().unwrap();
        assert_eq!(
            control
                .record_time_to_first_completed_mission(&m1, completed_at)
                .await,
            Some(300),
            "the first record reports the elapsed time, so the caller can emit \
             the activation event"
        );
        assert_eq!(
            control
                .mission_metrics
                .read()
                .await
                .get(crate::state::TIME_TO_FIRST_COMPLETED_MISSION_METRIC)
                .copied(),
            Some(300)
        );

        // A later mission completing does not overwrite the first measurement.
        let mut m2 = mission(MissionStatus::Completed);
        m2.id = "m2".to_string();
        m2.created_at = "2026-07-04T00:00:00Z".to_string();
        let later_completed_at = "2026-07-04T01:00:00Z".parse().unwrap();
        assert_eq!(
            control
                .record_time_to_first_completed_mission(&m2, later_completed_at)
                .await,
            None,
            "a later completion must not re-fire the activation event"
        );
        assert_eq!(
            control
                .mission_metrics
                .read()
                .await
                .get(crate::state::TIME_TO_FIRST_COMPLETED_MISSION_METRIC)
                .copied(),
            Some(300)
        );
    }

    #[tokio::test]
    async fn tick_blocks_and_records_decisions() {
        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        control
            .missions
            .write()
            .await
            .push(mission(MissionStatus::Active));
        control.tasks.write().await.push(Task {
            id: "t1".to_string(),
            status: TaskStatus::Failed,
            mission_id: Some("m1".to_string()),
            ..Task::default()
        });
        let mut last = HashMap::new();

        let d = tick(&control, &mut last).await;
        assert_eq!(d[0].decision, GoalDecision::AskUser);
        assert_eq!(
            control.missions.read().await[0].status,
            MissionStatus::UserInputNeeded
        );
        // The decision was recorded in the operator feed.
        let feed = control.operator_feed.read().await;
        assert!(
            feed.iter()
                .any(|e| e.kind == OperatorFeedKind::MissionUserInputNeeded)
        );
    }
}
