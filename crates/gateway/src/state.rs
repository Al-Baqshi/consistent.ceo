//! Shared state: ControlState + ServerState (with control_state) to avoid server <-> control_api cycle.
//! ControlState can be snapshotted to/from JSON for persistence.

use chrono::{DateTime, Utc};
use consistent_world_common::telemetry::CostTracker;
use consistent_world_memory::embedding::EmbeddingProvider;
use consistent_world_memory::store::MemoryStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::config;
use crate::config::AppConfig;
use crate::scheduler::Scheduler;
use consistent_world_common::capabilities::{CapabilityId, ExecutionLevel};
use consistent_world_common::types::ScopeType;

// ─── Control types (moved from control_api so state doesn’t depend on control_api) ───

/// A unit of work on the canonical `tasks` flow: created by chat, a heartbeat,
/// or an importer, picked up by a director or an external agent, and driven
/// to a terminal [`TaskStatus`] via [`set_task_status`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub dept: String,
    pub roles: Vec<String>,
    pub status: TaskStatus,
    pub tries: String,
    pub progress: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// When true, orchestrator will not auto-pick; task surfaces for human approval.
    #[serde(default)]
    pub requires_human_approval: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_director_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_agent_id: Option<String>,
    /// Connection that produced or owns this task, when created from an integration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    /// External producer name, e.g. `openclaw`, `github`, `stripe`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_source: Option<String>,
    /// Producer-stable id used for idempotency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    /// LLM output when task is completed by run_agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Task kind for routing (e.g. entity-enrichment, prompt-interpretation, content-draft). String for flexibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_kind: Option<String>,
    /// Flexible integration payload or structured task context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<TaskOutcome>,
    /// RFC3339 timestamp for when the task record was created.
    #[serde(default)]
    pub created_at: String,
    /// RFC3339 deadline. Drives the overdue sweep and the daily review; absent
    /// means the task is not time-bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
    /// RFC3339 stamp of the first transition into [`TaskStatus::Completed`].
    /// Consistency (streaks, per-day counts) is computed from this field, so it
    /// is written once and never overwritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    /// RFC3339 stamp of the most recent status change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_changed_at: Option<String>,
    /// Set when this task was auto-created as a follow-up (e.g. after `prompt-interpretation`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_task_id: Option<String>,
}

/// Read a string value from a task's `metadata` object. Used for durable-loop
/// bookkeeping (`startedAt`, `nextRetryAt`) so no new columns are needed.
pub fn task_meta_str(task: &Task, key: &str) -> Option<String> {
    task.metadata
        .as_ref()?
        .get(key)?
        .as_str()
        .map(str::to_string)
}

/// Set a string value in a task's `metadata` object, creating it if absent.
pub fn task_meta_set_str(task: &mut Task, key: &str, value: &str) {
    let meta = task
        .metadata
        .get_or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if !meta.is_object() {
        *meta = serde_json::Value::Object(serde_json::Map::new());
    }
    if let Some(map) = meta.as_object_mut() {
        map.insert(
            key.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
}

/// Remove a key from a task's `metadata` object.
pub fn task_meta_clear(task: &mut Task, key: &str) {
    if let Some(map) = task.metadata.as_mut().and_then(|v| v.as_object_mut()) {
        map.remove(key);
    }
}

/// Move a task's status **on a human's instruction**: a chat "reopen", the requeue
/// button, an approval decision, a cancel from the UI.
///
/// Unguarded on purpose — un-cancelling is a real, shipped, human-initiated flow,
/// and a person who says "start this again" outranks the stored state. Background
/// loops must use [`set_task_status_by_loop`] instead, which refuses to overrule a
/// cancel. The two exist so the *actor* is stated at every call site rather than
/// inferred from which file it happens to be in.
///
/// Stamping `completed_at` here (rather than at each of the call sites that finish
/// a task) is what makes consistency computable: task rows are never deleted, so
/// once every completion carries a timestamp, streaks and per-day counts fall out
/// of the task list alone. `completed_at` is written once — a task that is
/// re-completed keeps its original date. `status_changed_at` moves every time,
/// which is why assigning `task.status` directly is a bug even when the status is
/// right: the timestamps silently stop tracking reality.
pub fn set_task_status_by_human(task: &mut Task, status: TaskStatus) {
    let now = chrono::Utc::now().to_rfc3339();
    task.status = status;
    task.status_changed_at = Some(now.clone());
    if status == TaskStatus::Completed && task.completed_at.is_none() {
        task.completed_at = Some(now);
    }
}

/// Move a task's status **from a background loop**. Refuses to move it *off*
/// `Cancelled`.
///
/// The operator's stop button outranks any run still in flight. Nine loops can
/// close a task and none consults the others, so a run that finishes after the
/// human cancelled it would otherwise rewrite the status to `Completed` — and a
/// crash-retry branch could then put it back on `Queued` and launch a fresh
/// unattended run minutes after the human said stop.
///
/// Returns `false` (leaving the task untouched) when it refuses, so the caller can
/// still record what the run produced without claiming the task finished. It is
/// `#[must_use]` because ignoring the answer is how the cancel gets overwritten
/// anyway: the interesting branch is always the refusal.
///
/// Not the default, and not "the strict one" — the pair is split by actor. A human
/// asking to reopen goes through [`set_task_status_by_human`].
#[must_use]
pub fn set_task_status_by_loop(task: &mut Task, status: TaskStatus) -> bool {
    if task.status == TaskStatus::Cancelled && status != TaskStatus::Cancelled {
        return false;
    }
    set_task_status_by_human(task, status);
    true
}

/// A task's deadline, preferring the first-class field and falling back to the
/// `due_at` metadata key that reminders have always used.
pub fn task_due_at(task: &Task) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = task
        .due_at
        .clone()
        .or_else(|| task_meta_str(task, crate::reminders::REMINDER_DUE_AT_KEY))?;
    chrono::DateTime::parse_from_rfc3339(&raw)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Task kinds that are bookkeeping rather than work the operator owes anyone.
/// They carry due dates for their own schedulers, so the overdue sweep and the
/// daily review both skip them.
pub fn task_kind_is_internal(task: &Task) -> bool {
    matches!(
        task.task_kind.as_deref(),
        Some("reminder") | Some("note") | Some(crate::mission_engine::BEAT_TASK_KIND)
    )
}

/// Overdue is derived, never a stored status: adding a `TaskStatus` variant
/// would change the terminal-state arithmetic in `mission_dod` and
/// `mission_goal_loop`, which the goal loop depends on.
pub fn task_is_overdue(task: &Task, now: chrono::DateTime<chrono::Utc>) -> bool {
    if task_kind_is_internal(task) {
        return false;
    }
    if matches!(
        task.status,
        TaskStatus::Completed | TaskStatus::Cancelled | TaskStatus::Failed
    ) {
        return false;
    }
    task_due_at(task).is_some_and(|due| due < now)
}

/// Lifecycle state of a [`Task`]. `Completed`, `Cancelled`, and `Failed` are
/// terminal — the orchestrator loop never picks the task up again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskStatus {
    #[default]
    Backlog,
    Queued,
    InProgress,
    Failed,
    Completed,
    /// Explicitly stopped by a user or a cancelled parent mission. Terminal; the
    /// loop never picks it up again.
    Cancelled,
}

/// Finer-grained verdict on a finished task, layered on top of [`TaskStatus`]
/// so the orchestrator can tell "done" from "done but needs a human" or
/// "worth retrying with a stronger model".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskOutcome {
    Completed,
    NeedsReview,
    BlockedByPolicy,
    NeedsHumanApproval,
    NeedsStrongerModel,
    RetryableFailure,
    /// Terminal failure (e.g. enrichment LLM misconfiguration); do not retry automatically.
    Failed,
}

/// Lifecycle state of a [`Mission`]. `Completed` and `Cancelled` are terminal;
/// the goal loop only advances missions still in an active-ish state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MissionStatus {
    Planned,
    Active,
    Blocked,
    UserInputNeeded,
    Completed,
    /// Explicitly stopped by a user. Terminal; the goal loop ignores it.
    Cancelled,
}

/// A goal-oriented body of work for an [`Entity`], optionally driven by the
/// autonomous goal loop ([`MissionLoopConfig`] / [`MissionLoopState`]) rather
/// than by one-off tasks alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mission {
    pub id: String,
    pub entity_id: String,
    pub title: String,
    pub summary: String,
    pub status: MissionStatus,
    pub priority: String,
    pub goal: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_reviewed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_cadence_hours: Option<u32>,
    /// Markdown story-so-far. Always injected into the system prompt as canonical
    /// grounding, never compacted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub narrative: Option<String>,
    /// Pinned notes that always recall with this mission (canonical layer).
    #[serde(default)]
    pub canonical_notes: Vec<String>,
    /// Optional sticky assignment to a director row. Resolution falls back to
    /// the recommendation engine when None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_director_id: Option<String>,
    /// Concise final report (Markdown), generated when the mission completes or
    /// stops needing user input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_report: Option<String>,
    /// Present = this mission runs the iterate-until-done loop: heartbeats
    /// replan executable work from prior results until the definition of done
    /// is met or a cap trips.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_config: Option<MissionLoopConfig>,
    /// Progress of the loop; only meaningful while `loop_config` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_state: Option<MissionLoopState>,
}

/// Iteration and budget caps for a loop-enabled mission. The caps are the
/// safety story: exhausting either hard-stops the loop to `UserInputNeeded`
/// instead of iterating unattended forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionLoopConfig {
    /// What "finished" means, in the operator's words. Judged by the critic
    /// director against evidence, not by the builder's own claim.
    pub definition_of_done: String,
    #[serde(default = "default_loop_max_iterations")]
    pub max_iterations: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_usd: Option<f64>,
    /// Reviewer director for iteration verdicts; None = the seeded verifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critic_director_id: Option<String>,
}

/// `serde(default)` value for [`MissionLoopConfig::max_iterations`].
pub fn default_loop_max_iterations() -> u32 {
    5
}

/// No configuration may push a loop past this many replan rounds.
pub const LOOP_MAX_ITERATIONS_HARD_CAP: u32 = 25;

impl MissionLoopConfig {
    /// Clamps the configured value into `[1, LOOP_MAX_ITERATIONS_HARD_CAP]`,
    /// so no mission config can push the loop past the hard cap.
    pub fn effective_max_iterations(&self) -> u32 {
        self.max_iterations.clamp(1, LOOP_MAX_ITERATIONS_HARD_CAP)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MissionLoopState {
    /// Replan rounds consumed so far.
    #[serde(default)]
    pub iteration: u32,
    /// Builder + reviewer spend attributed to the loop.
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verdict: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_advanced_at: Option<String>,
}

/// A directed link from one entity to another (or an external party), e.g.
/// "vendor", "competitor", "customer".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    #[serde(rename = "type")]
    pub relationship_type: String,
    pub target: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// A filesystem root (or similar resource) an entity may read/write/execute
/// against, with per-capability flags gating what tools are allowed to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityResource {
    pub id: String,
    pub kind: String,
    pub root: String,
    #[serde(default)]
    pub read_allowed: bool,
    #[serde(default)]
    pub write_allowed: bool,
    #[serde(default)]
    pub exec_allowed: bool,
}

/// A live integration bound to an entity (social account, mailbox, domain,
/// API service, ...). Tracks auth/verification status plus the credential
/// reference (never the secret itself — see `secret_ref`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityConnection {
    pub id: String,
    pub entity_id: String,
    /// social_account | email_account | domain | api_service | website | storage
    pub kind: String,
    /// twitter | gmail | outlook | manual_dns | cloudflare | vercel | box ...
    pub provider: String,
    pub label: String,
    /// disconnected | pending_auth | connected | verification_required | limited | error | revoked
    pub status: String,
    #[serde(default)]
    pub capability_state: serde_json::Value,
    #[serde(default)]
    pub config: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_account_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_handle: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_checked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// In-flight OAuth/device-code handshake for connecting an [`EntityConnection`].
/// Short-lived: resolves into a connection (or expires) once auth completes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityConnectionSession {
    pub id: String,
    pub entity_id: String,
    pub provider: String,
    #[serde(default)]
    pub requested_capabilities: Vec<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pkce_verifier_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub poll_interval_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_message: Option<String>,
    /// pending | pending_auth | config_required | completed | failed | expired | cancelled
    pub status: String,
    pub expires_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: String,
}

/// What happened to an [`EntityConnection`], for the audit trail
/// ([`EntityConnectionEvent`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityConnectionEventKind {
    ConnectionCreated,
    OAuthStarted,
    OAuthCompleted,
    VerificationRequested,
    VerificationSucceeded,
    ConnectionReauthRequired,
    ConnectionValidated,
    ConnectionTestFailed,
    ConnectionDisconnected,
    ActionAttempted,
    ActionBlockedByPolicy,
    ActionSucceeded,
    /// Credential / secret replaced via API (rotation).
    SecretRotated,
}

/// One row of the append-only audit trail for an [`EntityConnection`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityConnectionEvent {
    pub id: String,
    pub timestamp: String,
    pub entity_id: String,
    pub connection_id: String,
    pub kind: EntityConnectionEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Entity-level allow/deny/approval lists that gate which tool capabilities
/// and execution levels an agent may use on this entity's behalf.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPolicies {
    #[serde(default)]
    pub allowed_capabilities: Vec<CapabilityId>,
    #[serde(default)]
    pub allowed_execution_levels: Vec<ExecutionLevel>,
    #[serde(default)]
    pub approval_required_capabilities: Vec<CapabilityId>,
}

/// Resolved runtime scope for an instruction/task boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedScope {
    pub scope_type: ScopeType,
    pub scope_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution_reason: Option<String>,
}

/// Optional channel/thread-level binding for default scope resolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChannelScopeBinding {
    pub channel: String,
    pub peer_pattern: String,
    pub scope_type: ScopeType,
    pub scope_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Fully-resolved policy + workspace context for one instruction/task, built
/// from a [`ResolvedScope`] plus the owning entity's [`ExecutionPolicies`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionContext {
    pub scope: ResolvedScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(default)]
    pub allowed_resource_roots: Vec<String>,
    #[serde(default)]
    pub allowed_capabilities: Vec<CapabilityId>,
    #[serde(default)]
    pub allowed_execution_levels: Vec<ExecutionLevel>,
    #[serde(default)]
    pub approval_required_capabilities: Vec<CapabilityId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_model_tier: Option<String>,
}

/// Local wall-clock quiet window (HH:MM strings, entity operator timezone implied).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuietHoursLocal {
    pub start: String,
    pub end: String,
}

/// Entity-level planner / automation mode (product default: keep routing simple).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AutomationMode {
    /// No automatic follow-ups or mission auto-create; heartbeat skips entity tasks.
    #[default]
    Manual,
    /// Create suggested work but mark for human approval where configured.
    Suggest,
    /// Full automatic pickup subject to policy + approvals.
    AutoRun,
}

/// Whether heartbeat-driven automation runs for this entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum EntityHeartbeatMode {
    #[default]
    Off,
    Suggest,
    Execute,
}

/// Persisted per-entity automation and chat/heartbeat preferences.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityRuntimeConfig {
    #[serde(default)]
    pub automation_mode: AutomationMode,
    #[serde(default)]
    pub heartbeat_mode: EntityHeartbeatMode,
    /// Primary chat session id for “Talk to this entity” (webhook / internal).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_chat_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_hours_local: Option<QuietHoursLocal>,
}

/// Resolved view for APIs and engines (execution context + runtime knobs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityRuntimeContext {
    pub entity_id: String,
    pub automation_mode: AutomationMode,
    pub heartbeat_mode: EntityHeartbeatMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_chat_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_hours_local: Option<QuietHoursLocal>,
    pub execution: ExecutionContext,
}

/// State of an [`ApprovalRequest`] in the human-in-the-loop queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

/// First-class approval queue item (tasks, connection actions, etc.).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub entity_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    pub action: String,
    pub status: ApprovalStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// Result of a policy check against an attempted capability use, recorded on
/// the corresponding [`ActionEvent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionOutcome {
    Allowed,
    Denied,
    ApprovalRequired,
    Failed,
}

/// Audit-log row for a single policy-gated capability attempt (allowed,
/// denied, needed approval, or failed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionEvent {
    pub id: String,
    pub timestamp: String,
    pub scope_type: ScopeType,
    pub scope_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub capability: CapabilityId,
    pub execution_level: ExecutionLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub outcome: ActionOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// High-level operator visibility: task lifecycle and approval signals (bounded in-memory feed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperatorFeedKind {
    TaskCreated,
    TaskCompleted,
    TaskBlocked,
    /// A retryable failure was scheduled for another attempt (with backoff).
    TaskRetryScheduled,
    /// A task/run was InProgress too long and was force-failed by the watchdog.
    TaskStuck,
    /// A task was cancelled by a user or a cancelled parent mission.
    TaskCancelled,
    /// A mission was cancelled by a user.
    MissionCancelled,
    /// A mission's final report was generated.
    MissionReportReady,
    /// A task completed but the adversarial verifier flagged its output for human review.
    VerificationNeedsReview,
    ApprovalNeeded,
    MissionCreated,
    /// The mission's Definition of Done was met; it was auto-completed.
    MissionCompleted,
    /// The *first* mission in this workspace ever reached definition-of-done.
    /// Emitted once, and never again — it is the activation moment, and a
    /// counter alone cannot say when it happened.
    MissionFirstCompleted,
    /// The autonomous goal loop recorded a per-mission decision this tick.
    GoalLoopDecision,
    /// A loop-enabled mission advanced one develop-verify iteration: new work
    /// was queued from the last round's results.
    MissionIterationAdvanced,
    MissionStalled,
    MissionMissingNextTask,
    MissionReviewDue,
    MissionFollowupSuppressed,
    MissionRecommendationAccepted,
    MissionRecommendationDismissed,
    MissionSnoozed,
    MissionUserInputNeeded,
    EntitySourceImported,
    ApprovalRequestCreated,
    ApprovalRequestResolved,
    /// Domain / connection verification succeeded (operator feed; `entity_id` + `connection_id` set).
    ConnectionVerified,
    /// Connection test or verification failed (`detail` has normalized message when available).
    ConnectionCheckFailed,
    /// A cron-scheduled task fired; `detail` carries the agent reply (truncated) or failure note.
    ScheduledTaskFired,
    /// A director heartbeat produced a summary worth surfacing. Mirrors the row
    /// written to `heartbeat_results` so beats reach the feed (and from there
    /// mobile/desktop notifications), not just the entity's chat bubbles.
    HeartbeatResult,
    /// A long-running external agent run (Claude Code et al.) finished cleanly.
    AgentRunCompleted,
    /// An external agent run failed, stalled, or was force-stopped.
    AgentRunFailed,
    /// A task passed its due date without being finished. Re-emitted once more
    /// after 24h so an ignored commitment escalates instead of going quiet.
    TaskOverdue,
    /// The single cross-entity morning brief: what is overdue, due today, and
    /// waiting on a decision. Deliberately one push per day, not one per entity.
    DailyDigest,
    /// An entity has work an agent could run but nowhere the operator actually
    /// chose for it to happen. Asked once per entity, never once per task.
    EntityWorkspaceNeeded,
}

/// One row in the bounded in-memory operator feed described by
/// [`OperatorFeedKind`] above.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperatorFeedEvent {
    pub id: String,
    pub timestamp: String,
    pub kind: OperatorFeedKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    /// Task / mission / approval entity when known (for scoped activity UI).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
}

/// Who can see an [`Entity`]. `Private` is the default so a freshly created
/// entity (e.g. a Personal Space) never leaks to teammates by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum EntityVisibility {
    Global,
    Workspace,
    #[default]
    Private,
}

/// Mermaid source texts for entity strategy / architecture visualization.
/// "Mirimai ID" = open the `.mmd` text in Cursor (or any Mermaid preview) to see where
/// the entity started, where it is, and where it is heading.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityFlowDiagrams {
    /// Prior-era flow (archive — rotate into here when current advances).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// As-built architecture / strategy right now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// Target-state flow the entity is building toward.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub future: Option<String>,
    /// ISO timestamp of the last time `current` was updated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

/// A person or project the app operates on behalf of — the top-level unit
/// that owns tasks, missions, connections, heartbeats, and chat context.
/// Includes both real business entities and a user's private Personal Space
/// (`is_personal = true`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entity {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub department: Option<String>,
    pub owner: Option<String>,
    pub status: String,
    #[serde(rename = "type")]
    pub entity_type: Option<String>,
    #[serde(default)]
    pub dept: String,
    #[serde(default)]
    pub file_count: u32,
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub visibility: EntityVisibility,
    /// True when this entity is a user's private Personal Space (auto-provisioned
    /// on sign-in). Personal entities are visible only to their owner, never to
    /// teammates who share the same workspace.
    #[serde(default)]
    pub is_personal: bool,
    /// The `principal_id` that owns this Personal Space. Source of truth for
    /// per-user isolation (the id string is opaque). `None` for business entities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub personal_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<EntityFile>>,
    // Entity hub fields (from entities_seed.json)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub markets: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goals: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_rules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_pipeline: Option<String>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enrichment: Option<EntityEnrichment>,
    #[serde(default)]
    pub resources: Vec<EntityResource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_policies: Option<ExecutionPolicies>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_workspace_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_model_tier: Option<String>,
    #[serde(default)]
    pub allowed_agent_groups: Vec<String>,
    #[serde(default)]
    pub setup: EntitySetup,
    #[serde(default)]
    pub readiness: EntityReadiness,
    /// Automation, chat binding, heartbeat, quiet hours (optional; legacy entities use orchestrator defaults).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_runtime: Option<EntityRuntimeConfig>,
    /// Accent colour (hex, e.g. `#a78bfa`) used by the chat UI to tint the
    /// surface when this entity is active. Optional so legacy entities load
    /// cleanly; the frontend falls back to a neutral default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub colour: Option<String>,
    /// Mirimai ID slots: previous / current / future Mermaid flowcharts for strategy visibility.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow_diagrams: Option<EntityFlowDiagrams>,
    /// State of the knowledge index built from this entity's source (repo,
    /// folder, or URL) into `entity_docs` — see [`crate::entity_knowledge`].
    /// Absent means the source was never indexed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_index: Option<EntityKnowledgeIndex>,
}

/// Where the entity's knowledge index stands. Written by
/// [`crate::entity_knowledge::index_entity`] at start (`indexing`) and end
/// (`ready` / `failed`), or `skipped` when the entity has no indexable source.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EntityKnowledgeIndex {
    /// `indexing` | `ready` | `failed` | `skipped`.
    pub status: String,
    /// What was indexed: a local path or a URL.
    #[serde(default)]
    pub source: String,
    /// `local_repo` | `local_folder` | `github_repo` | `website`.
    #[serde(default)]
    pub source_kind: String,
    #[serde(default)]
    pub doc_count: usize,
    /// How many of those docs also got an embedding (semantic recall).
    #[serde(default)]
    pub embedded_count: usize,
    #[serde(default)]
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Deterministic, colon-free id for a user's Personal Space entity.
///
/// Colon-free because SurrealDB uses `:` as the table/key separator inside a
/// record id, which is fragile through the `RecordId` round-trip. Scoped by
/// `(workspace_id, principal_id)` so a user who belongs to two workspaces gets a
/// distinct personal space in each. The id string is opaque — per-user isolation
/// keys off the `personal_user_id` field, never this string.
pub fn personal_space_id(workspace_id: &str, principal_id: &str) -> String {
    fn sanitize(s: &str) -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    }
    format!(
        "personal_{}_{}",
        sanitize(workspace_id),
        sanitize(principal_id)
    )
}

/// Onboarding answers captured for an entity (what it is, its goal, its
/// stack, where its work should live). Feeds [`EntityReadiness`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntitySetup {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub what_it_is: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_goal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success_criteria: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontend_stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_stack: Option<String>,
    /// What the operator answered when asked where this entity's work should
    /// live: `repo`, `local_folder`, or `vault_only`. Absent means *never
    /// asked* — which is a different thing from "the vault is fine", and the
    /// whole reason this field exists. See [`crate::entity_workspace`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_choice: Option<String>,
    /// RFC3339 instant the operator was last asked the question. Entity-level
    /// on purpose: an undecided entity with forty queued tasks asks once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_asked_at: Option<String>,
}

/// Derived "how set up is this entity" summary shown in the UI, with the
/// next onboarding question to ask when incomplete.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityReadiness {
    pub percent: u8,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_question: Option<String>,
    #[serde(default)]
    pub missing_fields: Vec<String>,
}

/// Lightweight file listing entry (name + display size) shown on an entity's
/// file tab; not the canonical upload record (see [`UploadedEntityFile`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityFile {
    pub name: String,
    pub size: String,
}

/// Canonical record of a file a user uploaded to an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadedEntityFile {
    pub id: String,
    pub entity_id: String,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub uploaded_at: String,
    pub path: String,
}

/// A file attached to one chat session's message (as opposed to an
/// entity-level upload — see [`UploadedEntityFile`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatAttachment {
    pub id: String,
    pub filename: String,
    pub content_type: String,
    pub size_bytes: u64,
    pub session_id: String,
    pub uploaded_at: String,
    pub path: String,
}

/// Lifecycle of a raw capture as it moves from the inbox toward a durable
/// artifact (task, memory, narrative) or gets dismissed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawCaptureStatus {
    #[default]
    Raw,
    Dismissed,
    PromotedToTask,
    PromotedToMemory,
    NarrativeUpdated,
}

/// A raw, pre-interpretation capture from chat, an external CLI agent run, or an
/// importer. Captures land in the workspace inbox (or an entity's `raw/`) before
/// any trust decision is made. SurrealDB / control state stays canonical; the
/// local vault mirror is an inspectable copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawCapture {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    /// Producer channel, e.g. `chat`, `external_agent`, `import`.
    pub source: String,
    /// Optional producer-stable reference (run id, url, message id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Model/provider that produced this capture when relevant
    /// (`claude` | `opencode` | `cursor-agent`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub raw_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default)]
    pub status: RawCaptureStatus,
    /// Set when the capture was promoted into a task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub promoted_task_id: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub created_at: String,
    pub updated_at: String,
}

/// Durable status record for an external coding-agent CLI run. The process
/// handle itself is runtime-only, but this record survives gateway restarts so
/// the UI and local vault always have an auditable trail.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalAgentRun {
    pub id: String,
    pub workspace_id: String,
    pub entity_id: String,
    pub provider: String,
    pub prompt: String,
    pub cwd: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default)]
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    /// Review policy chosen for this run: `none` | `manual` | `auto`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_policy: Option<String>,
    /// Review lifecycle indicator: `pending_review` | `review_running` |
    /// `reviewed` | `needs_review`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_status: Option<String>,
    /// Deterministic reviewer verdict: `pass` | `needs_review`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_verdict: Option<String>,
    /// On a primary run: the reviewer run that checked it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_run_id: Option<String>,
    /// On a reviewer run: the primary run it is reviewing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewed_run_id: Option<String>,
}

/// A first-class verification run: a shell test command or a browser URL check,
/// executed as a task-verification step and linked to a task/mission/entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerificationRun {
    pub id: String,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mission_id: Option<String>,
    /// `shell` | `browser`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// `running` | `passed` | `failed`.
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub logs: String,
    /// Paths to captured artifacts (e.g. screenshots) when available.
    #[serde(default)]
    pub artifacts: Vec<String>,
    /// Follow-up "next action" task created when the verification failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_task_id: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

/// Structured draft fields produced by entity-enrichment (v1 schema only).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichmentDraft {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goals: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operating_rules: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_pipeline: Option<String>,
}

/// Snapshot of entity fields sent to the enrichment model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrichmentInputSnapshot {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub department: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// Entity enrichment metadata: status, error, input snapshot, draft, and audit fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityEnrichment {
    /// pending | running | ready | applied | discarded | failed | provider_invalid | ai_unavailable
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_snapshot: Option<EnrichmentInputSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<EnrichmentDraft>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft_version: Option<u32>,
}

/// A configured LLM provider (name, type, base URL, key source).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProvider {
    pub id: String,
    pub name: String,
    pub provider_type: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

/// A model offered by an [`LlmProvider`]; `is_default` marks the fallback
/// choice when nothing more specific is configured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmModel {
    pub id: String,
    pub provider_id: String,
    pub name: String,
    pub is_default: bool,
}

/// Cloud deployment settings shown in the (legacy) dashboard cloud panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudConfig {
    pub provider: Option<String>,
    pub region: Option<String>,
    pub compute_profile: Option<String>,
}

/// A configured storage backend (legacy dashboard storage panel).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageBackend {
    pub id: String,
    pub name: String,
    pub backend_type: String,
    pub config: Option<HashMap<String, String>>,
}

/// Legacy match-and-route rule (project/department/tag → director/model)
/// predating the newer routing engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestrationRule {
    pub id: String,
    pub match_project: Option<String>,
    pub match_department: Option<String>,
    pub match_tag: Option<String>,
    pub director_id: Option<String>,
    pub model_id: Option<String>,
    pub retry_count: Option<u32>,
}

/// One message in an agent group chat (user or agent).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentMessage {
    pub id: String,
    pub text: String,
    pub time: String,
    pub is_agent: bool,
}

/// A grouping of [`Agent`]s (the seed-file precursor to first-class
/// [`Director`]s).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentGroup {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub description: String,
}

/// A seed-file agent definition (`agents_seed.json`). Superseded for editable
/// use cases by the first-class [`Director`] table, but still the source of
/// the built-in roster.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub id: String,
    pub group_id: String,
    pub name: String,
    pub role: String,
    pub kind: String,
    pub preferred_provider: String,
    pub preferred_model: String,
    pub mission: String,
    pub profile: String,
}

/// Per-director (agent group) orchestration state: heartbeat and which task kinds to auto-pick.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorState {
    pub id: String,
    pub heartbeat_enabled: bool,
    #[serde(default)]
    pub auto_task_kinds: Vec<String>,
}

/// First-class director (canonical row in the `directors` SurrealDB table).
///
/// Directors are editable from chat. Each has its own prompt, model (the
/// "muscle"), tool allowlist, rules (global + per-entity), and an append-only
/// `director_lessons` log scoped to its `id`. The legacy `agents_seed.json` /
/// `director_schemas.rs` mechanism becomes a *recommendation* layer over this
/// table.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Director {
    pub id: String,
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub role: String,
    /// `"global"` or `"entity:{entity_id}"`.
    #[serde(default)]
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Optional link to an agent group (operations, content-creative, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// System prompt / persona / training rules (markdown).
    #[serde(default)]
    pub prompt: String,
    /// The "muscle": which provider/model this director uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Tools this director may invoke.
    #[serde(default)]
    pub tool_allowlist: Vec<String>,
    /// Markdown rules applied across all projects this director works on.
    #[serde(default)]
    pub rules_global: String,
    /// Per-entity rule overlays. Key = entity id, value = markdown.
    #[serde(default)]
    pub rules_per_entity: HashMap<String, String>,
    /// Default heartbeat cadence (cron expression OR `"{secs}s"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_heartbeat: Option<String>,
    #[serde(default)]
    pub version: u32,
    #[serde(default = "default_true")]
    pub is_active: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

fn default_true() -> bool {
    true
}

/// Append-only learning log scoped to a director (`director_lessons` table).
/// Recalled when building the director's system prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectorLesson {
    pub id: String,
    pub director_id: String,
    /// `"global"`, `"entity:{id}"`, or `"mission:{id}"`.
    pub scope: String,
    pub lesson: String,
    /// `"correction"` | `"success"` | `"observation"`.
    pub source: String,
    /// `"hard"` (keep forever) or `"soft"` (decay over time).
    #[serde(default = "default_severity")]
    pub severity: String,
    /// Optional embedding for semantic recall (dim 1536 cosine, matches
    /// the `memories` table).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

fn default_severity() -> String {
    "soft".to_string()
}

/// A Cursor-style reference document attached to an **entity** (`entity_docs`
/// table). Each entity owns its own stack/architecture/conventions, so docs are
/// entity-scoped. Enabled docs are eagerly spliced into the canonical system
/// prompt (between the entity block and the mission block) so the model always
/// sees the entity's ground truth instead of guessing.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityDoc {
    pub id: String,
    pub entity_id: String,
    pub title: String,
    /// Markdown body. For `source == "url"` this is the snapshotted text.
    pub content: String,
    /// `"manual"` | `"url"` | `"file"`.
    #[serde(default = "default_doc_source")]
    pub source: String,
    /// Origin URL when `source == "url"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Whether this doc is spliced into the prompt. Lets a user keep a doc on
    /// file without spending context on it.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional embedding for a future semantic `@`-reference path (dim 1536
    /// cosine, matches `memories` / `director_lessons`). Unused by the v1 eager
    /// injection path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Vec<f32>>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

fn default_doc_source() -> String {
    "manual".to_string()
}

/// One fact about an entity's world, sourced from somewhere outside Consistent:
/// a customer in a CRM, a lead, a deal, a piece of content, a connected
/// account, a file, a metric snapshot, an event. See
/// [`crate::entity_records_store`] for how these are read and written.
///
/// Unlike [`Entity`]'s narrative fields (`profile`, `goals`, ...), a record is
/// structured and always carries where it came from — `source` +
/// `source_ref` — so the Architect's Verify step can resolve a claim against
/// it instead of trusting a paragraph. `kind` is open-ended on purpose: eight
/// names are documented (`contact`, `customer`, `deal`, `content`, `account`,
/// `file`, `metric`, `event`) but any string is accepted, so a new source
/// never waits on a schema change here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityRecord {
    /// Deterministic hash of (entityId, source, kind, sourceRef) — see
    /// [`crate::entity_records_store::record_key`]. Re-posting the same
    /// (source, sourceRef) upserts this row rather than duplicating it.
    pub id: String,
    pub entity_id: String,
    pub kind: String,
    /// Producer, e.g. `"baqshi_crm"`, `"monday"`, `"manual"`.
    pub source: String,
    /// The record's stable id *in that source system* — a CRM row id, a
    /// Monday item id, a URL. Together with `source` this is what makes an
    /// upsert idempotent and what a human can go verify.
    pub source_ref: String,
    /// Open shape — this module does not know a CRM customer from a Monday
    /// item, only that it has fields worth keeping.
    pub fields: serde_json::Value,
    /// Where to go check this claim: a URL, an API response id, a screenshot
    /// reference. Absent when the source has nothing better than its own say-so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    /// When the source last confirmed this row was current — not when it was
    /// first seen. Re-synced rows update this on every sync even if nothing
    /// else changed, so staleness is visible.
    pub fetched_at: String,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// Heartbeat = trigger spec (`heartbeats` table). Decoupled from director and
/// mission so one target can have multiple cadences (e.g. daily standup +
/// on-deploy).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Heartbeat {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// `"entity"` | `"mission"` | `"global"`.
    pub target_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    pub director_id: String,
    /// Either a cron expression or a `"{secs}s"` cadence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cadence_secs: Option<u64>,
    /// Free-text condition label (parsed later — for now informational).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_fire_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_until: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_token: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Seed file format (entities_seed.json). Used only for loading.
#[derive(Debug, Deserialize)]
struct EntitiesSeedFile {
    #[serde(default)]
    _version: u32,
    #[serde(default)]
    _source: Option<String>,
    entities: Vec<Entity>,
}

#[derive(Debug, Deserialize)]
struct AgentsSeedFile {
    #[serde(default)]
    version: u32,
    source: Option<String>,
    groups: Vec<AgentGroup>,
    agents: Vec<Agent>,
}

/// Serializable snapshot for persistence. All fields are plain data (no locks).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlStateSnapshot {
    pub tasks: Vec<Task>,
    pub entities: Vec<Entity>,
    #[serde(default)]
    pub missions: Vec<Mission>,
    pub llm_providers: Vec<LlmProvider>,
    pub llm_models: Vec<LlmModel>,
    pub llm_mode: String,
    pub current_model_display: String,
    pub cloud_config: CloudConfig,
    pub storage_backends: Vec<StorageBackend>,
    pub orchestration_rules: Vec<OrchestrationRule>,
    #[serde(default)]
    pub agent_messages: HashMap<String, Vec<AgentMessage>>,
    #[serde(default)]
    pub agent_groups: Vec<AgentGroup>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub director_states: Vec<DirectorState>,
    #[serde(default)]
    pub action_events: Vec<ActionEvent>,
    #[serde(default)]
    pub operator_feed: Vec<OperatorFeedEvent>,
    #[serde(default)]
    pub mission_metrics: HashMap<String, u64>,
    #[serde(default)]
    pub routing_task_kind_overrides: HashMap<String, config::TaskKindRoutingEntry>,
    #[serde(default)]
    pub routing_model_tier_overrides: HashMap<String, config::ModelTierRoute>,
    #[serde(default)]
    pub entity_connections: Vec<EntityConnection>,
    #[serde(default)]
    pub entity_connection_sessions: Vec<EntityConnectionSession>,
    #[serde(default)]
    pub entity_connection_events: Vec<EntityConnectionEvent>,
    #[serde(default)]
    pub approvals: Vec<ApprovalRequest>,
    #[serde(default)]
    pub entity_files: Vec<UploadedEntityFile>,
    #[serde(default)]
    pub chat_attachments: Vec<ChatAttachment>,
    #[serde(default)]
    pub raw_captures: Vec<RawCapture>,
    #[serde(default)]
    pub external_agent_runs: Vec<ExternalAgentRun>,
    #[serde(default)]
    pub verifications: Vec<VerificationRun>,
}

/// Key in `ControlState.mission_metrics` for the one-time "time to first completed
/// mission" measurement (roadmap Milestone 1, item 5): wall-clock seconds from the
/// first-ever completed mission's `created_at` to the moment it was auto-completed.
/// Set at most once per workspace/deployment lifetime.
pub const TIME_TO_FIRST_COMPLETED_MISSION_METRIC: &str = "time_to_first_completed_mission_seconds";

/// The gateway's in-memory source of truth: every domain collection behind an
/// `RwLock` for concurrent async access, plus `OnceCell`-attached Surreal
/// persisters (set once at startup) that keep it durable across restarts.
/// Most callers reach it via `Arc<ControlState>` shared through [`ServerState`].
///
/// # Lock order
///
/// Read this before adding a collection or an `await`. Two rules, and they are not stylistic:
///
/// 1. **Never hold a guard across an `await`.** Tokio's `RwLock` is
///    write-preferring: one waiting writer blocks every new reader. A guard held
///    across I/O turns a slow Surreal round-trip into a stall for every other
///    task touching that collection.
/// 2. **Never hold two guards at once.** If a path genuinely needs two, take
///    them in the order the fields are declared below, and say so in a comment.
///
/// Rule 2 exists because breaking it deadlocked the process: [`Self::snapshot`]
/// took all 28 read guards at once (temporary lifetime extension in a
/// tail-position struct literal) while [`Self::append_operator_feed_event`] took
/// `operator_feed` then `entities` — the reverse order. Note what made it hard to
/// see: two readers in opposite orders cannot hang each other, because read
/// guards are shared. It needed a third task queuing `entities.write()` before
/// rule 1 of the `RwLock` — writers go first — turned the reversed order into a
/// three-way cycle. Both paths are fixed; the rule is what keeps them fixed.
///
/// Watch for the shape that hides a violation: a `let` binding for a guard drops
/// at the end of the statement, but a guard created inside the block's *tail
/// expression* lives until the end of the block. `let x = lock.read().await.clone();`
/// is safe. `Struct { x: lock.read().await.clone(), .. }` as a return value is not.
pub struct ControlState {
    pub tasks: RwLock<Vec<Task>>,
    pub entities: RwLock<Vec<Entity>>,
    pub missions: RwLock<Vec<Mission>>,
    pub llm_providers: RwLock<Vec<LlmProvider>>,
    pub llm_models: RwLock<Vec<LlmModel>>,
    pub llm_mode: RwLock<String>,
    pub current_model_display: RwLock<String>,
    pub cloud_config: RwLock<CloudConfig>,
    pub storage_backends: RwLock<Vec<StorageBackend>>,
    pub orchestration_rules: RwLock<Vec<OrchestrationRule>>,
    pub agent_messages: RwLock<HashMap<String, Vec<AgentMessage>>>,
    pub agent_groups: RwLock<Vec<AgentGroup>>,
    pub agents: RwLock<Vec<Agent>>,
    pub director_states: RwLock<Vec<DirectorState>>,
    pub action_events: RwLock<Vec<ActionEvent>>,
    pub operator_feed: RwLock<Vec<OperatorFeedEvent>>,
    pub mission_metrics: RwLock<HashMap<String, u64>>,
    /// Runtime task-kind routing overrides (persisted in control snapshot).
    pub routing_task_kind_overrides: RwLock<HashMap<String, config::TaskKindRoutingEntry>>,
    /// Runtime model tier route overrides (persisted in control snapshot).
    pub routing_model_tier_overrides: RwLock<HashMap<String, config::ModelTierRoute>>,
    pub entity_connections: RwLock<Vec<EntityConnection>>,
    pub entity_connection_sessions: RwLock<Vec<EntityConnectionSession>>,
    pub entity_connection_events: RwLock<Vec<EntityConnectionEvent>>,
    pub approvals: RwLock<Vec<ApprovalRequest>>,
    pub entity_files: RwLock<Vec<UploadedEntityFile>>,
    pub chat_attachments: RwLock<Vec<ChatAttachment>>,
    /// Raw capture inbox: pre-interpretation material from chat, external agent
    /// runs, and importers. Canonical here; mirrored to the local vault.
    pub raw_captures: RwLock<Vec<RawCapture>>,
    /// External coding-agent CLI run status records. Canonical here; run output
    /// is also mirrored to entity `runs/` folders when available.
    pub external_agent_runs: RwLock<Vec<ExternalAgentRun>>,
    /// First-class verification runs (shell test commands, browser URL checks)
    /// linked to a task/mission/entity.
    pub verifications: RwLock<Vec<VerificationRun>>,
    /// Surreal-backed persister for entities. Set once at startup via
    /// [`ControlState::attach_entity_store`]; once set, entities are hydrated
    /// from Surreal on cold start and re-synced after each disk persist.
    pub entity_store: tokio::sync::OnceCell<Arc<crate::entity_store::SurrealEntityStore>>,
    /// Surreal-backed persister for operator feed and action events.
    pub feed_store: tokio::sync::OnceCell<Arc<crate::feed_store::FeedStore>>,
    /// Surreal-backed row-per-task persister (write-through on create,
    /// merge-hydrate on boot, upsert-only periodic sync).
    pub task_store: tokio::sync::OnceCell<Arc<crate::task_store::SurrealTaskStore>>,
    /// Canonical mission store. Mission mutations should write through here;
    /// the in-memory vector is a hot cache for the runtime loops.
    pub mission_store: tokio::sync::OnceCell<Arc<crate::mission_store::SurrealMissionStore>>,
    /// Surreal-backed heartbeat persister. Attached so entities created deep in
    /// `ControlState` (personal spaces) can be given their default operating
    /// rhythm at the moment they appear, not at the next restart.
    pub heartbeat_store: tokio::sync::OnceCell<Arc<crate::heartbeat_store::SurrealHeartbeatStore>>,
    /// Surreal-backed registry of the operator's notification devices.
    pub device_store: tokio::sync::OnceCell<Arc<crate::device_store::SurrealDeviceStore>>,
    /// Directors, for paths that need a persona without carrying `ServerState`
    /// — today the mission loop's critic, which reviews work as a named
    /// director rather than as an anonymous reviewer.
    pub directors: tokio::sync::OnceCell<Arc<crate::director_store::SurrealDirectorStore>>,
    /// Live external-agent child processes, so "cancel" can actually kill one.
    /// Runtime-only and intentionally absent from the snapshot: a process
    /// handle means nothing after a restart.
    pub agent_processes: crate::agent_sessions::AgentProcessRegistry,
    /// Fan-out to [`crate::notify::NotificationDispatcher`]. Every feed append
    /// drops a signal here; sending is non-blocking, so feed writes never wait
    /// on delivery and an absent dispatcher just means "feed-only".
    pub notification_tx: tokio::sync::OnceCell<crate::notify::NotificationSender>,
}

/// In-memory feed window. Deep enough that the daily digest can ask "did I
/// already brief today?" by scanning it, and that a busy day of task events
/// does not evict the morning's approvals before anyone reads them.
const OPERATOR_FEED_MAX: usize = 500;

/// `Task.task_kind` for a run executed by an external coding-agent CLI. These
/// tasks are driven by [`crate::agent_sessions`], not the orchestrator, so
/// several loops deliberately skip them.
pub const EXTERNAL_AGENT_RUN_TASK_KIND: &str = "external-agent-run";

impl ControlState {
    /// External CLI processes are owned by the gateway process that spawned
    /// them. If the gateway restarts while a run is still marked running, the
    /// child process handle is gone and the UI would otherwise poll forever.
    ///
    /// A run that captured a Claude Code `sessionId` is the exception: that id
    /// is a durable handle on the conversation, so the work can be picked up
    /// with `--resume` instead of thrown away. Those are marked `interrupted`
    /// and flagged `resumePending` for
    /// [`crate::agent_sessions`]' startup sweep; everything else keeps the old
    /// stale-and-fail behaviour.
    pub async fn mark_orphaned_external_agent_runs_stale(&self) {
        let now = chrono::Utc::now().to_rfc3339();
        let (stale_run_ids, resumable_run_ids) = {
            let mut runs = self.external_agent_runs.write().await;
            let mut stale = Vec::new();
            let mut resumable = Vec::new();
            for run in runs
                .iter_mut()
                .filter(|run| run.status == "running" || run.status == "queued")
            {
                let session_id = run
                    .metadata
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                if run.provider == "claude" && session_id.is_some() {
                    run.status = "interrupted".to_string();
                    run.error = Some(
                        "Gateway restarted mid-run; this session will be resumed.".to_string(),
                    );
                    resumable.push(run.id.clone());
                } else {
                    run.status = "stale".to_string();
                    run.error = Some(
                        "Gateway restarted before this external agent run completed.".to_string(),
                    );
                    run.completed_at = Some(now.clone());
                    stale.push(run.id.clone());
                }
            }
            (stale, resumable)
        };
        if stale_run_ids.is_empty() && resumable_run_ids.is_empty() {
            return;
        }
        let mut tasks = self.tasks.write().await;
        for task in tasks.iter_mut() {
            let Some(external_id) = task.external_id.as_ref() else {
                continue;
            };
            if stale_run_ids.contains(external_id) {
                task.status = TaskStatus::Failed;
                task.progress = 0;
                task.result = Some(
                    "External agent run became stale because the gateway restarted before completion."
                        .to_string(),
                );
            } else if resumable_run_ids.contains(external_id) {
                // Left InProgress on purpose so the run still reads as live
                // while the sweep picks it back up.
                task_meta_set_str(task, "resumePending", "true");
            }
        }
    }

    /// Startup recovery. First marks orphaned external-agent runs stale (failing
    /// their tasks), then brings tasks that were left `InProgress` (their
    /// in-process run died with the gateway) to a safe state: requeued for
    /// another attempt if retries remain, else failed with a reason. Queued /
    /// backlog tasks already resume through normal pickup. Returns
    /// `(requeued, failed)` counts.
    pub async fn recover_after_restart(&self, policy: crate::retry::RetryPolicy) -> (usize, usize) {
        self.mark_orphaned_external_agent_runs_stale().await;
        let mut requeued = 0usize;
        let mut failed = 0usize;
        let mut events: Vec<(OperatorFeedKind, Task)> = Vec::new();
        {
            let mut tasks = self.tasks.write().await;
            for task in tasks.iter_mut() {
                if task.status != TaskStatus::InProgress {
                    continue;
                }
                // External-agent runs are not orchestrator work: they were
                // already triaged just above (stale-failed, or left InProgress
                // to be resumed). Requeueing one here would hand it to a
                // director's LLM tool loop, which is not what it is.
                if task.task_kind.as_deref() == Some(EXTERNAL_AGENT_RUN_TASK_KIND) {
                    continue;
                }
                let attempt = task.tries.parse::<u32>().unwrap_or(0) + 1;
                task.tries = attempt.to_string();
                task_meta_clear(task, "startedAt");
                task_meta_clear(task, "nextRetryAt");
                task.assigned_director_id = None;
                task.assigned_agent_id = None;
                if attempt >= policy.max_attempts {
                    task.status = TaskStatus::Failed;
                    task.progress = 0;
                    task.result =
                        Some("Interrupted by gateway restart; retries exhausted.".to_string());
                    failed += 1;
                    events.push((OperatorFeedKind::TaskBlocked, task.clone()));
                } else {
                    task.status = TaskStatus::Queued;
                    task.progress = 0;
                    task.result = Some("Recovered after gateway restart; will retry.".to_string());
                    requeued += 1;
                    events.push((OperatorFeedKind::TaskRetryScheduled, task.clone()));
                }
            }
        }
        for (kind, task) in &events {
            self.push_operator_feed(*kind, task, task.result.clone())
                .await;
        }
        (requeued, failed)
    }

    /// Watchdog: force-fail tasks that have been `InProgress` past
    /// `threshold_secs` (their run hung or was killed) so the loop never spins
    /// forever. They become `Failed` with a `NeedsReview` outcome. Returns the
    /// tasks marked stuck so the caller can emit feed events.
    pub async fn detect_stuck_tasks(
        &self,
        threshold_secs: i64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<Task> {
        let mut stuck = Vec::new();
        let mut tasks = self.tasks.write().await;
        for task in tasks.iter_mut() {
            if task.status != TaskStatus::InProgress {
                continue;
            }
            // A Claude Code session is *meant* to run for hours or days, so the
            // 15-minute watchdog must never touch it. These tasks carry no
            // `startedAt` today and so already fall through, but that is an
            // accident of the create path — say it explicitly, because the
            // failure mode (multi-day work force-failed at minute 15) is
            // silent and expensive.
            if task.task_kind.as_deref() == Some(EXTERNAL_AGENT_RUN_TASK_KIND) {
                continue;
            }
            let Some(started) = task_meta_str(task, "startedAt") else {
                continue;
            };
            let Ok(started_dt) = chrono::DateTime::parse_from_rfc3339(&started) else {
                continue;
            };
            if (now - started_dt.with_timezone(&chrono::Utc)).num_seconds() < threshold_secs {
                continue;
            }
            task.status = TaskStatus::Failed;
            task.outcome = Some(TaskOutcome::NeedsReview);
            task.progress = 100;
            task.result = Some(format!(
                "Stuck: no completion after {threshold_secs}s; force-failed by the watchdog."
            ));
            task.assigned_director_id = None;
            task.assigned_agent_id = None;
            stuck.push(task.clone());
        }
        stuck
    }

    /// Load state from a JSON file if it exists; otherwise return None (caller uses new()).
    pub async fn load_from_path(path: &str) -> anyhow::Result<Option<Self>> {
        let expanded = config::expand_tilde_path(path);
        let path = Path::new(&expanded);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = tokio::fs::read(path).await?;
        let snap: ControlStateSnapshot = serde_json::from_slice(&bytes)?;
        Ok(Some(Self::from_snapshot(snap)))
    }

    /// Save current state to a JSON file. Creates parent dir if needed.
    pub async fn save_to_path(&self, path: &str) -> anyhow::Result<()> {
        let expanded = config::expand_tilde_path(path);
        if let Some(parent) = Path::new(&expanded).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let snap = self.snapshot().await;
        let bytes = serde_json::to_vec_pretty(&snap)?;
        tokio::fs::write(&expanded, bytes).await?;
        Ok(())
    }

    /// Point-in-time copy of every persisted collection.
    ///
    /// Each guard is taken, cloned from, and dropped before the next one — one
    /// lock held at a time, in the order declared on the struct. This used to be
    /// a single struct literal in tail position, where Rust's temporary lifetime
    /// extension kept all 28 read guards alive until the function returned: it
    /// held `entities` (2nd) while waiting for `operator_feed` (16th), which
    /// deadlocked against [`Self::append_operator_feed_event`] taking them the
    /// other way round — once any third task queued `entities.write()`, which is
    /// what makes a write-preferring `RwLock` block the reader in the middle. It
    /// also serialised the whole snapshot against every writer in the process for
    /// the duration of a 28-collection clone.
    ///
    /// The cost is that the snapshot is no longer one atomic instant across
    /// collections: a mission created while `tasks` is being cloned can land in the
    /// snapshot without its task. Worth being precise about how small that is,
    /// because it is the one thing this change gives up.
    ///
    /// Holding all 28 guards never bought referential integrity. Writers take one
    /// guard at a time — that is now rule 2 on this struct — so a two-collection
    /// mutation is already two separate writes, and a snapshot could always land
    /// between them. All the convoy bought was that no write could interleave
    /// *during* the snapshot, which is a narrower guarantee than it looks.
    ///
    /// And the three collections where a torn edge would matter do not depend on
    /// the snapshot at boot anyway: [`Self::hydrate_tasks_from_store`] merges rows
    /// the snapshot missed, while [`Self::hydrate_entities_from_store`] and
    /// [`Self::hydrate_missions_from_store`] replace the snapshot's list outright
    /// whenever their Surreal table has rows.
    pub async fn snapshot(&self) -> ControlStateSnapshot {
        let tasks = self.tasks.read().await.clone();
        let entities = self.entities.read().await.clone();
        let missions = self.missions.read().await.clone();
        let llm_providers = self.llm_providers.read().await.clone();
        let llm_models = self.llm_models.read().await.clone();
        let llm_mode = self.llm_mode.read().await.clone();
        let current_model_display = self.current_model_display.read().await.clone();
        let cloud_config = self.cloud_config.read().await.clone();
        let storage_backends = self.storage_backends.read().await.clone();
        let orchestration_rules = self.orchestration_rules.read().await.clone();
        let agent_messages = self.agent_messages.read().await.clone();
        let agent_groups = self.agent_groups.read().await.clone();
        let agents = self.agents.read().await.clone();
        let director_states = self.director_states.read().await.clone();
        let action_events = self.action_events.read().await.clone();
        let operator_feed = self.operator_feed.read().await.clone();
        let mission_metrics = self.mission_metrics.read().await.clone();
        let routing_task_kind_overrides = self.routing_task_kind_overrides.read().await.clone();
        let routing_model_tier_overrides = self.routing_model_tier_overrides.read().await.clone();
        let entity_connections = self.entity_connections.read().await.clone();
        let entity_connection_sessions = self.entity_connection_sessions.read().await.clone();
        let entity_connection_events = self.entity_connection_events.read().await.clone();
        let approvals = self.approvals.read().await.clone();
        let entity_files = self.entity_files.read().await.clone();
        let chat_attachments = self.chat_attachments.read().await.clone();
        let raw_captures = self.raw_captures.read().await.clone();
        let external_agent_runs = self.external_agent_runs.read().await.clone();
        let verifications = self.verifications.read().await.clone();
        ControlStateSnapshot {
            tasks,
            entities,
            missions,
            llm_providers,
            llm_models,
            llm_mode,
            current_model_display,
            cloud_config,
            storage_backends,
            orchestration_rules,
            agent_messages,
            agent_groups,
            agents,
            director_states,
            action_events,
            operator_feed,
            mission_metrics,
            routing_task_kind_overrides,
            routing_model_tier_overrides,
            entity_connections,
            entity_connection_sessions,
            entity_connection_events,
            approvals,
            entity_files,
            chat_attachments,
            raw_captures,
            external_agent_runs,
            verifications,
        }
    }

    pub fn from_snapshot(snap: ControlStateSnapshot) -> Self {
        Self {
            tasks: RwLock::new(snap.tasks),
            entities: RwLock::new(snap.entities),
            missions: RwLock::new(snap.missions),
            llm_providers: RwLock::new(snap.llm_providers),
            llm_models: RwLock::new(snap.llm_models),
            llm_mode: RwLock::new(snap.llm_mode),
            current_model_display: RwLock::new(snap.current_model_display),
            cloud_config: RwLock::new(snap.cloud_config),
            storage_backends: RwLock::new(snap.storage_backends),
            orchestration_rules: RwLock::new(snap.orchestration_rules),
            agent_messages: RwLock::new(snap.agent_messages),
            agent_groups: RwLock::new(snap.agent_groups),
            agents: RwLock::new(snap.agents),
            director_states: RwLock::new(snap.director_states),
            action_events: RwLock::new(snap.action_events),
            operator_feed: RwLock::new(snap.operator_feed),
            mission_metrics: RwLock::new(snap.mission_metrics),
            routing_task_kind_overrides: RwLock::new(snap.routing_task_kind_overrides),
            routing_model_tier_overrides: RwLock::new(snap.routing_model_tier_overrides),
            entity_connections: RwLock::new(snap.entity_connections),
            entity_connection_sessions: RwLock::new(snap.entity_connection_sessions),
            entity_connection_events: RwLock::new(snap.entity_connection_events),
            approvals: RwLock::new(snap.approvals),
            entity_files: RwLock::new(snap.entity_files),
            chat_attachments: RwLock::new(snap.chat_attachments),
            raw_captures: RwLock::new(snap.raw_captures),
            external_agent_runs: RwLock::new(snap.external_agent_runs),
            verifications: RwLock::new(snap.verifications),
            entity_store: tokio::sync::OnceCell::new(),
            feed_store: tokio::sync::OnceCell::new(),
            task_store: tokio::sync::OnceCell::new(),
            mission_store: tokio::sync::OnceCell::new(),
            heartbeat_store: tokio::sync::OnceCell::new(),
            device_store: tokio::sync::OnceCell::new(),
            directors: tokio::sync::OnceCell::new(),
            notification_tx: tokio::sync::OnceCell::new(),
            agent_processes: crate::agent_sessions::AgentProcessRegistry::default(),
        }
    }

    // Records the event in memory, mirrors it to the Surreal feed store (when
    // attached), and mirrors it into the entity's local vault folder.
    /// Mutate one task by id, under the write guard, and hand back whatever the
    /// closure returns.
    ///
    /// This exists because the alternative shipped a live defect. The orchestrator
    /// used to collect `(index, …)` candidates under a read guard, drop it, await a
    /// Surreal recall per candidate, then take the write guard and mutate
    /// `tasks[i]` by that saved index. Between the two, any of the other ~55
    /// writers could push or `retain` — deleting an entity removes its tasks and
    /// shifts every later index — so the write landed on a different task than the
    /// one that was chosen. Best case the wrong task was hijacked into an
    /// unattended agent run, including one a human had just cancelled; worst case
    /// the index was out of bounds, the loop panicked, and (before supervision) that
    /// subsystem was gone for the life of the process.
    ///
    /// Re-finding by id inside the guard makes that unrepresentable. Returns `None`
    /// if no task with `id` exists — which a caller that just chose the task should
    /// treat as "it was deleted underneath me", not as an error.
    ///
    /// Two things it deliberately does *not* do. It does not persist: the row write
    /// is an `await`, and the lock-order rule above forbids holding a guard across
    /// one, so callers follow up with [`Self::persist_task_row`] exactly as before.
    /// And it does not re-check status — only the caller knows which transition it
    /// is making, so status guards belong in the closure via
    /// [`set_task_status_by_loop`].
    pub async fn update_task<F, R>(&self, id: &str, f: F) -> Option<R>
    where
        F: FnOnce(&mut Task) -> R,
    {
        let mut tasks = self.tasks.write().await;
        let task = tasks.iter_mut().find(|t| t.id == id)?;
        Some(f(task))
    }

    /// [`Self::update_task`] for a loop claiming work: applies `f` only while the
    /// task is still in one of `expected`.
    ///
    /// The pattern every background loop needs and none of them had. A loop selects
    /// a candidate, does I/O, and comes back to claim it — by which point a human
    /// may have cancelled it, or another loop may have taken it. Returns `None` when
    /// the task is gone *or* has moved on, and both mean the same thing to the
    /// caller: someone else got there first, so don't touch it.
    pub async fn claim_task<F, R>(&self, id: &str, expected: &[TaskStatus], f: F) -> Option<R>
    where
        F: FnOnce(&mut Task) -> R,
    {
        let mut tasks = self.tasks.write().await;
        let task = tasks.iter_mut().find(|t| t.id == id)?;
        if !expected.contains(&task.status) {
            return None;
        }
        Some(f(task))
    }

    pub async fn append_action_event(&self, event: ActionEvent) {
        self.action_events.write().await.push(event.clone());
        if let Some(store) = self.feed_store.get() {
            let _: Result<(), _> = store.append_action_event(&event, None).await;
        }
        let entity = if let Some(entity_id) = event.entity_id.as_deref() {
            let entities = self.entities.read().await;
            entities.iter().find(|e| e.id == entity_id).cloned()
        } else {
            None
        };
        let workspace_id = entity
            .as_ref()
            .and_then(|e| e.workspace_id.as_deref())
            .unwrap_or("default")
            .to_string();
        crate::local_vault::mirror_action_event(&event, entity.as_ref(), &workspace_id).await;
    }

    // Central sink for every operator feed row: appends + trims the in-memory
    // window, mirrors to Surreal and the local vault, then fans out to the
    // notification dispatcher unless quiet hours are active for this entity.
    pub async fn append_operator_feed_event(&self, event: OperatorFeedEvent) {
        // Scoped so the guard is released before the first await. It used to be a
        // named binding that lived to the end of the function — held across the
        // Surreal append *and* the vault mirror, with `entities.read()` taken in
        // the middle, the reverse of the order [`Self::snapshot`] uses.
        //
        // Two paths in opposite orders were not enough to hang on their own: both
        // wanted `entities` for *read*, and read guards are shared. It took a
        // third actor — anything calling `entities.write()`, so entity creation,
        // an edit, the periodic sync — to close the cycle, because tokio's
        // `RwLock` is write-preferring and a queued writer blocks new readers:
        //
        //   snapshot holds entities.read, waits for operator_feed
        //     → this holds operator_feed.write, waits for entities.read
        //       → blocked behind the queued writer
        //         → writer waits for snapshot's entities.read
        //
        // All three run constantly. See the lock-order rule on `ControlState`,
        // and `snapshot_and_feed_append_do_not_deadlock` for the reproduction.
        {
            let mut feed = self.operator_feed.write().await;
            feed.push(event.clone());
            if feed.len() > OPERATOR_FEED_MAX {
                let drain = feed.len() - OPERATOR_FEED_MAX;
                feed.drain(0..drain);
            }
        }
        if let Some(store) = self.feed_store.get() {
            let _: Result<(), _> = store.append_operator_feed_event(&event, None).await;
        }
        let entity = if let Some(entity_id) = event.entity_id.as_deref() {
            let entities = self.entities.read().await;
            entities.iter().find(|e| e.id == entity_id).cloned()
        } else {
            None
        };
        let workspace_id = entity
            .as_ref()
            .and_then(|e| e.workspace_id.as_deref())
            .unwrap_or("default")
            .to_string();
        crate::local_vault::mirror_operator_feed_event(&event, entity.as_ref(), &workspace_id)
            .await;

        // Quiet hours suppress the interruption, not the history: the feed row
        // above is already written, so nothing is lost — the operator just
        // isn't woken for it. Windows are server-local wall-clock; see
        // [`crate::notify::quiet_hours_active`].
        let muted_by_quiet_hours = entity
            .as_ref()
            .and_then(|e| e.entity_runtime.as_ref())
            .and_then(|rt| rt.quiet_hours_local.as_ref())
            .is_some_and(|q| crate::notify::quiet_hours_active(q, chrono::Local::now().time()));

        // Hand the event to the companion dispatcher. `send` on an unbounded
        // channel doesn't await, so notification delivery can never slow down
        // (or fail) a feed write — a closed channel just means feed-only.
        if let Some(tx) = self.notification_tx.get()
            && crate::notify::is_notifiable(&event.kind)
            && !muted_by_quiet_hours
        {
            let _ = tx.send(crate::notify::NotificationSignal {
                event,
                workspace_id,
            });
        }
    }

    // The push_* helpers below are thin convenience wrappers: each builds an
    // `OperatorFeedEvent` from a domain record and hands it to
    // `append_operator_feed_event`.
    pub async fn push_operator_feed(
        &self,
        kind: OperatorFeedKind,
        task: &Task,
        detail: Option<String>,
    ) {
        self.append_operator_feed_event(OperatorFeedEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            kind,
            task_id: Some(task.id.clone()),
            mission_id: task.mission_id.clone(),
            entity_id: task.entity_id.clone(),
            connection_id: None,
            title: task.title.clone(),
            detail,
            approval_id: None,
        })
        .await;
    }

    pub async fn push_scheduled_fire_feed(
        &self,
        title: String,
        entity_id: Option<String>,
        connection_id: Option<String>,
        detail: Option<String>,
    ) {
        self.append_operator_feed_event(OperatorFeedEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            kind: OperatorFeedKind::ScheduledTaskFired,
            task_id: None,
            mission_id: None,
            entity_id,
            connection_id,
            title,
            detail,
            approval_id: None,
        })
        .await;
    }

    pub async fn push_mission_feed(
        &self,
        kind: OperatorFeedKind,
        mission: &Mission,
        detail: Option<String>,
    ) {
        self.append_operator_feed_event(OperatorFeedEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            kind,
            task_id: None,
            mission_id: Some(mission.id.clone()),
            entity_id: Some(mission.entity_id.clone()),
            connection_id: None,
            title: mission.title.clone(),
            detail,
            approval_id: None,
        })
        .await;
    }

    pub async fn push_approval_feed(
        &self,
        kind: OperatorFeedKind,
        approval: &ApprovalRequest,
        detail: Option<String>,
    ) {
        self.append_operator_feed_event(OperatorFeedEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            kind,
            task_id: approval.task_id.clone(),
            mission_id: None,
            entity_id: Some(approval.entity_id.clone()),
            connection_id: approval.connection_id.clone(),
            title: format!("Approval: {} ({})", approval.action, approval.id),
            detail,
            approval_id: Some(approval.id.clone()),
        })
        .await;
    }

    /// Operator-visible row without a backing task/mission (e.g. connection verify/test).
    pub async fn push_operator_feed_surface(
        &self,
        kind: OperatorFeedKind,
        title: impl Into<String>,
        detail: Option<String>,
        entity_id: Option<String>,
        connection_id: Option<String>,
    ) {
        self.append_operator_feed_event(OperatorFeedEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            kind,
            task_id: None,
            mission_id: None,
            entity_id,
            connection_id,
            title: title.into(),
            detail,
            approval_id: None,
        })
        .await;
    }

    /// Persist a new approval and emit feed (same as HTTP `POST /api/approvals`).
    pub async fn enqueue_approval_request(&self, approval: ApprovalRequest) {
        self.approvals.write().await.push(approval.clone());
        self.push_approval_feed(
            OperatorFeedKind::ApprovalRequestCreated,
            &approval,
            approval.detail.clone(),
        )
        .await;
    }

    pub async fn increment_mission_metric(&self, key: &str) {
        let mut metrics = self.mission_metrics.write().await;
        let entry = metrics.entry(key.to_string()).or_insert(0);
        *entry += 1;
    }

    /// Records `TIME_TO_FIRST_COMPLETED_MISSION_METRIC` the first time any mission
    /// completes; a no-op on every completion after that. `mission` must still carry
    /// its pre-completion `created_at` (unaffected by the status transition).
    ///
    /// Returns `Some(elapsed_seconds)` only on the call that actually recorded,
    /// so the caller can mark the activation moment. A write-once counter can
    /// say *how long* it took but never *when* it happened, and "when" is what
    /// makes the metric readable over time.
    pub async fn record_time_to_first_completed_mission(
        &self,
        mission: &Mission,
        completed_at: DateTime<Utc>,
    ) -> Option<u64> {
        let mut metrics = self.mission_metrics.write().await;
        if metrics.contains_key(TIME_TO_FIRST_COMPLETED_MISSION_METRIC) {
            return None;
        }
        let created_at = DateTime::parse_from_rfc3339(&mission.created_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or(completed_at);
        let elapsed_seconds = (completed_at - created_at).num_seconds().max(0) as u64;
        metrics.insert(
            TIME_TO_FIRST_COMPLETED_MISSION_METRIC.to_string(),
            elapsed_seconds,
        );
        Some(elapsed_seconds)
    }

    // Fans a single outcome out into up to three counters (overall, per-branch,
    // per-task-kind) so the dashboard can slice recommendation stats either way.
    pub async fn increment_recommendation_metric(
        &self,
        outcome: &str,
        branch: Option<&str>,
        task_kind: Option<&str>,
    ) {
        self.increment_mission_metric(&format!("outcome.{outcome}"))
            .await;
        if let Some(branch) = branch {
            self.increment_mission_metric(&format!("outcome.{outcome}.branch.{branch}"))
                .await;
        }
        if let Some(task_kind) = task_kind {
            self.increment_mission_metric(&format!("outcome.{outcome}.task_kind.{task_kind}"))
                .await;
        }
    }

    pub async fn append_entity_connection_event(&self, event: EntityConnectionEvent) {
        self.entity_connection_events.write().await.push(event);
    }

    /// Attach the Surreal entity persister. Must be called exactly once at
    /// startup, before `hydrate_entities_from_store`.
    pub fn attach_entity_store(&self, store: Arc<crate::entity_store::SurrealEntityStore>) {
        let _ = self.entity_store.set(store);
    }

    /// Attach the Surreal feed persister. Must be called exactly once at startup.
    pub fn attach_feed_store(&self, store: Arc<crate::feed_store::FeedStore>) {
        let _ = self.feed_store.set(store);
    }

    /// Attach the Surreal task persister. Must be called exactly once at
    /// startup, before `hydrate_tasks_from_store`.
    pub fn attach_task_store(&self, store: Arc<crate::task_store::SurrealTaskStore>) {
        let _ = self.task_store.set(store);
    }

    /// Attach the Surreal mission persister. Must be called exactly once at
    /// startup, before `hydrate_missions_from_store`.
    pub fn attach_mission_store(&self, store: Arc<crate::mission_store::SurrealMissionStore>) {
        let _ = self.mission_store.set(store);
    }

    /// Hydrate missions from the canonical store. Snapshot missions are only a
    /// migration fallback; once rows exist in Surreal they win on restart.
    pub async fn hydrate_missions_from_store(&self) -> anyhow::Result<()> {
        let Some(store) = self.mission_store.get() else {
            return Ok(());
        };
        let persisted = store.list_all().await?;
        if !persisted.is_empty() {
            *self.missions.write().await = persisted;
        } else {
            let in_memory = self.missions.read().await.clone();
            if !in_memory.is_empty() {
                store.sync_all(&in_memory).await?;
            }
        }
        Ok(())
    }

    /// Push the current in-memory mission Vec into Surreal (mirrors
    /// `sync_entities_to_store` / `sync_tasks_to_store`).
    pub async fn sync_missions_to_store(&self) -> anyhow::Result<()> {
        let Some(store) = self.mission_store.get() else {
            return Ok(());
        };
        let missions = self.missions.read().await.clone();
        store.sync_all(&missions).await
    }

    /// Write-through a single mission row. Errors are logged, not propagated —
    /// the in-memory update already succeeded.
    pub async fn persist_mission(&self, mission: &Mission) {
        if let Some(store) = self.mission_store.get()
            && let Err(e) = store.upsert(mission).await
        {
            tracing::warn!(mission_id = %mission.id, error = %e, "mission write-through failed; periodic sync will retry");
        }
    }

    /// Attach the Surreal heartbeat persister so auto-provisioned entities get
    /// their default beats immediately. Optional: without it, the startup
    /// backfill still catches them on the next boot.
    pub fn attach_heartbeat_store(
        &self,
        store: Arc<crate::heartbeat_store::SurrealHeartbeatStore>,
    ) {
        let _ = self.heartbeat_store.set(store);
    }

    /// Attach the Surreal device registry so `/api/devices` can register the
    /// operator's phone and desktop shell.
    pub fn attach_device_store(&self, store: Arc<crate::device_store::SurrealDeviceStore>) {
        let _ = self.device_store.set(store);
    }

    /// Attach the director registry so background loops can resolve a named
    /// director (the mission loop's critic) without `ServerState`.
    pub fn attach_directors(&self, store: Arc<crate::director_store::SurrealDirectorStore>) {
        let _ = self.directors.set(store);
    }

    /// Attach the companion dispatcher's sending half. Must be called exactly
    /// once at startup; without it every feed event stays feed-only.
    pub fn attach_notification_sender(&self, tx: crate::notify::NotificationSender) {
        let _ = self.notification_tx.set(tx);
    }

    /// Merge persisted task rows into the in-memory Vec: rows whose id the
    /// Vec doesn't know are appended (they were created by another instance
    /// or a previous life of this one); known ids keep the in-memory copy,
    /// which is at least as fresh. If the store is empty but memory has
    /// tasks (snapshot/file boot), push them as a one-time migration.
    pub async fn hydrate_tasks_from_store(&self) -> anyhow::Result<()> {
        let Some(store) = self.task_store.get() else {
            return Ok(());
        };
        let persisted = store.list_all().await?;
        let mut tasks = self.tasks.write().await;
        if persisted.is_empty() {
            if !tasks.is_empty() {
                tracing::info!(
                    count = tasks.len(),
                    "Migrating tasks into SurrealDB (one-time)"
                );
                store.upsert_all(&tasks).await?;
            }
            return Ok(());
        }
        let mut merged = 0usize;
        for row in persisted {
            if !tasks.iter().any(|t| t.id == row.id) {
                tasks.push(row);
                merged += 1;
            }
        }
        if merged > 0 {
            tracing::info!(
                merged,
                "Merged persisted task rows unknown to this instance"
            );
        }
        Ok(())
    }

    /// Upsert every in-memory task into Surreal. Deliberately never prunes —
    /// see the task_store module docs for the overlap-race reasoning.
    pub async fn sync_tasks_to_store(&self) -> anyhow::Result<()> {
        let Some(store) = self.task_store.get() else {
            return Ok(());
        };
        let tasks = self.tasks.read().await.clone();
        store.upsert_all(&tasks).await
    }

    /// Write-through a single task row. Called at task creation so the row
    /// exists before any redeploy can race it. Errors are logged, not
    /// propagated — the in-memory create already succeeded.
    pub async fn persist_task_row(&self, task: &Task) {
        if let Some(store) = self.task_store.get()
            && let Err(e) = store.upsert(task).await
        {
            tracing::warn!(task_id = %task.id, error = %e, "task write-through failed; periodic sync will retry");
        }
    }

    /// If the Surreal entity store has rows, overwrite the in-memory Vec with
    /// them. If the store is empty but the in-memory Vec has entries (e.g.
    /// loaded from JSON snapshot or `entities_seed.json`), push them into
    /// Surreal as a one-time migration. No-op if both are empty or the store
    /// was never attached.
    pub async fn hydrate_entities_from_store(&self) -> anyhow::Result<()> {
        let Some(store) = self.entity_store.get() else {
            return Ok(());
        };
        let persisted = store.list_all().await?;
        if !persisted.is_empty() {
            tracing::info!(count = persisted.len(), "Hydrated entities from SurrealDB");
            *self.entities.write().await = persisted;
            return Ok(());
        }
        let in_memory = self.entities.read().await.clone();
        if !in_memory.is_empty() {
            tracing::info!(
                count = in_memory.len(),
                "Migrating JSON/seed entities into SurrealDB (one-time)"
            );
            store.sync_all(&in_memory).await?;
        }
        Ok(())
    }

    /// Push the current in-memory entity Vec into Surreal. Called from the
    /// periodic persist loop after the JSON snapshot is written.
    pub async fn sync_entities_to_store(&self) -> anyhow::Result<()> {
        let Some(store) = self.entity_store.get() else {
            return Ok(());
        };
        let entities = self.entities.read().await.clone();
        store.sync_all(&entities).await
    }

    /// Idempotently ensure a private Personal Space entity exists for this user.
    ///
    /// The Personal Space is a real [`Entity`] (`is_personal = true`) bound to the
    /// caller's `principal_id`, so every existing subsystem — per-entity memory,
    /// tasks, feed, chat context — works for it for free. It is owned by
    /// `workspace_id` so tenant scoping stays intact, but visibility helpers gate
    /// it to its owner only, so teammates in a shared workspace never see it.
    ///
    /// Called from the auth bootstrap on every authed request; the read-lock fast
    /// path keeps the steady-state cost to a cheap scan of the (small) entity Vec.
    pub async fn ensure_personal_space(&self, principal_id: &str, workspace_id: &str) {
        let principal_id = principal_id.trim();
        let workspace_id = workspace_id.trim();
        if principal_id.is_empty() || workspace_id.is_empty() {
            return;
        }
        let already = |entities: &[Entity]| {
            entities.iter().any(|e| {
                e.is_personal
                    && e.personal_user_id.as_deref() == Some(principal_id)
                    && e.owner.as_deref() == Some(workspace_id)
            })
        };
        // Fast path: already provisioned for this (workspace, user).
        if already(&self.entities.read().await) {
            return;
        }

        let entity = Entity {
            id: personal_space_id(workspace_id, principal_id),
            name: "Personal".to_string(),
            description: Some(
                "Your private space — loose thoughts, questions, and anything not yet filed under an entity."
                    .to_string(),
            ),
            owner: Some(workspace_id.to_string()),
            status: "Active".to_string(),
            entity_type: Some("personal".to_string()),
            workspace_id: Some(workspace_id.to_string()),
            visibility: EntityVisibility::Private,
            is_personal: true,
            personal_user_id: Some(principal_id.to_string()),
            colour: Some("#8b5cf6".to_string()),
            ..Default::default()
        };

        {
            let mut entities = self.entities.write().await;
            // Double-check under the write lock: another request may have raced us.
            if already(&entities) {
                return;
            }
            entities.push(entity.clone());
        }

        // Best-effort immediate persist so the space survives a restart that
        // happens before the next periodic sync tick.
        if let Err(e) = self.sync_entities_to_store().await {
            tracing::warn!(error = %e, principal_id, "failed to persist personal space");
        }

        // A personal space is an entity like any other, so it gets the same
        // default rhythm. Only reached once per (workspace, user) — the
        // double-checked guard above returns early on every later call.
        if let Some(store) = self.heartbeat_store.get()
            && let Err(e) = crate::entity_heartbeat_defaults::provision_default_heartbeats(
                store.as_ref(),
                &entity,
                Some(workspace_id),
            )
            .await
        {
            tracing::warn!(error = %e, principal_id, "failed to provision personal space heartbeats");
        }
    }

    /// If entities are empty, try to load from entities_seed.json (cwd or ~/.consistent-world).
    /// No-op if entities already exist or seed file is missing/invalid.
    pub async fn load_entities_seed_if_empty(&self) {
        let mut entities = self.entities.write().await;
        if !entities.is_empty() {
            return;
        }
        let cwd = std::env::current_dir().ok();
        let paths: Vec<std::path::PathBuf> = [
            cwd.as_ref().map(|p| p.join("entities_seed.json")),
            Some(
                Path::new(&config::expand_tilde_path(
                    "~/.consistent-world/entities_seed.json",
                ))
                .to_path_buf(),
            ),
        ]
        .into_iter()
        .flatten()
        .collect();
        for path in paths {
            if !path.exists() {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(s) => match serde_json::from_str::<EntitiesSeedFile>(&s) {
                    Ok(seed) if !seed.entities.is_empty() => {
                        tracing::info!(path = %path.display(), count = seed.entities.len(), "Loaded entities from seed");
                        *entities = seed.entities;
                        return;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "entities_seed.json parse error");
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "Could not read entities seed file");
                }
            }
        }
    }

    /// Ensure every agent group has a DirectorState (heartbeat off, empty auto_task_kinds).
    /// Call after load so persisted state without director_states still works.
    pub async fn ensure_director_states_synced(&self) {
        let groups = self.agent_groups.read().await;
        let mut states = self.director_states.write().await;
        for g in groups.iter() {
            if !states.iter().any(|s| s.id == g.id) {
                states.push(DirectorState {
                    id: g.id.clone(),
                    heartbeat_enabled: false,
                    auto_task_kinds: vec![],
                });
            }
        }
    }

    /// If agent_groups/agents are empty, try to load from agents_seed.json (cwd or ~/.consistent-world).
    /// No-op if data already exist or seed file is missing/invalid.
    pub async fn load_agents_seed_if_empty(&self) {
        {
            let groups = self.agent_groups.read().await;
            if !groups.is_empty() {
                return;
            }
        }
        let cwd = std::env::current_dir().ok();
        let paths: Vec<std::path::PathBuf> = [
            cwd.as_ref().map(|p| p.join("agents_seed.json")),
            Some(
                Path::new(&config::expand_tilde_path(
                    "~/.consistent-world/agents_seed.json",
                ))
                .to_path_buf(),
            ),
        ]
        .into_iter()
        .flatten()
        .collect();
        for path in paths {
            if !path.exists() {
                continue;
            }
            match tokio::fs::read_to_string(&path).await {
                Ok(s) => match serde_json::from_str::<AgentsSeedFile>(&s) {
                    Ok(seed) => {
                        tracing::info!(path = %path.display(), groups = seed.groups.len(), agents = seed.agents.len(), "Loaded agents from seed");
                        self.apply_agents_seed(seed).await;
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "agents_seed.json parse error");
                    }
                },
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "Could not read agents seed file");
                }
            }
        }
        // No file on disk (e.g. container images that only ship the binary):
        // fall back to the seed baked in at compile time.
        match serde_json::from_str::<AgentsSeedFile>(crate::director_seed::EMBEDDED_SEED) {
            Ok(seed) => {
                tracing::info!(
                    groups = seed.groups.len(),
                    agents = seed.agents.len(),
                    "Loaded agents from embedded seed"
                );
                self.apply_agents_seed(seed).await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "embedded agents seed malformed; no agents loaded");
            }
        }
    }

    // Replaces agent_groups/agents wholesale and backfills a DirectorState
    // for any group that doesn't have one yet.
    async fn apply_agents_seed(&self, seed: AgentsSeedFile) {
        *self.agent_groups.write().await = seed.groups.clone();
        *self.agents.write().await = seed.agents;
        // Ensure one DirectorState per group (heartbeat off by default)
        let mut states = self.director_states.write().await;
        for g in &seed.groups {
            if !states.iter().any(|s| s.id == g.id) {
                states.push(DirectorState {
                    id: g.id.clone(),
                    heartbeat_enabled: false,
                    auto_task_kinds: vec![],
                });
            }
        }
    }

    pub fn new(default_provider: &str, default_base_url: &str, default_model: &str) -> Self {
        let provider_id = Uuid::new_v4().to_string();
        let model_id = Uuid::new_v4().to_string();
        let llm_providers = RwLock::new(vec![LlmProvider {
            id: provider_id.clone(),
            name: default_provider.to_string(),
            provider_type: default_provider.to_lowercase(),
            base_url: Some(default_base_url.to_string()),
            api_key_env: None,
        }]);
        let llm_models = RwLock::new(vec![LlmModel {
            id: model_id.clone(),
            provider_id: provider_id.clone(),
            name: default_model.to_string(),
            is_default: true,
        }]);
        Self {
            tasks: RwLock::new(Vec::new()),
            entities: RwLock::new(Vec::new()),
            missions: RwLock::new(Vec::new()),
            llm_providers,
            llm_models,
            llm_mode: RwLock::new("balance".to_string()),
            current_model_display: RwLock::new(default_model.to_string()),
            cloud_config: RwLock::new(CloudConfig {
                provider: None,
                region: None,
                compute_profile: None,
            }),
            storage_backends: RwLock::new(Vec::new()),
            orchestration_rules: RwLock::new(Vec::new()),
            agent_messages: RwLock::new(HashMap::new()),
            agent_groups: RwLock::new(Vec::new()),
            agents: RwLock::new(Vec::new()),
            director_states: RwLock::new(Vec::new()),
            action_events: RwLock::new(Vec::new()),
            operator_feed: RwLock::new(Vec::new()),
            mission_metrics: RwLock::new(HashMap::new()),
            routing_task_kind_overrides: RwLock::new(HashMap::new()),
            routing_model_tier_overrides: RwLock::new(HashMap::new()),
            entity_connections: RwLock::new(Vec::new()),
            entity_connection_sessions: RwLock::new(Vec::new()),
            entity_connection_events: RwLock::new(Vec::new()),
            approvals: RwLock::new(Vec::new()),
            entity_files: RwLock::new(Vec::new()),
            chat_attachments: RwLock::new(Vec::new()),
            raw_captures: RwLock::new(Vec::new()),
            external_agent_runs: RwLock::new(Vec::new()),
            verifications: RwLock::new(Vec::new()),
            entity_store: tokio::sync::OnceCell::new(),
            feed_store: tokio::sync::OnceCell::new(),
            task_store: tokio::sync::OnceCell::new(),
            mission_store: tokio::sync::OnceCell::new(),
            heartbeat_store: tokio::sync::OnceCell::new(),
            device_store: tokio::sync::OnceCell::new(),
            directors: tokio::sync::OnceCell::new(),
            notification_tx: tokio::sync::OnceCell::new(),
            agent_processes: crate::agent_sessions::AgentProcessRegistry::default(),
        }
    }
}

// ─── ServerState (moved from server so it can hold control_state without cycle) ───

/// Newtype wrapper so `Arc<dyn EmbeddingProvider>` can implement `Debug` (the
/// trait object itself can't) and live in `ServerState`.
#[derive(Clone)]
pub struct EmbeddingProviderState(pub Arc<dyn EmbeddingProvider>);

impl std::fmt::Debug for EmbeddingProviderState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EmbeddingProvider")
    }
}

/// Snapshot of the messaging runtime (adapters, agents, plugins) exposed via
/// `GET /api/runtime/status`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub messaging_enabled: bool,
    pub adapters: Vec<String>,
    pub agent_ids: Vec<String>,
    pub plugin_count: usize,
}

/// Everything an axum handler needs, threaded through as `State<ServerState>`.
/// Built with [`ServerState::new`] plus the `with_*` builder methods below;
/// most fields are `Option` because not every deployment wires up every
/// subsystem (e.g. `local_files` is only set for the desktop-bound gateway).
#[derive(Clone)]
pub struct ServerState {
    pub version: String,
    pub auth_token: Option<String>,
    pub memory: Arc<dyn MemoryStore>,
    pub memory_backend: String,
    pub scheduler: Option<Arc<Scheduler>>,
    pub embedding_provider: Option<EmbeddingProviderState>,
    pub cost_tracker: Option<Arc<Mutex<CostTracker>>>,
    pub control_state: Option<Arc<ControlState>>,
    pub app_config: Option<Arc<AppConfig>>,
    pub runtime_status: Option<RuntimeStatus>,
    /// Restart health for the supervised background loops. Present whenever the
    /// gateway booted its loops (so: always, outside tests that build a bare
    /// `ServerState`). See [`crate::supervisor`].
    pub subsystems: Option<Arc<crate::supervisor::SubsystemRegistry>>,
    pub session_cache: crate::memory_buffer::SessionCache,
    pub credit_ledger: Option<Arc<crate::billing::CreditLedger>>,
    pub tenant_store: Option<Arc<crate::tenant_store::TenantStore>>,
    /// Own-auth store (username/password sessions), replacing Clerk.
    pub auth_store: Option<Arc<crate::auth_store::AuthStore>>,
    /// Passkey (WebAuthn) service; None if RP config is invalid.
    pub passkey: Option<Arc<crate::passkey::PasskeyService>>,
    /// The two folders the chat agent may write to (`~/Desktop`, `~/Consistent`).
    /// `None` on network-bound deployments, where writing to the server's disk
    /// is meaningless — which is what keeps the file tools off there entirely.
    pub local_files: Option<Arc<crate::local_files::LocalFiles>>,
    /// External JWT issuer, when one is configured. `None` on every deployment
    /// that has not opted in — which is the default, and is what keeps local
    /// accounts the only way in for a self-hosted instance.
    pub external_identity: Option<Arc<crate::external_identity::ExternalVerifier>>,
    /// Phase 6: canonical-layer stores used by the prompt-assembly pipeline.
    pub mission_store: Option<Arc<crate::mission_store::SurrealMissionStore>>,
    pub director_store: Option<Arc<crate::director_store::SurrealDirectorStore>>,
    pub director_lessons_store:
        Option<Arc<crate::director_lessons_store::SurrealDirectorLessonsStore>>,
    /// Cursor-style entity docs eagerly spliced into the canonical prompt.
    pub entity_docs_store: Option<Arc<crate::entity_docs_store::SurrealEntityDocsStore>>,
    /// The entity ledger — structured, sourced facts (contacts, deals,
    /// content, accounts, files, metrics, events) fed by API/browser/push
    /// sources. See [`crate::entity_records_store`].
    pub entity_records_store: Option<Arc<crate::entity_records_store::SurrealEntityRecordsStore>>,
    /// Phase 9: per-entity heartbeat result log surfaced by the chat UI.
    pub heartbeat_results_store:
        Option<Arc<dyn crate::heartbeat_results_store::HeartbeatResultsStore>>,
    pub heartbeat_store: Option<Arc<crate::heartbeat_store::SurrealHeartbeatStore>>,
    /// Phase 1 (Build 3) — Bulletproof Architect shadow path. Inspect-only
    /// today; Forward/Director/Generate/Verify land in later phases.
    pub architect: Option<Arc<consistent_world_architect::Architect>>,
    pub audit_store: Option<Arc<consistent_world_architect::SurrealAuditStore>>,
    /// Live canvas (spatial brain map) persistence.
    pub canvas_store: Option<Arc<crate::canvas_store::CanvasStore>>,
    /// Per-user credential vaults (password manager + environment secrets).
    pub vault_manager: Option<Arc<crate::vault_manager::VaultManager>>,
    /// Claude-format skills: built-in, workspace-global and per-entity.
    pub skills: Option<Arc<crate::skills::SkillLibrary>>,
}

impl std::fmt::Debug for ServerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerState")
            .field("version", &self.version)
            .field("auth_token", &self.auth_token.as_ref().map(|_| "..."))
            .field("memory", &"Arc<dyn MemoryStore>")
            .field("memory_backend", &self.memory_backend)
            .field("scheduler", &self.scheduler.is_some())
            .field("embedding_provider", &self.embedding_provider.is_some())
            .field("cost_tracker", &self.cost_tracker.is_some())
            .field("control_state", &self.control_state.is_some())
            .field("app_config", &self.app_config.is_some())
            .field("runtime_status", &self.runtime_status.is_some())
            .field("session_cache", &self.session_cache.len())
            .finish()
    }
}

impl ServerState {
    pub fn new(version: String, auth_token: Option<String>, memory: Arc<dyn MemoryStore>) -> Self {
        Self {
            version,
            auth_token,
            memory,
            memory_backend: "surreal".to_string(),
            scheduler: None,
            embedding_provider: None,
            cost_tracker: None,
            control_state: None,
            app_config: None,
            runtime_status: None,
            subsystems: None,
            session_cache: crate::memory_buffer::create_session_cache(),
            credit_ledger: None,
            tenant_store: None,
            auth_store: None,
            passkey: None,
            local_files: None,
            external_identity: None,
            mission_store: None,
            director_store: None,
            director_lessons_store: None,
            entity_docs_store: None,
            entity_records_store: None,
            heartbeat_results_store: None,
            heartbeat_store: None,
            architect: None,
            audit_store: None,
            canvas_store: None,
            vault_manager: None,
            skills: None,
        }
    }

    pub fn with_skills(mut self, library: Arc<crate::skills::SkillLibrary>) -> Self {
        self.skills = Some(library);
        self
    }

    // Builder-style setters below: each takes ownership of `self`, sets one
    // optional field, and returns `Self` so calls can be chained onto `new`.
    pub fn with_canvas_store(mut self, store: Arc<crate::canvas_store::CanvasStore>) -> Self {
        self.canvas_store = Some(store);
        self
    }

    pub fn with_vault_manager(mut self, manager: Arc<crate::vault_manager::VaultManager>) -> Self {
        self.vault_manager = Some(manager);
        self
    }

    pub fn with_architect(mut self, architect: Arc<consistent_world_architect::Architect>) -> Self {
        self.architect = Some(architect);
        self
    }

    pub fn with_audit_store(
        mut self,
        store: Arc<consistent_world_architect::SurrealAuditStore>,
    ) -> Self {
        self.audit_store = Some(store);
        self
    }

    pub fn with_heartbeat_results_store(
        mut self,
        store: Arc<dyn crate::heartbeat_results_store::HeartbeatResultsStore>,
    ) -> Self {
        self.heartbeat_results_store = Some(store);
        self
    }

    pub fn with_heartbeat_store(
        mut self,
        store: Arc<crate::heartbeat_store::SurrealHeartbeatStore>,
    ) -> Self {
        self.heartbeat_store = Some(store);
        self
    }

    pub fn with_mission_store(
        mut self,
        store: Arc<crate::mission_store::SurrealMissionStore>,
    ) -> Self {
        self.mission_store = Some(store);
        self
    }

    pub fn with_director_store(
        mut self,
        store: Arc<crate::director_store::SurrealDirectorStore>,
    ) -> Self {
        self.director_store = Some(store);
        self
    }

    pub fn with_director_lessons_store(
        mut self,
        store: Arc<crate::director_lessons_store::SurrealDirectorLessonsStore>,
    ) -> Self {
        self.director_lessons_store = Some(store);
        self
    }

    pub fn with_entity_docs_store(
        mut self,
        store: Arc<crate::entity_docs_store::SurrealEntityDocsStore>,
    ) -> Self {
        self.entity_docs_store = Some(store);
        self
    }

    pub fn with_entity_records_store(
        mut self,
        store: Arc<crate::entity_records_store::SurrealEntityRecordsStore>,
    ) -> Self {
        self.entity_records_store = Some(store);
        self
    }

    pub fn with_memory_backend(mut self, backend: String) -> Self {
        self.memory_backend = backend;
        self
    }

    pub fn with_scheduler(mut self, scheduler: Arc<Scheduler>) -> Self {
        self.scheduler = Some(scheduler);
        self
    }

    pub fn with_subsystems(
        mut self,
        subsystems: Arc<crate::supervisor::SubsystemRegistry>,
    ) -> Self {
        self.subsystems = Some(subsystems);
        self
    }

    pub fn with_embedding_provider(mut self, provider: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedding_provider = Some(EmbeddingProviderState(provider));
        self
    }

    pub fn with_cost_tracker(mut self, tracker: Arc<Mutex<CostTracker>>) -> Self {
        self.cost_tracker = Some(tracker);
        self
    }

    pub fn with_control_state(mut self, control: Arc<ControlState>) -> Self {
        self.control_state = Some(control);
        self
    }

    pub fn with_app_config(mut self, config: Arc<AppConfig>) -> Self {
        self.app_config = Some(config);
        self
    }

    pub fn with_runtime_status(mut self, status: RuntimeStatus) -> Self {
        self.runtime_status = Some(status);
        self
    }

    pub fn with_credit_ledger(mut self, ledger: Arc<crate::billing::CreditLedger>) -> Self {
        self.credit_ledger = Some(ledger);
        self
    }

    pub fn with_tenant_store(mut self, store: Arc<crate::tenant_store::TenantStore>) -> Self {
        self.tenant_store = Some(store);
        self
    }

    pub fn with_auth_store(mut self, store: Arc<crate::auth_store::AuthStore>) -> Self {
        self.auth_store = Some(store);
        self
    }

    pub fn with_passkey(mut self, svc: Arc<crate::passkey::PasskeyService>) -> Self {
        self.passkey = Some(svc);
        self
    }

    pub fn with_local_files(mut self, files: Arc<crate::local_files::LocalFiles>) -> Self {
        self.local_files = Some(files);
        self
    }

    pub fn with_external_identity(
        mut self,
        verifier: Arc<crate::external_identity::ExternalVerifier>,
    ) -> Self {
        self.external_identity = Some(verifier);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hours_ago(h: i64) -> String {
        (chrono::Utc::now() - chrono::Duration::hours(h)).to_rfc3339()
    }

    fn hours_ahead(h: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::hours(h)).to_rfc3339()
    }

    fn task_with(id: &str, status: TaskStatus) -> Task {
        Task {
            id: id.to_string(),
            title: format!("task {id}"),
            status,
            ..Task::default()
        }
    }

    /// The defect this method exists to prevent: reproduced against position-based
    /// mutation, then shown unrepresentable through `update_task`.
    ///
    /// The orchestrator saved candidate *indices* under a read guard, released it,
    /// awaited Surreal I/O, then wrote `tasks[i]`. Deleting an entity calls `retain`
    /// on the task list, which shifts every later index down — so the write landed
    /// on whichever task had moved into that slot. Both outcomes were live: an
    /// in-range shift silently mutates the wrong task (below), and a shift past the
    /// end panics the loop, which before supervision killed it for the life of the
    /// process.
    #[tokio::test]
    async fn update_task_follows_the_id_not_the_position() {
        let control = Arc::new(ControlState::new("openai", "http://localhost", "m"));
        {
            let mut tasks = control.tasks.write().await;
            tasks.push(task_with("a", TaskStatus::Queued));
            tasks.push(task_with("b", TaskStatus::Queued));
            tasks.push(task_with("target", TaskStatus::Queued));
            tasks.push(task_with("d", TaskStatus::Queued));
        }

        // A loop selects "target", which is at index 2 right now.
        let picked_index = 2usize;
        let picked_id = "target";

        // I/O happens with no lock held, and meanwhile an entity is deleted, taking
        // its tasks with it.
        control.tasks.write().await.retain(|t| t.id != "a");

        // What the old code did next. The index is still in range, so nothing
        // complains — it just writes to the wrong task.
        let hit = {
            let mut tasks = control.tasks.write().await;
            tasks[picked_index].progress = 99;
            tasks[picked_index].id.clone()
        };
        assert_eq!(
            hit, "d",
            "the saved index now points at a different task entirely"
        );
        assert_eq!(
            control
                .tasks
                .read()
                .await
                .iter()
                .find(|t| t.id == picked_id)
                .unwrap()
                .progress,
            0,
            "and the task that was actually chosen was never touched"
        );

        // `update_task` cannot express that mistake: it takes an id, and re-finds it
        // inside the guard.
        {
            let mut tasks = control.tasks.write().await;
            for t in tasks.iter_mut() {
                t.progress = 0;
            }
        }
        let found = control
            .update_task(picked_id, |t| {
                t.progress = 42;
                t.id.clone()
            })
            .await;
        assert_eq!(found.as_deref(), Some("target"));

        let tasks = control.tasks.read().await;
        assert_eq!(
            tasks.iter().find(|t| t.id == "target").unwrap().progress,
            42,
            "the chosen task is the one that changed"
        );
        for bystander in ["b", "d"] {
            assert_eq!(
                tasks.iter().find(|t| t.id == bystander).unwrap().progress,
                0,
                "{bystander} was not touched"
            );
        }
    }

    #[tokio::test]
    async fn update_task_returns_none_for_a_task_that_was_deleted() {
        let control = Arc::new(ControlState::new("openai", "http://localhost", "m"));
        control
            .tasks
            .write()
            .await
            .push(task_with("gone", TaskStatus::Queued));
        control.tasks.write().await.retain(|t| t.id != "gone");

        let out = control.update_task("gone", |t| t.progress = 1).await;
        assert!(
            out.is_none(),
            "a caller that chose this task must be able to tell it vanished"
        );
    }

    /// `claim_task` is the half that closes the cancel hijack: the orchestrator
    /// selects a task, does I/O with no lock held, and by the time it claims, a
    /// human may have pressed stop. Claiming anyway launched a fresh unattended
    /// agent run on a cancelled task.
    #[tokio::test]
    async fn claim_task_refuses_once_the_status_has_moved() {
        let control = Arc::new(ControlState::new("openai", "http://localhost", "m"));
        control
            .tasks
            .write()
            .await
            .push(task_with("t1", TaskStatus::Queued));

        // The human cancels in the window between selection and claim.
        control
            .update_task("t1", |t| set_task_status_by_human(t, TaskStatus::Cancelled))
            .await
            .expect("task exists");

        let claimed = control
            .claim_task("t1", &[TaskStatus::Backlog, TaskStatus::Queued], |t| {
                t.progress = 10;
                t.id.clone()
            })
            .await;
        assert!(claimed.is_none(), "a cancelled task must not be claimable");

        let tasks = control.tasks.read().await;
        let t = tasks.iter().find(|t| t.id == "t1").unwrap();
        assert_eq!(t.status, TaskStatus::Cancelled, "and it is left untouched");
        assert_eq!(t.progress, 0);
    }

    #[tokio::test]
    async fn claim_task_succeeds_while_the_task_is_still_pickable() {
        let control = Arc::new(ControlState::new("openai", "http://localhost", "m"));
        control
            .tasks
            .write()
            .await
            .push(task_with("t1", TaskStatus::Backlog));

        let claimed = control
            .claim_task("t1", &[TaskStatus::Backlog, TaskStatus::Queued], |t| {
                assert!(set_task_status_by_loop(t, TaskStatus::InProgress));
                t.id.clone()
            })
            .await;
        assert_eq!(claimed.as_deref(), Some("t1"));

        let tasks = control.tasks.read().await;
        let t = tasks.iter().find(|t| t.id == "t1").unwrap();
        assert_eq!(t.status, TaskStatus::InProgress);
        assert!(
            t.status_changed_at.is_some(),
            "going through the setter stamps the transition"
        );
    }

    /// The pair is split by actor, and the difference is load-bearing: a loop must
    /// not overrule a human's cancel, and a human reopening a task must be able to.
    #[tokio::test]
    async fn the_two_setters_differ_only_on_a_cancelled_task() {
        let mut t = task_with("t1", TaskStatus::Cancelled);

        assert!(
            !set_task_status_by_loop(&mut t, TaskStatus::Completed),
            "a loop cannot close a task the human cancelled"
        );
        assert_eq!(t.status, TaskStatus::Cancelled);

        set_task_status_by_human(&mut t, TaskStatus::Queued);
        assert_eq!(
            t.status,
            TaskStatus::Queued,
            "but a person reopening it outranks the stored state"
        );

        // And on any other status the two agree.
        let mut t = task_with("t2", TaskStatus::InProgress);
        assert!(set_task_status_by_loop(&mut t, TaskStatus::Completed));
        assert_eq!(t.status, TaskStatus::Completed);
    }

    /// Regression guard for the deadlock between the two hottest paths in the
    /// process. It needs three actors, not two.
    ///
    /// `snapshot()` used to take all 28 read guards at once — temporary lifetime
    /// extension in a tail-position struct literal keeps every guard alive until
    /// the function returns — so it held `entities` (2nd) while waiting for
    /// `operator_feed` (16th). `append_operator_feed_event()` took
    /// `operator_feed` for *write* and held it across two awaits while taking
    /// `entities` for read: the reverse order.
    ///
    /// That alone does not deadlock, and assuming it did is how this bug hides:
    /// both paths want `entities` for *read*, and read guards are shared. The
    /// cycle closes only once a third task queues `entities.write()` — entity
    /// creation, an entity edit, the periodic entity sync. Tokio's `RwLock` is
    /// write-preferring, so a queued writer blocks new readers, and then:
    ///
    ///   snapshot holds entities.read, waits for operator_feed
    ///     → feed holds operator_feed.write, waits for entities.read
    ///       → blocked behind the queued writer
    ///         → writer waits for snapshot's entities.read
    ///
    /// All three run constantly in a live gateway. Multi-threaded on purpose:
    /// the interleaving needs real parallelism.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_and_feed_append_do_not_deadlock() {
        let control = Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ));

        const ROUNDS: usize = 400;

        let snapshotter = {
            let control = control.clone();
            tokio::spawn(async move {
                for _ in 0..ROUNDS {
                    let _ = control.snapshot().await;
                    tokio::task::yield_now().await;
                }
            })
        };

        let appender = {
            let control = control.clone();
            tokio::spawn(async move {
                for i in 0..ROUNDS {
                    // `entity_id` set on purpose: that is what makes the append
                    // take the `entities` read guard, which is one leg of the
                    // cycle. The entity need not exist — the guard is taken
                    // before the lookup can fail.
                    control
                        .push_operator_feed_surface(
                            OperatorFeedKind::TaskCreated,
                            format!("event {i}"),
                            None,
                            Some("ent_does_not_exist".to_string()),
                            None,
                        )
                        .await;
                    tokio::task::yield_now().await;
                }
            })
        };

        // The third actor, and the reason the other two can deadlock at all: a
        // writer queued on `entities` turns every later reader into a waiter.
        let entity_writer = {
            let control = control.clone();
            tokio::spawn(async move {
                for _ in 0..ROUNDS {
                    {
                        let entities = control.entities.write().await;
                        // Touch it so the guard cannot be optimised away.
                        let _ = entities.len();
                    }
                    tokio::task::yield_now().await;
                }
            })
        };

        let all = async {
            snapshotter.await.expect("snapshot loop must not panic");
            appender.await.expect("append loop must not panic");
            entity_writer.await.expect("entity writer must not panic");
        };

        tokio::time::timeout(std::time::Duration::from_secs(20), all)
            .await
            .expect(
                "snapshot(), append_operator_feed_event() and an entity writer deadlocked \
                 — see the lock-order rule on ControlState",
            );

        // And the work actually happened, so the test cannot pass by doing nothing.
        assert_eq!(control.operator_feed.read().await.len(), ROUNDS);
    }

    #[test]
    fn set_task_status_stamps_completion_once() {
        let mut task = Task {
            id: "t1".into(),
            status: TaskStatus::Queued,
            ..Task::default()
        };

        set_task_status_by_human(&mut task, TaskStatus::InProgress);
        assert!(task.status_changed_at.is_some());
        assert!(
            task.completed_at.is_none(),
            "only a Completed transition stamps completed_at"
        );

        set_task_status_by_human(&mut task, TaskStatus::Completed);
        let first = task.completed_at.clone().expect("completion stamped");

        // Re-completing must not move the date: streaks are computed from it.
        set_task_status_by_human(&mut task, TaskStatus::Completed);
        assert_eq!(task.completed_at.as_deref(), Some(first.as_str()));
    }

    #[test]
    fn the_guard_refuses_to_move_a_task_off_cancelled() {
        let mut task = Task {
            id: "t1".into(),
            status: TaskStatus::Cancelled,
            ..Task::default()
        };

        // Every loop-side closer: the operator's stop outranks the run.
        for attempt in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Queued,
            TaskStatus::InProgress,
            TaskStatus::Backlog,
        ] {
            assert!(
                !set_task_status_by_loop(&mut task, attempt),
                "{attempt:?} must not resurrect a cancelled task"
            );
            assert_eq!(task.status, TaskStatus::Cancelled);
            assert!(task.completed_at.is_none(), "and must not stamp completion");
        }

        // Re-cancelling is not a resurrection, so it is allowed through.
        assert!(set_task_status_by_loop(&mut task, TaskStatus::Cancelled));
        assert_eq!(task.status, TaskStatus::Cancelled);
    }

    #[test]
    fn the_guard_is_transparent_for_every_other_transition() {
        let mut task = Task {
            id: "t2".into(),
            status: TaskStatus::Queued,
            ..Task::default()
        };
        assert!(set_task_status_by_loop(&mut task, TaskStatus::InProgress));
        assert_eq!(task.status, TaskStatus::InProgress);

        assert!(set_task_status_by_loop(&mut task, TaskStatus::Completed));
        assert_eq!(task.status, TaskStatus::Completed);
        assert!(
            task.completed_at.is_some(),
            "the guard must still stamp completion"
        );

        // Cancelling a finished task is a legitimate human move.
        assert!(set_task_status_by_loop(&mut task, TaskStatus::Cancelled));
        assert_eq!(task.status, TaskStatus::Cancelled);
    }

    // `set_task_status` stays deliberately unguarded: un-cancelling is a real,
    // shipped, human-initiated flow (chat "reopen", the requeue button, and
    // mission-engine re-attach all call it on a cancelled task).
    #[test]
    fn the_unguarded_primitive_still_allows_a_human_un_cancel() {
        let mut task = Task {
            id: "t3".into(),
            status: TaskStatus::Cancelled,
            ..Task::default()
        };
        set_task_status_by_human(&mut task, TaskStatus::Queued);
        assert_eq!(task.status, TaskStatus::Queued);
    }

    #[test]
    fn task_due_at_falls_back_to_reminder_metadata() {
        let mut task = Task::default();
        assert!(task_due_at(&task).is_none());

        task_meta_set_str(
            &mut task,
            crate::reminders::REMINDER_DUE_AT_KEY,
            &hours_ago(1),
        );
        assert!(
            task_due_at(&task).is_some(),
            "legacy reminders keep working"
        );

        task.due_at = Some(hours_ahead(5));
        let resolved = task_due_at(&task).expect("first-class field wins");
        assert!(resolved > chrono::Utc::now());
    }

    #[test]
    fn task_is_overdue_only_for_live_work_past_its_date() {
        let now = chrono::Utc::now();
        let overdue = Task {
            status: TaskStatus::Queued,
            due_at: Some(hours_ago(2)),
            ..Task::default()
        };
        assert!(task_is_overdue(&overdue, now));

        let not_yet = Task {
            due_at: Some(hours_ahead(2)),
            ..overdue.clone()
        };
        assert!(!task_is_overdue(&not_yet, now));

        let undated = Task {
            due_at: None,
            ..overdue.clone()
        };
        assert!(!task_is_overdue(&undated, now));

        for terminal in [
            TaskStatus::Completed,
            TaskStatus::Cancelled,
            TaskStatus::Failed,
        ] {
            let done = Task {
                status: terminal,
                ..overdue.clone()
            };
            assert!(!task_is_overdue(&done, now), "{terminal:?} is not overdue");
        }

        // Reminders and engine beats have their own schedulers.
        let reminder = Task {
            task_kind: Some("reminder".into()),
            ..overdue.clone()
        };
        assert!(!task_is_overdue(&reminder, now));
        let beat = Task {
            task_kind: Some(crate::mission_engine::BEAT_TASK_KIND.into()),
            ..overdue.clone()
        };
        assert!(!task_is_overdue(&beat, now));
    }

    #[tokio::test]
    async fn recover_after_restart_requeues_or_fails_interrupted_tasks() {
        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        {
            let mut tasks = control.tasks.write().await;
            // Fresh interrupted task — should requeue.
            tasks.push(Task {
                id: "t-fresh".into(),
                status: TaskStatus::InProgress,
                tries: "0".into(),
                ..Task::default()
            });
            // Already retried up to the limit — should fail.
            tasks.push(Task {
                id: "t-exhausted".into(),
                status: TaskStatus::InProgress,
                tries: "2".into(),
                ..Task::default()
            });
            // Queued work is untouched (resumes normally).
            tasks.push(Task {
                id: "t-queued".into(),
                status: TaskStatus::Queued,
                ..Task::default()
            });
        }
        let (requeued, failed) = control
            .recover_after_restart(crate::retry::RetryPolicy::default())
            .await;
        assert_eq!(requeued, 1);
        assert_eq!(failed, 1);
        let tasks = control.tasks.read().await;
        assert_eq!(
            tasks.iter().find(|t| t.id == "t-fresh").unwrap().status,
            TaskStatus::Queued
        );
        assert_eq!(
            tasks.iter().find(|t| t.id == "t-exhausted").unwrap().status,
            TaskStatus::Failed
        );
        assert_eq!(
            tasks.iter().find(|t| t.id == "t-queued").unwrap().status,
            TaskStatus::Queued
        );
    }

    #[tokio::test]
    async fn detect_stuck_tasks_force_fails_old_in_progress() {
        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        let now = chrono::Utc::now();
        {
            let mut tasks = control.tasks.write().await;
            let mut old = Task {
                id: "t-old".into(),
                status: TaskStatus::InProgress,
                ..Task::default()
            };
            task_meta_set_str(
                &mut old,
                "startedAt",
                &(now - chrono::Duration::seconds(2000)).to_rfc3339(),
            );
            tasks.push(old);
            let mut fresh = Task {
                id: "t-recent".into(),
                status: TaskStatus::InProgress,
                ..Task::default()
            };
            task_meta_set_str(&mut fresh, "startedAt", &now.to_rfc3339());
            tasks.push(fresh);
        }
        let stuck = control.detect_stuck_tasks(900, now).await;
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0].id, "t-old");
        let tasks = control.tasks.read().await;
        let old = tasks.iter().find(|t| t.id == "t-old").unwrap();
        assert_eq!(old.status, TaskStatus::Failed);
        assert_eq!(old.outcome, Some(TaskOutcome::NeedsReview));
        // The recently-started task is left alone.
        assert_eq!(
            tasks.iter().find(|t| t.id == "t-recent").unwrap().status,
            TaskStatus::InProgress
        );
    }

    #[test]
    fn snapshot_deserializes_without_missions_field() {
        let raw = r#"{
            "tasks": [],
            "entities": [],
            "llm_providers": [],
            "llm_models": [],
            "llm_mode": "balance",
            "current_model_display": "default",
            "cloud_config": {"provider": null, "region": null, "compute_profile": null},
            "storage_backends": [],
            "orchestration_rules": [],
            "agent_messages": {},
            "agent_groups": [],
            "agents": [],
            "director_states": [],
            "action_events": [],
            "operator_feed": [],
            "mission_metrics": {},
            "routing_task_kind_overrides": {},
            "routing_model_tier_overrides": {},
            "entity_connections": [],
            "entity_connection_sessions": [],
            "entity_connection_events": [],
            "approvals": []
        }"#;
        let parsed: ControlStateSnapshot =
            serde_json::from_str(raw).expect("snapshot should parse with missions default");
        assert!(parsed.missions.is_empty());
    }

    #[test]
    fn snapshot_round_trip_with_missions() {
        let mission = Mission {
            id: "mission-1".to_string(),
            entity_id: "entity-1".to_string(),
            title: "Mission".to_string(),
            summary: "summary".to_string(),
            status: MissionStatus::Active,
            priority: "high".to_string(),
            goal: "goal".to_string(),
            owner: None,
            created_by: Some("user".to_string()),
            source: Some("test".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: Some(12),
            narrative: Some("Story so far".to_string()),
            canonical_notes: vec!["pinned note".to_string()],
            assigned_director_id: Some("dir_frontend".to_string()),
            final_report: None,
            loop_config: None,
            loop_state: None,
        };
        let snap = ControlStateSnapshot {
            tasks: vec![],
            entities: vec![],
            missions: vec![mission.clone()],
            llm_providers: vec![],
            llm_models: vec![],
            llm_mode: "balance".to_string(),
            current_model_display: "default".to_string(),
            cloud_config: CloudConfig {
                provider: None,
                region: None,
                compute_profile: None,
            },
            storage_backends: vec![],
            orchestration_rules: vec![],
            agent_messages: HashMap::new(),
            agent_groups: vec![],
            agents: vec![],
            director_states: vec![],
            action_events: vec![],
            operator_feed: vec![],
            mission_metrics: HashMap::new(),
            routing_task_kind_overrides: HashMap::new(),
            routing_model_tier_overrides: HashMap::new(),
            entity_connections: vec![],
            entity_connection_sessions: vec![],
            entity_connection_events: vec![],
            approvals: vec![],
            entity_files: vec![],
            chat_attachments: vec![],
            raw_captures: vec![],
            external_agent_runs: vec![],
            verifications: vec![],
        };
        let raw = serde_json::to_string(&snap).expect("serialize snapshot");
        let parsed: ControlStateSnapshot =
            serde_json::from_str(&raw).expect("deserialize snapshot");
        assert_eq!(parsed.missions.len(), 1);
        assert_eq!(parsed.missions[0].id, mission.id);
        assert_eq!(parsed.missions[0].review_cadence_hours, Some(12));
    }
}
