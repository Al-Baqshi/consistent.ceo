//! Heartbeat-driven task pickup per director. When a director's heartbeat is on,
//! this loop periodically grabs one queued task, assigns it to an agent, runs it
//! via run_agent (LLM call), and marks the task completed with result.
//!
//! After a successful **`prompt-interpretation`** task, enqueues one **follow-up**
//! task from the structured brief (director auto-task rule, step 2).

use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::AppConfig;
use crate::entity_memory;
use crate::entity_runtime::{
    effective_automation_mode, entity_heartbeat_skips_task_pickup,
    should_suppress_interpretation_followup,
};
use crate::interpreter::{PromptInterpretationBrief, parse_prompt_interpretation_brief};
use crate::run_agent;
use crate::state::{
    Agent, AutomationMode, ControlState, DirectorState, Entity, Mission, MissionStatus,
    OperatorFeedKind, Task, TaskOutcome, TaskStatus,
};
use consistent_world_memory::store::MemoryStore;

// Tick cadence and cooldown windows for the gap-reporting / follow-up logic below.
const TICK_SECS: u64 = 15;
const MISSION_GAP_REPORT_COOLDOWN_SECS: u64 = 300;
const MISSION_AUTOFOLLOWUP_COOLDOWN_SECS: u64 = 900;
const MISSION_REVIEW_DUE_COOLDOWN_SECS: u64 = 3600;
const MISSION_INACTIVITY_STALL_HOURS: i64 = 48;
/// A task InProgress longer than this (its run hung or was killed) is force-failed
/// by the watchdog. Well above run_agent's 240s LLM timeout to avoid false hits.
const TASK_STUCK_THRESHOLD_SECS: i64 = 900;

// True once `cooldown` has passed since `last`, or immediately if `last` is None.
fn cooldown_elapsed(last: Option<Instant>, cooldown: Duration, now: Instant) -> bool {
    match last {
        Some(ts) => now.duration_since(ts) >= cooldown,
        None => true,
    }
}

// Tallies a mission's tasks: total, blocked (Failed), completed, and whether
// any task is still open (Backlog/Queued/InProgress).
/// Move a task's status on a run-completion path, from a background loop.
///
/// Every caller sits inside a block that has already checked `Cancelled`, so the
/// loop-side guard cannot refuse here — but going through it keeps the actor stated,
/// keeps `status_changed_at` and `completed_at` stamped (assigning `task.status`
/// directly is what let those drift), and logs loudly rather than silently
/// overwriting if one of those outer guards is ever removed.
fn set_status_after_run(t: &mut Task, status: TaskStatus) {
    if !crate::state::set_task_status_by_loop(t, status) {
        warn!(
            task_id = %t.id,
            attempted = ?status,
            "orchestrator: refused to move a cancelled task after its run finished"
        );
    }
}

fn derive_mission_task_counts(tasks: &[&Task]) -> (usize, usize, usize, bool) {
    let task_count = tasks.len();
    let blocked_count = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Failed)
        .count();
    let completed_count = tasks
        .iter()
        .filter(|t| t.status == TaskStatus::Completed)
        .count();
    let has_next_task = tasks.iter().any(|t| {
        matches!(
            t.status,
            TaskStatus::Backlog | TaskStatus::Queued | TaskStatus::InProgress
        )
    });
    (task_count, blocked_count, completed_count, has_next_task)
}

// Whether an unclosed mission-next-step follow-up already exists for this
// mission, used to avoid creating duplicate follow-ups.
fn has_open_next_step_task(tasks: &[Task], mission_id: &str) -> bool {
    tasks.iter().any(|t| {
        t.mission_id.as_deref() == Some(mission_id)
            && t.task_kind.as_deref() == Some("mission-next-step")
            && matches!(
                t.status,
                TaskStatus::Backlog | TaskStatus::Queued | TaskStatus::InProgress
            )
    })
}

/// Tasks with a dedicated execution loop must never enter the generic director
/// loop. Otherwise two independent tickers can race the same queued row and run
/// it twice (or run it with the wrong execution contract).
fn specialized_loop_owner(task: &Task) -> Option<&'static str> {
    if task.task_kind.as_deref() == Some(crate::mission_engine::BEAT_TASK_KIND) {
        return Some("mission_engine_owned");
    }

    let explicitly_executable = match task
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(crate::task_executor::META_EXECUTABLE))
    {
        Some(serde_json::Value::Bool(value)) => *value,
        Some(serde_json::Value::String(value)) => value.eq_ignore_ascii_case("true"),
        _ => false,
    };
    explicitly_executable.then_some("task_executor_owned")
}

// Returns the reason this director cannot run `task` right now, or None if it can.
fn director_can_run_task(
    director: &DirectorState,
    agents: &[Agent],
    task: &Task,
    entity: Option<&Entity>,
    config: &AppConfig,
) -> Option<&'static str> {
    if !director.heartbeat_enabled {
        return Some("director_heartbeat_disabled");
    }
    if task.requires_human_approval {
        return Some("task_requires_human_approval");
    }
    if let Some(owner) = specialized_loop_owner(task) {
        return Some(owner);
    }
    if entity_heartbeat_skips_task_pickup(entity) {
        return Some("entity_heartbeat_off");
    }
    if let Some(kind) = task.task_kind.as_deref()
        && let Some(routed) = config
            .task_kind_routing
            .get(kind)
            .and_then(|e| e.director_id.as_deref())
        && routed != director.id
    {
        return Some("task_routed_to_other_director");
    }
    if !agents.iter().any(|a| a.group_id == director.id) {
        return Some("director_has_no_agents");
    }
    None
}

/// Aggregates `director_can_run_task` reasons across a mission's open tasks;
/// `None` means at least one director can run something right now.
pub(crate) async fn mission_autorun_block_reason(
    control: &ControlState,
    config: &AppConfig,
    mission: &Mission,
    tasks: &[Task],
) -> Option<String> {
    let mission_tasks: Vec<&Task> = tasks
        .iter()
        .filter(|t| {
            t.mission_id.as_deref() == Some(mission.id.as_str())
                && matches!(
                    t.status,
                    TaskStatus::Backlog | TaskStatus::Queued | TaskStatus::InProgress
                )
        })
        .collect();
    if mission_tasks.is_empty() {
        return Some("no_open_mission_tasks".to_string());
    }

    let entity = {
        let entities = control.entities.read().await;
        entities.iter().find(|e| e.id == mission.entity_id).cloned()
    };
    let directors = control.director_states.read().await.clone();
    let agents = control.agents.read().await.clone();

    let mut reasons = std::collections::BTreeSet::new();
    for task in mission_tasks {
        for director in &directors {
            match director_can_run_task(director, &agents, task, entity.as_ref(), config) {
                None => return None,
                Some(reason) => {
                    reasons.insert(reason);
                }
            }
        }
        if directors.is_empty() {
            reasons.insert("no_directors_configured");
        }
    }
    if reasons.is_empty() {
        None
    } else {
        Some(reasons.into_iter().collect::<Vec<_>>().join(","))
    }
}

// Truncates to `max` chars (by char count, not bytes), appending "..." when cut short.
fn trimmed_for_task_title(value: &str, max: usize) -> String {
    let cleaned = value.trim();
    if cleaned.chars().count() <= max {
        return cleaned.to_string();
    }
    cleaned
        .chars()
        .take(max.saturating_sub(3))
        .collect::<String>()
        + "..."
}

/// Recent-activity and staleness signals for one mission, derived from the operator feed.
#[derive(Debug, Clone, Default)]
struct MissionActivitySnapshot {
    recent_activity_count_24h: usize,
    last_progress_at: Option<DateTime<Utc>>,
    inactivity_hours: Option<i64>,
}

// Scans the feed for this mission's events in the last 24h and finds the
// most recent progress event (task completed/created, recommendation accepted).
fn mission_activity_snapshot(
    feed: &[crate::state::OperatorFeedEvent],
    mission_id: &str,
    now: DateTime<Utc>,
) -> MissionActivitySnapshot {
    let mut out = MissionActivitySnapshot::default();
    for event in feed
        .iter()
        .filter(|e| e.mission_id.as_deref() == Some(mission_id))
    {
        let Ok(ts) = DateTime::parse_from_rfc3339(&event.timestamp) else {
            continue;
        };
        let ts = ts.with_timezone(&Utc);
        if now.signed_duration_since(ts).num_hours() <= 24 {
            out.recent_activity_count_24h += 1;
        }
        let is_progress_event = matches!(
            event.kind,
            OperatorFeedKind::TaskCompleted
                | OperatorFeedKind::MissionRecommendationAccepted
                | OperatorFeedKind::TaskCreated
        );
        if is_progress_event {
            let replace = out.last_progress_at.map(|cur| ts > cur).unwrap_or(true);
            if replace {
                out.last_progress_at = Some(ts);
            }
        }
    }
    out.inactivity_hours = out
        .last_progress_at
        .map(|last| now.signed_duration_since(last).num_hours());
    out
}

// Whether this mission's review cadence (its own override, or the default) has elapsed.
fn is_review_due(mission: &Mission, default_cadence_hours: u32, now: DateTime<Utc>) -> bool {
    let cadence_hours = mission
        .review_cadence_hours
        .unwrap_or(default_cadence_hours)
        .max(1);
    mission
        .last_reviewed_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| {
            now.signed_duration_since(dt.with_timezone(&Utc))
                .num_hours()
                >= cadence_hours as i64
        })
        .unwrap_or(true)
}

/// Run the orchestrator loop. For each director with heartbeat on, picks one
/// backlog/queued task (skipping requires_human_approval), assigns to first agent
/// of that director, marks in-progress, then runs the agent via LLM and marks completed.
pub async fn run_loop(
    control: Arc<ControlState>,
    config: &AppConfig,
    memory: Option<Arc<dyn MemoryStore>>,
) {
    let mut ticker = interval(Duration::from_secs(TICK_SECS));
    let mut mission_gap_reported_at: HashMap<String, Instant> = HashMap::new();
    let mut mission_autofollowup_created_at: HashMap<String, Instant> = HashMap::new();
    let mut mission_review_due_reported_at: HashMap<String, Instant> = HashMap::new();
    let mut mission_goal_decision: HashMap<String, String> = HashMap::new();
    loop {
        ticker.tick().await;
        spawn_recurring_plan_children(control.clone()).await;
        // Watchdog: force-fail tasks stuck InProgress too long so the loop never
        // spins forever waiting on a dead run.
        for stuck in control
            .detect_stuck_tasks(TASK_STUCK_THRESHOLD_SECS, Utc::now())
            .await
        {
            let detail = stuck.result.clone();
            control
                .push_operator_feed(OperatorFeedKind::TaskStuck, &stuck, detail)
                .await;
        }
        // Autonomous goal loop: decide + complete/ask per active mission, then let
        // report_mission_gaps propose next tasks and task pickup execute them.
        crate::mission_goal_loop::tick(&control, &mut mission_goal_decision).await;
        report_mission_gaps(
            control.clone(),
            config,
            &mut mission_gap_reported_at,
            &mut mission_autofollowup_created_at,
            &mut mission_review_due_reported_at,
            &memory,
        )
        .await;
        if let Err(e) = control.sync_missions_to_store().await {
            tracing::warn!(error = %e, "mission canonical sync failed; retrying next loop");
        }
        let directors_on: Vec<String> = {
            let states = control.director_states.read().await;
            states
                .iter()
                .filter(|s| s.heartbeat_enabled)
                .map(|s| s.id.clone())
                .collect()
        };
        if directors_on.is_empty() {
            continue;
        }
        let entity_by_id: std::collections::HashMap<String, Entity> = {
            let entities = control.entities.read().await;
            entities.iter().map(|e| (e.id.clone(), e.clone())).collect()
        };
        let agents: Vec<(String, String)> = {
            let agents = control.agents.read().await;
            agents
                .iter()
                .map(|a| (a.group_id.clone(), a.id.clone()))
                .collect()
        };
        for director_id in directors_on {
            let director_agent_ids: Vec<String> = agents
                .iter()
                .filter(|(gid, _)| gid == &director_id)
                .map(|(_, aid)| aid.clone())
                .collect();
            let first_agent = director_agent_ids.first().cloned();
            if first_agent.is_none() {
                continue;
            }
            let now_rfc3339 = Utc::now().to_rfc3339();
            let task_id = {
                // Candidates are identified by id and by their position *in this
                // list*, never by their index in `control.tasks`. The read guard
                // below is dropped before the Surreal recalls, so any index into the
                // shared Vec is stale by the time we would use it — see
                // `ControlState::claim_task`.
                let candidates: Vec<(String, Option<String>, String, Option<String>)> = {
                    let tasks = control.tasks.read().await;
                    tasks
                        .iter()
                        .filter_map(|t| {
                            if specialized_loop_owner(t).is_some() {
                                return None;
                            }
                            if (t.status != TaskStatus::Backlog && t.status != TaskStatus::Queued)
                                || t.requires_human_approval
                            {
                                return None;
                            }
                            // Retry backoff: don't pick a task before its next-retry time.
                            if let Some(retry_at) = crate::state::task_meta_str(t, "nextRetryAt")
                                && retry_at.as_str() > now_rfc3339.as_str()
                            {
                                return None;
                            }
                            if let Some(assigned_director_id) = t.assigned_director_id.as_deref()
                                && assigned_director_id != director_id
                            {
                                return None;
                            }
                            if let Some(eid) = t.entity_id.as_ref()
                                && let Some(ent) = entity_by_id.get(eid)
                                && entity_heartbeat_skips_task_pickup(Some(ent))
                            {
                                return None;
                            }
                            let ok = match t.task_kind.as_deref() {
                                None => true,
                                Some(kind) => config
                                    .task_kind_routing
                                    .get(kind)
                                    .and_then(|e| e.director_id.as_deref())
                                    .map(|id| id == director_id)
                                    .unwrap_or(true),
                            };
                            if !ok {
                                return None;
                            }
                            Some((
                                t.id.clone(),
                                t.entity_id.clone(),
                                t.title.clone(),
                                t.task_kind.clone(),
                            ))
                        })
                        .collect()
                };

                let mut memory_by_entity: HashMap<String, Vec<String>> = HashMap::new();
                if let Some(ref mem) = memory {
                    let ids: HashSet<String> = candidates
                        .iter()
                        .filter_map(|(_, eid, _, _)| eid.clone())
                        .collect();
                    for eid in ids {
                        let lines = entity_memory::recall_entity_short(mem.clone(), &eid, 16).await;
                        if !lines.is_empty() {
                            memory_by_entity.insert(eid, lines);
                        }
                    }
                }

                fn orchestrator_memory_pick_score(
                    entity_id: &Option<String>,
                    title: &str,
                    task_kind: &Option<String>,
                    mem_map: &HashMap<String, Vec<String>>,
                ) -> i32 {
                    let Some(eid) = entity_id.as_ref() else {
                        return 0;
                    };
                    let Some(mem) = mem_map.get(eid) else {
                        return 0;
                    };
                    let title_lower = title.to_lowercase();
                    let kind_lower = task_kind.as_deref().unwrap_or("").to_lowercase();
                    let mut score = 0i32;
                    for line in mem {
                        for w in line.to_lowercase().split_whitespace() {
                            if w.len() < 4 {
                                continue;
                            }
                            if title_lower.contains(w) || kind_lower.contains(w) {
                                score += 1;
                            }
                        }
                    }
                    score
                }

                // Ties break toward the earliest candidate, which preserves queue
                // order; `rank` is the position within `candidates`, not in
                // `control.tasks`, so nothing here can be used as an index later.
                let mut best: Option<(usize, &String, i32)> = None;
                for (rank, (tid, eid, title, tk)) in candidates.iter().enumerate() {
                    let s = orchestrator_memory_pick_score(eid, title, tk, &memory_by_entity);
                    let replace = match best {
                        None => true,
                        Some((_, _, bs)) if s > bs => true,
                        Some((brank, _, bs)) if s == bs && rank < brank => true,
                        _ => false,
                    };
                    if replace {
                        best = Some((rank, tid, s));
                    }
                }

                let (picked_id, memory_bias) = match best {
                    Some((_, tid, s)) => (Some(tid.clone()), s),
                    None => (None, 0),
                };

                // Claim under the write guard, re-asserting that the task is still
                // pickable. The gap between selection and here contains Surreal
                // I/O with no lock held, so the task may have been cancelled by a
                // human, closed by another loop, or deleted with its entity.
                // `claim_task` returns None for all three, and all three mean the
                // same thing: leave it alone.
                match picked_id {
                    Some(tid) => control
                        .claim_task(&tid, &[TaskStatus::Backlog, TaskStatus::Queued], |t| {
                            t.assigned_director_id = Some(director_id.clone());
                            t.assigned_agent_id = first_agent.clone();
                            let claimed =
                                crate::state::set_task_status_by_loop(t, TaskStatus::InProgress);
                            debug_assert!(claimed, "claim_task already asserted Backlog|Queued");
                            t.progress = 10;
                            crate::state::task_meta_set_str(t, "startedAt", &now_rfc3339);
                            crate::state::task_meta_clear(t, "nextRetryAt");
                            t.id.clone()
                        })
                        .await
                        .inspect(|id| {
                            if memory_bias > 0 {
                                let memd =
                                    crate::planner::heartbeat_memory_bias_decision(memory_bias);
                                info!(
                                    task_id = %id,
                                    planner_hint = %memd.detail_str(),
                                    "orchestrator: memory-informed task pick"
                                );
                            }
                            info!(
                                task_id = %id,
                                director_id = %director_id,
                                agent_id = ?first_agent,
                                memory_bias = memory_bias,
                                "orchestrator: assigned task"
                            );
                        })
                        .or_else(|| {
                            info!(
                                task_id = %tid,
                                director_id = %director_id,
                                "orchestrator: candidate no longer pickable, skipping"
                            );
                            None
                        }),
                    None => None,
                }
            };
            if let Some(tid) = task_id {
                let control_clone = control.clone();
                let config = config.clone();
                let mem = memory.clone();
                tokio::spawn(async move {
                    let run_result =
                        run_agent::run_agent(control_clone.clone(), &config, tid.clone(), mem)
                            .await;
                    let followup_eligible = match &run_result {
                        Ok(result) => {
                            let tasks = control_clone.tasks.read().await;
                            tasks
                                .iter()
                                .find(|t| t.id == tid)
                                .map(|t| {
                                    t.task_kind.as_deref() == Some("prompt-interpretation")
                                        && result.outcome == TaskOutcome::Completed
                                })
                                .unwrap_or(false)
                        }
                        Err(_) => false,
                    };
                    match run_result {
                        Ok(result) => {
                            let operator_feed: Option<(OperatorFeedKind, Task, Option<String>)> = {
                                let mut tasks = control_clone.tasks.write().await;
                                if let Some(t) = tasks.iter_mut().find(|t| t.id == tid) {
                                    // A task cancelled mid-run keeps its terminal state.
                                    if t.status == TaskStatus::Cancelled {
                                        None
                                    } else {
                                        t.outcome = Some(result.outcome);
                                        match result.outcome {
                                            TaskOutcome::Completed | TaskOutcome::NeedsReview => {
                                                None
                                            }
                                            TaskOutcome::BlockedByPolicy => {
                                                set_status_after_run(t, TaskStatus::Failed);
                                                t.progress = 100;
                                                t.result = result.message.clone().or_else(|| {
                                                    Some("Task blocked by policy".to_string())
                                                });
                                                t.assigned_director_id = None;
                                                t.assigned_agent_id = None;
                                                let detail = t.result.clone();
                                                Some((
                                                    OperatorFeedKind::TaskBlocked,
                                                    t.clone(),
                                                    detail,
                                                ))
                                            }
                                            TaskOutcome::Failed => {
                                                set_status_after_run(t, TaskStatus::Failed);
                                                t.progress = 100;
                                                t.result = result
                                                    .message
                                                    .clone()
                                                    .or_else(|| Some("Task failed".to_string()));
                                                t.assigned_director_id = None;
                                                t.assigned_agent_id = None;
                                                let detail = t.result.clone();
                                                Some((
                                                    OperatorFeedKind::TaskBlocked,
                                                    t.clone(),
                                                    detail,
                                                ))
                                            }
                                            TaskOutcome::NeedsHumanApproval => {
                                                t.requires_human_approval = true;
                                                set_status_after_run(t, TaskStatus::Queued);
                                                t.progress = 0;
                                                t.assigned_director_id = None;
                                                t.assigned_agent_id = None;
                                                let detail = t.result.clone();
                                                Some((
                                                    OperatorFeedKind::ApprovalNeeded,
                                                    t.clone(),
                                                    detail,
                                                ))
                                            }
                                            TaskOutcome::NeedsStrongerModel => {
                                                set_status_after_run(t, TaskStatus::Queued);
                                                t.progress = 0;
                                                t.assigned_director_id = None;
                                                t.assigned_agent_id = None;
                                                if let Some(msg) = result.message {
                                                    t.result = Some(msg);
                                                }
                                                None
                                            }
                                            TaskOutcome::RetryableFailure => {
                                                let n: u32 = t.tries.parse().unwrap_or(0) + 1;
                                                t.tries = n.to_string();
                                                let err_msg = result.message.unwrap_or_else(|| {
                                                    "retryable failure".to_string()
                                                });
                                                let is_enrichment = t.task_kind.as_deref()
                                                    == Some("entity-enrichment");
                                                let policy = crate::retry::RetryPolicy::default();
                                                match policy.evaluate(n) {
                                                    crate::retry::RetryOutcome::Exhausted {
                                                        ..
                                                    } => {
                                                        if let Some(ref entity_id) = t.entity_id {
                                                            let mut entities = control_clone
                                                                .entities
                                                                .write()
                                                                .await;
                                                            if let Some(ent) = entities
                                                                .iter_mut()
                                                                .find(|e| e.id == *entity_id)
                                                                && let Some(ref mut en) =
                                                                    ent.enrichment
                                                            {
                                                                let st = if is_enrichment {
                                                                    crate::enrichment_llm::enrichment_status_for_llm_error_message(
                                                                        &err_msg,
                                                                    )
                                                                } else {
                                                                    "failed"
                                                                };
                                                                en.status = st.to_string();
                                                                en.error = Some(err_msg.clone());
                                                            }
                                                        }
                                                        set_status_after_run(t, TaskStatus::Failed);
                                                        t.progress = 100;
                                                        t.result = Some(if is_enrichment {
                                                            format!(
                                                                "Enrichment failed after {n} attempts: {err_msg}"
                                                            )
                                                        } else {
                                                            format!(
                                                                "Task failed after {n} attempts: {err_msg}"
                                                            )
                                                        });
                                                        crate::state::task_meta_clear(
                                                            t,
                                                            "nextRetryAt",
                                                        );
                                                        t.assigned_director_id = None;
                                                        t.assigned_agent_id = None;
                                                        let detail = t.result.clone();
                                                        Some((
                                                            OperatorFeedKind::TaskBlocked,
                                                            t.clone(),
                                                            detail,
                                                        ))
                                                    }
                                                    crate::retry::RetryOutcome::Retry {
                                                        attempt,
                                                        delay_secs,
                                                    } => {
                                                        let retry_at = (Utc::now()
                                                            + chrono::Duration::seconds(
                                                                delay_secs as i64,
                                                            ))
                                                        .to_rfc3339();
                                                        set_status_after_run(t, TaskStatus::Queued);
                                                        t.progress = 0;
                                                        crate::state::task_meta_clear(
                                                            t,
                                                            "startedAt",
                                                        );
                                                        crate::state::task_meta_set_str(
                                                            t,
                                                            "nextRetryAt",
                                                            &retry_at,
                                                        );
                                                        t.assigned_director_id = None;
                                                        t.assigned_agent_id = None;
                                                        let detail = format!(
                                                            "attempt {attempt} of {} failed; retry in {delay_secs}s. {err_msg}",
                                                            policy.max_attempts
                                                        );
                                                        Some((
                                                            OperatorFeedKind::TaskRetryScheduled,
                                                            t.clone(),
                                                            Some(detail),
                                                        ))
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    None
                                }
                            };
                            if let Some((kind, task, detail)) = operator_feed {
                                control_clone.push_operator_feed(kind, &task, detail).await;
                            }
                        }
                        Err(e) => {
                            warn!(task_id = %tid, error = %e, "orchestrator: run_agent fatal error");
                            // The Ok branch above has always kept a cancelled task
                            // cancelled; this one did not, so a runner crash after a
                            // human pressed stop rewrote `Cancelled` to `Failed`. The
                            // run's error is still recorded either way — what changes
                            // is that the human's decision survives it.
                            let operator_feed = control_clone
                                .update_task(&tid, |t| {
                                    let was_cancelled = t.status == TaskStatus::Cancelled;
                                    t.result = Some(format!(
                                        "reason=runner_fatal_error\nerror={}\naction={}",
                                        e.replace('\n', " "),
                                        if was_cancelled {
                                            "kept_cancelled"
                                        } else {
                                            "mark_failed"
                                        }
                                    ));
                                    if was_cancelled {
                                        return None;
                                    }
                                    set_status_after_run(t, TaskStatus::Failed);
                                    t.progress = 100;
                                    t.outcome = Some(TaskOutcome::Failed);
                                    t.assigned_director_id = None;
                                    t.assigned_agent_id = None;
                                    Some((
                                        OperatorFeedKind::TaskBlocked,
                                        t.clone(),
                                        t.result.clone(),
                                    ))
                                })
                                .await
                                .flatten();
                            if let Some((kind, task, detail)) = operator_feed {
                                control_clone.push_operator_feed(kind, &task, detail).await;
                                if let Some(mission_id) = task.mission_id.as_deref() {
                                    let mission = {
                                        let missions = control_clone.missions.read().await;
                                        missions.iter().find(|m| m.id == mission_id).cloned()
                                    };
                                    if let Some(mission) = mission {
                                        control_clone.push_mission_feed(
                                            OperatorFeedKind::MissionStalled,
                                            &mission,
                                            Some(format!(
                                                "reason=runner_fatal_error\ntask_id={}\ntask_kind={}\nerror={}",
                                                task.id,
                                                task.task_kind.as_deref().unwrap_or("unknown"),
                                                e.replace('\n', " ")
                                            )),
                                        ).await;
                                    }
                                }
                            }
                        }
                    }
                    if followup_eligible {
                        maybe_enqueue_followup_after_interpretation(control_clone, &tid).await;
                    }
                });
            }
        }
    }
}

/// For each active mission with no next executable task, emits an
/// operator-feed gap report and, per mission policy, auto-creates one
/// follow-up task (bootstrap/unblock/next-phase/user-input branch).
async fn report_mission_gaps(
    control: Arc<ControlState>,
    config: &AppConfig,
    last_reported_at: &mut HashMap<String, Instant>,
    last_autofollowup_at: &mut HashMap<String, Instant>,
    last_review_due_reported_at: &mut HashMap<String, Instant>,
    memory: &Option<Arc<dyn MemoryStore>>,
) {
    let missions = control.missions.read().await.clone();
    let tasks = control.tasks.read().await.clone();
    let feed = control.operator_feed.read().await.clone();

    let now = Instant::now();
    let now_utc = Utc::now();
    let cooldown = Duration::from_secs(MISSION_GAP_REPORT_COOLDOWN_SECS);
    let followup_cooldown = Duration::from_secs(MISSION_AUTOFOLLOWUP_COOLDOWN_SECS);
    let review_cooldown = Duration::from_secs(MISSION_REVIEW_DUE_COOLDOWN_SECS);
    let mut active_ids: Vec<String> = Vec::with_capacity(missions.len());

    for mission in missions {
        if !matches!(
            mission.status,
            MissionStatus::Active | MissionStatus::UserInputNeeded
        ) {
            continue;
        }
        // A loop-enabled mission has its own driver (the Drive beat calling
        // the replanner). Two drivers queueing follow-ups for the same mission
        // would fight: this one emits prose tasks the executor can't run,
        // which would also hold the loop gate open forever.
        if mission.loop_config.is_some() {
            active_ids.push(mission.id.clone());
            continue;
        }
        active_ids.push(mission.id.clone());
        if let Some(mem) = memory {
            let snippets =
                crate::entity_memory::recall_entity_short(mem.clone(), &mission.entity_id, 5).await;
            if !snippets.is_empty() {
                tracing::debug!(
                    entity_id = %mission.entity_id,
                    memory_snippets = snippets.len(),
                    "orchestrator: entity short memory before mission review"
                );
            }
        }
        // Review-due reporting is independent of whether a follow-up task is warranted.
        let review_due = is_review_due(
            &mission,
            config.mission_policy.default_review_cadence_hours,
            now_utc,
        );
        if review_due {
            let can_report_review_due = last_review_due_reported_at
                .get(&mission.id)
                .map(|last| now.duration_since(*last) >= review_cooldown)
                .unwrap_or(true);
            if can_report_review_due {
                let cadence = mission
                    .review_cadence_hours
                    .unwrap_or(config.mission_policy.default_review_cadence_hours);
                control
                    .push_mission_feed(
                        OperatorFeedKind::MissionReviewDue,
                        &mission,
                        Some(format!(
                            "reason=review_due cadence_hours={cadence} policy_followup_enabled={}",
                            config.mission_policy.followup_task_proposal_enabled
                        )),
                    )
                    .await;
                last_review_due_reported_at.insert(mission.id.clone(), now);
            }
        }
        if !config.mission_policy.followup_task_proposal_enabled {
            continue;
        }
        // Rate-limit how often this mission's gap gets reported.
        if !cooldown_elapsed(last_reported_at.get(&mission.id).copied(), cooldown, now) {
            continue;
        }

        let mission_tasks: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.mission_id.as_deref() == Some(mission.id.as_str()))
            .collect();
        let (task_count, blocked_count, completed_count, has_next_task) =
            derive_mission_task_counts(&mission_tasks);
        let activity = mission_activity_snapshot(&feed, &mission.id, now_utc);
        let stalled_by_inactivity = !has_next_task
            && task_count > 0
            && activity
                .inactivity_hours
                .map(|h| h >= MISSION_INACTIVITY_STALL_HOURS)
                .unwrap_or(false);
        if has_next_task {
            continue;
        }

        // Mission completion (DoD met) is owned by the autonomous goal loop
        // (`mission_goal_loop::tick`), which runs earlier in the same tick.

        // Classify the gap so the operator feed and auto-created task can explain why.
        let branch = if mission.status == MissionStatus::UserInputNeeded || stalled_by_inactivity {
            "user_input"
        } else if blocked_count > 0 {
            "unblock"
        } else if task_count == 0 {
            "bootstrap"
        } else {
            "next_phase"
        };
        let detail = format!(
            "reason=no_next_executable_task total={} blocked={} completed={} has_next_task=false cadence_hours={} branch={} policy_proposal_enabled={} policy_auto_create={} recent_activity_24h={} inactivity_hours={}",
            task_count,
            blocked_count,
            completed_count,
            mission
                .review_cadence_hours
                .unwrap_or(config.mission_policy.default_review_cadence_hours),
            branch,
            config.mission_policy.followup_task_proposal_enabled,
            config.mission_policy.followup_auto_create_enabled,
            activity.recent_activity_count_24h,
            activity.inactivity_hours.unwrap_or(-1)
        );
        info!(
            mission_id = %mission.id,
            entity_id = %mission.entity_id,
            mission_title = %mission.title,
            task_count = task_count,
            blocked_tasks = blocked_count,
            completed_tasks = completed_count,
            "orchestrator: active mission has no next executable task (report-only)"
        );
        let feed_kind = if blocked_count > 0 {
            OperatorFeedKind::MissionStalled
        } else {
            OperatorFeedKind::MissionMissingNextTask
        };
        control
            .push_mission_feed(feed_kind, &mission, Some(detail.clone()))
            .await;
        if stalled_by_inactivity && mission.status != MissionStatus::UserInputNeeded {
            {
                let mut missions_mut = control.missions.write().await;
                if let Some(m) = missions_mut.iter_mut().find(|m| m.id == mission.id) {
                    m.status = MissionStatus::UserInputNeeded;
                    m.updated_at = now_utc.to_rfc3339();
                }
            }
            control
                .push_mission_feed(
                    OperatorFeedKind::MissionUserInputNeeded,
                    &mission,
                    Some(format!(
                        "reason=inactive_no_progress inactivity_hours={} action=mark_user_input_needed",
                        activity.inactivity_hours.unwrap_or(-1)
                    )),
                )
                .await;
        }
        control
            .increment_recommendation_metric("emitted", Some(branch), None)
            .await;
        last_reported_at.insert(mission.id.clone(), now);

        // Every check below is a reason to skip auto-creating a follow-up task;
        // each is logged to the mission feed so the decision is auditable.
        if !config.mission_policy.followup_auto_create_enabled {
            control
                .push_mission_feed(
                    OperatorFeedKind::MissionFollowupSuppressed,
                    &mission,
                    Some(format!(
                        "reason=recommend_only_mode decision=skip_auto_create branch={branch}"
                    )),
                )
                .await;
            control
                .increment_recommendation_metric("suppressed_duplicate", Some(branch), None)
                .await;
            continue;
        }
        let can_autofollowup = cooldown_elapsed(
            last_autofollowup_at.get(&mission.id).copied(),
            followup_cooldown,
            now,
        );
        if !can_autofollowup {
            control
                .push_mission_feed(
                    OperatorFeedKind::MissionFollowupSuppressed,
                    &mission,
                    Some(format!(
                        "reason=autocreate_cooldown decision=skip_auto_create branch={branch}"
                    )),
                )
                .await;
            control
                .increment_recommendation_metric("suppressed_duplicate", Some(branch), None)
                .await;
            continue;
        }
        if has_open_next_step_task(&tasks, &mission.id) {
            control
                .push_mission_feed(
                    OperatorFeedKind::MissionFollowupSuppressed,
                    &mission,
                    Some(format!(
                        "reason=duplicate_open_next_step decision=skip_auto_create branch={branch}"
                    )),
                )
                .await;
            control
                .increment_recommendation_metric("suppressed_duplicate", Some(branch), None)
                .await;
            continue;
        }
        if task_count > 0 && completed_count == task_count && blocked_count == 0 {
            control
                .push_mission_feed(
                    OperatorFeedKind::MissionFollowupSuppressed,
                    &mission,
                    Some(format!(
                        "reason=all_tasks_completed decision=skip_low_value_followup branch={branch}"
                    )),
                )
                .await;
            control
                .increment_recommendation_metric("suppressed_complete", Some(branch), None)
                .await;
            continue;
        }
        // Demote next_phase eagerness: only allow when mission has substantial completed progress.
        if branch == "next_phase" && completed_count * 2 < task_count {
            control
                .push_mission_feed(
                    OperatorFeedKind::MissionFollowupSuppressed,
                    &mission,
                    Some("reason=insufficient_progress_for_next_phase decision=skip_low_signal branch=next_phase".to_string()),
                )
                .await;
            control
                .increment_recommendation_metric("suppressed_complete", Some(branch), None)
                .await;
            continue;
        }
        let entity_automation = {
            let entities = control.entities.read().await;
            entities
                .iter()
                .find(|e| e.id == mission.entity_id)
                .map(effective_automation_mode)
                .unwrap_or(AutomationMode::AutoRun)
        };
        if matches!(entity_automation, AutomationMode::Manual) {
            control
                .push_mission_feed(
                    OperatorFeedKind::MissionFollowupSuppressed,
                    &mission,
                    Some("reason=entity_manual_automation decision=skip_auto_create".to_string()),
                )
                .await;
            control
                .increment_recommendation_metric("suppressed_complete", Some(branch), None)
                .await;
            continue;
        }
        let require_human = matches!(entity_automation, AutomationMode::Suggest);
        let latest_completed_title = mission_tasks
            .iter()
            .rev()
            .find(|t| t.status == TaskStatus::Completed)
            .map(|t| trimmed_for_task_title(&t.title, 96));
        let latest_blocked_title = mission_tasks
            .iter()
            .rev()
            .find(|t| t.status == TaskStatus::Failed)
            .map(|t| trimmed_for_task_title(&t.title, 96));
        // Pick the follow-up task's framing based on the mission's specific gap.
        let (title, task_kind, rationale) = if mission.status == MissionStatus::UserInputNeeded {
            (
                format!("Resolve user-input dependency: {}", mission.title),
                "mission-user-input-next-step",
                "Mission explicitly needs user input. Capture decision and unblock execution.",
            )
        } else if task_count == 0 {
            (
                format!("Bootstrap mission execution slice: {}", mission.title),
                "mission-bootstrap-next-step",
                "Mission has no linked tasks; create first concrete execution slice.",
            )
        } else if blocked_count > 0 {
            let focus = latest_blocked_title
                .as_deref()
                .map(|t| format!(" (focus: {t})"))
                .unwrap_or_default();
            (
                format!("Unblock mission dependency: {}{}", mission.title, focus),
                "mission-unblock-next-step",
                "Mission has blocked tasks and no executable next step.",
            )
        } else {
            let focus = latest_completed_title
                .as_deref()
                .map(|t| format!(" (after: {t})"))
                .unwrap_or_default();
            (
                format!(
                    "Define next implementation step: {}{}",
                    mission.title, focus
                ),
                "mission-next-step",
                "Mission has no active executable task; queue the next highest-value step.",
            )
        };
        let new_task = Task {
            id: Uuid::new_v4().to_string(),
            title,
            dept: "General".to_string(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some(mission.entity_id.clone()),
            mission_id: Some(mission.id.clone()),
            priority: Some(mission.priority.clone()),
            requires_human_approval: require_human,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: Some(format!(
                "source=mission_review_loop\nreason=auto_followup\ntask_kind={}\nrationale={}\nobserved_total={}\nobserved_blocked={}\nobserved_completed={}",
                task_kind, rationale, task_count, blocked_count, completed_count
            )),
            task_kind: Some(task_kind.to_string()),
            metadata: None,
            outcome: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        };
        let generic_candidate = new_task
            .title
            .starts_with("Define next implementation step:");
        control.tasks.write().await.push(new_task.clone());
        control
            .push_operator_feed(
                OperatorFeedKind::TaskCreated,
                &new_task,
                Some("source=mission_review_loop decision=auto_create_next_step".to_string()),
            )
            .await;
        control
            .increment_recommendation_metric(
                "auto_created",
                Some(branch),
                new_task.task_kind.as_deref(),
            )
            .await;
        if generic_candidate {
            control
                .increment_mission_metric("generic_candidate.emitted")
                .await;
        }
        let all_tasks = control.tasks.read().await.clone();
        if let Some(reason) =
            mission_autorun_block_reason(&control, config, &mission, &all_tasks).await
        {
            control
                .push_mission_feed(
                    OperatorFeedKind::MissionStalled,
                    &mission,
                    Some(format!(
                        "reason=autorun_blocked\naction=auto_followup_create\nblockers={}\ncreated_task_id={}\ncreated_task_kind={}",
                        reason,
                        new_task.id,
                        new_task.task_kind.as_deref().unwrap_or("unknown")
                    )),
                )
                .await;
        }
        last_autofollowup_at.insert(mission.id.clone(), now);
    }

    last_reported_at.retain(|mission_id, _| active_ids.iter().any(|id| id == mission_id));
    last_autofollowup_at.retain(|mission_id, _| active_ids.iter().any(|id| id == mission_id));
    last_review_due_reported_at
        .retain(|mission_id, _| active_ids.iter().any(|id| id == mission_id));
}

/// Builds a follow-up task title from the interpretation brief's goal plus
/// constraints, truncated to keep the title reasonably sized.
fn build_followup_title(brief: &PromptInterpretationBrief) -> String {
    let mut title = brief.goal.trim().to_string();
    if title.is_empty() {
        title = "Follow-up from interpretation".to_string();
    }
    let c = brief.constraints.trim();
    if !c.is_empty() {
        let extra = if c.len() > 240 {
            // Slice on a char boundary: byte 240 can sit inside a multi-byte
            // char, and slicing there panics the orchestrator tick.
            format!(" | {}", &c[..c.floor_char_boundary(240)])
        } else {
            format!(" | {}", c)
        };
        if title.len() + extra.len() < 480 {
            title.push_str(&extra);
        }
    }
    title
}

/// Resolves which entity a follow-up task belongs to: the brief's explicit
/// entity id, else a name match, else the interpreting task's own entity.
fn resolve_entity_id_for_followup(
    entities: &[Entity],
    brief: &PromptInterpretationBrief,
    interpretation_entity_id: Option<&String>,
) -> Option<String> {
    let hint = brief.entity.trim();
    if hint.is_empty() || hint.eq_ignore_ascii_case("unknown") {
        return interpretation_entity_id.cloned();
    }
    if entities.iter().any(|e| e.id == hint) {
        return Some(hint.to_string());
    }
    let lower = hint.to_lowercase();
    entities
        .iter()
        .find(|e| e.name.trim().to_lowercase() == lower)
        .map(|e| e.id.clone())
        .or_else(|| interpretation_entity_id.cloned())
}

/// One follow-up per interpretation task (`source_task_id` idempotency).
async fn maybe_enqueue_followup_after_interpretation(
    control: Arc<ControlState>,
    interpret_task_id: &str,
) {
    let (result_text, dept, interp_entity_id) = {
        let tasks = control.tasks.read().await;
        let Some(t) = tasks.iter().find(|x| x.id == interpret_task_id) else {
            return;
        };
        if t.task_kind.as_deref() != Some("prompt-interpretation") {
            return;
        }
        let Some(ref raw) = t.result else {
            return;
        };
        (raw.clone(), t.dept.clone(), t.entity_id.clone())
    };

    let brief = match parse_prompt_interpretation_brief(&result_text) {
        Ok(b) => b,
        Err(_) => return,
    };

    let mut next_kind = brief.task_kind.trim().to_lowercase().replace(' ', "-");
    if next_kind.is_empty() || next_kind == "prompt-interpretation" {
        next_kind = "general".to_string();
    }

    let entities = control.entities.read().await;
    let entity_id = resolve_entity_id_for_followup(&entities, &brief, interp_entity_id.as_ref());
    let ent_ref = entity_id
        .as_ref()
        .and_then(|id| entities.iter().find(|e| &e.id == id));
    if should_suppress_interpretation_followup(ent_ref) {
        drop(entities);
        return;
    }
    let suggest_approval = ent_ref
        .map(|e| matches!(effective_automation_mode(e), AutomationMode::Suggest))
        .unwrap_or(false);
    drop(entities);

    let mut tasks = control.tasks.write().await;
    if tasks
        .iter()
        .any(|t| t.source_task_id.as_deref() == Some(interpret_task_id))
    {
        return;
    }

    if next_kind == "entity-enrichment" {
        match &entity_id {
            None => {
                info!(
                    interpret_task_id = %interpret_task_id,
                    "director: skip entity-enrichment follow-up (no entity_id resolved)"
                );
                return;
            }
            Some(eid) => {
                let active = tasks.iter().any(|t| {
                    t.entity_id.as_deref() == Some(eid.as_str())
                        && t.task_kind.as_deref() == Some("entity-enrichment")
                        && (t.status == TaskStatus::Queued || t.status == TaskStatus::InProgress)
                });
                if active {
                    info!(
                        interpret_task_id = %interpret_task_id,
                        entity_id = %eid,
                        "director: skip follow-up enrichment (already queued or running)"
                    );
                    return;
                }
            }
        }
    }

    let title = build_followup_title(&brief);
    let provenance = format!(
        "Spawned from interpretation task {} (suggested_worker={} urgency={})",
        interpret_task_id, brief.suggested_worker, brief.urgency
    );

    let follow = Task {
        id: Uuid::new_v4().to_string(),
        title,
        dept,
        roles: vec![],
        status: TaskStatus::Queued,
        tries: "0".to_string(),
        progress: 0,
        entity_id,
        mission_id: None,
        priority: None,
        requires_human_approval: suggest_approval,
        assigned_director_id: None,
        assigned_agent_id: None,
        connection_id: None,
        external_source: None,
        external_id: None,
        result: Some(provenance),
        task_kind: Some(next_kind),
        metadata: None,
        outcome: None,
        created_at: chrono::Utc::now().to_rfc3339(),
        due_at: None,
        completed_at: None,
        status_changed_at: None,
        source_task_id: Some(interpret_task_id.to_string()),
    };
    info!(
        interpret_task_id = %interpret_task_id,
        follow_task_id = %follow.id,
        task_kind = ?follow.task_kind,
        "director: enqueued follow-up from prompt-interpretation"
    );
    tasks.push(follow.clone());
    drop(tasks);
    control
        .push_operator_feed(OperatorFeedKind::TaskCreated, &follow, None)
        .await;
}

/// Extract the prompt body from a recurring-plan template's `result` field.
/// `create_task` stores user-provided content as `result = "Capture:\n<body>"`,
/// so strip that prefix when present. Falls back to the template's title if empty.
fn recurring_plan_prompt(template: &Task) -> String {
    let stripped = template
        .result
        .as_deref()
        .map(|r| r.strip_prefix("Capture:\n").unwrap_or(r).trim().to_string())
        .filter(|s| !s.is_empty());
    stripped.unwrap_or_else(|| template.title.clone())
}

/// On each orchestrator tick, scan for `recurring-plan` template tasks and spawn one
/// fresh child task per calendar day per template. Children run through the normal
/// heartbeat pickup path.
async fn spawn_recurring_plan_children(control: Arc<ControlState>) {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let templates: Vec<Task> = {
        let tasks = control.tasks.read().await;
        tasks
            .iter()
            .filter(|t| t.task_kind.as_deref() == Some("recurring-plan"))
            .cloned()
            .collect()
    };
    if templates.is_empty() {
        return;
    }

    let mut to_spawn: Vec<Task> = Vec::new();
    {
        let tasks = control.tasks.read().await;
        for tmpl in &templates {
            let already = tasks.iter().any(|t| {
                t.source_task_id.as_deref() == Some(tmpl.id.as_str())
                    && t.created_at.starts_with(&today)
            });
            if already {
                continue;
            }
            let prompt = recurring_plan_prompt(tmpl);
            let child = Task {
                id: Uuid::new_v4().to_string(),
                title: prompt,
                dept: tmpl.dept.clone(),
                roles: tmpl.roles.clone(),
                status: TaskStatus::Queued,
                tries: "0".to_string(),
                progress: 0,
                entity_id: tmpl.entity_id.clone(),
                mission_id: tmpl.mission_id.clone(),
                priority: tmpl.priority.clone(),
                requires_human_approval: false,
                assigned_director_id: None,
                assigned_agent_id: None,
                connection_id: None,
                external_source: None,
                external_id: None,
                result: None,
                task_kind: Some("recurring-instance".to_string()),
                metadata: None,
                outcome: None,
                created_at: Utc::now().to_rfc3339(),
                due_at: None,
                completed_at: None,
                status_changed_at: None,
                source_task_id: Some(tmpl.id.clone()),
            };
            to_spawn.push(child);
        }
    }

    if to_spawn.is_empty() {
        return;
    }

    {
        let mut tasks = control.tasks.write().await;
        for child in &to_spawn {
            tasks.push(child.clone());
        }
    }
    for child in to_spawn {
        info!(
            child_task_id = %child.id,
            source_task_id = ?child.source_task_id,
            "orchestrator: spawned recurring-plan child for today"
        );
        control
            .push_operator_feed(
                OperatorFeedKind::TaskCreated,
                &child,
                Some("Recurring-plan daily instance".to_string()),
            )
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entity() -> Entity {
        Entity {
            id: "ent-42".to_string(),
            name: "Acme Corp".to_string(),
            status: "Active".to_string(),
            dept: "General".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_entity_prefers_brief_id() {
        let e = sample_entity();
        let brief = PromptInterpretationBrief {
            entity: "ent-42".to_string(),
            task_kind: "general".to_string(),
            goal: "".to_string(),
            constraints: "".to_string(),
            suggested_worker: "".to_string(),
            urgency: "".to_string(),
        };
        assert_eq!(
            resolve_entity_id_for_followup(std::slice::from_ref(&e), &brief, None).as_deref(),
            Some("ent-42")
        );
    }

    #[test]
    fn resolve_entity_matches_name() {
        let e = sample_entity();
        let brief = PromptInterpretationBrief {
            entity: "Acme Corp".to_string(),
            task_kind: "".to_string(),
            goal: "".to_string(),
            constraints: "".to_string(),
            suggested_worker: "".to_string(),
            urgency: "".to_string(),
        };
        assert_eq!(
            resolve_entity_id_for_followup(&[e], &brief, None).as_deref(),
            Some("ent-42")
        );
    }

    #[test]
    fn resolve_unknown_falls_back_to_task_entity() {
        let e = sample_entity();
        let brief = PromptInterpretationBrief {
            entity: "unknown".to_string(),
            task_kind: "".to_string(),
            goal: "".to_string(),
            constraints: "".to_string(),
            suggested_worker: "".to_string(),
            urgency: "".to_string(),
        };
        let interp = Some("ent-99".to_string());
        assert_eq!(
            resolve_entity_id_for_followup(&[e], &brief, interp.as_ref()).as_deref(),
            Some("ent-99")
        );
    }

    /// Byte-budget truncation used to slice straight through multi-byte
    /// characters and panic. Every site that cuts user- or provider-supplied
    /// text by bytes gets Arabic (2-byte) and CJK (3-byte) input, shifted by
    /// 0–2 ASCII bytes so each budget lands mid-character in some case.
    #[test]
    fn byte_budget_truncation_never_splits_a_multibyte_char() {
        for body in ["مرحبا بالعالم ".repeat(40), "你好世界".repeat(60)] {
            for prefix in ["", "x", "xx"] {
                let text = format!("{prefix}{body}");
                let brief = PromptInterpretationBrief {
                    entity: "unknown".to_string(),
                    task_kind: "".to_string(),
                    goal: "متابعة".to_string(),
                    constraints: text.clone(),
                    suggested_worker: "".to_string(),
                    urgency: "".to_string(),
                };
                let title = build_followup_title(&brief);
                assert!(title.starts_with("متابعة | "));
                assert!(title.len() <= "متابعة | ".len() + 240);

                let adapter = crate::entity_adapters::user_facing_adapter_error(&text);
                assert!(adapter.ends_with('…'));
                assert!(adapter.len() <= 280 + '…'.len_utf8());

                let probe = crate::llm_provider_probe::truncate_body(&text, 101);
                assert!(probe.ends_with('…'));
                assert!(probe.len() <= 101 + '…'.len_utf8());
            }
        }
    }

    #[test]
    fn cooldown_elapsed_behavior() {
        let now = Instant::now();
        assert!(cooldown_elapsed(None, Duration::from_secs(10), now));
        assert!(!cooldown_elapsed(
            Some(now - Duration::from_secs(5)),
            Duration::from_secs(10),
            now
        ));
        assert!(cooldown_elapsed(
            Some(now - Duration::from_secs(15)),
            Duration::from_secs(10),
            now
        ));
    }

    #[test]
    fn specialized_execution_loops_exclusively_own_their_tasks() {
        let executable = Task {
            task_kind: Some("code".to_string()),
            metadata: Some(serde_json::json!({
                crate::task_executor::META_EXECUTABLE: true,
            })),
            ..Task::default()
        };
        assert_eq!(
            specialized_loop_owner(&executable),
            Some("task_executor_owned")
        );

        let string_flag = Task {
            metadata: Some(serde_json::json!({
                crate::task_executor::META_EXECUTABLE: "TRUE",
            })),
            ..Task::default()
        };
        assert_eq!(
            specialized_loop_owner(&string_flag),
            Some("task_executor_owned")
        );

        let mission_beat = Task {
            task_kind: Some(crate::mission_engine::BEAT_TASK_KIND.to_string()),
            ..Task::default()
        };
        assert_eq!(
            specialized_loop_owner(&mission_beat),
            Some("mission_engine_owned")
        );
    }

    #[test]
    fn ordinary_and_explicitly_non_executable_tasks_stay_director_eligible() {
        assert_eq!(specialized_loop_owner(&Task::default()), None);

        let task = Task {
            metadata: Some(serde_json::json!({
                crate::task_executor::META_EXECUTABLE: false,
            })),
            ..Task::default()
        };
        assert_eq!(specialized_loop_owner(&task), None);
    }

    #[test]
    fn derive_mission_task_counts_edge_cases() {
        let mk = |status: TaskStatus| Task {
            id: Uuid::new_v4().to_string(),
            title: "x".to_string(),
            dept: "General".to_string(),
            roles: vec![],
            status,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some("ent-1".to_string()),
            mission_id: Some("mission-1".to_string()),
            priority: None,
            requires_human_approval: false,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: None,
            task_kind: None,
            metadata: None,
            outcome: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        };
        let empty: Vec<&Task> = vec![];
        assert_eq!(derive_mission_task_counts(&empty), (0, 0, 0, false));

        let blocked_a = mk(TaskStatus::Failed);
        let blocked_b = mk(TaskStatus::Failed);
        assert_eq!(
            derive_mission_task_counts(&[&blocked_a, &blocked_b]),
            (2, 2, 0, false)
        );

        let completed_a = mk(TaskStatus::Completed);
        let completed_b = mk(TaskStatus::Completed);
        assert_eq!(
            derive_mission_task_counts(&[&completed_a, &completed_b]),
            (2, 0, 2, false)
        );

        let queued = mk(TaskStatus::Queued);
        let failed = mk(TaskStatus::Failed);
        assert_eq!(
            derive_mission_task_counts(&[&queued, &failed]),
            (2, 1, 0, true)
        );
    }

    #[test]
    fn has_open_next_step_task_suppresses_duplicates() {
        let open = Task {
            id: "t1".to_string(),
            title: "next".to_string(),
            dept: "General".to_string(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some("e".to_string()),
            mission_id: Some("m1".to_string()),
            priority: None,
            requires_human_approval: false,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: None,
            task_kind: Some("mission-next-step".to_string()),
            metadata: None,
            outcome: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        };
        let closed = Task {
            status: TaskStatus::Completed,
            ..open.clone()
        };
        assert!(has_open_next_step_task(&[open], "m1"));
        assert!(!has_open_next_step_task(&[closed], "m1"));
    }

    #[test]
    fn trimmed_for_task_title_applies_limit() {
        assert_eq!(trimmed_for_task_title("hello", 10), "hello");
        assert_eq!(trimmed_for_task_title("0123456789abcdef", 10), "0123456...");
    }

    #[test]
    fn mission_review_due_respects_override_and_default() {
        let mission = Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "x".to_string(),
            summary: "".to_string(),
            status: MissionStatus::Active,
            priority: "medium".to_string(),
            goal: "".to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_reviewed_at: Some("2026-01-02T00:00:00Z".to_string()),
            review_cadence_hours: Some(48),
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        };
        let now = DateTime::parse_from_rfc3339("2026-01-03T00:00:00Z")
            .expect("valid now")
            .with_timezone(&Utc);
        assert!(!is_review_due(&mission, 24, now));
        let later = DateTime::parse_from_rfc3339("2026-01-05T01:00:00Z")
            .expect("valid later")
            .with_timezone(&Utc);
        assert!(is_review_due(&mission, 24, later));
    }

    #[test]
    fn activity_snapshot_detects_inactivity() {
        let now = DateTime::parse_from_rfc3339("2026-01-05T12:00:00Z")
            .expect("valid now")
            .with_timezone(&Utc);
        let feed = vec![crate::state::OperatorFeedEvent {
            id: "e1".to_string(),
            timestamp: "2026-01-01T12:00:00Z".to_string(),
            kind: OperatorFeedKind::TaskCompleted,
            title: "done".to_string(),
            detail: None,
            task_id: None,
            mission_id: Some("m1".to_string()),
            entity_id: Some("e1".to_string()),
            connection_id: None,
            approval_id: None,
        }];
        let snap = mission_activity_snapshot(&feed, "m1", now);
        assert_eq!(snap.recent_activity_count_24h, 0);
        assert!(snap.inactivity_hours.unwrap_or_default() >= 96);
    }

    #[tokio::test]
    async fn review_loop_happy_path_proposes_when_no_next_task() {
        let control = Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        control.missions.write().await.push(Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Build platform".to_string(),
            summary: "".to_string(),
            status: MissionStatus::Active,
            priority: "high".to_string(),
            goal: "".to_string(),
            owner: None,
            created_by: Some("user".to_string()),
            source: Some("test".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        });
        let mut cfg = AppConfig::default();
        cfg.mission_policy.followup_task_proposal_enabled = true;
        cfg.mission_policy.followup_auto_create_enabled = false;

        let mut reported = HashMap::new();
        let mut created = HashMap::new();
        let mut review_due = HashMap::new();
        report_mission_gaps(
            control.clone(),
            &cfg,
            &mut reported,
            &mut created,
            &mut review_due,
            &None,
        )
        .await;

        let feed = control.operator_feed.read().await.clone();
        assert!(
            feed.iter()
                .any(|e| e.kind == OperatorFeedKind::MissionMissingNextTask)
        );
        let tasks = control.tasks.read().await;
        assert!(tasks.is_empty());
    }

    #[tokio::test]
    async fn review_loop_auto_create_is_idempotent_with_existing_open_task() {
        let control = Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        control.missions.write().await.push(Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Build platform".to_string(),
            summary: "".to_string(),
            status: MissionStatus::Active,
            priority: "high".to_string(),
            goal: "".to_string(),
            owner: None,
            created_by: Some("user".to_string()),
            source: Some("test".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        });
        control.tasks.write().await.push(Task {
            id: "seed".to_string(),
            title: "Mission next step review: Build platform".to_string(),
            dept: "General".to_string(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some("e1".to_string()),
            mission_id: Some("m1".to_string()),
            priority: Some("high".to_string()),
            requires_human_approval: false,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: None,
            task_kind: Some("mission-next-step".to_string()),
            metadata: None,
            outcome: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        });
        let mut cfg = AppConfig::default();
        cfg.mission_policy.followup_task_proposal_enabled = true;
        cfg.mission_policy.followup_auto_create_enabled = true;

        let mut reported = HashMap::new();
        let mut created = HashMap::new();
        let mut review_due = HashMap::new();
        report_mission_gaps(
            control.clone(),
            &cfg,
            &mut reported,
            &mut created,
            &mut review_due,
            &None,
        )
        .await;
        let tasks = control.tasks.read().await;
        let next_step_count = tasks
            .iter()
            .filter(|t| {
                t.mission_id.as_deref() == Some("m1")
                    && t.task_kind.as_deref() == Some("mission-next-step")
            })
            .count();
        assert_eq!(next_step_count, 1);
    }

    #[tokio::test]
    async fn review_loop_user_input_branch_creates_user_input_task() {
        let control = Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        control.missions.write().await.push(Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Legal approval needed".to_string(),
            summary: "".to_string(),
            status: MissionStatus::UserInputNeeded,
            priority: "high".to_string(),
            goal: "".to_string(),
            owner: None,
            created_by: Some("user".to_string()),
            source: Some("test".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        });
        let mut cfg = AppConfig::default();
        cfg.mission_policy.followup_task_proposal_enabled = true;
        cfg.mission_policy.followup_auto_create_enabled = true;
        let mut reported = HashMap::new();
        let mut created = HashMap::new();
        let mut review_due = HashMap::new();
        report_mission_gaps(
            control.clone(),
            &cfg,
            &mut reported,
            &mut created,
            &mut review_due,
            &None,
        )
        .await;
        let tasks = control.tasks.read().await;
        assert!(
            tasks
                .iter()
                .any(|t| t.task_kind.as_deref() == Some("mission-user-input-next-step"))
        );
    }

    #[tokio::test]
    async fn review_loop_demotes_next_phase_when_progress_is_low() {
        let control = Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        control.missions.write().await.push(Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Build platform".to_string(),
            summary: "".to_string(),
            status: MissionStatus::Active,
            priority: "high".to_string(),
            goal: "".to_string(),
            owner: None,
            created_by: Some("user".to_string()),
            source: Some("test".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        });
        control.tasks.write().await.push(Task {
            id: "seed".to_string(),
            title: "Existing incomplete task".to_string(),
            dept: "General".to_string(),
            roles: vec![],
            status: TaskStatus::Failed,
            tries: "0".to_string(),
            progress: 100,
            entity_id: Some("e1".to_string()),
            mission_id: Some("m1".to_string()),
            priority: Some("high".to_string()),
            requires_human_approval: false,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: None,
            task_kind: Some("mission-analysis".to_string()),
            metadata: None,
            outcome: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        });
        let mut cfg = AppConfig::default();
        cfg.mission_policy.followup_task_proposal_enabled = true;
        cfg.mission_policy.followup_auto_create_enabled = true;
        let mut reported = HashMap::new();
        let mut created = HashMap::new();
        let mut review_due = HashMap::new();
        report_mission_gaps(
            control.clone(),
            &cfg,
            &mut reported,
            &mut created,
            &mut review_due,
            &None,
        )
        .await;

        let tasks = control.tasks.read().await;
        let emitted_next_phase = tasks
            .iter()
            .any(|t| t.task_kind.as_deref() == Some("mission-next-step"));
        assert!(!emitted_next_phase);
    }

    #[tokio::test]
    async fn review_loop_marks_user_input_needed_when_inactive() {
        let control = Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        control.missions.write().await.push(Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Delayed legal alignment".to_string(),
            summary: "".to_string(),
            status: MissionStatus::Active,
            priority: "high".to_string(),
            goal: "".to_string(),
            owner: None,
            created_by: Some("user".to_string()),
            source: Some("test".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        });
        control.tasks.write().await.push(Task {
            id: "seed".to_string(),
            title: "Blocked task".to_string(),
            dept: "General".to_string(),
            roles: vec![],
            status: TaskStatus::Failed,
            tries: "0".to_string(),
            progress: 100,
            entity_id: Some("e1".to_string()),
            mission_id: Some("m1".to_string()),
            priority: Some("high".to_string()),
            requires_human_approval: false,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: None,
            task_kind: Some("mission-analysis".to_string()),
            metadata: None,
            outcome: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        });
        control
            .operator_feed
            .write()
            .await
            .push(crate::state::OperatorFeedEvent {
                id: "ev-old".to_string(),
                timestamp: "2025-01-01T00:00:00Z".to_string(),
                kind: OperatorFeedKind::TaskCompleted,
                title: "Old progress".to_string(),
                detail: None,
                task_id: Some("old-task".to_string()),
                mission_id: Some("m1".to_string()),
                entity_id: Some("e1".to_string()),
                connection_id: None,
                approval_id: None,
            });
        let mut cfg = AppConfig::default();
        cfg.mission_policy.followup_task_proposal_enabled = true;
        cfg.mission_policy.followup_auto_create_enabled = false;
        let mut reported = HashMap::new();
        let mut created = HashMap::new();
        let mut review_due = HashMap::new();
        report_mission_gaps(
            control.clone(),
            &cfg,
            &mut reported,
            &mut created,
            &mut review_due,
            &None,
        )
        .await;

        let missions = control.missions.read().await;
        let updated = missions
            .iter()
            .find(|m| m.id == "m1")
            .expect("mission exists");
        assert_eq!(updated.status, MissionStatus::UserInputNeeded);
        drop(missions);
        let feed = control.operator_feed.read().await;
        assert!(
            feed.iter()
                .any(|e| e.kind == OperatorFeedKind::MissionUserInputNeeded)
        );
    }

    #[tokio::test]
    async fn review_loop_emission_respects_cooldown_and_reemits_after_window() {
        let control = Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));
        control.missions.write().await.push(Mission {
            id: "m1".to_string(),
            entity_id: "e1".to_string(),
            title: "Platform rollout".to_string(),
            summary: "".to_string(),
            status: MissionStatus::Active,
            priority: "high".to_string(),
            goal: "".to_string(),
            owner: None,
            created_by: Some("user".to_string()),
            source: Some("test".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: vec![],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        });
        let mut cfg = AppConfig::default();
        cfg.mission_policy.followup_task_proposal_enabled = true;
        cfg.mission_policy.followup_auto_create_enabled = false;
        let mut reported = HashMap::new();
        let mut created = HashMap::new();
        let mut review_due = HashMap::new();

        report_mission_gaps(
            control.clone(),
            &cfg,
            &mut reported,
            &mut created,
            &mut review_due,
            &None,
        )
        .await;

        let first_emitted = control
            .operator_feed
            .read()
            .await
            .iter()
            .filter(|e| e.kind == OperatorFeedKind::MissionMissingNextTask)
            .count();
        assert_eq!(first_emitted, 1);

        // Immediate second run should be suppressed by cooldown map.
        report_mission_gaps(
            control.clone(),
            &cfg,
            &mut reported,
            &mut created,
            &mut review_due,
            &None,
        )
        .await;
        let second_emitted = control
            .operator_feed
            .read()
            .await
            .iter()
            .filter(|e| e.kind == OperatorFeedKind::MissionMissingNextTask)
            .count();
        assert_eq!(second_emitted, 1);

        // Force cooldown window elapsed and verify re-emission occurs.
        reported.insert(
            "m1".to_string(),
            Instant::now() - Duration::from_secs(MISSION_GAP_REPORT_COOLDOWN_SECS + 1),
        );
        report_mission_gaps(
            control.clone(),
            &cfg,
            &mut reported,
            &mut created,
            &mut review_due,
            &None,
        )
        .await;
        let third_emitted = control
            .operator_feed
            .read()
            .await
            .iter()
            .filter(|e| e.kind == OperatorFeedKind::MissionMissingNextTask)
            .count();
        assert_eq!(third_emitted, 2);
    }
}
