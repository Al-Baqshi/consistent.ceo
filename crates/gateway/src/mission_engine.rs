//! Missions that keep working on their own, over days or weeks.
//!
//! The operator says "keep going until this is done" and then closes the laptop
//! lid, gets on a plane, forgets about it. This module is what keeps the work
//! moving in the meantime: it attaches a Claude Code engine to a mission and
//! wakes it on a cadence, each beat resuming the same session so the agent
//! builds on itself instead of starting cold.
//!
//! ### Shape
//!
//! One durable **companion task** per engaged mission (`task_kind`
//! [`BEAT_TASK_KIND`]), carrying all engine state in `Task.metadata`. That
//! choice buys a lot for nothing: the whole `task_meta_*` persistence path,
//! restart survival, and visibility in every existing task surface — with no
//! new table and no schema migration.
//!
//! [`MissionEngineTicker`] is deliberately the same shape as
//! [`crate::reminders::ReminderTicker`] — scan for due rows, act, emit feed
//! events after dropping the lock.
//!
//! ### Why the agent is asked for a status trailer
//!
//! A beat ends with the agent stating `MISSION_STATUS: continuing|blocked|complete`.
//! Parsing a required trailer is deterministic and cheap; asking a second model
//! "is this done?" would be neither. The same reasoning as
//! [`crate::agent_review::parse_review_verdict`], and the same bias: anything
//! unparseable means *keep going*, never "done".
//!
//! ### Nothing declares itself finished
//!
//! `complete` does not complete the mission. It runs the existing reviewer
//! policy and then puts the mission in `UserInputNeeded` with a notification.
//! An autonomous system that can mark its own homework will, eventually, mark
//! it wrong — and a CEO who cannot trust "done" has to re-check everything,
//! which costs more than the automation saved.

use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::agent_sessions::{
    DEFAULT_STALL_TIMEOUT_SECS, PermissionMode, SessionRequest, run_claude_session,
};
use crate::state::{
    ControlState, Entity, ExternalAgentRun, Mission, OperatorFeedKind, Task, TaskStatus,
    task_meta_str,
};

/// `task_kind` of the companion task that drives an engaged mission.
pub const BEAT_TASK_KIND: &str = "agent-session-beat";

/// Metadata keys on that task. Strings because `task_meta_*` is string-typed;
/// the parse-with-fallback helpers below absorb anything malformed.
pub const META_MISSION_ID: &str = "missionId";
pub const META_NEXT_BEAT_AT: &str = "nextBeatAt";
pub const META_INTERVAL_MINUTES: &str = "beatIntervalMinutes";
pub const META_SESSION_ID: &str = "claudeSessionId";
pub const META_BEAT_COUNT: &str = "beatCount";
pub const META_COST_USD: &str = "costUsd";
pub const META_LAST_SUMMARY: &str = "lastSummary";
pub const META_PAUSED: &str = "paused";
pub const META_BUDGET_USD: &str = "budgetUsd";
pub const META_MAX_BEATS: &str = "maxBeats";
pub const META_CWD: &str = "cwd";
pub const META_ACTIVE_RUN_ID: &str = "activeRunId";
/// RFC3339 instant the Claude usage window reopens. Set when a beat is cut
/// short by the subscription limit; cleared on the next successful beat.
pub const META_WAITING_UNTIL: &str = "waitingOnUsageUntil";

/// Default gap between beats when the mission doesn't specify one.
pub const DEFAULT_INTERVAL_MINUTES: i64 = 240;
/// Each beat is a work session, not a whole day: stopping at a safe point and
/// writing down where things stand is what makes the *next* beat cheap.
pub const DEFAULT_BEAT_MINUTES: u32 = 25;

/// What the agent said about the mission at the end of a beat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeatStatus {
    Continuing,
    Blocked,
    Complete,
}

impl BeatStatus {
    /// Lowercase wire form of the status, as written in the `MISSION_STATUS` trailer.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Continuing => "continuing",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
        }
    }
}

/// Pull `<PREFIX>: <state> — <summary>` out of an agent's output, returning the
/// state word lowercased and the summary when there is one.
///
/// Scans from the end, because the trailer is the last thing the agent writes
/// and the transcript above it may quote the instruction verbatim. The
/// underscore in a prefix is also accepted as a space (`MISSION STATUS:`),
/// which is what agents write about a tenth of the time.
///
/// Deliberately says nothing about what the state word *means*: the mission
/// engine and [`crate::task_executor`] want opposite defaults out of a missing
/// trailer, so each maps the raw word itself.
pub fn parse_trailer_prefixed(output: &str, prefix: &str) -> Option<(String, Option<String>)> {
    let underscored = format!("{}:", prefix.trim_end_matches(':'));
    let spaced = underscored.replace('_', " ");
    for line in output.lines().rev() {
        let line = line.trim().trim_start_matches(['#', '*', '-', ' ']);
        let Some(rest) = line
            .strip_prefix(underscored.as_str())
            .or_else(|| line.strip_prefix(spaced.as_str()))
        else {
            continue;
        };
        let rest = rest.trim();
        let (state_word, summary) = match rest.split_once(['—', '-', ':']) {
            Some((a, b)) => (
                a.trim(),
                Some(b.trim().to_string()).filter(|s| !s.is_empty()),
            ),
            None => (rest, None),
        };
        return Some((state_word.to_ascii_lowercase(), summary));
    }
    None
}

/// Pull `MISSION_STATUS: <state> — <summary>` out of a beat's output.
///
/// Returns `Continuing` when the trailer is missing or unrecognised: a beat
/// that forgot to report is a beat that hasn't finished, and treating silence
/// as "complete" would hand the operator a false finish.
pub fn parse_status_trailer(output: &str) -> (BeatStatus, Option<String>) {
    match parse_trailer_prefixed(output, "MISSION_STATUS") {
        Some((state_word, summary)) => {
            let status = match state_word.as_str() {
                s if s.starts_with("complete") => BeatStatus::Complete,
                s if s.starts_with("block") => BeatStatus::Blocked,
                _ => BeatStatus::Continuing,
            };
            (status, summary)
        }
        None => (BeatStatus::Continuing, None),
    }
}

/// The instruction given to the agent each beat.
///
/// Two things carry the continuity: `--resume` gives the agent its own memory
/// of the conversation, and `PROGRESS.md` in the repo gives it a memory that
/// survives even losing the session store. Belt and braces, because the whole
/// point is surviving days of interruptions.
pub fn build_beat_prompt(
    mission: &Mission,
    pinned_notes: &[String],
    last_summary: Option<&str>,
    beat_number: u64,
    minutes: u32,
) -> String {
    build_beat_prompt_with_skills(mission, pinned_notes, last_summary, beat_number, minutes, None)
}

/// [`build_beat_prompt`] plus the entity's skills index (see `crate::skills`),
/// placed before the working rules so the agent knows which procedures exist
/// before it plans the beat.
pub fn build_beat_prompt_with_skills(
    mission: &Mission,
    pinned_notes: &[String],
    last_summary: Option<&str>,
    beat_number: u64,
    minutes: u32,
    skills: Option<&str>,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "You are working an ongoing mission for the operator. This is beat #{beat_number}.\n\n"
    ));
    s.push_str(&format!("MISSION: {}\n", mission.title));
    if !mission.goal.trim().is_empty() {
        s.push_str(&format!("GOAL: {}\n", mission.goal.trim()));
    }
    if !mission.summary.trim().is_empty() {
        s.push_str(&format!("CONTEXT: {}\n", mission.summary.trim()));
    }
    if !pinned_notes.is_empty() {
        s.push_str("\nPINNED NOTES (always true for this mission):\n");
        for note in pinned_notes {
            s.push_str(&format!("- {note}\n"));
        }
    }
    if let Some(prev) = last_summary.filter(|p| !p.trim().is_empty()) {
        s.push_str(&format!("\nWHERE YOU LEFT OFF: {}\n", prev.trim()));
    }
    if let Some(block) = skills.map(str::trim).filter(|b| !b.is_empty()) {
        s.push('\n');
        s.push_str(block);
        s.push('\n');
    }
    s.push_str(&format!(
        "\nHOW TO WORK THIS BEAT:\n\
         1. Start by checking the current state: `git status`, and read PROGRESS.md if it exists.\n\
         2. Work for up to about {minutes} minutes on the next most valuable step.\n\
         3. Stop at a safe point — never leave the tree broken or half-migrated.\n\
         4. Update PROGRESS.md with what you did, what is left, and the next concrete step.\n\
         \nThe operator may have changed files between beats; trust the working tree over your memory.\n\
         \nEnd your reply with exactly one line:\n\
         MISSION_STATUS: continuing|blocked|complete — <one sentence>\n\
         Use `blocked` only if you need a decision from the operator. \
         Use `complete` only if the goal is fully met.\n"
    ));
    s
}

// Read a metadata string as i64, falling back rather than erroring on garbage.
fn meta_i64(task: &Task, key: &str, fallback: i64) -> i64 {
    task_meta_str(task, key)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(fallback)
}

// Same idea for f64, defaulting to 0.0 (used for cost/budget tracking).
fn meta_f64(task: &Task, key: &str) -> f64 {
    task_meta_str(task, key)
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

// A metadata boolean, stored as the literal string "true".
fn meta_flag(task: &Task, key: &str) -> bool {
    task_meta_str(task, key).is_some_and(|v| v == "true")
}

/// Is this companion task due to run a beat at `now`?
///
/// Pure so the scheduling rules are testable without a clock or a database.
pub fn beat_is_due(task: &Task, now: DateTime<Utc>) -> bool {
    if task.task_kind.as_deref() != Some(BEAT_TASK_KIND) {
        return false;
    }
    if matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
    ) {
        return false;
    }
    if meta_flag(task, META_PAUSED) {
        return false;
    }
    // Budget and beat caps are hard stops: unattended spend is the one failure
    // mode the operator cannot notice in time to stop themselves.
    let budget = meta_f64(task, META_BUDGET_USD);
    if budget > 0.0 && meta_f64(task, META_COST_USD) >= budget {
        return false;
    }
    let max_beats = meta_i64(task, META_MAX_BEATS, 0);
    if max_beats > 0 && meta_i64(task, META_BEAT_COUNT, 0) >= max_beats {
        return false;
    }
    match task_meta_str(task, META_NEXT_BEAT_AT) {
        // No schedule yet = run the first beat immediately.
        None => true,
        Some(raw) => match DateTime::parse_from_rfc3339(&raw) {
            Ok(dt) => dt.with_timezone(&Utc) <= now,
            // An unparseable timestamp should not wedge the mission forever.
            Err(_) => true,
        },
    }
}

/// When a beat stopped because the Claude usage window was spent, the next one
/// should start when the window reopens — not on the normal cadence.
///
/// This is the whole point of the engine on a Max subscription: the CLI already
/// tells us `resetsAt`, so nobody has to sit at the desk waiting to type "keep
/// going". A reset already in the past (clock skew, or a limit reported without
/// a timestamp) floors to a short retry rather than hammering immediately.
pub fn next_beat_after_rate_limit(now: DateTime<Utc>, resets_at: Option<i64>) -> String {
    const FLOOR_MINUTES: i64 = 5;
    // A small cushion past the stated reset: coming back a second early just
    // burns another rejected run.
    const CUSHION_SECS: i64 = 60;
    // Checked arithmetic throughout: `resetsAt` comes from the CLI's output,
    // and this runs in a beat's completion handler — a panic here would leave
    // the run marked active and wedge the engine.
    let target = resets_at
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
        .and_then(|dt| dt.checked_add_signed(ChronoDuration::seconds(CUSHION_SECS)));
    let floor = now
        .checked_add_signed(ChronoDuration::minutes(FLOOR_MINUTES))
        .unwrap_or(DateTime::<Utc>::MAX_UTC);
    match target {
        Some(t) if t > floor => t.to_rfc3339(),
        _ => floor.to_rfc3339(),
    }
}

/// Upper bound on a beat interval: one year. The API rejects anything larger;
/// [`next_beat_at`] clamps to it as well, because the interval lives in
/// free-form task metadata and a value stored before the API validated it (or
/// written by some other path) must still not overflow.
pub const MAX_INTERVAL_MINUTES: i64 = 525_600;

/// Schedule the next beat `interval_minutes` from `now` (RFC3339), clamped to
/// `1..=MAX_INTERVAL_MINUTES`.
///
/// Never panics: it runs after a paid beat has finished, and a panic there
/// (`Duration::minutes` or `DateTime + Duration` overflowing on a nonsense
/// interval) would leave the beat's run id set and wedge the engine for good.
pub fn next_beat_at(now: DateTime<Utc>, interval_minutes: i64) -> String {
    let minutes = interval_minutes.clamp(1, MAX_INTERVAL_MINUTES);
    now.checked_add_signed(ChronoDuration::minutes(minutes))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
        .to_rfc3339()
}

/// Background loop that drives every engaged mission's beats forward.
pub struct MissionEngineTicker {
    control: Arc<ControlState>,
    poll_secs: u64,
    /// Skills offered to each beat (index + `SKILL.md` paths in the prompt).
    /// Beats run `bypassPermissions`, so the agent can read them in place.
    skills: Option<Arc<crate::skills::SkillLibrary>>,
}

impl MissionEngineTicker {
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
            skills,
        })
    }

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

    /// Poll forever, starting every beat that's due each tick.
    pub async fn run(self: Arc<Self>) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(self.poll_secs));
        // A laptop that was asleep for six hours should get one beat, not six.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let started = self.run_one_tick(Utc::now()).await;
            if started > 0 {
                tracing::info!(started, "mission engine: beats started");
            }
        }
    }

    /// Start every beat that is due at `now`. Returns how many were started.
    pub async fn run_one_tick(&self, now: DateTime<Utc>) -> usize {
        let due: Vec<Task> = {
            let tasks = self.control.tasks.read().await;
            tasks
                .iter()
                .filter(|t| beat_is_due(t, now))
                .cloned()
                .collect()
        };
        let mut started = 0usize;
        for task in due {
            // One beat per mission at a time. A previous beat still holding a
            // live process means this one would fight it over the same files.
            if let Some(active) = task_meta_str(&task, META_ACTIVE_RUN_ID)
                && self.control.agent_processes.is_running(&active).await
            {
                continue;
            }
            if self.start_beat(&task, now).await {
                started += 1;
            }
        }
        started
    }

    // Kick off one beat for a due companion task: resolve the mission, build the
    // prompt, record a running agent run, then spawn the actual Claude session
    // and let it update task/mission state asynchronously when it finishes.
    async fn start_beat(&self, task: &Task, now: DateTime<Utc>) -> bool {
        let Some(mission_id) = task_meta_str(task, META_MISSION_ID) else {
            return false;
        };
        let Some(cwd) = task_meta_str(task, META_CWD) else {
            tracing::warn!(task = %task.id, "mission engine: beat task has no cwd; skipping");
            return false;
        };
        let mission = {
            let missions = self.control.missions.read().await;
            missions.iter().find(|m| m.id == mission_id).cloned()
        };
        let Some(mission) = mission else {
            tracing::warn!(mission_id = %mission_id, "mission engine: mission is gone; pausing beat");
            self.set_meta(&task.id, &[(META_PAUSED, "true")]).await;
            return false;
        };

        // Re-establish at run time what attaching established, because the
        // beat task carries its mission link and its cwd in free-form metadata
        // that any task create can set: the task must belong to the mission's
        // entity, and the cwd must resolve inside that entity's roots. A beat
        // is an unattended, permission-bypassing Claude run — its jail is never
        // taken from metadata on trust.
        let entity = {
            let entities = self.control.entities.read().await;
            entities
                .iter()
                .find(|e| e.id == mission.entity_id && !e.status.eq_ignore_ascii_case("deleted"))
                .cloned()
        };
        let Some(entity) = entity.filter(|e| task.entity_id.as_deref() == Some(e.id.as_str()))
        else {
            tracing::warn!(
                task = %task.id,
                mission_id = %mission_id,
                "mission engine: beat task does not belong to the mission's entity; pausing beat"
            );
            self.set_meta(&task.id, &[(META_PAUSED, "true")]).await;
            return false;
        };
        let cwd = match crate::control_api::resolve_entity_agent_run_cwd(&entity, Some(&cwd)).await
        {
            Ok((path, _roots)) => path.to_string_lossy().to_string(),
            Err(status) => {
                tracing::warn!(
                    task = %task.id,
                    cwd = %cwd,
                    status = %status,
                    "mission engine: beat cwd is not inside the entity's roots; pausing beat"
                );
                self.set_meta(&task.id, &[(META_PAUSED, "true")]).await;
                return false;
            }
        };
        // The run is recorded in the entity's own workspace, so it is visible
        // (and cancellable) there and nowhere else.
        let run_workspace_id = entity
            .owner
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("default")
            .to_string();

        let beat_number = meta_i64(task, META_BEAT_COUNT, 0).max(0) as u64 + 1;
        let skills = self.skills_block(&entity).await;
        let prompt = build_beat_prompt_with_skills(
            &mission,
            &mission.canonical_notes,
            task_meta_str(task, META_LAST_SUMMARY).as_deref(),
            beat_number,
            DEFAULT_BEAT_MINUTES,
            skills.as_deref(),
        );
        let run_id = uuid::Uuid::new_v4().to_string();
        let session_id = task_meta_str(task, META_SESSION_ID);

        {
            let mut runs = self.control.external_agent_runs.write().await;
            runs.push(ExternalAgentRun {
                id: run_id.clone(),
                workspace_id: run_workspace_id,
                entity_id: mission.entity_id.clone(),
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
                    "missionId": mission_id,
                    "beat": beat_number,
                    "sessionId": session_id,
                }),
                review_policy: None,
                review_status: None,
                review_verdict: None,
                review_run_id: None,
                reviewed_run_id: None,
            });
        }

        self.set_meta(
            &task.id,
            &[
                (META_ACTIVE_RUN_ID, run_id.as_str()),
                (META_BEAT_COUNT, &beat_number.to_string()),
            ],
        )
        .await;

        let control = self.control.clone();
        let task_id = task.id.clone();
        let interval = meta_i64(task, META_INTERVAL_MINUTES, DEFAULT_INTERVAL_MINUTES);
        let prior_cost = meta_f64(task, META_COST_USD);
        let mission_title = mission.title.clone();
        let entity_id = mission.entity_id.clone();

        tokio::spawn(async move {
            let outcome = run_claude_session(
                control.clone(),
                SessionRequest {
                    run_id: run_id.clone(),
                    prompt,
                    cwd,
                    // Unattended by definition — nobody is there to answer a prompt.
                    permission_mode: PermissionMode::BypassPermissions,
                    max_turns: None,
                    resume_session_id: session_id,
                    stall_timeout_secs: DEFAULT_STALL_TIMEOUT_SECS,
                    mcp_config_path: None,
                },
            )
            .await;

            let (status, summary) = parse_status_trailer(&outcome.output);
            let total_cost = prior_cost + outcome.cost_usd.unwrap_or(0.0);
            let summary_text = summary.unwrap_or_else(|| {
                outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| "Beat finished.".to_string())
            });

            {
                let mut runs = control.external_agent_runs.write().await;
                if let Some(run) = runs.iter_mut().find(|r| r.id == run_id) {
                    run.status = outcome.status.clone();
                    run.output = outcome.output.clone();
                    run.error = outcome.error.clone();
                    run.completed_at = Some(Utc::now().to_rfc3339());
                }
            }

            // A failed beat must not silently end the mission — reschedule and
            // let the next beat re-orient from the working tree.
            let engine_failed = outcome.status != "completed" && !outcome.rate_limited;
            // A spent usage window never pauses the mission and never counts as the
            // agent reporting a state: the work waits for the window to reopen and
            // picks itself back up. This is what replaces the operator sitting at
            // the desk to type "keep going".
            let pause = !outcome.rate_limited
                && matches!(status, BeatStatus::Blocked | BeatStatus::Complete);
            let next_at = if outcome.rate_limited {
                next_beat_after_rate_limit(Utc::now(), outcome.resets_at)
            } else {
                next_beat_at(Utc::now(), interval)
            };
            let detail = if outcome.rate_limited {
                match DateTime::parse_from_rfc3339(&next_at) {
                    Ok(dt) => format!(
                        "Usage window spent. Resuming automatically at {}.",
                        dt.format("%H:%M on %d %b")
                    ),
                    Err(_) => "Usage window spent. Resuming automatically.".to_string(),
                }
            } else {
                summary_text.clone()
            };
            let mut updates: Vec<(&str, String)> = vec![
                (META_ACTIVE_RUN_ID, String::new()),
                (META_COST_USD, total_cost.to_string()),
                (META_LAST_SUMMARY, summary_text.clone()),
                (META_NEXT_BEAT_AT, next_at.clone()),
                (
                    META_WAITING_UNTIL,
                    if outcome.rate_limited {
                        next_at.clone()
                    } else {
                        String::new()
                    },
                ),
            ];
            if let Some(sid) = outcome.session_id.as_deref() {
                updates.push((META_SESSION_ID, sid.to_string()));
            }
            if pause {
                updates.push((META_PAUSED, "true".to_string()));
            }
            {
                let mut tasks = control.tasks.write().await;
                if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
                    let meta = t.metadata.get_or_insert_with(|| serde_json::json!({}));
                    if let Some(obj) = meta.as_object_mut() {
                        for (k, v) in &updates {
                            obj.insert((*k).to_string(), serde_json::json!(v));
                        }
                    }
                    t.result = Some(summary_text.clone());
                    if matches!(status, BeatStatus::Complete) && !outcome.rate_limited {
                        // The beat really did run, so the summary above stands —
                        // but a task cancelled while the beat was in flight stays
                        // cancelled.
                        if crate::state::set_task_status_by_loop(t, TaskStatus::Completed) {
                            t.progress = 100;
                        }
                    }
                }
            }

            // Move the mission itself only when a human is needed. Everything
            // else is just progress, and progress does not need a decision.
            if !outcome.rate_limited && matches!(status, BeatStatus::Blocked | BeatStatus::Complete)
            {
                let mut missions = control.missions.write().await;
                if let Some(m) = missions.iter_mut().find(|m| m.id == mission_id) {
                    m.status = crate::state::MissionStatus::UserInputNeeded;
                    m.updated_at = Utc::now().to_rfc3339();
                }
            }

            let (kind, headline) = if outcome.rate_limited {
                (
                    OperatorFeedKind::AgentRunCompleted,
                    "Waiting on your Claude window",
                )
            } else if engine_failed {
                (OperatorFeedKind::AgentRunFailed, "Beat failed")
            } else {
                match status {
                    BeatStatus::Complete => (
                        OperatorFeedKind::MissionUserInputNeeded,
                        "Ready for your review",
                    ),
                    BeatStatus::Blocked => (
                        OperatorFeedKind::MissionUserInputNeeded,
                        "Blocked — needs you",
                    ),
                    BeatStatus::Continuing => (OperatorFeedKind::AgentRunCompleted, "Progress"),
                }
            };
            control
                .push_operator_feed_surface(
                    kind,
                    format!("{headline}: {mission_title}"),
                    Some(detail),
                    Some(entity_id),
                    None,
                )
                .await;
        });

        true
    }

    async fn set_meta(&self, task_id: &str, pairs: &[(&str, &str)]) {
        let mut tasks = self.control.tasks.write().await;
        if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
            let meta = t.metadata.get_or_insert_with(|| serde_json::json!({}));
            if let Some(obj) = meta.as_object_mut() {
                for (k, v) in pairs {
                    obj.insert((*k).to_string(), serde_json::json!(v));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Task;

    fn beat_task(meta: serde_json::Value) -> Task {
        Task {
            task_kind: Some(BEAT_TASK_KIND.to_string()),
            status: TaskStatus::InProgress,
            metadata: Some(meta),
            ..Task::default()
        }
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-28T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn test_mission(title: &str, goal: &str, summary: &str, notes: Vec<String>) -> Mission {
        Mission {
            id: "mis-1".to_string(),
            entity_id: "ent-1".to_string(),
            title: title.to_string(),
            summary: summary.to_string(),
            status: crate::state::MissionStatus::Active,
            priority: "medium".to_string(),
            goal: goal.to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-07-01T00:00:00Z".to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: None,
            canonical_notes: notes,
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        }
    }

    #[test]
    fn a_complete_trailer_is_recognised_with_its_summary() {
        let (status, summary) = parse_status_trailer(
            "did some work\nMISSION_STATUS: complete — the CHANGELOG generator ships",
        );
        assert_eq!(status, BeatStatus::Complete);
        assert_eq!(summary.as_deref(), Some("the CHANGELOG generator ships"));
    }

    #[test]
    fn blocked_and_continuing_are_recognised() {
        assert_eq!(
            parse_status_trailer("MISSION_STATUS: blocked — need the API key").0,
            BeatStatus::Blocked
        );
        assert_eq!(
            parse_status_trailer("MISSION_STATUS: continuing — wrote the parser").0,
            BeatStatus::Continuing
        );
    }

    #[test]
    fn a_missing_trailer_means_keep_going_never_done() {
        // The dangerous failure is a false "complete", so silence must not mean done.
        let (status, summary) = parse_status_trailer("I finished everything, all done!");
        assert_eq!(status, BeatStatus::Continuing);
        assert!(summary.is_none());
    }

    #[test]
    fn the_last_trailer_wins_over_a_quoted_instruction() {
        // The agent often echoes the instruction before actually reporting.
        let output = "I was told to end with MISSION_STATUS: complete — <one sentence>\n\
                      ...did the work...\n\
                      MISSION_STATUS: continuing — still migrating callers";
        let (status, summary) = parse_status_trailer(output);
        assert_eq!(status, BeatStatus::Continuing);
        assert_eq!(summary.as_deref(), Some("still migrating callers"));
    }

    #[test]
    fn markdown_decoration_around_the_trailer_is_tolerated() {
        assert_eq!(
            parse_status_trailer("**MISSION_STATUS: complete — done").0,
            BeatStatus::Complete
        );
        assert_eq!(
            parse_status_trailer("- MISSION STATUS: blocked - waiting").0,
            BeatStatus::Blocked
        );
    }

    #[test]
    fn a_first_beat_with_no_schedule_is_due_immediately() {
        assert!(beat_is_due(&beat_task(serde_json::json!({})), now()));
    }

    #[test]
    fn a_future_beat_is_not_due_and_a_past_one_is() {
        let future = beat_task(serde_json::json!({ META_NEXT_BEAT_AT: "2026-07-28T18:00:00Z" }));
        assert!(!beat_is_due(&future, now()));
        let past = beat_task(serde_json::json!({ META_NEXT_BEAT_AT: "2026-07-28T06:00:00Z" }));
        assert!(beat_is_due(&past, now()));
    }

    #[test]
    fn a_paused_engine_never_beats() {
        let t = beat_task(serde_json::json!({ META_PAUSED: "true" }));
        assert!(!beat_is_due(&t, now()));
    }

    #[test]
    fn the_budget_cap_stops_unattended_spend() {
        let over = beat_task(serde_json::json!({ META_BUDGET_USD: "5", META_COST_USD: "5.01" }));
        assert!(!beat_is_due(&over, now()));
        let under = beat_task(serde_json::json!({ META_BUDGET_USD: "5", META_COST_USD: "1.20" }));
        assert!(beat_is_due(&under, now()));
    }

    #[test]
    fn the_beat_cap_stops_a_runaway_loop() {
        let done = beat_task(serde_json::json!({ META_MAX_BEATS: "3", META_BEAT_COUNT: "3" }));
        assert!(!beat_is_due(&done, now()));
        let left = beat_task(serde_json::json!({ META_MAX_BEATS: "3", META_BEAT_COUNT: "2" }));
        assert!(beat_is_due(&left, now()));
    }

    #[test]
    fn a_finished_or_cancelled_companion_task_stops_beating() {
        for status in [
            TaskStatus::Completed,
            TaskStatus::Cancelled,
            TaskStatus::Failed,
        ] {
            let mut t = beat_task(serde_json::json!({}));
            t.status = status;
            assert!(!beat_is_due(&t, now()));
        }
    }

    #[test]
    fn a_corrupt_schedule_does_not_wedge_the_mission_forever() {
        let t = beat_task(serde_json::json!({ META_NEXT_BEAT_AT: "not-a-timestamp" }));
        assert!(beat_is_due(&t, now()));
    }

    #[test]
    fn other_task_kinds_are_ignored() {
        let mut t = beat_task(serde_json::json!({}));
        t.task_kind = Some("reminder".to_string());
        assert!(!beat_is_due(&t, now()));
    }

    #[test]
    fn the_beat_prompt_carries_goal_notes_and_where_it_left_off() {
        let mission = test_mission(
            "Ship the CLI",
            "A working `cw` binary on brew",
            "Started from the Rust workspace",
            vec!["Never break the public API".to_string()],
        );
        let p = build_beat_prompt(
            &mission,
            &mission.canonical_notes,
            Some("wrote the parser"),
            4,
            25,
        );
        assert!(p.contains("beat #4"));
        assert!(p.contains("Ship the CLI"));
        assert!(p.contains("A working `cw` binary on brew"));
        assert!(p.contains("Never break the public API"));
        assert!(p.contains("wrote the parser"));
        assert!(
            p.contains("PROGRESS.md"),
            "continuity survives losing the session store"
        );
        assert!(
            p.contains("MISSION_STATUS:"),
            "the trailer must be requested"
        );
        assert!(p.contains("git status"), "trust the tree over memory");
    }

    #[test]
    fn the_first_beat_prompt_omits_a_nonexistent_handoff() {
        let mission = test_mission("Ship the CLI", "", "", vec![]);
        let p = build_beat_prompt(&mission, &[], None, 1, 25);
        assert!(!p.contains("WHERE YOU LEFT OFF"));
        assert!(!p.contains("## Skills"));
    }

    #[test]
    fn skills_ride_the_beat_prompt_before_the_working_rules() {
        let mission = test_mission("Ship the CLI", "", "", vec![]);
        let block = "## Skills\n- `release-checklist` — Ship safely. Use when releasing. — /x/SKILL.md";
        let p = build_beat_prompt_with_skills(&mission, &[], None, 2, 25, Some(block));
        let i_skills = p.find("release-checklist").unwrap();
        let i_rules = p.find("HOW TO WORK THIS BEAT").unwrap();
        assert!(i_skills < i_rules);
    }

    #[test]
    fn a_spent_window_schedules_the_next_beat_at_the_reset() {
        // The operator should never have to come back and say "keep going":
        // the CLI told us when the window reopens, so we wait exactly that long.
        let resets = (now() + ChronoDuration::hours(3)).timestamp();
        let next = next_beat_after_rate_limit(now(), Some(resets));
        let parsed = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        assert!(parsed > now() + ChronoDuration::hours(2));
        // Slightly past the reset, so we don't come back a second too early.
        assert!(parsed >= DateTime::from_timestamp(resets, 0).unwrap());
    }

    #[test]
    fn a_reset_already_in_the_past_floors_to_a_short_retry() {
        let past = (now() - ChronoDuration::hours(1)).timestamp();
        let next = next_beat_after_rate_limit(now(), Some(past));
        let parsed = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        // Never immediate (that would just hammer a closed window)...
        assert!(parsed > now());
        // ...and never far away either.
        assert!(parsed <= now() + ChronoDuration::minutes(6));
    }

    #[test]
    fn a_rate_limit_without_a_reset_time_still_retries() {
        let next = next_beat_after_rate_limit(now(), None);
        let parsed = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        assert!(parsed > now());
        assert!(parsed <= now() + ChronoDuration::minutes(6));
    }

    #[test]
    fn the_next_beat_is_scheduled_an_interval_out() {
        let next = next_beat_at(now(), 240);
        let parsed = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!((parsed - now()).num_minutes(), 240);
    }

    #[test]
    fn a_nonsense_interval_still_schedules_forward() {
        let next = next_beat_at(now(), 0);
        let parsed = DateTime::parse_from_rfc3339(&next)
            .unwrap()
            .with_timezone(&Utc);
        assert!(parsed > now());
    }

    #[test]
    fn an_absurd_interval_or_reset_never_panics() {
        // These used to overflow `Duration::minutes` / `DateTime + Duration`
        // inside a beat's completion handler, after the run was paid for.
        for interval in [i64::MAX, i64::MIN, -1, MAX_INTERVAL_MINUTES + 1] {
            let parsed = DateTime::parse_from_rfc3339(&next_beat_at(now(), interval))
                .unwrap()
                .with_timezone(&Utc);
            assert!(parsed > now(), "{interval}");
            assert!(
                parsed <= now() + ChronoDuration::minutes(MAX_INTERVAL_MINUTES),
                "{interval} is clamped to a year"
            );
        }
        for resets in [i64::MAX, i64::MIN, 8_210_266_876_799] {
            let parsed =
                DateTime::parse_from_rfc3339(&next_beat_after_rate_limit(now(), Some(resets)))
                    .unwrap()
                    .with_timezone(&Utc);
            assert!(parsed > now(), "{resets}");
        }
    }
}
