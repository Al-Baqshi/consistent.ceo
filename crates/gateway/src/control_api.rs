//! Control dashboard API: entities, tasks, LLM, cloud, storage, orchestration.

use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, Multipart, Path, Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{delete, get, patch, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path as StdPath, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};
use tracing::warn;
use uuid::Uuid;

use consistent_world_tools::{Tool, coding_agent::CodingAgentTool};

use crate::capture::{
    DEFAULT_CAPTURE_AGENT_ID, DEFAULT_CAPTURE_DIRECTOR_ID, build_llm_client,
    resolve_llm_for_preference, resolve_operations_llm,
};
use crate::config::AppConfig;
use crate::credentials_resolver;
use crate::entity_runtime::resolve_entity_runtime_context;
use crate::state::{
    ActionEvent, ActionOutcome, Agent, AgentGroup, AgentMessage, ApprovalRequest, ApprovalStatus,
    ChatAttachment, CloudConfig, ControlState, DirectorState, EnrichmentDraft,
    EnrichmentInputSnapshot, Entity, EntityConnection, EntityConnectionEvent,
    EntityConnectionEventKind, EntityConnectionSession, EntityEnrichment, EntityReadiness,
    EntityRecord, EntityResource, EntityRuntimeConfig, EntityRuntimeContext, EntitySetup,
    EntityVisibility, ExternalAgentRun, LlmModel, LlmProvider, Mission, MissionStatus,
    OperatorFeedEvent, OperatorFeedKind, OrchestrationRule, RawCapture, RawCaptureStatus,
    ServerState, StorageBackend, TIME_TO_FIRST_COMPLETED_MISSION_METRIC, Task, TaskOutcome,
    TaskStatus, UploadedEntityFile, VerificationRun,
};
use consistent_world_llm::client::{ChatMessage, CompletionRequest, Role};

const DEFAULT_WORKSPACE_ID: &str = "default";
const WORKSPACE_HEADER: &str = "x-workspace-id";

/// Per-request authenticated principal, attached by the auth middleware and
/// read by handlers via axum's `Extension` extractor.
#[derive(Debug, Clone)]
pub struct AuthContext {
    pub principal_id: String,
    pub workspace_id: String,
    /// Where this principal came from. A handler that cares — anything that
    /// treats "the user asked for this" as consent — must be able to tell a
    /// signed-in person from the holder of a deploy secret.
    pub kind: PrincipalKind,
}

/// How a request's principal was established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PrincipalKind {
    /// Resolved against an `auth_users` row via a session token or API key.
    /// There is a person (or their key) behind it.
    #[default]
    User,
    /// Synthesised from the static deploy secret, with no database lookup. It
    /// stands for "whoever holds the token" — the operator of this machine —
    /// not for any particular account.
    Static,
}

impl AuthContext {
    /// Convenience for handlers gating on provenance.
    pub fn is_static(&self) -> bool {
        self.kind == PrincipalKind::Static
    }
}

/// Resolved workspace + viewer for the current request, inserted into request
/// extensions by [`workspace_context_middleware`] for handlers to pull out.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceCtx {
    pub(crate) id: String,
    /// The authenticated `principal_id` (the viewer). Used to gate per-user
    /// Personal Spaces so teammates in a shared workspace can't see each other's.
    pub(crate) principal_id: String,
}

/// Gate for instance-global mutations (LLM providers/keys, routing overrides,
/// cloud config, directors, shell verifications). 403 unless the caller is an
/// instance admin — see [`crate::server::is_instance_admin`] for who that is.
pub(crate) fn require_instance_admin(
    auth: &AuthContext,
    state: &ServerState,
) -> Result<(), StatusCode> {
    if crate::server::is_instance_admin(auth, crate::server::gateway_is_self_hosted(state)) {
        Ok(())
    } else {
        tracing::warn!(principal = %auth.principal_id, "refused an instance-admin action");
        Err(StatusCode::FORBIDDEN)
    }
}

/// Reads the `x-workspace-id` header, falling back to the default workspace.
fn workspace_id_from_headers(headers: &HeaderMap) -> String {
    headers
        .get(WORKSPACE_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_WORKSPACE_ID)
        .to_string()
}

/// Axum middleware: resolves the workspace/principal for this request (from
/// [`AuthContext`] if present, else the raw header) and stashes it as a
/// [`WorkspaceCtx`] extension for downstream handlers.
pub(crate) async fn workspace_context_middleware(
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let (workspace_id, principal_id) = request
        .extensions()
        .get::<AuthContext>()
        .map(|ctx| (ctx.workspace_id.clone(), ctx.principal_id.clone()))
        .unwrap_or_else(|| {
            (
                workspace_id_from_headers(request.headers()),
                "anonymous".to_string(),
            )
        });
    request.extensions_mut().insert(WorkspaceCtx {
        id: workspace_id,
        principal_id,
    });
    next.run(request).await
}

/// The workspace an entity belongs to, derived from its `owner` field.
fn entity_workspace_id(entity: &Entity) -> &str {
    entity
        .owner
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_WORKSPACE_ID)
}

/// Ids of entities in `workspace_id` visible to `viewer` (see [`is_entity_visible`]).
fn entity_ids_for_workspace(
    entities: &[Entity],
    workspace_id: &str,
    viewer: Option<&str>,
) -> HashSet<String> {
    entities
        .iter()
        .filter(|e| is_entity_visible(e, workspace_id, viewer))
        .map(|e| e.id.clone())
        .collect()
}

/// Whether `entity` is visible to `viewer` (a `principal_id`) within `workspace_id`.
///
/// Business entities are workspace-scoped exactly as before. Personal Spaces
/// (`is_personal`) are additionally gated to their owner: a personal entity is
/// visible only when `viewer` matches its `personal_user_id`. `viewer = None`
/// (background/unauthenticated paths) can NEVER see a personal entity — this
/// fails closed, so a call site that forgets to pass the viewer hides personal
/// spaces rather than leaking them across users.
pub(crate) fn is_entity_visible(entity: &Entity, workspace_id: &str, viewer: Option<&str>) -> bool {
    if entity.status.eq_ignore_ascii_case("deleted") {
        return false;
    }
    if entity_workspace_id(entity) != workspace_id {
        return false;
    }
    if entity.is_personal {
        return matches!(viewer, Some(pid) if entity.personal_user_id.as_deref() == Some(pid));
    }
    true
}

/// Lowercases and hyphenates `input` into a URL/path-safe slug segment.
fn slugify_segment(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Slugifies `input` for use as a workspace id, generating a random one if
/// the input slugifies to nothing.
fn safe_workspace_id(input: &str) -> String {
    let slug = slugify_segment(input);
    if slug.is_empty() {
        format!("workspace-{}", Uuid::new_v4().simple())
    } else {
        slug
    }
}

/// Best-effort extraction of profile/goals/context/rules sections from a
/// free-form entity description, used to seed foundation fields on creation.
fn infer_entity_foundation(
    name: &str,
    description: Option<&str>,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    // Trims stray punctuation and drops fragments too short to be useful.
    fn clean(value: &str) -> Option<String> {
        let trimmed = value.trim().trim_matches('-').trim_matches(':').trim();
        if trimmed.len() < 6 {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    // Finds the first `label:` marker in `text` and returns the text up to
    // whichever known section label comes next.
    fn extract_labeled_section(text: &str, labels: &[&str]) -> Option<String> {
        let lower = text.to_ascii_lowercase();
        let mut best = None;
        for label in labels {
            let needle = format!("{}:", label.to_ascii_lowercase());
            if let Some(idx) = lower.find(&needle) {
                best = Some((idx, needle.len()));
                break;
            }
        }
        let (start_idx, label_len) = best?;
        let start = start_idx + label_len;
        let remainder = &text[start..];
        let next = [
            "profile:",
            "about:",
            "summary:",
            "goals:",
            "goal:",
            "active context:",
            "context:",
            "current context:",
            "operating rules:",
            "rules:",
            "constraints:",
        ]
        .iter()
        .filter_map(|marker| remainder.to_ascii_lowercase().find(marker))
        .filter(|idx| *idx > 0)
        .min()
        .unwrap_or(remainder.len());
        clean(&remainder[..next])
    }

    let Some(raw) = description.map(str::trim).filter(|s| !s.is_empty()) else {
        return (None, None, None, None);
    };

    let profile = extract_labeled_section(raw, &["profile", "about", "summary"]).or_else(|| {
        if raw.len() >= 24
            && raw.len() <= 280
            && !raw.to_ascii_lowercase().contains("goals:")
            && !raw.to_ascii_lowercase().contains("context:")
            && !raw.to_ascii_lowercase().contains("rules:")
        {
            clean(raw)
        } else {
            None
        }
    });
    let goals = extract_labeled_section(raw, &["goals", "goal"]);
    let active_context =
        extract_labeled_section(raw, &["active context", "current context", "context"]);
    let operating_rules =
        extract_labeled_section(raw, &["operating rules", "rules", "constraints"]);

    let profile = profile.and_then(|value| {
        let lower = value.to_ascii_lowercase();
        if lower == name.to_ascii_lowercase() {
            None
        } else {
            Some(value)
        }
    });

    (profile, goals, active_context, operating_rules)
}

/// Slug to use as the entity's workspace-root folder name, preferring the
/// caller's `requested` value and falling back to the entity name.
fn derive_entity_workspace_root(
    _workspace_id: &str,
    requested: Option<&str>,
    name: &str,
) -> Option<String> {
    let base = requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(name);
    let slug = slugify_segment(base);
    if slug.is_empty() { None } else { Some(slug) }
}

/// Builds the entity's on-disk path under `/spaces/<workspace_id>/<root>`.
fn derive_entity_path(workspace_id: &str, workspace_root: Option<&str>) -> Option<String> {
    let root = workspace_root?.trim();
    if root.is_empty() {
        return None;
    }
    Some(
        PathBuf::from("/spaces")
            .join(workspace_id)
            .join(root)
            .to_string_lossy()
            .to_string(),
    )
}

// Per-record `entity_id` matchers, used to filter each collection down to
// the records that belong to a given entity.
fn task_references_entity(task: &Task, entity_id: &str) -> bool {
    task.entity_id.as_deref() == Some(entity_id)
}

fn feed_references_entity(event: &OperatorFeedEvent, entity_id: &str) -> bool {
    event.entity_id.as_deref() == Some(entity_id)
}

fn approval_references_entity(approval: &ApprovalRequest, entity_id: &str) -> bool {
    approval.entity_id == entity_id
}

fn mission_references_entity(mission: &Mission, entity_id: &str) -> bool {
    mission.entity_id == entity_id
}

fn connection_references_entity(connection: &EntityConnection, entity_id: &str) -> bool {
    connection.entity_id == entity_id
}

fn connection_event_references_entity(event: &EntityConnectionEvent, entity_id: &str) -> bool {
    event.entity_id == entity_id
}

fn connection_session_references_entity(
    session: &EntityConnectionSession,
    entity_id: &str,
) -> bool {
    session.entity_id == entity_id
}

// Workspace-visibility checks: a record tied to an entity is visible when
// that entity is in `entity_ids`; an entity-less record only shows in the
// default workspace.
fn task_visible_in_workspace(
    task: &Task,
    entity_ids: &HashSet<String>,
    workspace_id: &str,
) -> bool {
    if let Some(ref eid) = task.entity_id {
        entity_ids.contains(eid)
    } else {
        workspace_id == DEFAULT_WORKSPACE_ID
    }
}

fn feed_visible_in_workspace(
    event: &OperatorFeedEvent,
    entity_ids: &HashSet<String>,
    workspace_id: &str,
) -> bool {
    if let Some(ref entity_id) = event.entity_id {
        entity_ids.contains(entity_id)
    } else {
        workspace_id == DEFAULT_WORKSPACE_ID
    }
}

mod github_device_api;
use github_device_api::*;
mod captures_api;
use captures_api::*;
mod captures_external_api;
use captures_external_api::*;
mod missions_api;
use missions_api::*;
mod tasks_api;
use tasks_api::*;
mod entity_import_api;
pub(crate) use entity_import_api::*;
mod entity_records_api;
use entity_records_api::*;
mod approvals_api;
use approvals_api::*;
mod devices_api;
pub(crate) use devices_api::*;
mod workspaces_api;
pub(crate) use workspaces_api::*;
mod verification_api;
pub(crate) use verification_api::*;
mod entity_files_api;
pub(crate) use entity_files_api::*;
mod chat_attachments_api;
use chat_attachments_api::*;
mod mission_engine_api;
pub(crate) use mission_engine_api::*;
mod directors_api;
pub(crate) use directors_api::*;
mod agent_groups_api;
use agent_groups_api::*;
mod llm_credentials_api;
use llm_credentials_api::*;
mod llm_providers_api;
use llm_providers_api::*;
mod heartbeats_api;
pub(crate) use heartbeats_api::*;
mod routing_api;
use routing_api::*;
mod ollama_api;
use ollama_api::*;
mod capture_chat_api;
use capture_chat_api::*;
mod entity_connections_api;
use entity_connections_api::*;
mod daily_review;
use daily_review::*;
mod skills_api;
use skills_api::*;

// ─── Pagination ──────────────────────────────────────────────────────────────

/// Generic page of results returned by list endpoints that support
/// `limit`/`offset` pagination.
#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T> {
    pub items: Vec<T>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
    #[serde(rename = "hasMore")]
    pub has_more: bool,
}

/// Query-string parameters accepted by paginated list endpoints.
#[derive(Debug, Deserialize)]
pub struct PaginationQuery {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// Slices `items` down to the requested page (limit capped at 200).
fn apply_pagination<T: Clone>(
    items: Vec<T>,
    total: usize,
    query: &PaginationQuery,
) -> PaginatedResponse<T> {
    let limit = query.limit.unwrap_or(50).min(200);
    let offset = query.offset.unwrap_or(0);
    let has_more = offset + limit < total;
    let sliced = if offset >= items.len() {
        Vec::new()
    } else {
        let end = (offset + limit).min(items.len());
        items[offset..end].to_vec()
    };
    PaginatedResponse {
        items: sliced,
        total,
        limit,
        offset,
        has_more,
    }
}

// ─── Request bodies ─────────────────────────────────────────────────────────

/// Body for `POST` task-creation endpoints.
#[derive(Debug, Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    pub dept: Option<String>,
    pub roles: Option<Vec<String>>,
    pub entity_id: Option<String>,
    pub mission_id: Option<String>,
    pub priority: Option<String>,
    /// When true, orchestrator will not auto-pick; task waits for human approval.
    pub requires_human_approval: Option<bool>,
    /// Task kind for routing (e.g. entity-enrichment, content-draft). String for flexibility.
    pub task_kind: Option<String>,
    /// Optional captured body stored in `result` for v1 note/task/reminder flows.
    pub content: Option<String>,
    /// Optional director lane to assign immediately at creation time.
    pub director_id: Option<String>,
    /// Optional agent to assign immediately at creation time.
    pub agent_id: Option<String>,
    /// Optional connection that created or owns this task.
    pub connection_id: Option<String>,
    /// External producer name, e.g. `openclaw`.
    pub external_source: Option<String>,
    /// Producer-stable id for idempotency and traceability.
    pub external_id: Option<String>,
    /// Flexible structured payload from integrations or importers.
    pub metadata: Option<serde_json::Value>,
    /// RFC3339 deadline; drives the overdue sweep and the daily review.
    #[serde(default)]
    pub due_at: Option<String>,
}

/// Body for launching a coding-agent run (Claude/OpenCode/Cursor) scoped to
/// an entity's local repo directory.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateEntityAgentRunRequest {
    /// claude | opencode | cursor-agent
    pub kind: String,
    pub prompt: String,
    /// Optional working directory. Must be inside the entity local/repo root.
    pub cwd: Option<String>,
    pub timeout_secs: Option<u64>,
    pub title: Option<String>,
    pub create_task: Option<bool>,
    /// Review policy for this run: `none` (default) | `manual` | `auto`.
    #[serde(default)]
    pub review_policy: Option<String>,
    /// Reviewer agent kind for `auto` review. Defaults to the run's own kind.
    #[serde(default)]
    pub reviewer_provider: Option<String>,
    /// `bypassPermissions` (default) | `acceptEdits` | `default`. Claude only —
    /// every run is confined to the entity's cwd regardless; this controls how
    /// much the agent stops to ask inside that jail.
    #[serde(default)]
    pub permission_mode: Option<String>,
    /// Cap the agent's turns. Claude only.
    #[serde(default)]
    pub max_turns: Option<u32>,
    /// Continue a previous Claude Code session instead of starting cold.
    #[serde(default)]
    pub resume_session_id: Option<String>,
}

/// Response for a coding-agent run: status, output, and optional review
/// outcome when a review policy was requested.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityAgentRunResponse {
    pub run_id: String,
    pub provider: String,
    pub entity_id: String,
    pub task_id: Option<String>,
    pub status: String,
    pub cwd: String,
    pub output: String,
    pub error: Option<String>,
    pub started_at: String,
    pub completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_verdict: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_run_id: Option<String>,
}

/// Partial-update body for `PATCH` task endpoints; all fields optional.
#[derive(Debug, Deserialize)]
pub struct UpdateTaskRequest {
    pub status: Option<TaskStatus>,
    pub progress: Option<u8>,
    pub result: Option<String>,
    /// RFC3339 deadline. Absent leaves the existing due date untouched.
    #[serde(default)]
    pub due_at: Option<String>,
}

/// Query-string parameters for listing raw captures.
#[derive(Debug, Deserialize)]
struct CaptureQuery {
    entity_id: Option<String>,
    status: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    /// When true, attach a deterministic routing suggestion to each capture.
    #[serde(default)]
    with_route: bool,
    /// Entity currently focused in the UI, used as a weak routing fallback.
    #[serde(default)]
    selected_entity_id: Option<String>,
}

/// A raw capture optionally decorated with a routing suggestion. `route` is
/// omitted (flattened capture only) unless `with_route=true` was requested, so
/// the shape is backward compatible with the plain capture list.
#[derive(Debug, Serialize)]
pub struct RawCaptureWithRoute {
    #[serde(flatten)]
    pub capture: RawCapture,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<crate::capture_routing::RouteSuggestion>,
}

/// Body for creating a raw capture (an unprocessed note/thought/idea).
#[derive(Debug, Deserialize)]
pub struct CreateCaptureRequest {
    /// The raw, pre-interpretation text to capture.
    pub raw_text: String,
    /// Entity this capture belongs to; omit for inbox-level captures.
    #[serde(default)]
    pub entity_id: Option<String>,
    /// Producer channel; defaults to `chat` when absent.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub source_ref: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// Partial-update body for a raw capture.
#[derive(Debug, Deserialize)]
pub struct UpdateCaptureRequest {
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub status: Option<RawCaptureStatus>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// Body for promoting a capture into a task.
#[derive(Debug, Deserialize)]
pub struct PromoteCaptureTaskRequest {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub dept: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    #[serde(default)]
    pub director_id: Option<String>,
    #[serde(default)]
    pub task_kind: Option<String>,
    #[serde(default)]
    pub requires_human_approval: Option<bool>,
}

/// Response after promoting a capture: the (now-updated) capture plus the
/// task it produced.
#[derive(Debug, Serialize)]
pub struct PromoteCaptureResponse {
    pub capture: RawCapture,
    pub task: Task,
}

/// Body for promoting a capture into a narrative/journal update on an entity.
#[derive(Debug, Deserialize)]
pub struct UpdateCaptureNarrativeRequest {
    /// Entity to attach the narrative update to. Falls back to the entity
    /// already on the capture when omitted.
    #[serde(default)]
    pub entity_id: Option<String>,
    /// Reviewed note to append. Falls back to the capture summary, then the
    /// raw text, when omitted.
    #[serde(default)]
    pub note: Option<String>,
}

/// Body for promoting a capture into a stored memory/knowledge note.
#[derive(Debug, Deserialize)]
pub struct PromoteCaptureMemoryRequest {
    #[serde(default)]
    pub title: Option<String>,
    /// Entity to file the note under. Falls back to the capture's entity;
    /// when neither is present the note lands in the workspace inbox.
    #[serde(default)]
    pub entity_id: Option<String>,
    /// Target knowledge area: `canonical` (default) or `lessons`.
    #[serde(default)]
    pub kind: Option<String>,
    /// Note body. Falls back to the capture summary, then the raw text.
    #[serde(default)]
    pub summary: Option<String>,
}

/// Response after marking a capture reviewed: the capture plus the vault
/// note path that recorded it.
#[derive(Debug, Serialize)]
pub struct CaptureReviewResponse {
    pub capture: RawCapture,
    /// Absolute path of the local vault note written by the review action.
    pub note_path: String,
}

/// Body for requesting (and optionally auto-applying) a routing suggestion
/// for a capture.
#[derive(Debug, Deserialize)]
pub struct RouteCaptureRequest {
    /// Entity currently focused in the UI, used as a weak routing fallback.
    #[serde(default)]
    pub selected_entity_id: Option<String>,
    /// When true, apply the suggestion if its confidence clears the threshold.
    #[serde(default)]
    pub auto_apply: bool,
}

/// Response carrying a capture's routing suggestion and whether it was applied.
#[derive(Debug, Serialize)]
pub struct RouteCaptureResponse {
    pub capture: RawCapture,
    pub suggestion: crate::capture_routing::RouteSuggestion,
    /// True when the suggestion was applied (capture.entity_id was set).
    pub applied: bool,
}

/// Body for manually attaching a capture to an entity.
#[derive(Debug, Deserialize)]
pub struct AttachCaptureRequest {
    pub entity_id: String,
}

/// External ingestion payload: arbitrary tools (Claude Code, Codex, Manus,
/// OpenClaw, browser sessions, shell scripts, webhooks) post logs/prompts/results
/// here and they land in the raw capture loop.
#[derive(Debug, Clone, Deserialize)]
pub struct ExternalCaptureRequest {
    pub raw_text: String,
    /// Producer channel, e.g. `claude_code | codex | manus | openclaw | browser | shell | webhook`.
    pub source: String,
    #[serde(default)]
    pub source_ref: Option<String>,
    /// Producer-stable id used for idempotency within `(workspace, source)`.
    #[serde(default)]
    pub external_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub entity_id: Option<String>,
    /// Working directory the tool ran in (feeds routing).
    #[serde(default)]
    pub cwd: Option<String>,
    /// Repository URL the tool operated on (feeds routing).
    #[serde(default)]
    pub repo_url: Option<String>,
    /// A page/resource URL relevant to the capture (feeds routing).
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// When true, run deterministic routing and attach only at high confidence.
    #[serde(default)]
    pub auto_route: bool,
    /// UI-focused entity used as a weak routing fallback.
    #[serde(default)]
    pub selected_entity_id: Option<String>,
}

/// Response for a single external-ingestion capture.
#[derive(Debug, Serialize)]
pub struct ExternalCaptureResponse {
    pub capture: RawCapture,
    pub suggestion: crate::capture_routing::RouteSuggestion,
    /// True when routing attached an entity to this capture.
    pub applied: bool,
    /// True when an existing capture was returned instead of creating a new one.
    pub existing: bool,
}

/// Body for ingesting a batch of external captures in one call.
#[derive(Debug, Deserialize)]
pub struct ExternalCaptureBatchRequest {
    pub items: Vec<ExternalCaptureRequest>,
}

/// Outcome for one item within an [`ExternalCaptureBatchRequest`].
#[derive(Debug, Serialize)]
pub struct ExternalCaptureBatchItemResult {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<RawCapture>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<crate::capture_routing::RouteSuggestion>,
    #[serde(default)]
    pub applied: bool,
    #[serde(default)]
    pub existing: bool,
    /// HTTP-style status code for a failed item (e.g. 400, 403).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_status: Option<u16>,
}

/// Response summarizing a batch external-capture ingestion.
#[derive(Debug, Serialize)]
pub struct ExternalCaptureBatchResponse {
    pub results: Vec<ExternalCaptureBatchItemResult>,
    pub created: usize,
    pub existing: usize,
    pub failed: usize,
}

/// Body for `POST` entity creation.
#[derive(Debug, Deserialize)]
pub struct CreateEntityRequest {
    pub name: String,
    pub description: Option<String>,
    pub department: Option<String>,
    pub owner: Option<String>,
    pub status: Option<String>,
    /// Optional hub type (e.g. brand, product). Ignored when absent.
    #[serde(default)]
    pub entity_type: Option<String>,
    /// Optional workspace root hint for entity + space flows.
    #[serde(default)]
    pub default_workspace_root: Option<String>,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub setup: Option<EntitySetup>,
}

/// Body for ingesting an external URL (repo, doc, etc.) as source context,
/// either into an existing entity or a newly created one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestEntitySourceRequest {
    pub url: String,
    /// Attach source context to this entity when present; otherwise create a new entity.
    #[serde(default)]
    pub entity_id: Option<String>,
    /// Optional user-provided name hint for a newly created entity.
    #[serde(default)]
    pub name: Option<String>,
}

/// Response after ingesting a source URL into an entity.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestEntitySourceResponse {
    pub entity: Entity,
    pub created: bool,
    pub attached: bool,
    pub source_url: String,
    pub summary: String,
}

/// LLM-extracted narrative fields parsed out of an ingested external source,
/// used to seed an entity's profile/goals/stack fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SourceNarrative {
    entity_name: Option<String>,
    profile: Option<String>,
    goals: Option<String>,
    active_context: Option<String>,
    operating_rules: Option<String>,
    entity_type: Option<String>,
    primary_domain: Option<String>,
    repo_path: Option<String>,
    backend_stack: Option<String>,
    frontend_stack: Option<String>,
    database_stack: Option<String>,
    success_criteria: Option<String>,
    summary: Option<String>,
}

/// Fetched content of an external source (repo README, doc page, etc.)
/// before it's summarized into a [`SourceNarrative`].
#[derive(Debug, Clone)]
pub(crate) struct SourceContext {
    pub(crate) url: String,
    pub(crate) kind: String,
    pub(crate) title: Option<String>,
    pub(crate) text: String,
}

/// Partial-update body for `PATCH` entity endpoints; all fields optional.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEntityRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub department: Option<String>,
    pub status: Option<String>,
    #[serde(default)]
    pub entity_type: Option<String>,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub goals: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub active_context: Option<String>,
    #[serde(default)]
    pub operating_rules: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub content_pipeline: Option<String>,
    #[serde(default)]
    pub setup: Option<EntitySetup>,
    #[serde(default)]
    pub entity_runtime: Option<EntityRuntimeConfig>,
    #[serde(default)]
    pub preferred_model_tier: Option<String>,
    #[serde(default)]
    pub local_path: Option<Option<String>>,
    #[serde(default)]
    pub flow_diagrams: Option<crate::state::EntityFlowDiagrams>,
}

/// Body for creating an entity connection (an external integration link,
/// e.g. GitHub, Slack) on an entity.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateEntityConnectionRequest {
    pub kind: String,
    pub provider: String,
    pub label: Option<String>,
    pub status: Option<String>,
    pub capability_state: Option<serde_json::Value>,
    pub config: Option<serde_json::Value>,
    pub secret_ref: Option<String>,
    pub secret_value: Option<String>,
    pub external_account_id: Option<String>,
    pub external_handle: Option<String>,
    pub scopes: Option<Vec<String>>,
    pub created_by: Option<String>,
}

/// One item in a bulk entity-records upsert — see [`upsert_entity_records`].
/// `kind` is open-ended (documented: `contact`, `customer`, `deal`,
/// `content`, `account`, `file`, `metric`, `event`, but any string is
/// accepted). `source` + `source_ref` together identify the row *in the
/// source system* and are what makes re-posting it idempotent.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpsertEntityRecordItem {
    pub kind: String,
    pub source: String,
    pub source_ref: String,
    #[serde(default)]
    pub fields: serde_json::Value,
    pub evidence: Option<String>,
    /// When the source confirmed this row was current. Defaults to now — most
    /// callers sync and post in the same breath, so there is rarely a
    /// meaningful gap to record.
    pub fetched_at: Option<String>,
    pub created_by: Option<String>,
}

/// Body for `POST /api/entities/{id}/records` — a batch, because a sync run
/// naturally produces many rows at once and a per-row round trip would make
/// syncing hundreds of CRM customers needlessly slow.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkUpsertEntityRecordsRequest {
    pub records: Vec<UpsertEntityRecordItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BulkUpsertEntityRecordsResponse {
    pub upserted: usize,
    pub records: Vec<EntityRecord>,
}

/// Query-string filter for `GET /api/entities/{id}/records`.
#[derive(Debug, Deserialize)]
pub struct EntityRecordsListQuery {
    pub kind: Option<String>,
}

/// Body for ingesting an OpenClaw order into a task.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenClawOrderIngestRequest {
    /// OpenClaw order id. `id` is accepted as a compatibility alias.
    pub order_id: Option<String>,
    pub id: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub mission_id: Option<String>,
    pub director_id: Option<String>,
    pub task_kind: Option<String>,
    pub requires_human_approval: Option<bool>,
    #[serde(default)]
    pub payload: Option<serde_json::Value>,
}

/// Partial-update body for an entity connection.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEntityConnectionRequest {
    pub label: Option<String>,
    pub status: Option<String>,
    pub capability_state: Option<serde_json::Value>,
    pub config: Option<serde_json::Value>,
    pub secret_ref: Option<String>,
    pub secret_value: Option<String>,
    pub external_account_id: Option<String>,
    pub external_handle: Option<String>,
    pub scopes: Option<Vec<String>>,
}

/// Body for verifying an entity connection (e.g. confirming a webhook token).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyEntityConnectionRequest {
    pub verification_method: Option<String>,
    pub token: Option<String>,
}

/// Body for starting an OAuth-style connection session for an entity.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateEntityConnectionSessionRequest {
    pub provider: String,
    pub requested_capabilities: Option<Vec<String>>,
    pub created_by: Option<String>,
}

/// Body for completing a connection session once the external provider
/// callback has returned its credentials.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FinalizeEntityConnectionSessionRequest {
    pub status: Option<String>,
    pub secret_ref: Option<String>,
    pub secret_value: Option<String>,
    pub external_account_id: Option<String>,
    pub external_handle: Option<String>,
    pub scopes: Option<Vec<String>>,
}

// GitHub OAuth device-flow wire types, used to authorize a connection
// without a browser redirect back to this server.
/// GitHub's response to starting a device-flow authorization.
#[derive(Debug, Deserialize)]
struct GitHubDeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

/// GitHub's response when polling a device code for the resulting token.
#[derive(Debug, Deserialize)]
struct GitHubDeviceTokenResponse {
    access_token: Option<String>,
    scope: Option<String>,
    token_type: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// Minimal shape of GitHub's `/user` response, used to label the connection.
#[derive(Debug, Deserialize)]
struct GitHubUserResponse {
    id: u64,
    login: String,
}

/// Body for triggering an ad hoc action on an entity through a given channel.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PerformEntityActionRequest {
    pub action: String,
    pub channel: String,
    pub payload: Option<serde_json::Value>,
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
}

/// Partial apply: list of draft field names to copy to final entity (e.g. ["profile", "goals"]).
#[derive(Debug, Deserialize)]
pub struct ApplyEnrichmentRequest {
    #[serde(default)]
    pub fields: Vec<String>,
    pub draft_version: Option<u32>,
}

/// Body for registering an LLM provider (e.g. Anthropic, Ollama).
#[derive(Debug, Deserialize)]
pub struct CreateLlmProviderRequest {
    pub name: String,
    pub provider_type: String,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

/// Partial-update body for an LLM provider.
#[derive(Debug, Deserialize)]
pub struct UpdateLlmProviderRequest {
    pub name: Option<String>,
    pub provider_type: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
}

/// Body for registering a model under an LLM provider.
#[derive(Debug, Deserialize)]
pub struct CreateLlmModelRequest {
    pub provider_id: String,
    pub name: String,
    pub is_default: Option<bool>,
}

/// Partial-update body for an LLM model.
#[derive(Debug, Deserialize)]
pub struct UpdateLlmModelRequest {
    pub name: Option<String>,
    pub is_default: Option<bool>,
}

/// Body for switching the global LLM routing mode (e.g. auto vs. pinned).
#[derive(Debug, Deserialize)]
pub struct SetLlmModeRequest {
    pub mode: String,
}

/// Body for pinning the active provider/model pair.
#[derive(Debug, Deserialize)]
pub struct SetLlmProfileRequest {
    pub provider: String,
    pub model: String,
}

/// Current LLM routing state returned to the dashboard: mode, resolved
/// provider/model, and whether a secret is configured for it.
#[derive(Debug, Serialize)]
struct LlmProfileDto {
    mode: String,
    #[serde(rename = "current_model")]
    current_model: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    preferred_provider: Option<String>,
    preferred_model: Option<String>,
    secret_configured: bool,
    secret_source: Option<String>,
}

/// Partial-update body for the workspace's cloud compute configuration.
#[derive(Debug, Deserialize)]
pub struct UpdateCloudConfigRequest {
    pub provider: Option<String>,
    pub region: Option<String>,
    pub compute_profile: Option<String>,
}

/// Body for registering a storage backend (e.g. S3-compatible bucket).
#[derive(Debug, Deserialize)]
pub struct CreateStorageBackendRequest {
    pub name: String,
    pub backend_type: String,
    pub config: Option<HashMap<String, String>>,
}

/// Body for creating a task-orchestration routing rule (which director/model
/// handles tasks matching a project/department/tag).
#[derive(Debug, Deserialize)]
pub struct CreateOrchestrationRuleRequest {
    pub match_project: Option<String>,
    pub match_department: Option<String>,
    pub match_tag: Option<String>,
    pub director_id: Option<String>,
    pub model_id: Option<String>,
    pub retry_count: Option<u32>,
}

/// Body for posting a chat message.
#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
}

/// Body for creating an agent group (department/team grouping of agents).
#[derive(Debug, Deserialize)]
pub struct CreateAgentGroupRequest {
    pub id: Option<String>,
    pub name: String,
    pub icon: Option<String>,
    pub description: Option<String>,
}

/// Body for creating a chat agent within a group.
#[derive(Debug, Deserialize)]
pub struct CreateAgentRequest {
    pub id: Option<String>,
    pub group_id: String,
    pub name: String,
    pub role: String,
    pub kind: Option<String>,
    pub preferred_provider: Option<String>,
    pub preferred_model: Option<String>,
    pub mission: Option<String>,
    pub profile: Option<String>,
}

/// Body for issuing a director command (an ad hoc instruction run by a director).
#[derive(Debug, Deserialize)]
pub struct CreateCommandRequest {
    pub director_id: String,
    pub agent_id: Option<String>,
    pub entity_id: Option<String>,
    pub mission_id: Option<String>,
    pub prompt: String,
    /// Optional mode hint, e.g. "auto", "draft-only", "decision-required".
    pub mode: Option<String>,
    /// Optional capture intent from Home: note | task | reminder (v1 note/reminder are task-backed).
    pub capture_intent: Option<String>,
}

/// Body for creating a mission under an entity.
#[derive(Debug, Deserialize)]
pub struct CreateMissionRequest {
    pub entity_id: String,
    pub title: String,
    pub summary: Option<String>,
    pub status: Option<MissionStatus>,
    pub priority: Option<String>,
    pub goal: Option<String>,
    pub owner: Option<String>,
    pub created_by: Option<String>,
    pub source: Option<String>,
    pub review_cadence_hours: Option<u32>,
    pub narrative: Option<String>,
    pub canonical_notes: Option<Vec<String>>,
    pub assigned_director_id: Option<String>,
    /// Presence enables the iterate-until-done loop on this mission.
    #[serde(default)]
    pub loop_config: Option<crate::state::MissionLoopConfig>,
}

/// Partial-update body for a mission; all fields optional.
#[derive(Debug, Deserialize)]
pub struct UpdateMissionRequest {
    pub title: Option<String>,
    pub summary: Option<String>,
    pub status: Option<MissionStatus>,
    pub priority: Option<String>,
    pub goal: Option<String>,
    pub owner: Option<String>,
    pub last_reviewed_at: Option<String>,
    pub review_cadence_hours: Option<u32>,
    pub narrative: Option<String>,
    pub canonical_notes: Option<Vec<String>>,
    pub assigned_director_id: Option<String>,
}

/// Response after issuing a director command: the task it created plus routing.
#[derive(Debug, Serialize)]
pub struct CommandResponse {
    pub task_id: String,
    pub director_id: String,
    pub agent_id: Option<String>,
    pub entity_id: Option<String>,
}

// ─── Route helpers (status string <-> enum) ──────────────────────────────────

fn task_status_from_str(s: &str) -> TaskStatus {
    match s.to_lowercase().as_str() {
        "backlog" => TaskStatus::Backlog,
        "queued" | "in-queue" => TaskStatus::Queued,
        "in-progress" | "in_progress" => TaskStatus::InProgress,
        "failed" | "error" => TaskStatus::Failed,
        "completed" => TaskStatus::Completed,
        "cancelled" | "canceled" => TaskStatus::Cancelled,
        _ => TaskStatus::Backlog,
    }
}

fn task_status_to_str(s: TaskStatus) -> &'static str {
    match s {
        TaskStatus::Backlog => "backlog",
        TaskStatus::Queued => "queued",
        TaskStatus::InProgress => "in-progress",
        TaskStatus::Failed => "failed",
        TaskStatus::Completed => "completed",
        TaskStatus::Cancelled => "cancelled",
    }
}

/// Maps a free-form capture-intent string to one of the canonical values
/// (`note` | `task` | `reminder`), or `None` if unrecognized.
fn normalize_capture_intent(raw: Option<&str>) -> Option<&'static str> {
    match raw.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if v == "note" => Some("note"),
        Some(v) if v == "task" => Some("task"),
        Some(v) if v == "reminder" || v == "followup" || v == "follow-up" => Some("reminder"),
        _ => None,
    }
}

/// True when `raw` is present but doesn't normalize to a known capture intent.
fn has_invalid_capture_intent(raw: Option<&str>) -> bool {
    match raw {
        None => false,
        Some(value) => normalize_capture_intent(Some(value)).is_none(),
    }
}

/// Scores how fully an entity's foundation fields are filled in, and picks
/// the next onboarding question to ask for the biggest missing piece.
fn compute_entity_readiness(entity: &Entity) -> EntityReadiness {
    let mut score = 0u8;
    let mut missing = Vec::new();

    if !entity.name.trim().is_empty() {
        score = score.saturating_add(10);
    }
    if entity
        .setup
        .what_it_is
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
    {
        score = score.saturating_add(15);
    } else {
        missing.push("what_it_is".to_string());
    }
    if entity
        .setup
        .primary_goal
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
        || entity
            .goals
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some()
    {
        score = score.saturating_add(20);
    } else {
        missing.push("primary_goal".to_string());
    }
    if entity
        .stage
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
    {
        score = score.saturating_add(10);
    } else {
        missing.push("stage".to_string());
    }
    if entity
        .setup
        .success_criteria
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_some()
    {
        score = score.saturating_add(10);
    } else {
        missing.push("success_criteria".to_string());
    }
    let has_asset = !entity.domains.is_empty()
        || entity
            .setup
            .primary_domain
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some()
        || entity
            .setup
            .primary_email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some()
        || entity
            .setup
            .repo_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some();
    if has_asset {
        score = score.saturating_add(15);
    } else {
        missing.push("asset".to_string());
    }
    let tech_points = [
        entity.setup.backend_stack.as_deref(),
        entity.setup.frontend_stack.as_deref(),
        entity.setup.database_stack.as_deref(),
    ]
    .iter()
    .filter(|v| v.map(str::trim).filter(|s| !s.is_empty()).is_some())
    .count() as u8;
    score = score.saturating_add((tech_points * 6).min(20));
    if tech_points == 0 {
        missing.push("director_context".to_string());
    }
    let label = if score < 15 {
        "name only"
    } else if score < 40 {
        "basic setup"
    } else if score < 70 {
        "partially configured"
    } else if score < 90 {
        "execution ready"
    } else {
        "well configured"
    }
    .to_string();
    let next_question = missing.first().map(|field| match field.as_str() {
        "what_it_is" => "What is this entity about?".to_string(),
        "primary_goal" => "What is the main goal for this entity?".to_string(),
        "stage" => "What stage is this entity at right now?".to_string(),
        "success_criteria" => "What does success look like for this entity?".to_string(),
        "asset" => "Does this entity have a domain, email, or codebase yet?".to_string(),
        "director_context" => {
            "If this entity involves product or code, what stack or tools does it use?".to_string()
        }
        _ => "What should we know next about this entity?".to_string(),
    });
    EntityReadiness {
        percent: score.min(100),
        label,
        next_question,
        missing_fields: missing,
    }
}

/// Recomputes and stores `entity.readiness` in place.
pub(crate) fn refresh_entity_readiness(entity: &mut Entity) {
    entity.readiness = compute_entity_readiness(entity);
}

/// Normalizes a raw task-kind string, mapping capture-style synonyms
/// (followup/follow-up) onto the canonical `reminder` kind.
fn canonicalize_task_kind(raw: Option<String>) -> Option<String> {
    match raw.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) if v == "note" => Some("note".to_string()),
        Some(v) if v == "task" => Some("task".to_string()),
        Some(v) if v == "reminder" || v == "followup" || v == "follow-up" => {
            Some("reminder".to_string())
        }
        Some(v) if v.is_empty() => None,
        Some(v) => Some(v),
        None => None,
    }
}

/// Display label prefix for a capture, based on its intent.
fn capture_prefix(intent: Option<&str>) -> &'static str {
    match intent {
        Some("note") => "Note",
        Some("task") => "Task",
        Some("reminder") => "Reminder",
        _ => "Capture",
    }
}

/// Human-readable detail message for the feed event emitted when a task is
/// created from a capture.
fn task_created_detail(task_kind: Option<&str>) -> Option<String> {
    match task_kind {
        Some("note") => Some("Note captured".to_string()),
        Some("task") => Some("Task created from capture".to_string()),
        Some("reminder") => Some("Reminder scheduled from capture".to_string()),
        Some("entity-enrichment") => Some("Entity enrichment queued".to_string()),
        _ => None,
    }
}

/// Whether `priority` is one of the accepted task priority values.
pub(crate) fn is_valid_priority(priority: &str) -> bool {
    matches!(
        priority.to_ascii_lowercase().as_str(),
        "low" | "medium" | "high" | "critical"
    )
}

/// Actions a human can take on a mission's review prompt.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MissionActionType {
    ReviewNow,
    SnoozeReview,
    AcceptProposedNextStep,
    DismissNoAction,
    MarkUserInputNeeded,
}

/// Body for the mission-review-action endpoint.
#[derive(Debug, Deserialize)]
struct MissionActionRequest {
    action: MissionActionType,
    hours: Option<u32>,
    note: Option<String>,
}

/// Resolves a task's `entity_id` against its `mission_id` (if given),
/// rejecting a request whose entity doesn't match the mission's entity.
fn resolve_task_entity_and_validate_mission(
    missions: &[Mission],
    entity_id: Option<String>,
    mission_id: Option<String>,
) -> Result<(Option<String>, Option<String>), StatusCode> {
    if let Some(mid) = mission_id.as_ref() {
        let mission = missions
            .iter()
            .find(|m| m.id == *mid)
            .ok_or(StatusCode::BAD_REQUEST)?;
        if let Some(eid) = entity_id.as_ref() {
            if eid != &mission.entity_id {
                return Err(StatusCode::BAD_REQUEST);
            }
            return Ok((entity_id, mission_id));
        }
        return Ok((Some(mission.entity_id.clone()), mission_id));
    }
    Ok((entity_id, mission_id))
}

// ─── Shared create paths (handlers + capture/commit) ─────────────────────────

/// Core entity-creation logic shared by the `POST` entity handler and other
/// internal callers (e.g. capture-to-entity flows): builds the [`Entity`],
/// seeds enrichment/readiness, provisions default heartbeats, and persists it.
pub(crate) async fn internal_create_entity(
    state: &ServerState,
    s: &crate::state::ControlState,
    workspace_id: &str,
    body: CreateEntityRequest,
) -> Result<Entity, StatusCode> {
    let config_loaded = state.app_config.is_some();
    let can_enrich = state
        .app_config
        .as_ref()
        .map(|cfg| crate::config::llm_ready_for_entity_enrichment(cfg.as_ref()))
        .unwrap_or(false);
    let id = Uuid::new_v4().to_string();
    let dept = body
        .department
        .clone()
        .unwrap_or_else(|| "General".to_string());
    let workspace_root = derive_entity_workspace_root(
        workspace_id,
        body.default_workspace_root.as_deref(),
        &body.name,
    );
    let entity_path = derive_entity_path(workspace_id, workspace_root.as_deref());
    let (profile, goals, active_context, operating_rules) =
        infer_entity_foundation(&body.name, body.description.as_deref());
    let input_snapshot = EnrichmentInputSnapshot {
        name: Some(body.name.clone()),
        description: body.description.clone(),
        department: body.department.clone(),
        owner: body.owner.clone(),
    };
    let enrichment = if can_enrich {
        EntityEnrichment {
            status: "pending".to_string(),
            error: None,
            input_snapshot: Some(input_snapshot),
            draft: None,
            generated_by: None,
            model: None,
            generated_at: None,
            draft_version: Some(1),
        }
    } else {
        let msg = if !config_loaded {
            "LLM settings are not loaded yet. The entity was created — open Settings or refresh, then use “Retry enrichment”. You can also fill in foundation fields manually below."
                .to_string()
        } else {
            "LLM is not configured for enrichment (missing or empty API key, or no keyless local provider). The entity was created — add a provider key in Settings or fix routing, then “Retry enrichment”, or continue manually."
                .to_string()
        };
        EntityEnrichment {
            status: "ai_unavailable".to_string(),
            error: Some(msg),
            input_snapshot: Some(input_snapshot),
            draft: None,
            generated_by: None,
            model: None,
            generated_at: None,
            draft_version: Some(1),
        }
    };
    if let Some(ref provided_owner) = body.owner
        && provided_owner.trim() != workspace_id
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let mut entity = Entity {
        id: id.clone(),
        name: body.name,
        description: body.description,
        department: body.department.clone(),
        owner: Some(workspace_id.to_string()),
        status: body.status.unwrap_or_else(|| "Active".to_string()),
        entity_type: body.entity_type.clone(),
        dept: dept.clone(),
        file_count: 0,
        path: entity_path,
        workspace_id: Some(workspace_id.to_string()),
        visibility: EntityVisibility::Workspace,
        local_path: None,
        files: None,
        stage: body.stage,
        domains: vec![],
        markets: vec![],
        profile,
        goals,
        active_context,
        operating_rules,
        notes: None,
        content_pipeline: None,
        relationships: vec![],
        enrichment: Some(enrichment),
        resources: vec![],
        execution_policies: None,
        default_workspace_root: workspace_root,
        memory_scope: None,
        preferred_model_tier: None,
        allowed_agent_groups: vec![],
        setup: body.setup.unwrap_or_default(),
        readiness: EntityReadiness::default(),
        entity_runtime: None,
        colour: Some(crate::entity_creation::pick_entity_colour(&id)),
        ..Default::default()
    };
    refresh_entity_readiness(&mut entity);
    entity.local_path = crate::local_vault::ensure_entity_folder(&entity, workspace_id)
        .await
        .map(|p| p.to_string_lossy().to_string());
    s.entities.write().await.push(entity.clone());

    // Every entity gets an operating rhythm from birth — a daily pulse, a
    // weekly review, and an ideas beat, routed to the directors that match what
    // this entity actually is. Best-effort: a heartbeat store that is missing or
    // erroring must never fail entity creation.
    if let Some(hb_store) = state.heartbeat_store.as_ref()
        && let Err(e) = crate::entity_heartbeat_defaults::provision_default_heartbeats(
            hb_store.as_ref(),
            &entity,
            Some(workspace_id),
        )
        .await
    {
        tracing::warn!(entity_id = %entity.id, error = %e, "default heartbeat provisioning failed");
    }

    let already_queued = {
        let tasks = s.tasks.read().await;
        tasks.iter().any(|t| {
            t.entity_id.as_deref() == Some(id.as_str())
                && t.task_kind.as_deref() == Some("entity-enrichment")
                && (t.status == TaskStatus::Queued || t.status == TaskStatus::InProgress)
        })
    };
    if can_enrich && !already_queued {
        let enrichment_task = Task {
            id: Uuid::new_v4().to_string(),
            title: format!("Enrich entity: {}", entity.name),
            dept: dept.clone(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some(id),
            mission_id: None,
            priority: None,
            requires_human_approval: false,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: None,
            task_kind: Some("entity-enrichment".to_string()),
            metadata: None,
            outcome: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        };
        let t = enrichment_task.clone();
        s.tasks.write().await.push(enrichment_task);
        s.push_operator_feed(OperatorFeedKind::TaskCreated, &t, None)
            .await;
    }

    Ok(entity)
}

/// Trims `value` and turns an empty result into `None`.
fn non_empty_string(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Builds a read-only [`EntityResource`] pointing at an ingested source URL.
fn source_resource(context: &SourceContext) -> EntityResource {
    EntityResource {
        id: Uuid::new_v4().to_string(),
        kind: context.kind.clone(),
        root: context.url.clone(),
        read_allowed: true,
        write_allowed: false,
        exec_allowed: false,
    }
}

/// Renders a [`SourceNarrative`] into the profile/goals/context/rules text
/// block used to seed an entity's description.
fn source_brief(narrative: &SourceNarrative, context: &SourceContext) -> String {
    let profile = narrative
        .profile
        .as_deref()
        .or(narrative.summary.as_deref())
        .unwrap_or("Source imported into Consistent for entity onboarding.");
    let goals = narrative
        .goals
        .as_deref()
        .unwrap_or("Clarify the entity goal and convert the source context into actionable work.");
    let active_context = narrative
        .active_context
        .as_deref()
        .unwrap_or("Imported from an external source. Review and refine the operating narrative.");
    let operating_rules = narrative
        .operating_rules
        .as_deref()
        .unwrap_or("Treat source material as context until Romeo confirms it as durable truth.");
    format!(
        "Profile: {profile}\n\nGoals: {goals}\n\nActive context: {active_context}\n\nOperating rules: {operating_rules}\n\nSource: {}\n\nSource excerpt:\n{}",
        context.url,
        context.text.chars().take(1800).collect::<String>()
    )
}

/// Derives a title-cased entity name from the last path segment of a URL.
fn source_title_from_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let tail = trimmed
        .rsplit('/')
        .find(|part| !part.trim().is_empty())
        .unwrap_or("Imported entity");
    tail.replace(['-', '_'], " ")
        .split_whitespace()
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds a [`SourceNarrative`] without an LLM call, used when enrichment is
/// unavailable so source ingestion still produces a usable entity draft.
fn fallback_source_narrative(context: &SourceContext, name_hint: Option<&str>) -> SourceNarrative {
    let name = name_hint
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| context.title.clone())
        .unwrap_or_else(|| source_title_from_url(&context.url));
    let excerpt = context
        .text
        .lines()
        .map(str::trim)
        .filter(|line| line.len() > 12)
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    SourceNarrative {
        entity_name: Some(name),
        profile: Some(
            excerpt
                .chars()
                .take(420)
                .collect::<String>()
                .trim()
                .to_string(),
        )
        .filter(|v| !v.is_empty()),
        goals: Some("Review the imported source and turn it into clear goals, tasks, and operating context.".to_string()),
        active_context: Some(format!("Imported from {}.", context.url)),
        operating_rules: Some(
            "Use imported source context as a draft until confirmed by the operator.".to_string(),
        ),
        entity_type: if context.kind == "github_repo" {
            Some("product".to_string())
        } else {
            None
        },
        repo_path: if context.kind == "github_repo" {
            Some(context.url.clone())
        } else {
            None
        },
        summary: Some(excerpt.chars().take(500).collect::<String>()),
        ..Default::default()
    }
}

/// Crude HTML-to-text: drops tags and entities, collapses whitespace.
fn strip_html_to_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len().min(12000));
    let mut in_tag = false;
    let mut last_space = false;
    for ch in input.chars() {
        match ch {
            '<' => {
                in_tag = true;
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            '>' => in_tag = false,
            _ if in_tag => {}
            '&' => {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            c if c.is_whitespace() => {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            c => {
                out.push(c);
                last_space = false;
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parses `owner/repo` out of a `github.com` URL, if it is one.
pub(crate) fn github_owner_repo(url: &str) -> Option<(String, String)> {
    let without_scheme = url
        .trim()
        .strip_prefix("https://")
        .or_else(|| url.trim().strip_prefix("http://"))?;
    let without_host = without_scheme.strip_prefix("github.com/")?;
    let mut parts = without_host.split('/').filter(|p| !p.trim().is_empty());
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.trim_end_matches(".git").to_string();
    if owner.is_empty() || repo.is_empty() {
        None
    } else {
        Some((owner, repo))
    }
}

/// [`fetch_entity_source_with_token`] without a GitHub token.
async fn fetch_entity_source(url: &str) -> Result<SourceContext, StatusCode> {
    fetch_entity_source_with_token(url, None).await
}

/// Fetches `url` and builds a [`SourceContext`]: GitHub repo URLs get
/// special-cased metadata + README via the GitHub API, everything else is
/// fetched as a page and stripped down to plain text.
pub(crate) async fn fetch_entity_source_with_token(
    url: &str,
    github_token: Option<&str>,
) -> Result<SourceContext, StatusCode> {
    let clean_url = url.trim();
    if !(clean_url.starts_with("https://") || clean_url.starts_with("http://")) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let client = reqwest::Client::builder()
        .user_agent("consistent-world-entity-source-ingest/0.1")
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    if let Some((owner, repo)) = github_owner_repo(clean_url) {
        // Only the subset of GitHub's repo API response this ingest path needs.
        #[derive(Deserialize)]
        struct GithubRepo {
            full_name: Option<String>,
            description: Option<String>,
            html_url: Option<String>,
            homepage: Option<String>,
            language: Option<String>,
        }
        let api_url = format!("https://api.github.com/repos/{owner}/{repo}");
        let mut repo_req = client.get(&api_url);
        if let Some(token) = github_token.map(str::trim).filter(|v| !v.is_empty()) {
            repo_req = repo_req.bearer_auth(token);
        }
        let repo_meta: Option<GithubRepo> = match repo_req.send().await {
            Ok(response) if response.status().is_success() => response.json().await.ok(),
            _ => None,
        };
        let readme_url = format!("https://api.github.com/repos/{owner}/{repo}/readme");
        let mut readme_req = client
            .get(readme_url)
            .header("Accept", "application/vnd.github.raw");
        if let Some(token) = github_token.map(str::trim).filter(|v| !v.is_empty()) {
            readme_req = readme_req.bearer_auth(token);
        }
        let readme = match readme_req.send().await {
            Ok(response) if response.status().is_success() => {
                response.text().await.unwrap_or_default()
            }
            _ => String::new(),
        };
        if repo_meta.is_some() || !readme.trim().is_empty() {
            let meta = repo_meta;
            let title = meta
                .as_ref()
                .and_then(|m| m.full_name.clone())
                .or_else(|| Some(format!("{owner}/{repo}")));
            let mut parts = Vec::new();
            if let Some(m) = meta {
                if let Some(desc) = m.description {
                    parts.push(format!("Description: {desc}"));
                }
                if let Some(homepage) = m.homepage.filter(|v| !v.trim().is_empty()) {
                    parts.push(format!("Homepage: {homepage}"));
                }
                if let Some(language) = m.language {
                    parts.push(format!("Primary language: {language}"));
                }
                if let Some(html_url) = m.html_url {
                    parts.push(format!("Repository: {html_url}"));
                }
            }
            if !readme.trim().is_empty() {
                parts.push(format!("README:\n{readme}"));
            }
            return Ok(SourceContext {
                url: clean_url.to_string(),
                kind: "github_repo".to_string(),
                title,
                text: parts.join("\n\n").chars().take(12000).collect(),
            });
        }
    }

    let response = client
        .get(clean_url)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !response.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let body = response.text().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
    let title = body
        .to_ascii_lowercase()
        .find("<title")
        .and_then(|idx| body[idx..].find('>').map(|end| idx + end + 1))
        .and_then(|start| {
            body[start..]
                .to_ascii_lowercase()
                .find("</title>")
                .map(|end| (start, start + end))
        })
        .map(|(start, end)| strip_html_to_text(&body[start..end]))
        .filter(|v| !v.trim().is_empty());
    Ok(SourceContext {
        url: clean_url.to_string(),
        kind: "website".to_string(),
        title,
        text: strip_html_to_text(&body).chars().take(12000).collect(),
    })
}

/// Finds a usable GitHub token from an existing entity connection, so source
/// ingestion of private repos doesn't require a separate credential prompt.
pub(crate) async fn github_token_for_source_ingest(
    s: &ControlState,
    workspace_id: &str,
    entity_id: Option<&str>,
) -> Option<String> {
    let workspace_entity_ids = if entity_id.is_none() {
        let entities = s.entities.read().await;
        // GitHub connections live on business entities; personal spaces have none.
        Some(entity_ids_for_workspace(&entities, workspace_id, None))
    } else {
        None
    };
    let connections = s.entity_connections.read().await;
    connections
        .iter()
        .find(|connection| {
            let entity_matches = entity_id
                .map(|id| connection.entity_id == id)
                .unwrap_or_else(|| {
                    workspace_entity_ids
                        .as_ref()
                        .map(|ids| ids.contains(&connection.entity_id))
                        .unwrap_or(false)
                });
            entity_matches
                && connection.provider.eq_ignore_ascii_case("github")
                && connection.status == "connected"
                && connection.secret_ref.is_some()
        })
        .and_then(|connection| credentials_resolver::read_entity_connection_secret(&connection.id))
}

/// Asks the operations LLM to turn fetched source text into a [`SourceNarrative`];
/// falls back to [`fallback_source_narrative`] when no LLM is configured, the
/// call fails, or the response isn't valid JSON.
async fn extract_source_narrative(
    state: &ServerState,
    config: &AppConfig,
    control: &ControlState,
    workspace_id: &str,
    context: &SourceContext,
    name_hint: Option<&str>,
) -> SourceNarrative {
    let Some(resolved) = resolve_operations_llm(config, control).await else {
        return fallback_source_narrative(context, name_hint);
    };
    let api_key = credentials_resolver::resolve_llm_api_key(&resolved.api_key_env);
    let provider = build_llm_client(config, &resolved.base_url, &api_key, &resolved.api_key_env);
    let messages = vec![
        ChatMessage {
            role: Role::System,
            content: "Extract a Consistent entity foundation from source material. Return strict JSON only. Fields: entityName, profile, goals, activeContext, operatingRules, entityType, primaryDomain, repoPath, backendStack, frontendStack, databaseStack, successCriteria, summary. Keep values concise and useful for long-term operational memory.".to_string(),
            tool_calls: None,
            tool_call_id: None,
        },
        ChatMessage {
            role: Role::User,
            content: format!(
                "Name hint: {}\nSource URL: {}\nSource kind: {}\nTitle: {}\n\nSource text:\n{}",
                name_hint.unwrap_or(""),
                context.url,
                context.kind,
                context.title.as_deref().unwrap_or(""),
                context.text.chars().take(9000).collect::<String>()
            ),
            tool_calls: None,
            tool_call_id: None,
        },
    ];
    let request = CompletionRequest {
        model: resolved.model,
        messages,
        tools: None,
        max_tokens: Some(900),
        temperature: Some(0.2),
        stream: false,
    };
    match provider.complete(request).await {
        Ok(response) => {
            if let Some(ledger) = state.credit_ledger.as_ref() {
                let calc = consistent_world_llm::cost::CostCalculator::new();
                let raw_cost = calc.calculate(
                    &response.model,
                    response.usage.input_tokens,
                    response.usage.output_tokens,
                );
                if raw_cost > 0.0 {
                    let _ = ledger.debit_for_cost(workspace_id, raw_cost).await;
                }
            }
            let raw = response.content.unwrap_or_default();
            let cleaned = raw
                .trim()
                .trim_start_matches("```json")
                .trim_start_matches("```")
                .trim_end_matches("```")
                .trim();
            serde_json::from_str::<SourceNarrative>(cleaned)
                .unwrap_or_else(|_| fallback_source_narrative(context, name_hint))
        }
        Err(err) => {
            tracing::warn!(error = %err, "source narrative extraction failed");
            fallback_source_narrative(context, name_hint)
        }
    }
}

/// Fills in an entity's empty foundation fields from an ingested source's
/// narrative, never overwriting a field the entity already has.
fn merge_source_into_entity(
    entity: &mut Entity,
    narrative: &SourceNarrative,
    context: &SourceContext,
) {
    let profile = non_empty_string(narrative.profile.clone()).or_else(|| narrative.summary.clone());
    if entity
        .description
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        entity.description = profile.clone();
    }
    if entity
        .profile
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        entity.profile = profile.clone();
    }
    if entity
        .goals
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        entity.goals = non_empty_string(narrative.goals.clone());
    }
    if entity
        .active_context
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        entity.active_context = non_empty_string(narrative.active_context.clone())
            .or_else(|| Some(format!("Source imported from {}.", context.url)));
    }
    if entity
        .operating_rules
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty()
    {
        entity.operating_rules = non_empty_string(narrative.operating_rules.clone());
    }
    if entity.entity_type.is_none() {
        entity.entity_type = non_empty_string(narrative.entity_type.clone());
    }
    if entity.setup.what_it_is.is_none() {
        entity.setup.what_it_is = profile;
    }
    if entity.setup.primary_goal.is_none() {
        entity.setup.primary_goal = non_empty_string(narrative.goals.clone());
    }
    if entity.setup.success_criteria.is_none() {
        entity.setup.success_criteria = non_empty_string(narrative.success_criteria.clone());
    }
    if entity.setup.primary_domain.is_none() {
        entity.setup.primary_domain = non_empty_string(narrative.primary_domain.clone());
    }
    if entity.setup.repo_path.is_none() {
        entity.setup.repo_path = non_empty_string(narrative.repo_path.clone()).or_else(|| {
            if context.kind == "github_repo" {
                Some(context.url.clone())
            } else {
                None
            }
        });
    }
    if entity.setup.backend_stack.is_none() {
        entity.setup.backend_stack = non_empty_string(narrative.backend_stack.clone());
    }
    if entity.setup.frontend_stack.is_none() {
        entity.setup.frontend_stack = non_empty_string(narrative.frontend_stack.clone());
    }
    if entity.setup.database_stack.is_none() {
        entity.setup.database_stack = non_empty_string(narrative.database_stack.clone());
    }
    if !entity.resources.iter().any(|r| r.root == context.url) {
        entity.resources.push(source_resource(context));
    }
    refresh_entity_readiness(entity);
}

/// Core task-creation logic shared by the `POST` task handler and internal
/// callers: validates the entity/mission link, dedupes on
/// `(external_source, external_id)`, persists, and mirrors to the local vault.
pub(crate) async fn internal_create_task(
    s: &crate::state::ControlState,
    workspace_id: &str,
    viewer: Option<&str>,
    body: CreateTaskRequest,
) -> Result<Task, StatusCode> {
    let CreateTaskRequest {
        title,
        dept,
        roles,
        entity_id,
        mission_id,
        priority,
        requires_human_approval,
        task_kind,
        content,
        director_id,
        agent_id,
        connection_id,
        external_source,
        external_id,
        metadata,
        due_at,
    } = body;
    let task_kind = canonicalize_task_kind(task_kind);
    // Beat tasks are the mission engine's own bookkeeping (it builds them
    // directly) and drive unattended, permission-bypassing runs. Nothing that
    // comes through this generic path — HTTP, chat, capture — may mint one.
    if task_kind.as_deref() == Some(crate::mission_engine::BEAT_TASK_KIND) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let (resolved_entity_id, mission_id) = {
        let missions = s.missions.read().await;
        resolve_task_entity_and_validate_mission(&missions, entity_id, mission_id)?
    };
    let entity_for_vault = {
        let entities = s.entities.read().await;
        if let Some(ref eid) = resolved_entity_id {
            let Some(entity) = entities.iter().find(|e| e.id == *eid) else {
                return Err(StatusCode::BAD_REQUEST);
            };
            if !is_entity_visible(entity, workspace_id, viewer) {
                return Err(StatusCode::FORBIDDEN);
            }
            Some(entity.clone())
        } else if workspace_id != DEFAULT_WORKSPACE_ID {
            return Err(StatusCode::BAD_REQUEST);
        } else {
            None
        }
    };
    let external_source = external_source
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let external_id = external_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if let (Some(source), Some(id)) = (external_source.as_deref(), external_id.as_deref()) {
        let entity_ids = {
            let entities = s.entities.read().await;
            entity_ids_for_workspace(&entities, workspace_id, viewer)
        };
        if let Some(existing) = s.tasks.read().await.iter().find(|t| {
            t.external_source.as_deref() == Some(source)
                && t.external_id.as_deref() == Some(id)
                && task_visible_in_workspace(t, &entity_ids, workspace_id)
        }) {
            return Ok(existing.clone());
        }
    }

    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let task = Task {
        id: id.clone(),
        title,
        dept: dept.unwrap_or_else(|| "General".to_string()),
        roles: roles.unwrap_or_default(),
        status: TaskStatus::Backlog,
        tries: "0".to_string(),
        progress: 0,
        entity_id: resolved_entity_id,
        mission_id,
        priority,
        requires_human_approval: requires_human_approval.unwrap_or(false),
        assigned_director_id: director_id,
        assigned_agent_id: agent_id,
        connection_id,
        external_source,
        external_id,
        result: content
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|v| format!("Capture:\n{v}")),
        task_kind,
        metadata,
        outcome: None,
        created_at: now,
        due_at,
        completed_at: None,
        status_changed_at: None,
        source_task_id: None,
    };
    s.tasks.write().await.push(task.clone());
    // Write-through: the Surreal row must exist before any redeploy can race
    // this create (see task_store module docs).
    s.persist_task_row(&task).await;
    s.push_operator_feed(
        OperatorFeedKind::TaskCreated,
        &task,
        task_created_detail(task.task_kind.as_deref()),
    )
    .await;
    if task.requires_human_approval {
        s.push_operator_feed(OperatorFeedKind::ApprovalNeeded, &task, None)
            .await;
    }
    crate::local_vault::mirror_task(&task, entity_for_vault.as_ref(), workspace_id).await;
    Ok(task)
}

/// [`internal_create_task`], but the task starts `Queued` — the shape the
/// task executor scans for — instead of `Backlog`. Internal callers only (the
/// public POST body has no way to reach this): the caller owns the safety
/// story, normally by also setting `requires_human_approval` so the task
/// waits at the approval gate rather than running on creation.
pub(crate) async fn internal_create_task_queued(
    s: &crate::state::ControlState,
    workspace_id: &str,
    viewer: Option<&str>,
    body: CreateTaskRequest,
) -> Result<Task, StatusCode> {
    let created = internal_create_task(s, workspace_id, viewer, body).await?;
    let queued = {
        let mut tasks = s.tasks.write().await;
        let Some(t) = tasks.iter_mut().find(|t| t.id == created.id) else {
            return Ok(created);
        };
        crate::state::set_task_status_by_human(t, TaskStatus::Queued);
        t.clone()
    };
    s.persist_task_row(&queued).await;
    Ok(queued)
}

/// Parses a status string into [`RawCaptureStatus`], accepting both
/// underscore and hyphen separators.
fn raw_capture_status_from_str(s: &str) -> Option<RawCaptureStatus> {
    match s.trim().to_ascii_lowercase().as_str() {
        "raw" => Some(RawCaptureStatus::Raw),
        "dismissed" => Some(RawCaptureStatus::Dismissed),
        "promoted_to_task" | "promoted-to-task" => Some(RawCaptureStatus::PromotedToTask),
        "promoted_to_memory" | "promoted-to-memory" => Some(RawCaptureStatus::PromotedToMemory),
        "narrative_updated" | "narrative-updated" => Some(RawCaptureStatus::NarrativeUpdated),
        _ => None,
    }
}

/// Derive a task title from a capture: first non-empty line of the summary, else
/// the raw text, truncated to a sane length.
fn capture_title(capture: &RawCapture) -> String {
    let base = capture
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| capture.raw_text.trim());
    let first_line = base.lines().next().unwrap_or("Captured note").trim();
    let title: String = first_line.chars().take(120).collect();
    if title.is_empty() {
        "Captured note".to_string()
    } else {
        title
    }
}

/// Re-mirror a capture to the local vault after a status change, resolving its
/// entity (if any) for the correct folder.
async fn mirror_capture_change(s: &crate::state::ControlState, capture: &RawCapture) {
    let entity = if let Some(ref eid) = capture.entity_id {
        s.entities
            .read()
            .await
            .iter()
            .find(|e| e.id == *eid)
            .cloned()
    } else {
        None
    };
    crate::local_vault::mirror_raw_capture(capture, entity.as_ref()).await;
}

/// Create a raw capture in control state and mirror it to the local vault.
/// Reusable from HTTP handlers and internal flows (e.g. external agent runs).
pub(crate) async fn internal_create_capture(
    s: &crate::state::ControlState,
    workspace_id: &str,
    viewer: Option<&str>,
    body: CreateCaptureRequest,
) -> Result<RawCapture, StatusCode> {
    let raw_text = body.raw_text.trim().to_string();
    if raw_text.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let entity_id = body
        .entity_id
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    // Validate entity visibility and grab it for the vault mirror path.
    let entity_for_vault = if let Some(ref eid) = entity_id {
        let entities = s.entities.read().await;
        let Some(entity) = entities.iter().find(|e| e.id == *eid) else {
            return Err(StatusCode::BAD_REQUEST);
        };
        if !is_entity_visible(entity, workspace_id, viewer) {
            return Err(StatusCode::FORBIDDEN);
        }
        Some(entity.clone())
    } else {
        None
    };
    let source = body
        .source
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "chat".to_string());
    let now = chrono::Utc::now().to_rfc3339();
    let capture = RawCapture {
        id: Uuid::new_v4().to_string(),
        workspace_id: workspace_id.to_string(),
        entity_id,
        source,
        source_ref: body
            .source_ref
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        session_id: body
            .session_id
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        provider: body
            .provider
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        raw_text,
        summary: body
            .summary
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        status: RawCaptureStatus::Raw,
        promoted_task_id: None,
        metadata: body.metadata.unwrap_or(serde_json::Value::Null),
        created_at: now.clone(),
        updated_at: now,
    };
    s.raw_captures.write().await.push(capture.clone());
    crate::local_vault::mirror_raw_capture(&capture, entity_for_vault.as_ref()).await;
    Ok(capture)
}

/// Core director-command logic: validates the director/agent pair, builds a
/// task from the free-form prompt (title, dept, roles inferred from the
/// director/agent, gated behind approval for non-`auto` modes), and returns
/// the routing.
pub(crate) async fn internal_create_command(
    s: &crate::state::ControlState,
    workspace_id: &str,
    viewer: Option<&str>,
    body: CreateCommandRequest,
) -> Result<CommandResponse, StatusCode> {
    if has_invalid_capture_intent(body.capture_intent.as_deref()) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let groups = s.agent_groups.read().await;
    if !groups.iter().any(|g| g.id == body.director_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    drop(groups);

    let mut agent_id = None;
    if let Some(ref aid) = body.agent_id {
        let agents = s.agents.read().await;
        let ok = agents
            .iter()
            .any(|a| a.id == *aid && a.group_id == body.director_id);
        if !ok {
            return Err(StatusCode::BAD_REQUEST);
        }
        agent_id = Some(aid.clone());
    }

    let (resolved_entity_id, resolved_mission_id) = {
        let missions = s.missions.read().await;
        resolve_task_entity_and_validate_mission(
            &missions,
            body.entity_id.clone(),
            body.mission_id.clone(),
        )?
    };
    {
        let entities = s.entities.read().await;
        if let Some(ref eid) = resolved_entity_id {
            let Some(entity) = entities.iter().find(|e| e.id == *eid) else {
                return Err(StatusCode::BAD_REQUEST);
            };
            if !is_entity_visible(entity, workspace_id, viewer) {
                return Err(StatusCode::FORBIDDEN);
            }
        } else if workspace_id != DEFAULT_WORKSPACE_ID {
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    let mut dept = {
        let groups = s.agent_groups.read().await;
        groups
            .iter()
            .find(|g| g.id == body.director_id)
            .map(|g| g.name.clone())
            .unwrap_or_else(|| "General".to_string())
    };
    if let Some(ref eid) = resolved_entity_id {
        let entities = s.entities.read().await;
        if let Some(e) = entities.iter().find(|e| e.id == *eid)
            && !e.dept.is_empty()
        {
            dept = e.dept.clone();
        }
    }

    let mut roles: Vec<String> = {
        let groups = s.agent_groups.read().await;
        groups
            .iter()
            .find(|g| g.id == body.director_id)
            .map(|g| vec![g.name.clone()])
            .unwrap_or_default()
    };
    if let Some(ref aid) = agent_id {
        let agents = s.agents.read().await;
        if let Some(a) = agents.iter().find(|a| a.id == *aid) {
            roles.push(a.role.clone());
        }
    }

    let requires_human_approval = matches!(
        body.mode.as_deref(),
        Some("decision-required" | "requires-approval" | "draft-only")
    );
    let status = if requires_human_approval {
        TaskStatus::Backlog
    } else {
        TaskStatus::Queued
    };

    let capture_intent = normalize_capture_intent(body.capture_intent.as_deref());
    let task_kind = capture_intent.map(str::to_string);

    let trimmed = body.prompt.trim();
    let first_line = trimmed.lines().next().unwrap_or("");
    let mut title = format!("{}: {}", capture_prefix(capture_intent), first_line);
    if title.len() > 140 {
        // Byte 137 can fall inside a multi-byte char (Arabic, CJK, emoji),
        // and `String::truncate` panics there; back off to a char boundary.
        title.truncate(title.floor_char_boundary(137));
        title.push_str("...");
    }

    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let task = Task {
        id: id.clone(),
        title,
        dept,
        roles,
        status,
        tries: "0".to_string(),
        progress: if status == TaskStatus::Completed {
            100
        } else {
            0
        },
        entity_id: resolved_entity_id.clone(),
        mission_id: resolved_mission_id,
        priority: None,
        requires_human_approval,
        assigned_director_id: Some(body.director_id.clone()),
        assigned_agent_id: agent_id.clone(),
        connection_id: None,
        external_source: None,
        external_id: None,
        result: Some(format!("Prompt:\n{}", body.prompt)),
        task_kind,
        metadata: None,
        outcome: None,
        created_at: now,
        due_at: None,
        completed_at: None,
        status_changed_at: None,
        source_task_id: None,
    };
    s.tasks.write().await.push(task.clone());
    // Write-through: the Surreal row must exist before any redeploy can race
    // this create (see task_store module docs).
    s.persist_task_row(&task).await;
    s.push_operator_feed(
        OperatorFeedKind::TaskCreated,
        &task,
        task_created_detail(task.task_kind.as_deref()),
    )
    .await;
    if task.requires_human_approval {
        s.push_operator_feed(OperatorFeedKind::ApprovalNeeded, &task, None)
            .await;
    }

    Ok(CommandResponse {
        task_id: id,
        director_id: body.director_id,
        agent_id,
        entity_id: resolved_entity_id,
    })
}

// ─── Handlers ───────────────────────────────────────────────────────────────

/// Extracts the [`ControlState`], failing the request with 503 when the
/// gateway hasn't finished initializing it yet.
fn control(state: &ServerState) -> Result<&crate::state::ControlState, StatusCode> {
    state
        .control_state
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

/// Query-string filters for listing tasks.
#[derive(Debug, Deserialize)]
struct TaskQuery {
    status: Option<String>,
    entity_id: Option<String>,
    mission_id: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Handler: issues a director command (see [`internal_create_command`]).
async fn create_command(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateCommandRequest>,
) -> Result<Json<CommandResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let resp = internal_create_command(
        s,
        &workspace_id,
        Some(workspace.principal_id.as_str()),
        body,
    )
    .await?;
    Ok(Json(resp))
}

/// Query-string filters for listing missions.
#[derive(Debug, Deserialize)]
struct MissionQuery {
    entity_id: Option<String>,
    status: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
}

/// Validate and record a mission. Does not plan: task decomposition is
/// [`crate::mission_planner::plan_and_attach_tasks`], which the HTTP handler
/// spawns and the chat tool awaits.
pub(crate) async fn internal_create_mission(
    s: &crate::state::ControlState,
    workspace_id: &str,
    viewer: Option<&str>,
    body: CreateMissionRequest,
) -> Result<Mission, StatusCode> {
    if body.entity_id.trim().is_empty() || body.title.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(priority) = body.priority.as_deref()
        && !is_valid_priority(priority)
    {
        return Err(StatusCode::BAD_REQUEST);
    }

    {
        let entities = s.entities.read().await;
        let Some(entity) = entities.iter().find(|e| e.id == body.entity_id) else {
            return Err(StatusCode::BAD_REQUEST);
        };
        if !is_entity_visible(entity, workspace_id, viewer) {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    // A mission with a definition of done is a mission the operator asked to
    // be *driven*, so it starts Active. The Drive beat resolves an entity's
    // active mission (`list_active_for_entity` filters on status = 'active'),
    // so a loop mission left Planned would never be seen by the beat that is
    // supposed to advance it — the loop would sit there forever, doing nothing,
    // with no error anywhere.
    let default_status = if body.loop_config.is_some() {
        MissionStatus::Active
    } else {
        MissionStatus::Planned
    };
    let mission = Mission {
        id: Uuid::new_v4().to_string(),
        entity_id: body.entity_id,
        title: body.title.trim().to_string(),
        summary: body.summary.unwrap_or_default(),
        status: body.status.unwrap_or(default_status),
        priority: body.priority.unwrap_or_else(|| "medium".to_string()),
        goal: body.goal.unwrap_or_default(),
        owner: body.owner,
        created_by: body.created_by,
        source: body.source,
        created_at: now.clone(),
        updated_at: now,
        last_reviewed_at: None,
        review_cadence_hours: body.review_cadence_hours,
        narrative: body.narrative,
        canonical_notes: body.canonical_notes.unwrap_or_default(),
        assigned_director_id: body.assigned_director_id,
        final_report: None,
        loop_state: body
            .loop_config
            .is_some()
            .then(crate::state::MissionLoopState::default),
        loop_config: body.loop_config,
    };
    s.missions.write().await.push(mission.clone());
    s.persist_mission(&mission).await;
    s.push_mission_feed(OperatorFeedKind::MissionCreated, &mission, None)
        .await;
    // A mission that asks to be driven needs the beat that drives it.
    // Best-effort, exactly like default heartbeat provisioning: a missing or
    // erroring store must never fail mission creation.
    if mission.loop_config.is_some()
        && let Some(hb_store) = s.heartbeat_store.get()
    {
        let entity = s
            .entities
            .read()
            .await
            .iter()
            .find(|e| e.id == mission.entity_id)
            .cloned();
        if let Some(entity) = entity
            && let Err(e) = crate::entity_heartbeat_defaults::provision_drive_beat(
                hb_store.as_ref(),
                &entity,
                Some(workspace_id),
            )
            .await
        {
            tracing::warn!(mission_id = %mission.id, error = %e, "drive beat provisioning failed");
        }
    }
    Ok(mission)
}

/// True when this deployment wants missions decomposed into tasks on creation.
pub(crate) fn mission_planning_enabled(state: &ServerState) -> bool {
    state
        .app_config
        .as_ref()
        .map(|cfg| cfg.mission_policy.starter_task_auto_queueing_enabled)
        .unwrap_or(true)
}

/// Build the mission's final report, store it on the mission, and emit a feed
/// event. Reusable from the goal loop and the report endpoint.
pub(crate) async fn generate_mission_report(
    s: &crate::state::ControlState,
    mission_id: &str,
) -> Option<String> {
    let mission = {
        s.missions
            .read()
            .await
            .iter()
            .find(|m| m.id == mission_id)
            .cloned()?
    };
    let tasks_all = s.tasks.read().await.clone();
    let mission_tasks: Vec<&Task> = tasks_all
        .iter()
        .filter(|t| t.mission_id.as_deref() == Some(mission_id))
        .collect();
    let verifs_all = s.verifications.read().await.clone();
    let mission_verifs: Vec<&VerificationRun> = verifs_all
        .iter()
        .filter(|v| v.mission_id.as_deref() == Some(mission_id))
        .collect();
    let report =
        crate::mission_report::build_mission_report(&mission, &mission_tasks, &mission_verifs);
    {
        let mut missions = s.missions.write().await;
        if let Some(m) = missions.iter_mut().find(|m| m.id == mission_id) {
            m.final_report = Some(report.clone());
            m.updated_at = chrono::Utc::now().to_rfc3339();
        }
    }
    s.push_mission_feed(
        OperatorFeedKind::MissionReportReady,
        &mission,
        Some("Final mission report generated.".to_string()),
    )
    .await;
    Some(report)
}

/// Response wrapping a mission's final report text.
#[derive(Debug, Serialize)]
pub struct MissionReportResponse {
    pub mission_id: String,
    pub report: String,
}

/// Aggregated mission-recommendation metrics for the dashboard.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MissionMetricsView {
    recommendation_emitted: u64,
    recommendation_accepted: u64,
    recommendation_dismissed: u64,
    duplicate_suppressed: u64,
    stale_unresolved: u64,
    /// Wall-clock seconds from the first-ever completed mission's `created_at` to its
    /// completion. `None` until the workspace has completed at least one mission.
    time_to_first_completed_mission_seconds: Option<u64>,
    by_branch: HashMap<String, HashMap<String, u64>>,
}

/// Counts of unresolved mission recommendations, overall and per branch.
struct StaleRecommendationStats {
    total: u64,
    by_branch: HashMap<String, u64>,
}

// ─── Entities ──────────────────────────────────────────────────────────────

/// Query-string parameters for listing entities.
#[derive(Debug, Deserialize)]
struct EntityListQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

/// `GET` handler: lists entities visible in the workspace, paginated.
/// `GET` handler: lists entities visible in the workspace, refreshing each
/// one's readiness score before returning it.
async fn list_entities(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<EntityListQuery>,
) -> Result<Json<PaginatedResponse<Entity>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entities = s.entities.read().await;
    let filtered: Vec<Entity> = entities
        .iter()
        .filter(|e| is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str())))
        .cloned()
        .map(|mut e| {
            refresh_entity_readiness(&mut e);
            e
        })
        .collect();
    let total = filtered.len();
    let pag = apply_pagination(
        filtered,
        total,
        &PaginationQuery {
            limit: q.limit,
            offset: q.offset,
        },
    );
    Ok(Json(pag))
}

/// `POST` handler: creates an entity (see [`internal_create_entity`]).
async fn create_entity(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<CreateEntityRequest>,
) -> Result<Json<Entity>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity = internal_create_entity(&state, s, &workspace_id, body).await?;
    Ok(Json(entity))
}

// ─── Cloud config and local vault ──────────────────────────────────────────

/// `GET` handler: the workspace's cloud compute configuration.
async fn get_cloud_config(
    State(state): State<ServerState>,
) -> Result<Json<CloudConfig>, StatusCode> {
    let s = control(&state)?;
    Ok(Json(s.cloud_config.read().await.clone()))
}

/// `GET` handler: the local vault's on-disk layout (ensuring it exists first).
async fn get_local_vault() -> Json<serde_json::Value> {
    let root = crate::local_vault::vault_root();
    let _ = tokio::fs::create_dir_all(&root).await;
    crate::local_vault::ensure_workspace_layout(DEFAULT_WORKSPACE_ID).await;
    let workspace = root.join("workspaces").join(DEFAULT_WORKSPACE_ID);
    Json(serde_json::json!({
        "root": root.to_string_lossy(),
        "exists": tokio::fs::try_exists(&root).await.unwrap_or(false),
        "canonicalDatabase": "surrealdb",
        "mirrorFormat": "markdown",
        "workspace": workspace.to_string_lossy(),
        "chats": workspace.join("chats").to_string_lossy(),
        "entities": workspace.join("entities").to_string_lossy(),
        "notes": workspace.join("inbox").join("notes").to_string_lossy(),
        "environments": workspace.join("environments").to_string_lossy(),
        "wallet": workspace.join("wallet").to_string_lossy()
    }))
}

/// `PATCH` handler: partial-updates the cloud compute configuration.
async fn update_cloud_config(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<UpdateCloudConfigRequest>,
) -> Result<Json<CloudConfig>, StatusCode> {
    // Instance-global: one tenant must not repoint everyone's compute.
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    let mut c = s.cloud_config.write().await;
    if let Some(p) = body.provider {
        c.provider = Some(p);
    }
    if let Some(r) = body.region {
        c.region = Some(r);
    }
    if let Some(cp) = body.compute_profile {
        c.compute_profile = Some(cp);
    }
    Ok(Json(c.clone()))
}

// ─── Coding-agent CLIs: the out-of-the-box execution harness ───

/// Cheap, side-effect-free status for every coding-agent CLI we can drive.
/// Safe to poll from the settings page.
async fn list_coding_agents() -> Result<Json<Vec<crate::agent_cli_probe::CliStatus>>, StatusCode> {
    Ok(Json(crate::agent_cli_probe::probe_all().await))
}

/// Actually run the CLI once. Costs a token or two, so it's an explicit
/// operator action rather than part of the status poll — this is the only
/// check that can tell "installed" from "signed in".
async fn test_coding_agent(
    Path(kind): Path<String>,
) -> Result<Json<crate::agent_cli_probe::CliVerification>, StatusCode> {
    if !crate::agent_cli_probe::KNOWN_KINDS.contains(&kind.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(Json(crate::agent_cli_probe::verify(&kind).await))
}

// ─── Router ───────────────────────────────────────────────────────────────────

/// Assembles every control-plane route defined in this module onto one
/// [`Router`], bound to the shared [`ServerState`]. This is what `main.rs`
/// mounts as the gateway's HTTP API.
pub fn control_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/audit/recent", get(list_recent_audit))
        .route("/api/audit/turn/{turn_id}", get(list_turn_audit))
        .route("/api/skills", get(list_skills).post(create_skill))
        .route("/api/skills/upload", post(upload_skill))
        .route(
            "/api/skills/local",
            get(list_local_skill_sources).post(import_local_skills),
        )
        .route(
            "/api/skills/{id}",
            get(get_skill).patch(update_skill).delete(delete_skill),
        )
        .route("/api/skills/{id}/enabled", post(set_skill_enabled))
        .route("/api/skills/{id}/copy", post(copy_skill))
        .route(
            "/api/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route("/api/tasks", get(list_tasks).post(create_task))
        .route("/api/tasks/{id}", get(get_task).patch(update_task))
        .route("/api/tasks/{id}/requeue", post(requeue_task))
        .route("/api/tasks/{id}/cancel", post(cancel_task))
        .route("/api/tasks/{id}/snooze", post(snooze_task))
        .route("/api/daily-review", get(get_daily_review))
        .route("/api/captures", get(list_captures).post(create_capture))
        .route("/api/captures/{id}", patch(update_capture))
        .route("/api/captures/{id}/dismiss", post(dismiss_capture))
        .route(
            "/api/captures/{id}/promote-task",
            post(promote_capture_task),
        )
        .route(
            "/api/captures/{id}/update-narrative",
            post(update_capture_narrative),
        )
        .route(
            "/api/captures/{id}/promote-memory",
            post(promote_capture_memory),
        )
        .route("/api/captures/{id}/route", post(route_capture))
        .route("/api/captures/{id}/attach", post(attach_capture))
        .route("/api/external-captures", post(create_external_capture))
        .route(
            "/api/external-captures/batch",
            post(create_external_captures_batch),
        )
        .route("/api/missions", get(list_missions).post(create_mission))
        .route("/api/missions/{id}", get(get_mission).patch(update_mission))
        .route("/api/missions/{id}/dod", get(get_mission_dod))
        .route("/api/missions/{id}/report", get(get_mission_report))
        .route("/api/missions/{id}/cancel", post(cancel_mission))
        .route("/api/missions/{id}/actions", post(apply_mission_action))
        .route("/api/commands", post(create_command))
        .route("/api/entities", get(list_entities).post(create_entity))
        .route("/api/entity-source-ingest", post(ingest_entity_source))
        .route("/api/entity-import/analyze", post(analyze_entity_import))
        .route(
            "/api/entity-import/jobs/{job_id}",
            get(get_entity_import_job),
        )
        .route("/api/entity-import/commit", post(commit_entity_import))
        .route(
            "/api/entities/{id}/knowledge",
            get(crate::entity_knowledge::get_entity_knowledge),
        )
        .route(
            "/api/entities/{id}/knowledge/reindex",
            post(crate::entity_knowledge::reindex_entity_knowledge),
        )
        .route(
            "/api/entities/{id}/knowledge/docs/{doc_id}",
            patch(crate::entity_knowledge::set_knowledge_doc_enabled)
                .delete(crate::entity_knowledge::delete_knowledge_doc),
        )
        .route(
            "/api/entities/conversational/colour",
            post(conversational_entity_colour),
        )
        .route(
            "/api/entities/conversational/sessions",
            get(list_conversational_entity_sessions),
        )
        .route(
            "/api/entities/conversational/{session_id}",
            delete(conversational_entity_cancel),
        )
        .route(
            "/api/entities/{id}",
            get(get_entity).patch(update_entity).delete(delete_entity),
        )
        .route(
            "/api/entities/{id}/runtime-context",
            get(get_entity_runtime_context),
        )
        .route(
            "/api/entities/{id}/workspace",
            get(get_entity_workspace).post(set_entity_workspace),
        )
        .route(
            "/api/entity-workspace/pending",
            get(list_pending_entity_workspaces),
        )
        .route(
            "/api/entities/{id}/agent-runs",
            post(create_entity_agent_run),
        )
        .route(
            "/api/entities/{id}/agent-runs/{run_id}",
            get(get_entity_agent_run),
        )
        .route(
            "/api/entities/{id}/agent-runs/{run_id}/cancel",
            post(cancel_entity_agent_run),
        )
        .route(
            "/api/entities/{id}/memory-preview",
            get(get_entity_memory_preview),
        )
        .route(
            "/api/entities/{id}/connections",
            get(list_entity_connections).post(create_entity_connection),
        )
        .route(
            "/api/entities/{id}/records",
            get(list_entity_records).post(upsert_entity_records),
        )
        .route(
            "/api/entities/{id}/connections/{connection_id}",
            patch(update_entity_connection).delete(delete_entity_connection),
        )
        .route(
            "/api/entities/{id}/connections/{connection_id}/test",
            post(test_entity_connection),
        )
        .route(
            "/api/entities/{id}/connections/{connection_id}/openclaw/orders",
            post(ingest_openclaw_order),
        )
        .route(
            "/api/entities/{id}/connections/{connection_id}/verify",
            post(verify_entity_connection),
        )
        .route(
            "/api/entities/{id}/connection-events",
            get(list_entity_connection_events),
        )
        .route(
            "/api/entities/{id}/connections/sessions",
            post(create_entity_connection_session),
        )
        .route(
            "/api/entities/{id}/connections/sessions/{session_id}",
            get(get_entity_connection_session).post(finalize_entity_connection_session),
        )
        .route(
            "/api/entities/{id}/actions/perform",
            post(perform_entity_action_endpoint),
        )
        .route(
            "/api/entities/{id}/enrichment/apply",
            post(apply_enrichment),
        )
        .route(
            "/api/entities/{id}/enrichment/discard",
            post(discard_enrichment),
        )
        .route(
            "/api/entities/{id}/enrichment/regenerate",
            post(regenerate_enrichment),
        )
        .route(
            "/api/entities/{id}/files",
            get(list_entity_files).post(upload_entity_file),
        )
        .route("/api/chat/attachments", post(upload_chat_attachment))
        .route("/api/chat/attachments/{id}", get(get_chat_attachment))
        .route("/api/chat/attachments/{id}/thumb", get(get_chat_attachment))
        .route(
            "/api/routing/task-kinds",
            get(list_task_kind_recommendations),
        )
        .route(
            "/api/routing/task-kinds/{task_kind}",
            patch(update_task_kind_routing_override),
        )
        .route("/api/routing/model-tiers", get(list_model_tier_routes))
        .route(
            "/api/routing/model-tiers/{tier}",
            patch(update_model_tier_route),
        )
        .route("/api/llm/provider-status", get(list_llm_provider_status))
        .route(
            "/api/llm/providers",
            get(list_llm_providers).post(create_llm_provider),
        )
        .route("/api/llm/providers/{id}", patch(update_llm_provider))
        .route(
            "/api/llm/providers/{name}/credential",
            post(set_llm_provider_credential),
        )
        .route("/api/llm/providers/{name}/test", post(test_llm_provider))
        .route(
            "/api/llm/providers/{name}/verify",
            post(verify_llm_provider),
        )
        .route(
            "/api/llm/providers/{name}/models",
            get(list_llm_provider_models),
        )
        // First run: download a local model into Ollama, with progress.
        .route(
            "/api/ollama/pull",
            get(get_ollama_pull).post(start_ollama_pull),
        )
        .route(
            "/api/llm/models",
            get(list_llm_models).post(create_llm_model),
        )
        .route("/api/llm/models/{id}", patch(update_llm_model))
        .route("/api/llm/mode", get(get_llm_mode).post(set_llm_mode))
        .route(
            "/api/llm/profile",
            get(get_llm_profile).post(set_llm_profile),
        )
        .route(
            "/api/heartbeats",
            get(list_heartbeats).post(create_heartbeat),
        )
        .route(
            "/api/heartbeats/{id}",
            patch(update_heartbeat).delete(delete_heartbeat),
        )
        .route("/api/admin/backup", post(run_admin_backup))
        .route(
            "/api/entities/{id}/heartbeat-results",
            get(list_entity_heartbeat_results),
        )
        .route("/api/local-vault", get(get_local_vault))
        .route(
            "/api/cloud/config",
            get(get_cloud_config).patch(update_cloud_config),
        )
        .route(
            "/api/storage/backends",
            get(list_storage_backends).post(create_storage_backend),
        )
        .route(
            "/api/orchestration/rules",
            get(list_orchestration_rules).post(create_orchestration_rule),
        )
        .route("/api/directors", get(list_directors))
        .route("/api/directors/{id}", patch(update_director))
        .route("/api/llm/muscles", get(list_muscles))
        .route(
            "/api/directors/{id}/heartbeat",
            post(set_director_heartbeat),
        )
        .route("/api/directors/{id}/lessons", get(list_director_lessons))
        .route(
            "/api/directors/{id}/lessons/{lesson_id}",
            delete(delete_director_lesson),
        )
        .route(
            "/api/agent-groups",
            get(list_agent_groups).post(create_agent_group),
        )
        .route("/api/agents", get(list_agents).post(create_agent))
        .route("/api/agents/{group_id}/messages", get(get_agent_messages))
        .route("/api/agents/{group_id}/send", post(send_agent_message))
        .route("/api/actions", get(list_action_events))
        .route("/api/operator-feed", get(list_operator_feed))
        .route("/api/devices", get(list_devices).post(register_device))
        // Compatibility alias: shipped mobile builds POST here. Same handler,
        // same body — registering was a guaranteed 405 without it, so those
        // installs never created a delivery target.
        .route("/api/devices/register", post(register_device))
        .route("/api/devices/{id}", delete(delete_device))
        .route(
            "/api/missions/{id}/engine",
            get(get_mission_engine)
                .post(attach_mission_engine)
                .delete(detach_mission_engine),
        )
        .route(
            "/api/missions/{id}/engine/beat-now",
            post(beat_mission_engine_now),
        )
        .route("/api/integrations/coding-agents", get(list_coding_agents))
        .route(
            "/api/integrations/coding-agents/{kind}/test",
            post(test_coding_agent),
        )
        .route("/api/activity", get(list_activity))
        .route(
            "/api/verifications",
            get(list_verifications).post(run_verification),
        )
        .route("/api/approvals", get(list_approvals).post(create_approval))
        .route(
            "/api/approvals/{id}",
            get(get_approval).patch(patch_approval),
        )
        .route("/api/missions/policy", get(get_mission_policy))
        .route("/api/missions/metrics", get(get_mission_metrics))
        .route("/api/agent-runs/{id}/artifacts", get(list_run_artifacts))
        .route(
            "/api/agent-runs/{id}/artifacts/{name}",
            get(get_run_artifact),
        )
        .route("/api/pair/payload", get(get_pair_payload))
        .route("/api/remote/status", get(get_remote_status))
        .layer(middleware::from_fn(workspace_context_middleware))
        .with_state(state)
}

/// LLM inference endpoints gated by `require_credit` in `server::build_router`.
/// Split out of `control_routes` so the credit middleware can be layered on
/// only these routes without affecting read-only ones.
pub fn inference_routes(state: ServerState) -> Router {
    Router::new()
        .route("/api/capture/interpret", post(capture_interpret))
        .route("/api/capture/commit", post(capture_commit))
        .route("/api/capture/chat", post(capture_chat_handler))
        .route(
            "/api/capture/chat/stream",
            post(capture_chat_stream_handler),
        )
        .route(
            "/api/entities/conversational/start",
            post(conversational_entity_start),
        )
        .route(
            "/api/entities/conversational/answer",
            post(conversational_entity_answer),
        )
        .route(
            "/api/entities/conversational/complete",
            post(conversational_entity_complete),
        )
        .layer(middleware::from_fn(workspace_context_middleware))
        .with_state(state)
}

#[cfg(test)]
mod artifact_jail_tests {
    use super::artifact_dir_for_run;

    #[test]
    fn a_traversal_run_id_never_resolves() {
        for bad in ["../../etc", "..", "a/b", "run id", "", &"x".repeat(65)] {
            assert!(
                artifact_dir_for_run(bad).is_none(),
                "{bad:?} must not resolve to a directory"
            );
        }
    }

    #[test]
    fn a_wellformed_run_id_with_no_directory_yet_is_simply_empty() {
        // Valid shape, but nothing on disk: an empty listing, never an error
        // path that leaks whether the id exists.
        assert!(artifact_dir_for_run("11111111-2222-3333-4444-555555555555").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// The exact stderr a revoked ChatGPT token produced on 2026-09-05, trimmed
    /// to the shape that matters: pages of log spew, with the one useful
    /// sentence buried inside it.
    const REVOKED_CODEX_STDERR: &str = concat!(
        "codex CLI failed: Reading additional input from stdin...\n",
        "2026-09-05T22:02:40.749669Z ERROR codex_models_manager::manager: failed to refresh ",
        "available models: unexpected status 401 Unauthorized: Encountered invalidated oauth ",
        "token for user, failing request, auth error code: token_revoked\n",
        "2026-09-05T22:02:42.637490Z ERROR codex_login::auth::manager: Failed to refresh token: ",
        "401 Unauthorized: refresh_token_invalidated\n",
    );

    #[test]
    fn revoked_cli_login_reads_as_an_instruction_not_a_stack_trace() {
        let msg = humanize_provider_error("CODEX_CLI", REVOKED_CODEX_STDERR);
        assert!(msg.contains("codex login"), "must name the fix: {msg}");
        assert!(!msg.contains("ERROR"), "must not leak log lines: {msg}");
        assert!(msg.len() < 200, "must fit on a card: {msg}");
    }

    #[test]
    fn claude_api_error_points_at_signing_in_again() {
        let raw = r#"claude-code CLI failed (exit status: 1): {"is_error":true,"terminal_reason":"api_error"}"#;
        let msg = humanize_provider_error("CLAUDE_CODE_CLI", raw);
        assert!(msg.contains("claude"), "must name the CLI: {msg}");
        assert!(msg.contains("Test"), "must name the next step: {msg}");
    }

    #[test]
    fn outdated_cli_points_at_updating_not_signing_in() {
        let raw = r#"claude-code CLI failed (exit status: 1): {"is_error":true,"result":"API Error: 400 Claude Code 2.1.231 does not support this model; version 2.1.251 or newer is required. Run 'claude update', or update the Claude desktop app, then try again."}"#;
        let msg = humanize_provider_error("CLAUDE_CODE_CLI", raw);
        assert!(msg.contains("too old"), "must name the cause: {msg}");
        assert!(
            msg.contains("2.1.251"),
            "must surface the required version: {msg}"
        );
        assert!(msg.contains("claude update"), "must name the fix: {msg}");
        assert!(
            !msg.contains("sign-in"),
            "updating is not a sign-in problem: {msg}"
        );
        assert!(
            !msg.contains("in Terminal"),
            "that phrase triggers the desktop Sign in hint: {msg}"
        );
        assert!(msg.len() < 200, "must fit on a card: {msg}");
    }

    #[test]
    fn usage_limits_are_not_reported_as_broken_sign_ins() {
        let msg = humanize_provider_error("CLAUDE_CODE_CLI", "error: rate limit exceeded");
        assert!(msg.contains("usage limit"), "{msg}");
        assert!(!msg.contains("sign-in has expired"), "{msg}");
    }

    #[test]
    fn only_id_named_user_workspaces_get_their_owners_name() {
        let uid = "1a4b5e13791c47de9e2594ed014440d3";
        assert!(workspace_needs_owner_name(uid, uid));
        assert!(workspace_needs_owner_name(uid, "Untitled workspace"));
        assert!(
            !workspace_needs_owner_name(uid, "Acme"),
            "an explicit name wins"
        );
        assert!(!workspace_needs_owner_name("default", "default"));
        assert!(!workspace_needs_owner_name("default", "Default"));
    }

    #[test]
    fn unrecognised_failures_fall_back_to_the_first_useful_line() {
        let msg = humanize_provider_error(
            "",
            "2026-09-05T22:02:40Z ERROR noise::worker: ignore me\nthe disk is full\n",
        );
        assert_eq!(msg, "the disk is full");
    }

    fn sample_mission(entity_id: &str) -> Mission {
        Mission {
            id: "mission-1".to_string(),
            entity_id: entity_id.to_string(),
            title: "Test Mission".to_string(),
            summary: "".to_string(),
            status: MissionStatus::Planned,
            priority: "medium".to_string(),
            goal: "".to_string(),
            owner: None,
            created_by: None,
            source: None,
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
        }
    }

    #[test]
    fn resolve_task_entity_accepts_valid_mission_id() {
        let missions = vec![sample_mission("entity-a")];
        let out = resolve_task_entity_and_validate_mission(
            &missions,
            Some("entity-a".to_string()),
            Some("mission-1".to_string()),
        )
        .expect("validation should pass");
        assert_eq!(out.0.as_deref(), Some("entity-a"));
        assert_eq!(out.1.as_deref(), Some("mission-1"));
    }

    #[test]
    fn resolve_task_entity_rejects_invalid_mission_id() {
        let missions = vec![sample_mission("entity-a")];
        let out = resolve_task_entity_and_validate_mission(
            &missions,
            Some("entity-a".to_string()),
            Some("missing-mission".to_string()),
        );
        assert!(matches!(out, Err(StatusCode::BAD_REQUEST)));
    }

    #[test]
    fn resolve_task_entity_rejects_mismatched_entity() {
        let missions = vec![sample_mission("entity-a")];
        let out = resolve_task_entity_and_validate_mission(
            &missions,
            Some("entity-b".to_string()),
            Some("mission-1".to_string()),
        );
        assert!(matches!(out, Err(StatusCode::BAD_REQUEST)));
    }

    fn sample_entity(id: &str, owner: &str) -> Entity {
        Entity {
            id: id.to_string(),
            name: id.to_string(),
            owner: Some(owner.to_string()),
            status: "Active".to_string(),
            dept: "General".to_string(),
            ..Default::default()
        }
    }

    fn sample_personal(id: &str, owner: &str, user: &str) -> Entity {
        Entity {
            id: id.to_string(),
            name: "Personal".to_string(),
            owner: Some(owner.to_string()),
            status: "Active".to_string(),
            dept: "General".to_string(),
            is_personal: true,
            personal_user_id: Some(user.to_string()),
            visibility: crate::state::EntityVisibility::Private,
            ..Default::default()
        }
    }

    #[test]
    fn personal_space_visible_only_to_owner() {
        let p = sample_personal("personal_ws_userA", "ws", "userA");
        assert!(is_entity_visible(&p, "ws", Some("userA")), "owner sees it");
        assert!(
            !is_entity_visible(&p, "ws", Some("userB")),
            "teammate in same workspace must not"
        );
        assert!(
            !is_entity_visible(&p, "ws", None),
            "background/unauthed paths never see a personal space"
        );
        let biz = sample_entity("ent-1", "ws");
        assert!(is_entity_visible(&biz, "ws", Some("userB")));
        assert!(is_entity_visible(&biz, "ws", None));
    }

    #[test]
    fn personal_spaces_isolated_in_shared_workspace() {
        let entities = vec![
            sample_entity("biz", "ws"),
            sample_personal("personal_ws_userA", "ws", "userA"),
            sample_personal("personal_ws_userB", "ws", "userB"),
        ];
        let for_a = entity_ids_for_workspace(&entities, "ws", Some("userA"));
        assert!(for_a.contains("biz"));
        assert!(for_a.contains("personal_ws_userA"));
        assert!(!for_a.contains("personal_ws_userB"));
        let for_b = entity_ids_for_workspace(&entities, "ws", Some("userB"));
        assert!(for_b.contains("personal_ws_userB"));
        assert!(!for_b.contains("personal_ws_userA"));
    }

    #[test]
    fn personal_space_id_is_colon_free_and_scoped() {
        let id = crate::state::personal_space_id("org_x", "user_2abc");
        assert!(
            !id.contains(':'),
            "colon breaks SurrealDB RecordId round-trip"
        );
        assert_eq!(id, "personal_org_x_user_2abc");
        // Different workspace => different personal space for the same user.
        assert_ne!(id, crate::state::personal_space_id("org_y", "user_2abc"));
    }

    fn sample_task(id: &str, entity_id: Option<&str>) -> Task {
        Task {
            id: id.to_string(),
            title: id.to_string(),
            dept: "General".to_string(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: entity_id.map(|v| v.to_string()),
            mission_id: None,
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
        }
    }

    #[test]
    fn workspace_entity_filter_isolated() {
        let entities = vec![
            sample_entity("entity-a", "ws-a"),
            sample_entity("entity-b", "ws-b"),
        ];
        let ws_a = entity_ids_for_workspace(&entities, "ws-a", None);
        let ws_b = entity_ids_for_workspace(&entities, "ws-b", None);
        assert!(ws_a.contains("entity-a"));
        assert!(!ws_a.contains("entity-b"));
        assert!(ws_b.contains("entity-b"));
        assert!(!ws_b.contains("entity-a"));
    }

    #[test]
    fn workspace_task_visibility_isolated() {
        let mut ws_a_entities = HashSet::new();
        ws_a_entities.insert("entity-a".to_string());
        let t_a = sample_task("task-a", Some("entity-a"));
        let t_b = sample_task("task-b", Some("entity-b"));
        assert!(task_visible_in_workspace(&t_a, &ws_a_entities, "ws-a"));
        assert!(!task_visible_in_workspace(&t_b, &ws_a_entities, "ws-a"));
    }

    #[test]
    fn workspace_mission_visibility_isolated() {
        let mut ws_a_entities = HashSet::new();
        ws_a_entities.insert("entity-a".to_string());
        let m_a = sample_mission("entity-a");
        let m_b = sample_mission("entity-b");
        assert!(ws_a_entities.contains(&m_a.entity_id));
        assert!(!ws_a_entities.contains(&m_b.entity_id));
    }

    #[test]
    fn workspace_approval_visibility_isolated() {
        let mut ws_a_entities = HashSet::new();
        ws_a_entities.insert("entity-a".to_string());
        let a_a = ApprovalRequest {
            id: "ap-a".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            entity_id: "entity-a".to_string(),
            connection_id: None,
            task_id: None,
            action: "approve".to_string(),
            status: ApprovalStatus::Pending,
            detail: None,
            created_by: None,
        };
        let a_b = ApprovalRequest {
            entity_id: "entity-b".to_string(),
            ..a_a.clone()
        };
        assert!(ws_a_entities.contains(&a_a.entity_id));
        assert!(!ws_a_entities.contains(&a_b.entity_id));
    }

    #[test]
    fn workspace_operator_feed_visibility_isolated() {
        let mut ws_a_entities = HashSet::new();
        ws_a_entities.insert("entity-a".to_string());
        let event_a = OperatorFeedEvent {
            id: "feed-a".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            kind: OperatorFeedKind::TaskCreated,
            title: "created".to_string(),
            detail: None,
            task_id: None,
            mission_id: None,
            entity_id: Some("entity-a".to_string()),
            connection_id: None,
            approval_id: None,
        };
        let event_b = OperatorFeedEvent {
            entity_id: Some("entity-b".to_string()),
            ..event_a.clone()
        };
        assert!(feed_visible_in_workspace(&event_a, &ws_a_entities, "ws-a"));
        assert!(!feed_visible_in_workspace(&event_b, &ws_a_entities, "ws-a"));
    }

    #[test]
    fn canonicalize_task_kind_maps_reminder_aliases() {
        assert_eq!(
            canonicalize_task_kind(Some("follow-up".to_string())).as_deref(),
            Some("reminder")
        );
        assert_eq!(
            canonicalize_task_kind(Some("FoLLoWup".to_string())).as_deref(),
            Some("reminder")
        );
        assert_eq!(
            canonicalize_task_kind(Some(" note ".to_string())).as_deref(),
            Some("note")
        );
    }

    #[test]
    fn stale_recommendation_resolved_by_user_input_needed() {
        let stale_ts = (Utc::now() - chrono::Duration::hours(30)).to_rfc3339();
        let resolve_ts = (Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let feed = vec![
            OperatorFeedEvent {
                id: "feed-1".to_string(),
                timestamp: stale_ts,
                kind: OperatorFeedKind::MissionStalled,
                title: "mission stalled".to_string(),
                detail: Some("branch=user_input".to_string()),
                task_id: None,
                mission_id: Some("mission-1".to_string()),
                entity_id: Some("entity-1".to_string()),
                connection_id: None,
                approval_id: None,
            },
            OperatorFeedEvent {
                id: "feed-2".to_string(),
                timestamp: resolve_ts,
                kind: OperatorFeedKind::MissionUserInputNeeded,
                title: "needs user input".to_string(),
                detail: None,
                task_id: None,
                mission_id: Some("mission-1".to_string()),
                entity_id: Some("entity-1".to_string()),
                connection_id: None,
                approval_id: None,
            },
        ];
        let stale = derive_stale_recommendations(&feed, 24);
        assert_eq!(stale.total, 0);
    }

    /// The Today screens (web and mobile) are written against these exact keys.
    /// Renaming a field here silently empties a section over there, so the
    /// contract is pinned rather than inferred.
    #[test]
    fn daily_review_payload_keys_are_stable() {
        let now = Utc::now();
        let mut task = sample_task("task-a", Some("entity-a"));
        task.due_at = Some((now - chrono::Duration::hours(2)).to_rfc3339());
        let names: HashMap<String, String> = [("entity-a".to_string(), "Acme".to_string())].into();

        let payload = DailyReviewResponse {
            generated_at: now.to_rfc3339(),
            overdue: vec![task_lite(&task, &names, now)],
            due_today: Vec::new(),
            pending_approvals: Vec::new(),
            missions_awaiting_input: Vec::new(),
            heartbeat_highlights: Vec::new(),
            upcoming_heartbeats: Vec::new(),
            streak: crate::accountability::compute_streak(&[], now),
            suggested_focus: Vec::new(),
        };
        let json = serde_json::to_value(&payload).expect("serialize");
        for key in [
            "generatedAt",
            "overdue",
            "dueToday",
            "pendingApprovals",
            "missionsAwaitingInput",
            "heartbeatHighlights",
            "upcomingHeartbeats",
            "streak",
            "suggestedFocus",
        ] {
            assert!(json.get(key).is_some(), "missing {key}");
        }
        let row = &json["overdue"][0];
        for key in [
            "id",
            "title",
            "entityId",
            "entityName",
            "missionId",
            "priority",
            "dueAt",
            "status",
            "overdueHours",
        ] {
            assert!(row.get(key).is_some(), "missing task key {key}");
        }
        assert_eq!(row["entityName"], serde_json::json!("Acme"));
        assert_eq!(row["overdueHours"], serde_json::json!(2));
        for key in [
            "currentStreakDays",
            "completedToday",
            "completedLast7",
            "completedLast30",
        ] {
            assert!(
                json["streak"].get(key).is_some(),
                "missing streak key {key}"
            );
        }
    }
}
