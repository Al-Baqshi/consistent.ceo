//! Tasks the planner marked executable run themselves.
//!
//! Until now, autonomous execution only happened if the operator manually
//! attached a mission engine to a mission. That is one setup step too many for
//! the thing the product is actually for: the operator states a goal, the
//! planner breaks it into tasks, and the ones that are plain code/file work get
//! done without anybody asking again.
//!
//! ### Shape
//!
//! [`TaskExecutorTicker`] is deliberately the same shape as
//! [`crate::mission_engine::MissionEngineTicker`]: poll, pick what is due,
//! create an [`ExternalAgentRun`] row, hand it to
//! [`crate::agent_sessions::run_claude_session`], parse a required status
//! trailer, then write the task and the operator feed. Nothing new is invented
//! here — the execution engine, the cwd jail, the run row, the feed all exist.
//!
//! ### What keeps this safe
//!
//! The run is unattended with `--dangerously-skip-permissions`, so the
//! containment boundary is the **directory**: `resolve_entity_agent_run_cwd`
//! resolves the entity's local path (or its vault folder) and nothing runs
//! without one. A task whose entity has no resolvable root is marked and
//! *never retried* — a retry loop against a missing directory is how an
//! unattended system turns a misconfiguration into a runaway.
//!
//! And before any of that: a directory the *gateway* invented is not a
//! workspace the operator chose. [`crate::entity_workspace`] tells the two
//! apart, and an entity nobody has answered for gets a question
//! ([`WorkspaceGate::Ask`]) instead of a coding agent — once per entity, with
//! the task left Queued so answering releases it.
//!
//! Outward-facing work is excluded by construction: the planner sets
//! `requires_human_approval` on anything that talks to the world, and that flag
//! disqualifies a task here, routing it through the existing approval gauntlet
//! instead.
//!
//! Only **one** executor run happens at a time, globally. The mission engine is
//! already competing for the operator's single Claude session window; stacking
//! more would just make everything slower and noisier.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use chrono::{DateTime, Utc};

use crate::agent_sessions::{
    DEFAULT_STALL_TIMEOUT_SECS, PermissionMode, SessionOutcome, SessionRequest, run_claude_session,
};
use crate::entity_runtime::effective_automation_mode;
use crate::entity_workspace::{WorkspaceStatus, workspace_status};
use crate::mission_engine::{
    META_ACTIVE_RUN_ID, next_beat_after_rate_limit, parse_trailer_prefixed,
};
use crate::state::{
    AutomationMode, ControlState, Entity, ExternalAgentRun, Mission, OperatorFeedEvent,
    OperatorFeedKind, Task, TaskOutcome, TaskStatus, set_task_status_by_loop, task_meta_str,
};

/// Metadata key the planner writes when a task is plain code/file work an
/// unattended coding agent can finish on its own.
pub const META_EXECUTABLE: &str = "executable";
/// Set the moment this task is handed to the executor. Its presence is what
/// stops a task being picked up while a claim stands — cleared when a run was
/// cut short by the usage window or crashed outright, both genuinely worth
/// retrying.
pub const META_AUTO_RUN_ATTEMPTED: &str = "autoRunAttempted";
/// How many times the executor has started this task (integer as string).
/// Bounds automatic retries so a broken task can never loop unattended.
pub const META_ATTEMPT_COUNT: &str = "autoRunAttempts";
/// RFC3339 instant before which the task must not be picked up again.
pub const META_NEXT_ATTEMPT_AT: &str = "nextAttemptAt";
/// The last run this task produced, kept after the run finishes.
///
/// `META_ACTIVE_RUN_ID` is cleared on completion — which is exactly when the
/// run's artifacts (browser screenshots) become interesting to look at. Without
/// a durable pointer, a finished task has no way to say which run made its
/// files, and the UI has to go hunting through the activity log for it.
pub const META_LAST_RUN_ID: &str = "lastRunId";
/// Stamped by the approval endpoint the moment a human approves the task's
/// linked approval request. An explicit human "go" outranks the entity's
/// automation-mode default.
pub const META_APPROVED_AT: &str = "approvedAt";
/// Free-form task detail written by the planner.
pub const META_DESCRIPTION: &str = "description";

/// Marker value recorded when the entity has no local directory to work in.
/// Distinct from a timestamp so the reason survives in the task row.
pub const ATTEMPT_NO_CWD: &str = "no_cwd";

/// How many times the executor may start a task on its own. The second attempt
/// exists for runs that crashed (CLI died, stall timeout) — a `blocked`
/// verdict from a finished run never auto-retries, it needs a human.
pub const MAX_AUTO_ATTEMPTS: u32 = 2;
/// Backoff before an automatic retry of a crashed run.
const CRASH_RETRY_BACKOFF_MINS: i64 = 10;

/// How many times the executor has started this task.
pub fn attempt_count(task: &Task) -> u32 {
    meta_marker(task, META_ATTEMPT_COUNT)
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0)
}

/// What the executor does about an entity whose workspace nobody chose.
///
/// The old behaviour was to run anyway, in whatever folder
/// `resolve_entity_agent_run_cwd` produced — which, with nothing configured, is
/// a notes folder the gateway created itself. An unattended coding agent
/// building a product into `~/.consistent-world/vault/...` is not a workspace
/// decision, it is the absence of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceGate {
    /// Somewhere real, or the operator explicitly said notes-only.
    Proceed,
    /// Ask, then leave the task Queued so answering it releases the work.
    Ask,
    /// Already asked. Keep waiting silently — the question is a question, not
    /// a drumbeat.
    WaitQuietly,
}

/// Decide the gate from a workspace status and whether the operator was already
/// asked. Pure so the "asks instead of running" guarantee is a unit test.
pub fn workspace_gate(status: &WorkspaceStatus, already_asked: bool) -> WorkspaceGate {
    if !status.needs_decision {
        return WorkspaceGate::Proceed;
    }
    if already_asked {
        WorkspaceGate::WaitQuietly
    } else {
        WorkspaceGate::Ask
    }
}

/// The trailer the executed task must end with. Its own prefix, not the mission
/// engine's, so a task run and a mission beat can never be confused.
pub const TASK_STATUS_PREFIX: &str = "TASK_STATUS";

/// What the agent said about the task at the end of its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRunStatus {
    Done,
    Blocked,
}

/// Read a metadata string, treating the empty string as absent.
///
/// Markers are *cleared* by writing `""` (the same way the mission engine
/// clears its active-run id), so "set" has to mean "set to something".
fn meta_marker(task: &Task, key: &str) -> Option<String> {
    task_meta_str(task, key).filter(|v| !v.trim().is_empty())
}

fn meta_flag_true(task: &Task, key: &str) -> bool {
    match task.metadata.as_ref().and_then(|m| m.get(key)) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => false,
    }
}

/// Is this task one the executor may pick up on its own, at `now`?
///
/// Pure so the whole safety matrix is testable without a clock, a database, or
/// a Claude process. Every clause here is a reason *not* to run: the default
/// posture is to leave a task alone.
pub fn task_is_auto_executable_at(task: &Task, entity: &Entity, now: DateTime<Utc>) -> bool {
    // The planner marks what an unattended coding agent can actually finish.
    // A missing key is a "no" — tasks created before the planner existed, or by
    // any other path, are not silently eligible.
    if !meta_flag_true(task, META_EXECUTABLE) {
        return false;
    }
    if task.status != TaskStatus::Queued {
        return false;
    }
    // This is the whole outward-facing safety story: anything that touches the
    // world carries this flag and goes through the approval gauntlet instead.
    if task.requires_human_approval {
        return false;
    }
    // Bounded attempts. Unattended work that retries itself without limit is
    // how a broken task becomes an infinite loop nobody is watching; only a
    // crashed run gets its marker cleared for one more try.
    if attempt_count(task) >= MAX_AUTO_ATTEMPTS {
        return false;
    }
    if meta_marker(task, META_AUTO_RUN_ATTEMPTED).is_some() {
        return false;
    }
    // An entity kept out of auto-run still executes work its operator
    // explicitly approved: the approval stamp is a stronger signal than the
    // automation-mode default.
    if !matches!(effective_automation_mode(entity), AutomationMode::AutoRun)
        && meta_marker(task, META_APPROVED_AT).is_none()
    {
        return false;
    }
    if let Some(raw) = meta_marker(task, META_NEXT_ATTEMPT_AT)
        && let Ok(at) = DateTime::parse_from_rfc3339(&raw)
        && at.with_timezone(&Utc) > now
    {
        return false;
    }
    true
}

/// [`task_is_auto_executable_at`] against the wall clock.
pub fn task_is_auto_executable(task: &Task, entity: &Entity) -> bool {
    task_is_auto_executable_at(task, entity, Utc::now())
}

/// Pull `TASK_STATUS: done|blocked — <summary>` out of a run's output.
///
/// The bias is the opposite of the mission engine's: silence there means "keep
/// going", silence here means **blocked**. A task is a single shot, so an
/// unparseable reply is a task nobody can vouch for — surfacing it for review
/// is cheap, and a false "done" is what makes an operator stop trusting the
/// whole system.
pub fn parse_task_status_trailer(output: &str) -> (TaskRunStatus, Option<String>) {
    match parse_trailer_prefixed(output, TASK_STATUS_PREFIX) {
        Some((word, summary)) => {
            let done = word.starts_with("done")
                || word.starts_with("complete")
                || word.starts_with("finish");
            (
                if done {
                    TaskRunStatus::Done
                } else {
                    TaskRunStatus::Blocked
                },
                summary,
            )
        }
        None => (TaskRunStatus::Blocked, None),
    }
}

/// The instruction handed to the agent for one task.
pub fn build_task_prompt(task: &Task, mission: Option<&Mission>, skills: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str(
        "You are completing one task for the operator, unattended. Nobody is watching, \
         so finish it or say precisely why you cannot.\n\n",
    );
    s.push_str(&format!("TASK: {}\n", task.title.trim()));
    let description = meta_marker(task, META_DESCRIPTION)
        .or_else(|| task.result.clone().filter(|r| !r.trim().is_empty()));
    if let Some(detail) = description {
        s.push_str(&format!("DETAILS: {}\n", detail.trim()));
    }
    if let Some(mission) = mission {
        s.push_str(&format!("\nPARENT MISSION: {}\n", mission.title.trim()));
        if !mission.goal.trim().is_empty() {
            s.push_str(&format!("MISSION GOAL: {}\n", mission.goal.trim()));
        }
    }
    let skills = skills.map(str::trim).filter(|b| !b.is_empty());
    if let Some(block) = skills {
        s.push('\n');
        s.push_str(block);
        s.push('\n');
    }
    s.push_str(
        "\nHOW TO WORK:\n\
         1. Work only inside the current project directory. Never read or write files \
         outside it, and never run commands that reach beyond it.",
    );
    if skills.is_some() {
        s.push_str(" The one exception: you may read the skill files listed above.");
    }
    s.push_str(
        "\n\
         2. Check the current state first (`git status`, the relevant files) before changing anything.\n\
         3. Do the smallest complete change that finishes this task, and leave the tree working — \
         never half-migrated.\n\
         4. Do not send anything outward (email, posts, messages, deploys). Anything outward-facing \
         belongs to the operator's approval queue, not to you.\n",
    );
    s.push_str(&format!(
        "\nEnd your reply with exactly one line:\n\
         {TASK_STATUS_PREFIX}: done|blocked — <one sentence>\n\
         Use `done` only if the task is actually finished. Use `blocked` if you need a \
         decision, a credential, or anything outside this directory.\n"
    ));
    s
}

/// Everything a finished run changes about the task, decided without touching
/// any shared state so all three terminal outcomes can be unit-tested.
#[derive(Debug, Clone)]
pub struct TaskRunUpdate {
    pub status: TaskStatus,
    pub outcome: Option<TaskOutcome>,
    pub progress: Option<u8>,
    /// Written to `task.result` and used as the feed detail.
    pub summary: String,
    /// When set, the task is scheduled for another attempt at this instant.
    pub next_attempt_at: Option<String>,
    /// Retryable endings (spent usage window, crashed run) release the claim
    /// marker; verdicts keep it so a task is never executed past its attempts.
    pub clear_attempt_marker: bool,
    /// A spent usage window is not a verdict on the work *or* an attempt: the
    /// task gets its attempt count back too.
    pub refund_attempt: bool,
    pub feed_kind: OperatorFeedKind,
    pub headline: &'static str,
}

/// Decide what a finished run means for the task, given how many attempts the
/// task has consumed (including the run being judged).
///
/// Four outcomes: the window ran out (try again later, no verdict, attempt
/// refunded), the agent reported `done`, the CLI crashed with attempts to
/// spare (one automatic retry after a backoff), or anything else at all — a
/// `blocked` trailer, a missing trailer, a crash on the last attempt — which
/// lands on the operator's desk.
pub fn plan_task_run_update(
    outcome: &SessionOutcome,
    attempts: u32,
    now: DateTime<Utc>,
) -> TaskRunUpdate {
    if outcome.rate_limited {
        let next = next_beat_after_rate_limit(now, outcome.resets_at);
        let detail = match DateTime::parse_from_rfc3339(&next) {
            Ok(dt) => format!(
                "Usage window spent. Retrying automatically at {}.",
                dt.format("%H:%M on %d %b")
            ),
            Err(_) => "Usage window spent. Retrying automatically.".to_string(),
        };
        return TaskRunUpdate {
            // Back where it started: a spent window is not a verdict on the work.
            status: TaskStatus::Queued,
            outcome: None,
            progress: None,
            summary: detail,
            next_attempt_at: Some(next),
            clear_attempt_marker: true,
            refund_attempt: true,
            feed_kind: OperatorFeedKind::AgentRunCompleted,
            headline: "Waiting on your Claude window",
        };
    }

    let (status, summary) = parse_task_status_trailer(&outcome.output);
    let run_failed = outcome.status != "completed";
    if status == TaskRunStatus::Done && !run_failed {
        return TaskRunUpdate {
            status: TaskStatus::Completed,
            outcome: Some(TaskOutcome::Completed),
            progress: Some(100),
            summary: summary.unwrap_or_else(|| "Task completed.".to_string()),
            next_attempt_at: None,
            clear_attempt_marker: false,
            refund_attempt: false,
            feed_kind: OperatorFeedKind::AgentRunCompleted,
            headline: "Executed",
        };
    }

    // A crashed CLI is not a verdict either — the agent never got to say
    // anything. One automatic retry after a backoff; a crash on the final
    // attempt falls through to the operator's desk like any other failure.
    if run_failed && attempts < MAX_AUTO_ATTEMPTS {
        let next = now + chrono::Duration::minutes(CRASH_RETRY_BACKOFF_MINS);
        return TaskRunUpdate {
            status: TaskStatus::Queued,
            outcome: None,
            progress: None,
            summary: format!(
                "The run ended without finishing ({}). Retrying automatically in {} minutes.",
                outcome
                    .error
                    .as_deref()
                    .filter(|e| !e.trim().is_empty())
                    .unwrap_or(&outcome.status),
                CRASH_RETRY_BACKOFF_MINS
            ),
            next_attempt_at: Some(next.to_rfc3339()),
            clear_attempt_marker: true,
            refund_attempt: false,
            feed_kind: OperatorFeedKind::AgentRunCompleted,
            headline: "Retrying after a failed run",
        };
    }

    let reason = summary
        .or_else(|| outcome.error.clone())
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| {
            "The agent finished without reporting a task status; needs a look.".to_string()
        });
    TaskRunUpdate {
        status: TaskStatus::Failed,
        outcome: Some(TaskOutcome::NeedsReview),
        progress: None,
        summary: reason,
        next_attempt_at: None,
        clear_attempt_marker: false,
        refund_attempt: false,
        feed_kind: OperatorFeedKind::TaskBlocked,
        headline: "Blocked",
    }
}

/// Downgrade a `done` verdict that no on-disk evidence backs. The trailer is
/// what the agent *said*; the pack is what it *did* — and a done claim from a
/// jail directory showing zero commits, zero diff, and zero new dirty files is
/// a claim nobody can vouch for. Browser actions are exempt: their work
/// legitimately leaves no file trace.
pub fn apply_evidence_to_update(
    update: &mut TaskRunUpdate,
    task: &Task,
    pack: &crate::evidence::EvidencePack,
) {
    if update.status != TaskStatus::Completed || !pack.shows_no_observable_change() {
        return;
    }
    if task_meta_str(task, "actionKind").as_deref() == Some("browser") {
        return;
    }
    update.status = TaskStatus::Failed;
    update.outcome = Some(TaskOutcome::NeedsReview);
    update.progress = None;
    update.summary = format!(
        "Reported done, but nothing in the project directory changed — needs a look. \
         The agent said: {}",
        update.summary
    );
    update.feed_kind = OperatorFeedKind::TaskBlocked;
    update.headline = "Unverified";
}

/// Close a task row from a finished executor-owned run: status, outcome,
/// result, meta markers, write-through, and the operator-feed event. Shared
/// with the restart-resume path so a resumed run can never leave its task
/// `InProgress` forever.
pub async fn apply_run_outcome_to_task(
    control: &Arc<ControlState>,
    task_id: &str,
    outcome: &SessionOutcome,
    evidence: Option<&crate::evidence::EvidencePack>,
) {
    let completed = {
        let mut tasks = control.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
            // The operator's stop button outranks the run. A task cancelled
            // while its run was in flight must stay cancelled: without this,
            // the finishing run rewrites the status, and the crash-retry
            // branch can put it back on Queued and launch a brand-new
            // unattended run ten minutes after the human said stop.
            if t.status == TaskStatus::Cancelled {
                let note = format!(
                    "Run finished after this task was cancelled; result kept, task left cancelled.\n{}",
                    outcome.output.trim()
                );
                t.result = Some(note.chars().take(2000).collect());
                write_meta(t, &[(META_ACTIVE_RUN_ID, "")]);
                let cancelled = t.clone();
                drop(tasks);
                control.persist_task_row(&cancelled).await;
                return;
            }
            let attempts = attempt_count(t);
            let mut update = plan_task_run_update(outcome, attempts, Utc::now());
            if let Some(pack) = evidence {
                apply_evidence_to_update(&mut update, t, pack);
                if let Some(v) = serde_json::to_value(pack).ok()
                    && let Some(obj) = t
                        .metadata
                        .get_or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                {
                    obj.insert("evidence".to_string(), v);
                }
            }
            // The `Cancelled` early-return above already covers the one case this
            // refuses, and covers it better — it keeps the run's output with a note
            // explaining why the task stayed cancelled. Calling the loop-side setter
            // anyway states the actor, and keeps this correct if that guard ever moves.
            if !set_task_status_by_loop(t, update.status) {
                tracing::warn!(
                    task_id,
                    "refused to move a cancelled task from a loop; leaving it cancelled"
                );
                return;
            }
            if let Some(progress) = update.progress {
                t.progress = progress;
            }
            if update.outcome.is_some() {
                t.outcome = update.outcome;
            }
            t.result = Some(update.summary.clone());
            let refunded = attempts.saturating_sub(1).to_string();
            // Carry the finished run forward before clearing the active
            // pointer, so the task still knows which run produced its output.
            let finished_run = meta_marker(t, META_ACTIVE_RUN_ID).unwrap_or_default();
            let mut pairs: Vec<(&str, &str)> = vec![(META_ACTIVE_RUN_ID, "")];
            if !finished_run.is_empty() {
                pairs.push((META_LAST_RUN_ID, finished_run.as_str()));
            }
            if update.clear_attempt_marker {
                pairs.push((META_AUTO_RUN_ATTEMPTED, ""));
            }
            if update.refund_attempt {
                pairs.push((META_ATTEMPT_COUNT, refunded.as_str()));
            }
            if let Some(next) = update.next_attempt_at.as_deref() {
                pairs.push((META_NEXT_ATTEMPT_AT, next));
            }
            write_meta(t, &pairs);
            Some((t.clone(), update))
        } else {
            None
        }
    };
    let Some((t, update)) = completed else {
        return;
    };
    control.persist_task_row(&t).await;
    control
        .append_operator_feed_event(OperatorFeedEvent {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            kind: update.feed_kind,
            task_id: Some(t.id.clone()),
            mission_id: t.mission_id.clone(),
            entity_id: t.entity_id.clone(),
            connection_id: None,
            title: format!("{}: {}", update.headline, t.title),
            detail: Some(update.summary),
            approval_id: None,
        })
        .await;
}

/// Polls for planner-marked executable tasks and starts at most one unattended
/// Claude Code run at a time.
/// Run the critic over a finished builder run, when the task belongs to a
/// loop-enabled mission and the builder claimed success.
///
/// Reuses the existing review spine (`agent_review`): `start_review` creates
/// the linked reviewer run with its PASS / NEEDS_REVIEW contract, the reviewer
/// executes in the *same directory* as the work it is judging (so it can read
/// the diff rather than take the builder's word), and `finish_review`
/// downgrades the task to `NeedsReview` on a failed verdict — which is exactly
/// what makes the replanner queue a fix next iteration.
async fn maybe_review_loop_run(
    control: &Arc<ControlState>,
    run_id: &str,
    task_id: &str,
    pack: &crate::evidence::EvidencePack,
) {
    // Only successful runs are worth reviewing: a blocked task already needs a
    // human, and reviewing it would spend a second session to say so again.
    let mission_id = {
        let tasks = control.tasks.read().await;
        let Some(task) = tasks.iter().find(|t| t.id == task_id) else {
            return;
        };
        if task.status != TaskStatus::Completed {
            return;
        }
        task.mission_id.clone()
    };
    let Some(mission_id) = mission_id else {
        return;
    };
    let critic_director_id = {
        let missions = control.missions.read().await;
        let Some(mission) = missions
            .iter()
            .find(|m| m.id == mission_id && m.loop_config.is_some())
        else {
            return;
        };
        mission
            .loop_config
            .as_ref()
            .and_then(|c| c.critic_director_id.clone())
    };
    let Some(reviewer) = crate::agent_review::start_review(control, run_id, "claude").await else {
        return;
    };
    // Hand the reviewer the evidence, so its verdict is grounded in what
    // changed on disk rather than in the builder's prose. A mission that named
    // a critic director gets that director's standards; otherwise the generic
    // reviewer persona applies.
    let persona = match critic_director_id.as_deref() {
        Some(id) => resolve_critic_persona(control, id).await,
        None => None,
    };
    let prompt = match persona {
        Some(p) => format!(
            "{p}\n\n---\n\n{}\n\n## Evidence from the working directory\n{}\n",
            reviewer.prompt,
            evidence_summary(pack)
        ),
        None => format!(
            "{}\n\n## Evidence from the working directory\n{}\n",
            reviewer.prompt,
            evidence_summary(pack)
        ),
    };
    let outcome = run_claude_session(
        control.clone(),
        SessionRequest {
            run_id: reviewer.id.clone(),
            prompt,
            cwd: reviewer.cwd.clone(),
            permission_mode: PermissionMode::BypassPermissions,
            max_turns: None,
            resume_session_id: None,
            stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
            mcp_config_path: None,
        },
    )
    .await;
    crate::agent_review::finish_review(control, &reviewer.id, &outcome.output, &outcome.status)
        .await;
    // The reviewer is loop spend too — a critic that costs more than the
    // builder must not be invisible to the budget.
    crate::mission_replanner::accrue_loop_cost(
        control,
        &mission_id,
        outcome.cost_usd.unwrap_or(0.0),
    )
    .await;
}

/// The named critic's own prompt, when the mission pointed at one and the
/// director registry is attached. A missing director is not an error: the
/// review still happens, with the generic reviewer persona.
async fn resolve_critic_persona(control: &Arc<ControlState>, director_id: &str) -> Option<String> {
    let store = control.directors.get()?;
    let director = store.get(director_id).await.ok().flatten()?;
    let prompt = director.prompt.trim();
    (!prompt.is_empty()).then(|| prompt.to_string())
}

fn evidence_summary(pack: &crate::evidence::EvidencePack) -> String {
    if !pack.is_git {
        return "The working directory is not a git repository, so file-level evidence is \
                unavailable. Judge the output on its own terms."
            .to_string();
    }
    format!(
        "commits: {}\nfiles changed: {}\ninsertions: {}\ndeletions: {}\nuncommitted changes: {}",
        pack.commits,
        pack.files_changed,
        pack.insertions,
        pack.deletions,
        if pack.dirty_delta.is_empty() {
            "none".to_string()
        } else {
            pack.dirty_delta.join(", ")
        }
    )
}

/// Materialise the browser MCP config for one run. Written under the
/// gateway's own directory (not the jail) so the task cannot rewrite the
/// policy it is about to run under.
async fn write_browser_mcp_config(run_id: &str) -> Option<String> {
    let binary = std::env::current_exe().ok()?;
    let home = crate::config::expand_tilde_path("~/.consistent-world");
    let profile_dir = format!("{home}/browser-profile");
    let artifact_dir = format!("{home}/artifacts/{run_id}");
    let config_dir = format!("{home}/mcp");
    tokio::fs::create_dir_all(&config_dir).await.ok()?;
    tokio::fs::create_dir_all(&profile_dir).await.ok()?;
    tokio::fs::create_dir_all(&artifact_dir).await.ok()?;
    // Per-run config: the artifact directory differs per run, so a shared file
    // would hand one run's screenshots to the next.
    let path = format!("{config_dir}/browser-{run_id}.json");
    // Headful: the operator signs into sites in this profile once, by hand,
    // and those sessions are what make later unattended runs useful.
    let config = crate::mcp_browser::mcp_config_json(
        &binary.to_string_lossy(),
        &profile_dir,
        &artifact_dir,
        false,
    );
    tokio::fs::write(&path, serde_json::to_vec_pretty(&config).ok()?)
        .await
        .ok()?;
    Some(path)
}

pub struct TaskExecutorTicker {
    control: Arc<ControlState>,
    poll_secs: u64,
    /// One executor run at a time, globally.
    in_flight: Arc<AtomicBool>,
    /// Skills offered to each run (index + file paths in the prompt). The run
    /// is `bypassPermissions`, so the agent can read them where they live.
    skills: Option<Arc<crate::skills::SkillLibrary>>,
}

/// Releases the single-flight slot even when the run future panics or is
/// dropped mid-await; a leaked slot would wedge the executor for the process
/// lifetime.
struct InFlightGuard(Arc<AtomicBool>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl TaskExecutorTicker {
    pub fn new(control: Arc<ControlState>, poll_secs: u64) -> Arc<Self> {
        Self::with_skills(control, poll_secs, None)
    }

    pub fn with_skills(
        control: Arc<ControlState>,
        poll_secs: u64,
        skills: Option<Arc<crate::skills::SkillLibrary>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            control,
            poll_secs: poll_secs.max(5),
            in_flight: Arc::new(AtomicBool::new(false)),
            skills,
        })
    }

    /// The agent-session skills block for this entity, or `None` when there
    /// is no library or nothing active.
    async fn skills_block(&self, entity: &Entity) -> Option<String> {
        let library = self.skills.clone()?;
        let workspace_id = crate::capture::entity_workspace_id(entity).to_string();
        let entity_id = entity.id.clone();
        tokio::task::spawn_blocking(move || {
            crate::skills::render_agent_skills_index(
                &library.agent_skill_paths(&workspace_id, Some(&entity_id)),
            )
        })
        .await
        .ok()
        .flatten()
    }

    /// Poll loop: check for eligible tasks every `poll_secs`.
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(self.poll_secs));
        // A laptop asleep for six hours should wake to one run, not six.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if self.run_one_tick(Utc::now()).await > 0 {
                tracing::info!("task executor: run started");
            }
        }
    }

    /// Start at most one eligible task at `now`. Returns how many were started.
    pub async fn run_one_tick(&self, now: DateTime<Utc>) -> usize {
        if self.in_flight.load(Ordering::SeqCst) {
            return 0;
        }
        let entities: Vec<Entity> = self.control.entities.read().await.clone();
        let candidates: Vec<(Task, Entity)> = {
            let tasks = self.control.tasks.read().await;
            tasks
                .iter()
                .filter_map(|t| {
                    let entity_id = t.entity_id.as_deref()?;
                    let entity = entities.iter().find(|e| e.id == entity_id)?;
                    task_is_auto_executable_at(t, entity, now).then(|| (t.clone(), entity.clone()))
                })
                .collect()
        };

        for (task, entity) in candidates {
            // A run still holding a live process would fight this one over the
            // same files.
            if let Some(active) = meta_marker(&task, META_ACTIVE_RUN_ID)
                && self.control.agent_processes.is_running(&active).await
            {
                continue;
            }
            // Before anything else: has anybody actually said where this
            // entity's work happens? If not, ask — and leave the task Queued so
            // the answer releases it, rather than burning it on a guess.
            let status = workspace_status(&entity);
            match workspace_gate(&status, status.asked_at.is_some()) {
                WorkspaceGate::Proceed => {}
                WorkspaceGate::Ask => {
                    self.ask_where_this_lives(&task, &entity, &status, now)
                        .await;
                    continue;
                }
                WorkspaceGate::WaitQuietly => continue,
            }
            // The jail is the directory. No directory, no run — ever, for this
            // task: retrying a missing path just burns the usage window.
            let Ok((cwd, _roots)) =
                crate::control_api::resolve_entity_agent_run_cwd(&entity, None).await
            else {
                self.set_meta(&task.id, &[(META_AUTO_RUN_ATTEMPTED, ATTEMPT_NO_CWD)])
                    .await;
                self.push_task_feed(
                    &task,
                    OperatorFeedKind::TaskBlocked,
                    format!("Blocked: {}", task.title),
                    format!(
                        "Cannot run this task automatically: '{}' has no local project directory \
                         configured. Set the entity's local path and re-queue the task.",
                        entity.name
                    ),
                )
                .await;
                continue;
            };
            if self
                .in_flight
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return 0;
            }
            if self
                .start_run(&task, &entity, cwd.to_string_lossy().to_string(), now)
                .await
            {
                return 1;
            }
            // The task moved on between the scan and the claim; give the slot
            // back rather than blocking every later tick.
            self.in_flight.store(false, Ordering::SeqCst);
        }
        0
    }

    /// Claim the task and start its run. `false` means the claim lost a race
    /// and nothing was started.
    async fn start_run(
        &self,
        task: &Task,
        entity: &Entity,
        cwd: String,
        now: DateTime<Utc>,
    ) -> bool {
        let mission = match task.mission_id.as_deref() {
            Some(id) => {
                let missions = self.control.missions.read().await;
                missions.iter().find(|m| m.id == id).cloned()
            }
            None => None,
        };
        let skills = self.skills_block(entity).await;
        let prompt = build_task_prompt(task, mission.as_ref(), skills.as_deref());
        let run_id = uuid::Uuid::new_v4().to_string();

        // Claim the task first, re-checking under the write lock: the
        // orchestrator picks up queued tasks too, and the scan above is a
        // snapshot. An InProgress task is not picked up again by anyone, and
        // the attempt marker is what makes "run once" true even across a
        // restart. Deliberately no `startedAt` — that key is what arms the
        // 15-minute stuck-task watchdog, and a Claude run is meant to be long.
        let claimed = {
            let mut tasks = self.control.tasks.write().await;
            let Some(t) = tasks.iter_mut().find(|t| t.id == task.id) else {
                return false;
            };
            if t.status != TaskStatus::Queued
                || meta_marker(t, META_AUTO_RUN_ATTEMPTED).is_some()
                || attempt_count(t) >= MAX_AUTO_ATTEMPTS
            {
                return false;
            }
            if !set_task_status_by_loop(t, TaskStatus::InProgress) {
                return false;
            }
            let next_count = (attempt_count(t) + 1).to_string();
            write_meta(
                t,
                &[
                    (META_ACTIVE_RUN_ID, run_id.as_str()),
                    (META_AUTO_RUN_ATTEMPTED, &now.to_rfc3339()),
                    (META_ATTEMPT_COUNT, next_count.as_str()),
                    (META_NEXT_ATTEMPT_AT, ""),
                ],
            );
            t.clone()
        };
        // The claim must survive a crash: relying on the periodic sync leaves a
        // window where a restart reverts the task to Queued with no attempt
        // marker and re-runs a possibly half-completed session.
        self.control.persist_task_row(&claimed).await;

        let workspace_id = entity
            .owner
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("default")
            .to_string();

        {
            let mut runs = self.control.external_agent_runs.write().await;
            runs.push(ExternalAgentRun {
                id: run_id.clone(),
                workspace_id,
                entity_id: entity.id.clone(),
                provider: "claude".to_string(),
                prompt: prompt.clone(),
                cwd: cwd.clone(),
                status: "running".to_string(),
                task_id: Some(task.id.clone()),
                output: String::new(),
                error: None,
                started_at: now.to_rfc3339(),
                completed_at: None,
                metadata: serde_json::json!({
                    "source": "task-executor",
                    "taskId": task.id,
                    "missionId": task.mission_id,
                }),
                review_policy: None,
                review_status: None,
                review_verdict: None,
                review_run_id: None,
                reviewed_run_id: None,
            });
        }

        let control = self.control.clone();
        let in_flight = self.in_flight.clone();
        let task_id = task.id.clone();
        let evidence_cwd = std::path::PathBuf::from(&cwd);
        // Browser reach is granted per task, never globally: only a task the
        // operator asked for as browser work gets the MCP server, and it drives
        // a dedicated Chrome profile — never the operator's own.
        let mcp_config_path = if task_meta_str(task, "actionKind").as_deref() == Some("browser") {
            write_browser_mcp_config(&run_id).await
        } else {
            None
        };

        tokio::spawn(async move {
            let _in_flight = InFlightGuard(in_flight);
            let baseline = crate::evidence::snapshot(&evidence_cwd).await;
            let outcome = run_claude_session(
                control.clone(),
                SessionRequest {
                    run_id: run_id.clone(),
                    prompt,
                    cwd,
                    // Unattended by definition — nobody is there to answer a
                    // prompt. The directory jail above is the boundary.
                    permission_mode: PermissionMode::BypassPermissions,
                    max_turns: None,
                    resume_session_id: None,
                    stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
                    mcp_config_path,
                },
            )
            .await;

            {
                let mut runs = control.external_agent_runs.write().await;
                if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                    run.status = outcome.status.clone();
                    run.output = outcome.output.clone();
                    run.error = outcome.error.clone();
                    run.completed_at = Some(Utc::now().to_rfc3339());
                }
            }

            let pack = crate::evidence::collect(&evidence_cwd, baseline.as_ref()).await;
            apply_run_outcome_to_task(&control, &task_id, &outcome, Some(&pack)).await;
            // Spend counts against the mission's loop budget before the next
            // iteration is considered, so a cap can actually stop the loop.
            if let Some(mission_id) =
                crate::mission_replanner::loop_mission_id_for_task(&control, &task_id).await
            {
                crate::mission_replanner::accrue_loop_cost(
                    &control,
                    &mission_id,
                    outcome.cost_usd.unwrap_or(0.0),
                )
                .await;
            }
            // The critic half of the loop: on a loop-enabled mission, a run
            // that claims success is reviewed by a second agent before the
            // iteration counts it as done. Nothing marks its own homework.
            maybe_review_loop_run(&control, &run_id, &task_id, &pack).await;
        });

        true
    }

    /// Put the question on the operator's desk and stamp the entity so it is
    /// asked once, not once per queued task.
    ///
    /// The task is deliberately left Queued with no attempt marker: the moment
    /// the operator answers, the next tick picks it up with nothing to re-queue
    /// by hand.
    async fn ask_where_this_lives(
        &self,
        task: &Task,
        entity: &Entity,
        status: &WorkspaceStatus,
        now: DateTime<Utc>,
    ) {
        // Claim the ask under the write lock. Two ticks racing here would
        // otherwise send the same question twice.
        {
            let mut entities = self.control.entities.write().await;
            let Some(e) = entities.iter_mut().find(|e| e.id == entity.id) else {
                return;
            };
            if e.setup
                .workspace_asked_at
                .as_deref()
                .is_some_and(|v| !v.trim().is_empty())
            {
                return;
            }
            e.setup.workspace_asked_at = Some(now.to_rfc3339());
        }
        if let Err(e) = self.control.sync_entities_to_store().await {
            tracing::warn!(entity_id = %entity.id, error = %e, "workspace ask marker write-through failed");
        }

        let detail = match (status.is_vault_fallback, status.path.as_deref()) {
            (true, Some(path)) => format!(
                "{} Right now the only place I have is the notes folder I made at {path} — \
                 I won't build a project in there unless you tell me to.",
                status
                    .question
                    .clone()
                    .unwrap_or_else(|| crate::entity_workspace::workspace_question(entity))
            ),
            (_, Some(path)) => format!(
                "{} The path set for it, {path}, isn't on this machine.",
                status
                    .question
                    .clone()
                    .unwrap_or_else(|| crate::entity_workspace::workspace_question(entity))
            ),
            _ => status
                .question
                .clone()
                .unwrap_or_else(|| crate::entity_workspace::workspace_question(entity)),
        };
        self.push_task_feed(
            task,
            OperatorFeedKind::EntityWorkspaceNeeded,
            format!("Where should {} live?", entity.name.trim()),
            format!("{detail} Work is queued and waiting on your answer."),
        )
        .await;
    }

    async fn set_meta(&self, task_id: &str, pairs: &[(&str, &str)]) {
        let mut tasks = self.control.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
            write_meta(t, pairs);
        }
    }

    async fn push_task_feed(
        &self,
        task: &Task,
        kind: OperatorFeedKind,
        title: String,
        detail: String,
    ) {
        self.control
            .append_operator_feed_event(OperatorFeedEvent {
                id: uuid::Uuid::new_v4().to_string(),
                timestamp: Utc::now().to_rfc3339(),
                kind,
                task_id: Some(task.id.clone()),
                mission_id: task.mission_id.clone(),
                entity_id: task.entity_id.clone(),
                connection_id: None,
                title,
                detail: Some(detail),
                approval_id: None,
            })
            .await;
    }
}

// Merges key/value pairs into task.metadata, creating the object if absent or malformed.
pub(crate) fn write_meta(task: &mut Task, pairs: &[(&str, &str)]) {
    let meta = task.metadata.get_or_insert_with(|| serde_json::json!({}));
    if !meta.is_object() {
        *meta = serde_json::json!({});
    }
    if let Some(obj) = meta.as_object_mut() {
        for (k, v) in pairs {
            obj.insert((*k).to_string(), serde_json::json!(v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{EntityRuntimeConfig, MissionStatus};

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-01T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn entity(mode: Option<AutomationMode>) -> Entity {
        let mut e = Entity {
            id: "ent-1".to_string(),
            name: "Acme".to_string(),
            ..Entity::default()
        };
        if let Some(mode) = mode {
            e.entity_runtime = Some(EntityRuntimeConfig {
                automation_mode: mode,
                heartbeat_mode: crate::state::EntityHeartbeatMode::Off,
                default_chat_session_id: None,
                quiet_hours_local: None,
            });
        }
        e
    }

    fn task() -> Task {
        Task {
            id: "task-1".to_string(),
            title: "Add a CHANGELOG entry".to_string(),
            entity_id: Some("ent-1".to_string()),
            status: TaskStatus::Queued,
            metadata: Some(serde_json::json!({ META_EXECUTABLE: true })),
            ..Task::default()
        }
    }

    fn outcome(status: &str, output: &str) -> SessionOutcome {
        SessionOutcome {
            status: status.to_string(),
            output: output.to_string(),
            ..SessionOutcome::default()
        }
    }

    #[test]
    fn a_planner_marked_queued_task_on_an_autorun_entity_is_eligible() {
        assert!(task_is_auto_executable_at(&task(), &entity(None), now()));
    }

    #[test]
    fn the_executable_flag_is_required_and_missing_means_no() {
        let mut t = task();
        t.metadata = Some(serde_json::json!({}));
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
        t.metadata = None;
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
        // The planner may write it as a JSON bool or a string; both count.
        t.metadata = Some(serde_json::json!({ META_EXECUTABLE: "true" }));
        assert!(task_is_auto_executable_at(&t, &entity(None), now()));
        t.metadata = Some(serde_json::json!({ META_EXECUTABLE: false }));
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
    }

    #[test]
    fn only_queued_tasks_run() {
        for status in [
            TaskStatus::Backlog,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let mut t = task();
            t.status = status;
            assert!(
                !task_is_auto_executable_at(&t, &entity(None), now()),
                "{status:?} must not auto-run"
            );
        }
    }

    #[test]
    fn work_needing_a_human_decision_never_auto_runs() {
        // This is the whole outward-facing safety story.
        let mut t = task();
        t.requires_human_approval = true;
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
    }

    #[test]
    fn an_already_attempted_task_is_never_run_twice() {
        let mut t = task();
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_AUTO_RUN_ATTEMPTED: "2026-08-01T09:00:00Z",
        }));
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
        // A cleared marker (empty string) means "never attempted".
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_AUTO_RUN_ATTEMPTED: "",
        }));
        assert!(task_is_auto_executable_at(&t, &entity(None), now()));
    }

    #[test]
    fn a_no_cwd_task_stays_marked_and_never_retries() {
        let mut t = task();
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_AUTO_RUN_ATTEMPTED: ATTEMPT_NO_CWD,
        }));
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
    }

    #[test]
    fn entities_not_in_autorun_keep_their_tasks() {
        for mode in [AutomationMode::Manual, AutomationMode::Suggest] {
            assert!(!task_is_auto_executable_at(
                &task(),
                &entity(Some(mode)),
                now()
            ));
        }
        assert!(task_is_auto_executable_at(
            &task(),
            &entity(Some(AutomationMode::AutoRun)),
            now()
        ));
    }

    #[test]
    fn an_explicit_approval_outranks_the_automation_mode_default() {
        let mut t = task();
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_APPROVED_AT: "2026-08-01T09:00:00Z",
        }));
        for mode in [AutomationMode::Manual, AutomationMode::Suggest] {
            assert!(
                task_is_auto_executable_at(&t, &entity(Some(mode)), now()),
                "{mode:?} must yield to an explicit approval"
            );
        }
        // Every other veto still applies to approved tasks.
        t.requires_human_approval = true;
        assert!(!task_is_auto_executable_at(
            &t,
            &entity(Some(AutomationMode::Manual)),
            now()
        ));
    }

    #[test]
    fn a_backoff_in_the_future_holds_the_task_and_a_past_one_releases_it() {
        let mut t = task();
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_NEXT_ATTEMPT_AT: "2026-08-01T18:00:00Z",
        }));
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_NEXT_ATTEMPT_AT: "2026-08-01T06:00:00Z",
        }));
        assert!(task_is_auto_executable_at(&t, &entity(None), now()));
        // A corrupt timestamp must not wedge the task forever.
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_NEXT_ATTEMPT_AT: "not-a-timestamp",
        }));
        assert!(task_is_auto_executable_at(&t, &entity(None), now()));
    }

    #[test]
    fn a_done_trailer_is_recognised_with_its_summary() {
        let (status, summary) =
            parse_task_status_trailer("worked on it\nTASK_STATUS: done — added the entry");
        assert_eq!(status, TaskRunStatus::Done);
        assert_eq!(summary.as_deref(), Some("added the entry"));
    }

    #[test]
    fn a_blocked_trailer_carries_the_reason() {
        let (status, summary) =
            parse_task_status_trailer("TASK_STATUS: blocked — needs the staging API key");
        assert_eq!(status, TaskRunStatus::Blocked);
        assert_eq!(summary.as_deref(), Some("needs the staging API key"));
    }

    #[test]
    fn a_missing_or_unparseable_trailer_means_blocked_never_done() {
        // The dangerous failure is a false "done": it retires a task nobody did.
        assert_eq!(
            parse_task_status_trailer("All finished, everything works!").0,
            TaskRunStatus::Blocked
        );
        assert_eq!(
            parse_task_status_trailer("TASK_STATUS: ???").0,
            TaskRunStatus::Blocked
        );
    }

    #[test]
    fn the_last_trailer_wins_and_decoration_is_tolerated() {
        let output = "I was told to end with TASK_STATUS: done — <one sentence>\n\
                      ...did the work...\n\
                      **TASK_STATUS: blocked - the migration needs a decision";
        let (status, summary) = parse_task_status_trailer(output);
        assert_eq!(status, TaskRunStatus::Blocked);
        assert_eq!(summary.as_deref(), Some("the migration needs a decision"));
    }

    #[test]
    fn a_mission_trailer_is_not_a_task_trailer() {
        // Separate prefixes so a beat's report can never retire a task.
        assert_eq!(
            parse_task_status_trailer("MISSION_STATUS: complete — shipped").0,
            TaskRunStatus::Blocked
        );
    }

    #[test]
    fn a_done_run_completes_the_task() {
        let update = plan_task_run_update(
            &outcome("completed", "TASK_STATUS: done — shipped the parser"),
            1,
            now(),
        );
        assert_eq!(update.status, TaskStatus::Completed);
        assert_eq!(update.outcome, Some(TaskOutcome::Completed));
        assert_eq!(update.progress, Some(100));
        assert_eq!(update.summary, "shipped the parser");
        assert_eq!(update.feed_kind, OperatorFeedKind::AgentRunCompleted);
        assert!(update.next_attempt_at.is_none());
        assert!(!update.clear_attempt_marker);
    }

    #[test]
    fn a_blocked_run_lands_on_the_operators_desk() {
        let update = plan_task_run_update(
            &outcome(
                "completed",
                "TASK_STATUS: blocked — no database credentials",
            ),
            1,
            now(),
        );
        assert_eq!(update.status, TaskStatus::Failed);
        assert_eq!(update.outcome, Some(TaskOutcome::NeedsReview));
        assert_eq!(update.feed_kind, OperatorFeedKind::TaskBlocked);
        assert_eq!(update.summary, "no database credentials");
    }

    #[test]
    fn a_crashed_run_is_reviewed_not_silently_completed() {
        let mut o = outcome("failed", "TASK_STATUS: done — I totally did it");
        o.error = Some("claude CLI exited with code 1".to_string());
        let update = plan_task_run_update(&o, MAX_AUTO_ATTEMPTS, now());
        assert_eq!(update.status, TaskStatus::Failed);
        assert_eq!(update.outcome, Some(TaskOutcome::NeedsReview));
    }

    #[tokio::test]
    async fn a_cancelled_task_is_never_resurrected_by_its_own_finishing_run() {
        // The operator hit stop while the run was in flight. When the run
        // lands, it must not rewrite the status — and above all must not take
        // the crash-retry branch back to Queued, which would start a fresh
        // unattended run minutes after the human said stop.
        let control = Arc::new(ControlState::new("openai", "https://x", "m"));
        let mut t = task();
        t.status = TaskStatus::Cancelled;
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_ACTIVE_RUN_ID: "run-9",
            META_ATTEMPT_COUNT: "1",
        }));
        let task_id = t.id.clone();
        control.tasks.write().await.push(t);

        // A crashed run: the branch that would otherwise requeue for retry.
        let mut crashed = outcome("failed", "");
        crashed.error = Some("stall timeout".to_string());
        apply_run_outcome_to_task(&control, &task_id, &crashed, None).await;

        let tasks = control.tasks.read().await;
        let after = tasks.iter().find(|t| t.id == task_id).unwrap();
        assert_eq!(after.status, TaskStatus::Cancelled, "cancel must stand");
        assert!(
            meta_marker(after, META_NEXT_ATTEMPT_AT).is_none(),
            "no retry may be scheduled for cancelled work"
        );
        assert!(meta_marker(after, META_ACTIVE_RUN_ID).is_none());
        assert!(
            after.result.as_deref().unwrap_or("").contains("cancelled"),
            "the run's outcome is still recorded for the operator"
        );
    }

    #[tokio::test]
    async fn a_finished_task_remembers_which_run_produced_its_output() {
        let control = Arc::new(ControlState::new("openai", "https://x", "m"));
        let mut t = task();
        t.status = TaskStatus::InProgress;
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_ACTIVE_RUN_ID: "run-42",
            META_ATTEMPT_COUNT: "1",
        }));
        let task_id = t.id.clone();
        control.tasks.write().await.push(t);

        apply_run_outcome_to_task(
            &control,
            &task_id,
            &outcome("completed", "TASK_STATUS: done — shipped"),
            None,
        )
        .await;

        let tasks = control.tasks.read().await;
        let after = tasks.iter().find(|t| t.id == task_id).unwrap();
        assert_eq!(after.status, TaskStatus::Completed);
        // The active pointer is cleared, but the run is still findable — which
        // is what makes a finished run's screenshots reachable.
        assert!(meta_marker(after, META_ACTIVE_RUN_ID).is_none());
        assert_eq!(
            meta_marker(after, META_LAST_RUN_ID).as_deref(),
            Some("run-42")
        );
    }

    #[test]
    fn a_done_with_no_observable_change_is_downgraded_to_review() {
        let mut update = plan_task_run_update(
            &outcome("completed", "TASK_STATUS: done — refactored the module"),
            1,
            now(),
        );
        let pack = crate::evidence::EvidencePack {
            is_git: true,
            ..Default::default()
        };
        apply_evidence_to_update(&mut update, &task(), &pack);
        assert_eq!(update.status, TaskStatus::Failed);
        assert_eq!(update.outcome, Some(TaskOutcome::NeedsReview));
        assert!(update.summary.contains("nothing in the project directory"));
    }

    #[test]
    fn evidence_of_change_and_non_git_dirs_leave_a_done_verdict_alone() {
        let done = || {
            plan_task_run_update(
                &outcome("completed", "TASK_STATUS: done — shipped"),
                1,
                now(),
            )
        };
        // Real change: verdict stands.
        let mut update = done();
        let pack = crate::evidence::EvidencePack {
            is_git: true,
            commits: 1,
            ..Default::default()
        };
        apply_evidence_to_update(&mut update, &task(), &pack);
        assert_eq!(update.status, TaskStatus::Completed);
        // Non-git dir: can't prove absence, verdict stands.
        let mut update = done();
        apply_evidence_to_update(&mut update, &task(), &Default::default());
        assert_eq!(update.status, TaskStatus::Completed);
        // Browser actions legitimately leave no file trace.
        let mut update = done();
        let mut t = task();
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true, "actionKind": "browser",
        }));
        let pack = crate::evidence::EvidencePack {
            is_git: true,
            ..Default::default()
        };
        apply_evidence_to_update(&mut update, &t, &pack);
        assert_eq!(update.status, TaskStatus::Completed);
    }

    #[test]
    fn a_crashed_first_attempt_retries_once_with_backoff() {
        let mut o = outcome("failed", "");
        o.error = Some("stall timeout".to_string());
        let update = plan_task_run_update(&o, 1, now());
        assert_eq!(update.status, TaskStatus::Queued);
        assert!(update.outcome.is_none());
        assert!(update.clear_attempt_marker, "the claim must be released");
        assert!(!update.refund_attempt, "a crash still consumes its attempt");
        let next = update.next_attempt_at.expect("a retry must be scheduled");
        let parsed = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        assert!(parsed > now());
    }

    #[test]
    fn a_crash_on_the_final_attempt_lands_on_the_operators_desk() {
        let mut o = outcome("failed", "");
        o.error = Some("stall timeout".to_string());
        let update = plan_task_run_update(&o, MAX_AUTO_ATTEMPTS, now());
        assert_eq!(update.status, TaskStatus::Failed);
        assert_eq!(update.outcome, Some(TaskOutcome::NeedsReview));
        assert_eq!(update.feed_kind, OperatorFeedKind::TaskBlocked);
    }

    #[test]
    fn an_explicit_blocked_verdict_never_auto_retries() {
        // The agent said something meaningful; retrying would just burn the
        // second attempt on the same missing decision.
        let update = plan_task_run_update(
            &outcome("completed", "TASK_STATUS: blocked — need credentials"),
            1,
            now(),
        );
        assert_eq!(update.status, TaskStatus::Failed);
        assert!(update.next_attempt_at.is_none());
    }

    #[test]
    fn the_attempt_count_bounds_eligibility() {
        let mut t = task();
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_ATTEMPT_COUNT: "2",
        }));
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
        // One consumed attempt with a released claim marker is still eligible.
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_ATTEMPT_COUNT: "1",
            META_AUTO_RUN_ATTEMPTED: "",
        }));
        assert!(task_is_auto_executable_at(&t, &entity(None), now()));
    }

    #[test]
    fn a_spent_usage_window_requeues_the_task_instead_of_failing_it() {
        let mut o = outcome("failed", "");
        o.rate_limited = true;
        o.resets_at = Some((now() + chrono::Duration::hours(3)).timestamp());
        let update = plan_task_run_update(&o, 1, now());
        // Not a verdict on the work: back to Queued with its attempt returned.
        assert_eq!(update.status, TaskStatus::Queued);
        assert!(update.outcome.is_none());
        assert!(update.clear_attempt_marker);
        let next = update.next_attempt_at.expect("a retry must be scheduled");
        let parsed = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        assert!(parsed > now() + chrono::Duration::hours(2));
    }

    #[test]
    fn a_requeued_task_is_eligible_again_only_after_the_window_reopens() {
        // The reschedule and the eligibility rule have to agree, or the task
        // either spins immediately or never comes back.
        let mut o = outcome("failed", "");
        o.rate_limited = true;
        o.resets_at = Some((now() + chrono::Duration::hours(3)).timestamp());
        let update = plan_task_run_update(&o, 1, now());

        let mut t = task();
        t.status = update.status;
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_AUTO_RUN_ATTEMPTED: "",
            META_NEXT_ATTEMPT_AT: update.next_attempt_at.clone().unwrap(),
        }));
        assert!(!task_is_auto_executable_at(&t, &entity(None), now()));
        assert!(task_is_auto_executable_at(
            &t,
            &entity(None),
            now() + chrono::Duration::hours(4)
        ));
    }

    #[test]
    fn the_prompt_carries_the_task_the_mission_and_the_directory_jail() {
        let mut t = task();
        t.mission_id = Some("mis-1".to_string());
        t.metadata = Some(serde_json::json!({
            META_EXECUTABLE: true,
            META_DESCRIPTION: "Append the 0.4.0 section",
        }));
        let mission = Mission {
            id: "mis-1".to_string(),
            entity_id: "ent-1".to_string(),
            title: "Ship 0.4.0".to_string(),
            summary: String::new(),
            status: MissionStatus::Active,
            priority: "medium".to_string(),
            goal: "A tagged release on brew".to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-07-01T00:00:00Z".to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: Vec::new(),
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        };
        let p = build_task_prompt(&t, Some(&mission), None);
        assert!(p.contains("Add a CHANGELOG entry"));
        assert!(p.contains("Append the 0.4.0 section"));
        assert!(p.contains("Ship 0.4.0"));
        assert!(p.contains("A tagged release on brew"));
        assert!(
            p.contains("only inside the current project directory"),
            "the jail must be stated to the agent as well as enforced"
        );
        assert!(p.contains("TASK_STATUS: done|blocked"));
    }

    /// The executor's workspace gate, exercised through the real
    /// `workspace_status` so the two halves cannot drift apart.
    mod workspace_gate_tests {
        use super::*;
        use crate::entity_workspace::{CHOICE_VAULT_ONLY, WorkspaceKind, workspace_status_with};
        use std::path::PathBuf;

        fn tempdir(name: &str) -> PathBuf {
            let dir =
                std::env::temp_dir().join(format!("cw-exec-gate-{name}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn app_roots(base: &std::path::Path) -> Vec<PathBuf> {
            vec![base.join("app-owned")]
        }

        #[test]
        fn an_entity_with_nowhere_to_work_is_asked_not_run() {
            let base = tempdir("nowhere");
            let status = workspace_status_with(&entity(None), &app_roots(&base));
            assert_eq!(status.kind, WorkspaceKind::Unset);
            assert_eq!(workspace_gate(&status, false), WorkspaceGate::Ask);
        }

        #[test]
        fn the_vault_fallback_alone_is_never_enough_to_start_a_coding_agent() {
            // The whole bug: a folder the gateway made, handed to an unattended
            // agent as if someone had picked it.
            let base = tempdir("vault");
            let vault = base.join("app-owned/vault/entities/acme");
            std::fs::create_dir_all(&vault).unwrap();
            let mut e = entity(None);
            e.local_path = Some(vault.to_string_lossy().to_string());
            let status = workspace_status_with(&e, &app_roots(&base));
            assert!(status.is_vault_fallback);
            assert_eq!(workspace_gate(&status, false), WorkspaceGate::Ask);
        }

        #[test]
        fn notes_only_is_an_answer_and_lets_the_work_run() {
            let base = tempdir("notes-only");
            let vault = base.join("app-owned/vault/entities/acme");
            std::fs::create_dir_all(&vault).unwrap();
            let mut e = entity(None);
            e.local_path = Some(vault.to_string_lossy().to_string());
            e.setup.workspace_choice = Some(CHOICE_VAULT_ONLY.to_string());
            let status = workspace_status_with(&e, &app_roots(&base));
            assert_eq!(workspace_gate(&status, false), WorkspaceGate::Proceed);
        }

        #[test]
        fn a_real_project_folder_proceeds_without_a_question() {
            let base = tempdir("real");
            let work = base.join("projects/acme");
            std::fs::create_dir_all(&work).unwrap();
            let mut e = entity(None);
            e.local_path = Some(work.to_string_lossy().to_string());
            let status = workspace_status_with(&e, &app_roots(&base));
            assert_eq!(workspace_gate(&status, false), WorkspaceGate::Proceed);
        }

        #[test]
        fn the_question_is_asked_once_per_entity_not_once_per_task() {
            let base = tempdir("once");
            let status = workspace_status_with(&entity(None), &app_roots(&base));
            // Forty queued tasks against one undecided entity must not produce
            // forty notifications.
            assert_eq!(workspace_gate(&status, true), WorkspaceGate::WaitQuietly);
        }

        #[test]
        fn asking_leaves_the_task_eligible_so_an_answer_releases_it() {
            // Nothing about the ask marks the task; it stays exactly as
            // eligible as it was, which is what makes "answer and it runs" true
            // without a manual re-queue.
            let base = tempdir("eligible");
            let status = workspace_status_with(&entity(None), &app_roots(&base));
            assert_eq!(workspace_gate(&status, false), WorkspaceGate::Ask);
            assert!(task_is_auto_executable_at(&task(), &entity(None), now()));
        }
    }

    #[test]
    fn a_task_without_a_mission_or_description_still_produces_a_usable_prompt() {
        let mut t = task();
        t.metadata = Some(serde_json::json!({ META_EXECUTABLE: true }));
        t.result = Some("port the old parser".to_string());
        let p = build_task_prompt(&t, None, None);
        assert!(!p.contains("PARENT MISSION"));
        // The prior result is the only detail there is; use it.
        assert!(p.contains("port the old parser"));
        assert!(p.contains("TASK_STATUS:"));
        assert!(!p.contains("## Skills"));
    }

    #[test]
    fn skills_ride_the_task_prompt_with_a_read_exception_to_the_jail() {
        let mut t = task();
        t.metadata = Some(serde_json::json!({ META_EXECUTABLE: true }));
        let block = "## Skills\n- `release-checklist` — Ship safely. Use when releasing. — /x/SKILL.md";
        let p = build_task_prompt(&t, None, Some(block));
        assert!(p.contains("release-checklist"));
        assert!(p.contains("you may read the skill files listed above"));
        let i_skills = p.find("## Skills").unwrap();
        let i_rules = p.find("HOW TO WORK").unwrap();
        assert!(i_skills < i_rules, "the rule that permits reading them follows the list");
    }
}
