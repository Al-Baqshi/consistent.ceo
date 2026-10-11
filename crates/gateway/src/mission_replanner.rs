//! Mission replanner: the feedback edge of the iterate-until-done loop.
//!
//! The planner turns a goal into a first plan. This module turns *results*
//! into the next plan: it reads what the last iteration's tasks actually did
//! (their verdicts, their evidence packs, the critic's reasons), compares that
//! to the mission's definition of done, and emits the next 1-3 executable
//! tasks — or says the goal is met.
//!
//! Two rules make it safe to run unattended:
//!
//! 1. **No fallback.** Unlike [`crate::mission_planner`], a degraded LLM
//!    returns `None` and the loop simply waits for the next beat. Canned tasks
//!    would be actioned by a coding agent on the operator's machine; a paused
//!    loop is recoverable, invented work is not.
//! 2. **Caps are checked before every replan**, never inside the model's
//!    reasoning. Iterations and spend are counted in Rust
//!    ([`LoopGate`]), and exhausting either hands the mission back to the
//!    operator instead of iterating forever.

use std::sync::Arc;

use consistent_world_llm::client::{ChatMessage, CompletionRequest, Role};
use serde_json::json;
use uuid::Uuid;

use crate::capture::{build_llm_client, resolve_operations_llm};
use crate::credentials_resolver::resolve_llm_api_key;
use crate::mission_planner::{PlannedTask, parse_planned_tasks};
use crate::state::{
    ControlState, Mission, MissionLoopState, MissionStatus, OperatorFeedEvent, OperatorFeedKind,
    ServerState, Task, TaskOutcome, TaskStatus,
};

/// Most tasks one replan round may queue. Small on purpose: an iteration is a
/// step, not a rewrite of the plan.
const MAX_REPLAN_TASKS: usize = 3;
/// Same ceiling shape as the planner: a stuck local model must not wedge a beat.
const REPLANNER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Why a loop may or may not advance right now. Pure so every cap is testable
/// without a clock, a model, or a database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopGate {
    /// Queue the next iteration.
    Advance,
    /// Work from the current iteration is still running or awaiting approval.
    Busy,
    /// Every task is done and the critic is satisfied — the goal is met.
    GoalMet,
    /// A cap tripped or the work is blocked; hand back to the operator.
    Stop(String),
}

/// Decide whether a loop-enabled mission may start another iteration.
///
/// `tasks` are this mission's tasks. The order of checks matters: caps are
/// evaluated before "goal met" so a loop that ran out of budget mid-iteration
/// never reports success it did not verify.
pub fn loop_gate(mission: &Mission, tasks: &[&Task]) -> LoopGate {
    let Some(config) = mission.loop_config.as_ref() else {
        return LoopGate::Stop("mission has no loop config".to_string());
    };
    let state = mission.loop_state.clone().unwrap_or_default();

    if let Some(budget) = config.budget_usd
        && state.cost_usd >= budget
    {
        return LoopGate::Stop(format!(
            "budget spent: ${:.2} of ${:.2}",
            state.cost_usd, budget
        ));
    }

    // Anything still moving means this iteration has not finished.
    let unfinished = tasks.iter().any(|t| {
        matches!(
            t.status,
            TaskStatus::Backlog | TaskStatus::Queued | TaskStatus::InProgress
        )
    });
    if unfinished {
        return LoopGate::Busy;
    }

    // A task parked at the approval gate is waiting on a human, not on us.
    if tasks
        .iter()
        .any(|t| t.requires_human_approval && t.status != TaskStatus::Cancelled)
    {
        return LoopGate::Busy;
    }

    let needs_more_work = tasks.iter().any(|t| {
        matches!(t.status, TaskStatus::Failed)
            || matches!(
                t.outcome,
                Some(TaskOutcome::NeedsReview)
                    | Some(TaskOutcome::RetryableFailure)
                    | Some(TaskOutcome::Failed)
                    | Some(TaskOutcome::NeedsStrongerModel)
            )
    });

    // A cancelled task is the operator saying no. Treating it as success would
    // let declining one approval declare the whole mission "definition of done
    // met" — the loop congratulating itself for work the human refused.
    if tasks.iter().any(|t| t.status == TaskStatus::Cancelled) {
        return LoopGate::Stop(
            "some work was cancelled — the goal cannot be called met without it".to_string(),
        );
    }

    if !needs_more_work && !tasks.is_empty() {
        return LoopGate::GoalMet;
    }

    // Only now does the iteration cap matter: there is more work to do, and
    // the question is whether the loop is still allowed to do it.
    let max = config.effective_max_iterations();
    if state.iteration >= max {
        return LoopGate::Stop(format!("reached the {max}-iteration limit"));
    }
    LoopGate::Advance
}

/// Add one run's cost to its mission's loop budget.
///
/// Without this the budget cap in [`loop_gate`] could never fire: the field
/// existed, nothing ever wrote it. Both halves of an iteration count — the
/// builder's run and the critic's review — because both are spend the
/// operator's cap is meant to bound.
pub async fn accrue_loop_cost(control: &Arc<ControlState>, mission_id: &str, cost_usd: f64) {
    if cost_usd <= 0.0 {
        return;
    }
    let updated = {
        let mut missions = control.missions.write().await;
        let Some(m) = missions
            .iter_mut()
            .find(|m| m.id == mission_id && m.loop_config.is_some())
        else {
            return;
        };
        let mut state = m.loop_state.clone().unwrap_or_default();
        state.cost_usd += cost_usd;
        m.loop_state = Some(state);
        m.clone()
    };
    control.persist_mission(&updated).await;
}

/// The mission a task belongs to, if that mission runs the loop.
pub async fn loop_mission_id_for_task(
    control: &Arc<ControlState>,
    task_id: &str,
) -> Option<String> {
    let mission_id = {
        let tasks = control.tasks.read().await;
        tasks
            .iter()
            .find(|t| t.id == task_id)
            .and_then(|t| t.mission_id.clone())?
    };
    let missions = control.missions.read().await;
    missions
        .iter()
        .any(|m| m.id == mission_id && m.loop_config.is_some())
        .then_some(mission_id)
}

/// A one-line result summary per finished task, for the replanner prompt.
fn task_result_line(task: &Task) -> String {
    let verdict = match (task.status, &task.outcome) {
        (TaskStatus::Completed, _) => "done",
        (TaskStatus::Cancelled, _) => "cancelled",
        (_, Some(TaskOutcome::NeedsReview)) => "needs review",
        (TaskStatus::Failed, _) => "failed",
        _ => "unfinished",
    };
    let detail = task
        .result
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("no report");
    let detail: String = detail.chars().take(300).collect();
    let evidence = task
        .metadata
        .as_ref()
        .and_then(|m| m.get("evidence"))
        .and_then(|v| serde_json::from_value::<crate::evidence::EvidencePack>(v.clone()).ok())
        .map(|p| {
            if !p.is_git {
                " [no git evidence]".to_string()
            } else {
                format!(
                    " [evidence: {} commits, {} files, {} uncommitted]",
                    p.commits,
                    p.files_changed,
                    p.dirty_delta.len()
                )
            }
        })
        .unwrap_or_default();
    format!("- {} → {verdict}: {detail}{evidence}", task.title.trim())
}

/// The replanner prompt. Deliberately as small as the planner's — heavy
/// prompts come back empty on the local fallback tier.
pub fn build_replanner_prompt(
    mission: &Mission,
    entity_name: &str,
    finished: &[&Task],
    iteration: u32,
    max_iterations: u32,
) -> String {
    let dod = mission
        .loop_config
        .as_ref()
        .map(|c| c.definition_of_done.trim())
        .unwrap_or_default();
    let mut prompt = format!(
        "You are driving one mission for {entity} toward a finish line, one iteration at a time.\n\n\
Mission: {title}\n\
Goal: {goal}\n\
Definition of done: {dod}\n\
This is iteration {next} of at most {max}.\n\n\
What the last round of work produced:\n",
        entity = entity_name.trim(),
        title = mission.title.trim(),
        goal = mission.goal.trim(),
        dod = dod,
        next = iteration + 1,
        max = max_iterations,
    );
    if finished.is_empty() {
        prompt.push_str("- (nothing yet — this is the first iteration)\n");
    } else {
        for task in finished.iter().take(12) {
            prompt.push_str(&task_result_line(task));
            prompt.push('\n');
        }
    }
    prompt.push_str(
        "\nDecide what to do next.\n\
\n\
Rules:\n\
- If the definition of done is already satisfied by the results above, reply with exactly: []\n\
- Otherwise emit 1-3 tasks that close the remaining gap. Each is one specific, checkable piece of work naming the real file, artifact, or step.\n\
- Fix what the results say is broken before adding anything new. A task that reported \"needs review\" or produced no evidence of change is unfinished work, not a finished one.\n\
- Never repeat a task that already succeeded.\n\
- executable: true only for code or file work an unattended coding agent can finish alone inside the project directory. Everything else is false.\n\
- requires_approval: true for anything outward-facing — sending, publishing, paying, posting, or contacting a person.\n\
\n\
Reply with ONLY the JSON array. No prose, no markdown fences.\n\
[{\"title\":\"...\",\"description\":\"...\",\"executable\":true,\"requires_approval\":false}]\n",
    );
    prompt
}

/// Ask the model for the next iteration's tasks. `None` means "could not
/// plan" (degraded model, timeout, unparseable) — the caller pauses the loop.
/// `Some(vec![])` means "the model says the goal is met".
async fn replan_with_llm(
    state: &ServerState,
    control: &Arc<ControlState>,
    mission: &Mission,
    entity_name: &str,
    finished: &[&Task],
) -> Option<Vec<PlannedTask>> {
    let config = state.app_config.as_ref()?;
    let resolved = resolve_operations_llm(config, control).await?;
    let api_key = resolve_llm_api_key(&resolved.api_key_env);
    let provider = build_llm_client(config, &resolved.base_url, &api_key, &resolved.api_key_env);

    let loop_config = mission.loop_config.as_ref()?;
    let loop_state = mission.loop_state.clone().unwrap_or_default();
    let prompt = build_replanner_prompt(
        mission,
        entity_name,
        finished,
        loop_state.iteration,
        loop_config.effective_max_iterations(),
    );
    let request = CompletionRequest {
        model: resolved.model,
        messages: vec![ChatMessage {
            role: Role::User,
            content: prompt,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        max_tokens: Some(1000),
        temperature: Some(0.2),
        stream: false,
    };

    let response = match tokio::time::timeout(REPLANNER_TIMEOUT, provider.complete(request)).await {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::warn!(mission_id = %mission.id, error = %e, "mission replanner LLM failed; loop pauses this beat");
            return None;
        }
        Err(_) => {
            tracing::warn!(mission_id = %mission.id, "mission replanner timed out; loop pauses this beat");
            return None;
        }
    };
    let raw = response.content.unwrap_or_default();
    if raw.trim().is_empty() {
        tracing::warn!(mission_id = %mission.id, "mission replanner returned empty; loop pauses this beat");
        return None;
    }
    // An explicit empty array is a real answer ("done"), not a parse failure.
    let cleaned = raw.trim();
    if cleaned == "[]" || cleaned.ends_with("[]") {
        return Some(Vec::new());
    }
    parse_planned_tasks(&raw)
}

/// What one advance attempt did, for the caller's narration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvanceOutcome {
    /// Queued this many tasks for the next iteration.
    Queued(usize),
    /// The loop is not ready to advance (still working, awaiting approval).
    Held,
    /// The definition of done is met; the mission was completed.
    Completed,
    /// A cap tripped or the model could not plan; the reason is operator-facing.
    Stopped(String),
    /// The model was unavailable this beat; try again next beat.
    Paused,
}

/// Advance a loop-enabled mission by at most one iteration.
///
/// This is the whole "heartbeat does the actions" mechanism: the beat calls
/// here, and the tasks queued land in the executor's scan on its next tick.
pub async fn advance_mission_loop(
    state: &ServerState,
    control: &Arc<ControlState>,
    mission: &Mission,
) -> AdvanceOutcome {
    if mission.loop_config.is_none() {
        return AdvanceOutcome::Held;
    }
    let all_tasks = control.tasks.read().await.clone();
    let mission_tasks: Vec<&Task> = all_tasks
        .iter()
        .filter(|t| t.mission_id.as_deref() == Some(mission.id.as_str()))
        .collect();

    match loop_gate(mission, &mission_tasks) {
        LoopGate::Busy => return AdvanceOutcome::Held,
        LoopGate::GoalMet => {
            complete_loop_mission(control, mission).await;
            return AdvanceOutcome::Completed;
        }
        LoopGate::Stop(reason) => {
            stop_loop_mission(control, mission, &reason).await;
            return AdvanceOutcome::Stopped(reason);
        }
        LoopGate::Advance => {}
    }

    let (entity_name, dept) = {
        let entities = control.entities.read().await;
        match entities.iter().find(|e| e.id == mission.entity_id) {
            Some(e) => (
                e.name.clone(),
                if e.dept.is_empty() {
                    "General".to_string()
                } else {
                    e.dept.clone()
                },
            ),
            None => (mission.entity_id.clone(), "General".to_string()),
        }
    };

    let Some(planned) =
        replan_with_llm(state, control, mission, &entity_name, &mission_tasks).await
    else {
        // Deliberately no fallback: see the module docs.
        return AdvanceOutcome::Paused;
    };
    if planned.is_empty() {
        complete_loop_mission(control, mission).await;
        return AdvanceOutcome::Completed;
    }

    let iteration = mission
        .loop_state
        .as_ref()
        .map(|s| s.iteration)
        .unwrap_or(0)
        + 1;
    let now = chrono::Utc::now();
    // Lineage: the newest finished task this iteration reacted to.
    let source_task_id = mission_tasks.last().map(|t| t.id.clone());
    let tasks: Vec<Task> = planned
        .into_iter()
        .take(MAX_REPLAN_TASKS)
        .filter(|p| !p.title.trim().is_empty())
        .map(|p| Task {
            id: Uuid::new_v4().to_string(),
            title: p.title.trim().to_string(),
            dept: dept.clone(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some(mission.entity_id.clone()),
            mission_id: Some(mission.id.clone()),
            priority: Some(mission.priority.clone()),
            // Deliberately NOT gated here. The replanner has no way to raise an
            // approval request, and a task carrying `requires_human_approval`
            // with no approval row attached can never be ungated — it would sit
            // unfinished forever, holding `loop_gate` at Busy and freezing the
            // whole mission. Outward-facing work is instead kept out of the
            // loop entirely: it is not `executable`, so the executor will not
            // touch it, and it surfaces to the operator as ordinary work.
            requires_human_approval: false,
            assigned_director_id: mission.assigned_director_id.clone(),
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: None,
            task_kind: Some("mission-loop".to_string()),
            metadata: Some(json!({
                "planner": "replanner",
                // Anything the model flagged as outward-facing is never handed
                // to the unattended executor, whatever else it claimed.
                "executable": p.executable && !p.requires_approval,
                "needsApproval": p.requires_approval,
                "description": p.description,
                "iteration": iteration,
            })),
            outcome: None,
            created_at: now.to_rfc3339(),
            due_at: Some((now + chrono::Duration::days(1)).to_rfc3339()),
            completed_at: None,
            status_changed_at: None,
            source_task_id: source_task_id.clone(),
        })
        .collect();
    if tasks.is_empty() {
        return AdvanceOutcome::Paused;
    }

    {
        let mut all = control.tasks.write().await;
        all.extend(tasks.iter().cloned());
    }
    for task in &tasks {
        control.persist_task_row(task).await;
        control
            .push_operator_feed(
                OperatorFeedKind::TaskCreated,
                task,
                Some(format!("source=mission_loop iteration={iteration}")),
            )
            .await;
    }

    let queued = tasks.len();
    let updated = {
        let mut missions = control.missions.write().await;
        missions.iter_mut().find(|m| m.id == mission.id).map(|m| {
            m.loop_state = Some(MissionLoopState {
                iteration,
                cost_usd: m.loop_state.as_ref().map(|s| s.cost_usd).unwrap_or(0.0),
                last_verdict: m.loop_state.as_ref().and_then(|s| s.last_verdict.clone()),
                last_advanced_at: Some(now.to_rfc3339()),
            });
            if m.status == MissionStatus::Planned {
                m.status = MissionStatus::Active;
            }
            m.updated_at = now.to_rfc3339();
            m.clone()
        })
    };
    if let Some(m) = updated {
        control.persist_mission(&m).await;
    }
    control
        .append_operator_feed_event(OperatorFeedEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: now.to_rfc3339(),
            kind: OperatorFeedKind::MissionIterationAdvanced,
            task_id: None,
            mission_id: Some(mission.id.clone()),
            entity_id: Some(mission.entity_id.clone()),
            connection_id: None,
            title: format!("Iteration {iteration}: {}", mission.title),
            detail: Some(format!(
                "Queued {queued} task(s) toward the definition of done."
            )),
            approval_id: None,
        })
        .await;
    AdvanceOutcome::Queued(queued)
}

async fn complete_loop_mission(control: &Arc<ControlState>, mission: &Mission) {
    let completed_at = chrono::Utc::now();
    let now = completed_at.to_rfc3339();
    let updated = {
        let mut missions = control.missions.write().await;
        missions.iter_mut().find(|m| m.id == mission.id).map(|m| {
            m.status = MissionStatus::Completed;
            m.updated_at = now.clone();
            m.last_reviewed_at = Some(now.clone());
            m.clone()
        })
    };
    if let Some(m) = updated {
        control.persist_mission(&m).await;
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
            Some("reason=loop_definition_of_done_met".to_string()),
        )
        .await;
    crate::control_api::generate_mission_report(control, &mission.id).await;
}

async fn stop_loop_mission(control: &Arc<ControlState>, mission: &Mission, reason: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    let updated = {
        let mut missions = control.missions.write().await;
        missions.iter_mut().find(|m| m.id == mission.id).map(|m| {
            m.status = MissionStatus::UserInputNeeded;
            m.updated_at = now.clone();
            m.clone()
        })
    };
    if let Some(m) = updated {
        control.persist_mission(&m).await;
    }
    control
        .push_mission_feed(
            OperatorFeedKind::MissionUserInputNeeded,
            mission,
            Some(format!("reason=loop_stopped {reason}")),
        )
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{MissionLoopConfig, MissionLoopState};

    fn mission(iteration: u32, cost: f64, budget: Option<f64>, max: u32) -> Mission {
        Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Ship the importer".to_string(),
            summary: String::new(),
            status: MissionStatus::Active,
            priority: "medium".to_string(),
            goal: "CSV import works end to end".to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-08-01T00:00:00Z".to_string(),
            updated_at: "2026-08-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: Some(MissionLoopConfig {
                definition_of_done: "importing a CSV creates rows".to_string(),
                max_iterations: max,
                budget_usd: budget,
                critic_director_id: None,
            }),
            loop_state: Some(MissionLoopState {
                iteration,
                cost_usd: cost,
                last_verdict: None,
                last_advanced_at: None,
            }),
        }
    }

    fn task(status: TaskStatus, outcome: Option<TaskOutcome>) -> Task {
        Task {
            id: Uuid::new_v4().to_string(),
            title: "do a thing".to_string(),
            mission_id: Some("m1".to_string()),
            status,
            outcome,
            ..Task::default()
        }
    }

    #[test]
    fn work_in_flight_holds_the_loop() {
        let m = mission(1, 0.0, None, 5);
        for status in [
            TaskStatus::Queued,
            TaskStatus::InProgress,
            TaskStatus::Backlog,
        ] {
            let t = task(status, None);
            assert_eq!(loop_gate(&m, &[&t]), LoopGate::Busy, "{status:?}");
        }
    }

    #[test]
    fn a_task_at_the_approval_gate_holds_the_loop() {
        let m = mission(1, 0.0, None, 5);
        let mut t = task(TaskStatus::Failed, Some(TaskOutcome::NeedsReview));
        t.requires_human_approval = true;
        assert_eq!(loop_gate(&m, &[&t]), LoopGate::Busy);
    }

    #[test]
    fn all_clean_results_mean_the_goal_is_met() {
        let m = mission(1, 0.0, None, 5);
        let t = task(TaskStatus::Completed, Some(TaskOutcome::Completed));
        assert_eq!(loop_gate(&m, &[&t]), LoopGate::GoalMet);
    }

    #[test]
    fn a_flagged_result_advances_the_loop() {
        let m = mission(1, 0.0, None, 5);
        let t = task(TaskStatus::Failed, Some(TaskOutcome::NeedsReview));
        assert_eq!(loop_gate(&m, &[&t]), LoopGate::Advance);
    }

    #[test]
    fn a_loop_mission_with_no_tasks_yet_plans_its_first_iteration() {
        // This is now the normal start state: the planner deliberately seeds
        // nothing for a loop mission (inventing work would pin the loop Busy
        // behind tasks that can never satisfy the goal), so the first Drive
        // beat does the planning. Empty must read as "begin", never as "done".
        let m = mission(0, 0.0, None, 3);
        assert_eq!(loop_gate(&m, &[]), LoopGate::Advance);
    }

    #[test]
    fn declining_an_approval_never_reads_as_the_goal_being_met() {
        // The operator cancelled a task. That is a refusal, not a completion —
        // the loop must stop and ask, never declare victory.
        let m = mission(1, 0.0, None, 5);
        let done = task(TaskStatus::Completed, Some(TaskOutcome::Completed));
        let refused = task(TaskStatus::Cancelled, None);
        assert!(matches!(
            loop_gate(&m, &[&done, &refused]),
            LoopGate::Stop(_)
        ));
    }

    #[test]
    fn the_iteration_cap_stops_the_loop_not_the_success_path() {
        // Exhausted iterations with work outstanding: stop.
        let m = mission(5, 0.0, None, 5);
        let t = task(TaskStatus::Failed, Some(TaskOutcome::NeedsReview));
        assert!(matches!(loop_gate(&m, &[&t]), LoopGate::Stop(_)));
        // Exhausted iterations but everything succeeded: still a success.
        let done = task(TaskStatus::Completed, Some(TaskOutcome::Completed));
        assert_eq!(loop_gate(&m, &[&done]), LoopGate::GoalMet);
    }

    #[tokio::test]
    async fn spend_accrues_so_the_budget_cap_can_actually_fire() {
        let control = Arc::new(ControlState::new("openai", "https://x", "m"));
        let m = mission(1, 0.0, Some(1.0), 5);
        control.missions.write().await.push(m.clone());

        crate::mission_replanner::accrue_loop_cost(&control, &m.id, 0.4).await;
        crate::mission_replanner::accrue_loop_cost(&control, &m.id, 0.4).await;
        {
            let missions = control.missions.read().await;
            let after = missions.iter().find(|x| x.id == m.id).unwrap();
            let spent = after.loop_state.as_ref().unwrap().cost_usd;
            assert!((spent - 0.8).abs() < 1e-9, "spend accrues: {spent}");
            // Still under the cap, so the loop may continue.
            let t = task(TaskStatus::Failed, Some(TaskOutcome::NeedsReview));
            assert_eq!(loop_gate(after, &[&t]), LoopGate::Advance);
        }

        // One more run crosses it, and the loop stops.
        crate::mission_replanner::accrue_loop_cost(&control, &m.id, 0.3).await;
        let missions = control.missions.read().await;
        let after = missions.iter().find(|x| x.id == m.id).unwrap();
        let t = task(TaskStatus::Failed, Some(TaskOutcome::NeedsReview));
        assert!(matches!(loop_gate(after, &[&t]), LoopGate::Stop(_)));
    }

    #[tokio::test]
    async fn a_mission_without_a_loop_never_accrues_loop_spend() {
        let control = Arc::new(ControlState::new("openai", "https://x", "m"));
        let mut m = mission(1, 0.0, Some(1.0), 5);
        m.loop_config = None;
        m.loop_state = None;
        control.missions.write().await.push(m.clone());
        crate::mission_replanner::accrue_loop_cost(&control, &m.id, 5.0).await;
        let missions = control.missions.read().await;
        let after = missions.iter().find(|x| x.id == m.id).unwrap();
        assert!(after.loop_state.is_none(), "no loop, no loop accounting");
    }

    #[test]
    fn the_budget_cap_outranks_everything() {
        let m = mission(1, 12.0, Some(10.0), 5);
        let t = task(TaskStatus::Completed, Some(TaskOutcome::Completed));
        assert!(matches!(loop_gate(&m, &[&t]), LoopGate::Stop(_)));
    }

    #[test]
    fn the_hard_cap_bounds_a_reckless_config() {
        let m = mission(30, 0.0, None, 9_999);
        let t = task(TaskStatus::Failed, Some(TaskOutcome::NeedsReview));
        assert!(matches!(loop_gate(&m, &[&t]), LoopGate::Stop(_)));
    }

    #[test]
    fn the_prompt_carries_the_finish_line_and_the_evidence() {
        let m = mission(1, 0.0, None, 5);
        let mut t = task(TaskStatus::Failed, Some(TaskOutcome::NeedsReview));
        t.title = "wire the parser".to_string();
        t.result = Some("could not find the schema".to_string());
        t.metadata = Some(json!({
            "evidence": { "isGit": true, "commits": 0, "filesChanged": 0,
                          "insertions": 0, "deletions": 0, "dirtyDelta": [] }
        }));
        let p = build_replanner_prompt(&m, "Acme", &[&t], 1, 5);
        assert!(p.contains("importing a CSV creates rows"), "DoD missing");
        assert!(p.contains("iteration 2 of at most 5"), "position missing");
        assert!(p.contains("wire the parser"), "prior work missing");
        assert!(p.contains("could not find the schema"), "reason missing");
        assert!(p.contains("0 commits"), "evidence missing");
        assert!(p.contains("[]"), "the done answer must be describable");
    }

    #[test]
    fn a_first_iteration_prompt_says_so() {
        let m = mission(0, 0.0, None, 5);
        let p = build_replanner_prompt(&m, "Acme", &[], 0, 5);
        assert!(p.contains("first iteration"));
    }
}
