//! Import an existing project into an entity by observing it, not interviewing.
//!
//! The manual paths (`/api/entities`, the conversational flow) assume the
//! operator will *type in* what an entity is. For a project that already exists
//! — a repo on disk, a GitHub URL — that is backwards: the answers are sitting
//! in the README, the manifests, and the git log. This module reads them.
//!
//! Flow: `analyze` classifies the source and runs the right engine —
//!
//! - **local folder/repo** → a read-only [`crate::agent_sessions`] Claude run
//!   *inside* the project directory, so the analysis is grounded in the actual
//!   file tree and recent git history, not a summary of a summary;
//! - **GitHub URL / website** → the existing source fetch
//!   ([`crate::control_api::fetch_entity_source_with_token`]) plus a structured
//!   completion, since there is no local checkout to walk.
//!
//! Either way the result is a *draft*, never a committed entity: the operator
//! reviews and edits it, then `commit` creates the entity through
//! [`crate::control_api::internal_create_entity`] (which provisions default
//! heartbeats) and creates only the suggested tasks/reminders the operator kept.
//! Jobs are in-memory on purpose — an interrupted analysis is re-run, not
//! resumed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use axum::http::StatusCode;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

use consistent_world_llm::client::{ChatMessage, CompletionRequest, Role};

use crate::capture::{build_llm_client, resolve_operations_llm};
use crate::config::AppConfig;
use crate::control_api::{
    CreateEntityRequest, CreateTaskRequest, existing_local_path, fetch_entity_source_with_token,
    github_token_for_source_ingest, internal_create_entity, internal_create_task,
    is_entity_visible, is_valid_priority, refresh_entity_readiness,
};
use crate::credentials_resolver;
use crate::state::{
    ControlState, Entity, EntitySetup, OperatorFeedEvent, OperatorFeedKind, ServerState,
};

/// Keep a working set, not a history: jobs exist to be polled once and
/// committed. Completed rows past this cap are dropped oldest-first.
const MAX_JOBS: usize = 40;
/// Hard wall for one analysis. The stall timeout below catches a wedged CLI;
/// this catches an agent that keeps happily producing events forever.
const LOCAL_ANALYSIS_TIMEOUT_SECS: u64 = 900;
/// Per-event silence budget for the local Claude session.
const LOCAL_STALL_TIMEOUT_SECS: u64 = 300;
/// Turn cap for the local session: enough to read a repo, not enough to wander.
const LOCAL_MAX_TURNS: u32 = 30;
const MAX_SUGGESTED_TASKS: usize = 8;

// ─── Draft shapes (the reviewable proposal) ──────────────────────────────────

/// One next-step the analysis found, shown to the operator for review before
/// [`commit_import`] turns kept rows into real tasks/reminders.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuggestedTask {
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub priority: Option<String>,
    /// `task` | `reminder`. Reminders carry `due_at` and ride the reminder
    /// ticker; tasks go to the orchestrator.
    #[serde(default = "default_task_kind")]
    pub kind: String,
    #[serde(default, alias = "due_at")]
    pub due_at: Option<String>,
    /// Pre-checked in the review UI. The operator has the final say — commit
    /// only creates rows where this survived review.
    #[serde(default = "default_true")]
    pub include: bool,
}

fn default_task_kind() -> String {
    "task".to_string()
}

fn default_true() -> bool {
    true
}

/// The reviewable proposal an analysis produces — either engine
/// ([`analyze_local`] or [`analyze_remote`]) fills this in, matching the JSON
/// contract in [`draft_schema_instructions`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityImportDraft {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, alias = "entity_type")]
    pub entity_type: Option<String>,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub goals: Option<String>,
    #[serde(default, alias = "active_context")]
    pub active_context: Option<String>,
    #[serde(default, alias = "operating_rules")]
    pub operating_rules: Option<String>,
    /// Free-form notes from the operator's context step; land on
    /// `Entity::notes` and in the import brief doc.
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub setup: EntitySetup,
    /// CEO-level briefing: what this is, where it stands, what needs attention.
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default, alias = "suggested_tasks")]
    pub suggested_tasks: Vec<SuggestedTask>,
}

/// What the operator said about the entity before analysis ran. The code
/// tells the engine what exists; this tells it what the operator *means* —
/// so it is treated as authoritative: it overrides the analysis wherever both
/// have an answer, and rides the prompt so the suggested tasks aim at the
/// stated goal instead of whatever the README happened to mention.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImportContext {
    #[serde(default, alias = "what_it_is")]
    pub what_it_is: Option<String>,
    #[serde(default, alias = "primary_goal")]
    pub primary_goal: Option<String>,
    #[serde(default, alias = "success_criteria")]
    pub success_criteria: Option<String>,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default, alias = "active_context")]
    pub active_context: Option<String>,
    #[serde(default, alias = "operating_rules")]
    pub operating_rules: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

impl ImportContext {
    fn clean(value: &Option<String>) -> Option<String> {
        value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    }

    /// Trimmed copy with blanks dropped, so "the operator answered" always
    /// means a non-empty answer.
    pub fn normalized(&self) -> ImportContext {
        ImportContext {
            what_it_is: Self::clean(&self.what_it_is),
            primary_goal: Self::clean(&self.primary_goal),
            success_criteria: Self::clean(&self.success_criteria),
            stage: Self::clean(&self.stage).map(|s| s.to_ascii_lowercase()),
            active_context: Self::clean(&self.active_context),
            operating_rules: Self::clean(&self.operating_rules),
            notes: Self::clean(&self.notes),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.labelled().is_empty()
    }

    /// `(label, answer)` pairs in display order — the shape both the prompt
    /// block and the import brief doc render.
    pub fn labelled(&self) -> Vec<(String, String)> {
        let n = self.normalized();
        [
            ("What it is", n.what_it_is),
            ("Main goal", n.primary_goal),
            ("Success looks like", n.success_criteria),
            ("Stage", n.stage),
            ("Happening right now", n.active_context),
            ("Rules and constraints", n.operating_rules),
            ("Notes", n.notes),
        ]
        .into_iter()
        .filter_map(|(label, value)| value.map(|v| (label.to_string(), v)))
        .collect()
    }

    /// Prompt block for either engine. Empty string when nothing was given.
    pub fn prompt_block(&self) -> String {
        let rows = self.labelled();
        if rows.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\nThe operator described the project in their own words. Treat these answers as authoritative: where they conflict with what you observe, the operator is right about intent and the code is right about state. Aim the goals and the suggested next steps at the stated goal.\n",
        );
        for (label, value) in rows {
            out.push_str(&format!("- {label}: {value}\n"));
        }
        out
    }
}

/// Operator answers override what the analysis found, field by field. Rules
/// are the one exception: the operator's constraints and the conventions the
/// analysis found are different things, so both are kept.
fn apply_operator_context(draft: &mut EntityImportDraft, context: &ImportContext) {
    let ctx = context.normalized();
    if let Some(v) = ctx.what_it_is.clone() {
        draft.setup.what_it_is = Some(v.clone());
        if draft
            .description
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            draft.description = Some(v);
        }
    }
    if let Some(v) = ctx.primary_goal.clone() {
        draft.setup.primary_goal = Some(v.clone());
        if draft
            .goals
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
        {
            draft.goals = Some(v);
        }
    }
    if let Some(v) = ctx.success_criteria.clone() {
        draft.setup.success_criteria = Some(v);
    }
    if let Some(v) = ctx.stage.clone() {
        draft.stage = Some(v);
    }
    if let Some(v) = ctx.active_context.clone() {
        draft.active_context = Some(v);
    }
    if let Some(v) = ctx.operating_rules.clone() {
        draft.operating_rules = match draft
            .operating_rules
            .as_deref()
            .map(str::trim)
            .filter(|found| !found.is_empty() && *found != v)
        {
            Some(found) => Some(format!(
                "{v}\n\nConventions observed in the source: {found}"
            )),
            None => Some(v),
        };
    }
    if let Some(v) = ctx.notes.clone() {
        draft.notes = Some(v);
    }
}

// ─── Jobs ────────────────────────────────────────────────────────────────────

/// Tracked entry for one `analyze` call. Held only in the process-local
/// [`jobs`] map — the client polls this by id until `status` leaves
/// `running`, then posts the (possibly edited) draft to [`commit_import`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportJob {
    pub id: String,
    #[serde(skip)]
    pub workspace_id: String,
    pub source: String,
    /// `local_repo` | `local_folder` | `github_repo` | `website`.
    pub source_kind: String,
    /// `running` | `ready` | `failed` | `committed`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<EntityImportDraft>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Set once the draft has been committed, so a stale tab can follow along.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    /// The operator's context answers, kept so commit can write them into the
    /// import brief doc verbatim even after the draft was edited.
    #[serde(skip)]
    pub context: Option<ImportContext>,
    pub started_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<String>,
}

// Process-local job registry, lazily initialised on first use. Jobs are not
// persisted — an in-flight analysis lost to a restart is simply re-run.
fn jobs() -> &'static RwLock<HashMap<String, ImportJob>> {
    static JOBS: OnceLock<RwLock<HashMap<String, ImportJob>>> = OnceLock::new();
    JOBS.get_or_init(|| RwLock::new(HashMap::new()))
}

// Register a new job, evicting the oldest completed ones first once the
// registry hits `MAX_JOBS` so a long-running gateway doesn't grow unbounded.
async fn insert_job(job: ImportJob) {
    let mut map = jobs().write().await;
    if map.len() >= MAX_JOBS {
        let mut done: Vec<(String, String)> = map
            .values()
            .filter(|j| j.status != "running")
            .map(|j| (j.started_at.clone(), j.id.clone()))
            .collect();
        done.sort();
        for (_, id) in done.into_iter().take(map.len() + 1 - MAX_JOBS) {
            map.remove(&id);
        }
    }
    map.insert(job.id.clone(), job);
}

// Move a job from `running` to `ready`/`failed` and record its result.
// A no-op if the job id is unknown (e.g. already evicted by `insert_job`).
async fn finish_job(job_id: &str, draft: Option<EntityImportDraft>, error: Option<String>) {
    let mut map = jobs().write().await;
    if let Some(job) = map.get_mut(job_id) {
        job.status = if error.is_some() { "failed" } else { "ready" }.to_string();
        job.draft = draft;
        job.error = error;
        job.completed_at = Some(Utc::now().to_rfc3339());
    }
}

/// Look up a job for polling, scoped to the requesting workspace so one
/// workspace can never read another's in-flight analysis.
pub async fn get_job(workspace_id: &str, job_id: &str) -> Option<ImportJob> {
    let map = jobs().read().await;
    map.get(job_id)
        .filter(|j| j.workspace_id == workspace_id)
        .cloned()
}

// ─── Requests / responses ────────────────────────────────────────────────────

/// Body of the `analyze` request that kicks off a background [`ImportJob`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeImportRequest {
    /// A local folder path (absolute or `~/…`) or an http(s) URL.
    pub source: String,
    /// Optional name hint for the drafted entity.
    #[serde(default)]
    pub name: Option<String>,
    /// Fill an existing entity instead of drafting a new one. The entity's
    /// current values seed whatever the analysis leaves blank, so the review
    /// screen always shows the union of what exists and what was found.
    #[serde(default)]
    pub entity_id: Option<String>,
    /// Extra resources the operator wants considered — a repo URL, the live
    /// site, a business-plan link. They ride along in the analysis prompt;
    /// only `source` is fetched directly.
    #[serde(default)]
    pub links: Vec<String>,
    /// The operator's own account of the entity (context step). Authoritative
    /// over the analysis — see [`ImportContext`].
    #[serde(default)]
    pub context: Option<ImportContext>,
}

/// Immediate reply to `analyze` — just enough for the client to start polling
/// [`get_job`].
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AnalyzeImportResponse {
    pub job_id: String,
    pub source_kind: String,
}

/// Body of the `commit` request: the operator-reviewed draft, plus whether it
/// creates a new entity or is applied to an existing one.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitImportRequest {
    /// Provenance only — commit trusts the (possibly edited) draft in the body,
    /// not whatever the job produced.
    #[serde(default)]
    pub job_id: Option<String>,
    /// Apply the draft to this existing entity instead of creating a new one.
    #[serde(default)]
    pub entity_id: Option<String>,
    pub draft: EntityImportDraft,
}

/// Result of `commit`: the settled entity plus a count of what was created
/// from the kept suggestions.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitImportResponse {
    pub entity: Entity,
    /// False when the draft was applied to an existing entity.
    pub created: bool,
    pub created_tasks: usize,
    pub created_reminders: usize,
    /// True when the source is being indexed into the entity's knowledge base
    /// in the background; poll `GET /api/entities/{id}/knowledge`.
    pub indexing: bool,
}

// ─── Source classification ───────────────────────────────────────────────────

// What `classify_source` decided the operator pointed at; picks which
// analysis engine (`analyze_local` vs `analyze_remote`) runs.
enum ImportSource {
    Local(PathBuf),
    Url(String),
}

/// Decide what the operator pointed at. A URL is taken at its word; a path must
/// actually be a directory on this machine — a typo'd path fails here, loudly,
/// instead of producing a confidently empty analysis.
fn classify_source(raw: &str) -> Result<(ImportSource, String), StatusCode> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let kind = if crate::control_api::github_owner_repo(trimmed).is_some() {
            "github_repo"
        } else {
            "website"
        };
        return Ok((ImportSource::Url(trimmed.to_string()), kind.to_string()));
    }
    let path = existing_local_path(Some(trimmed)).ok_or(StatusCode::BAD_REQUEST)?;
    if !path.is_dir() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let kind = if path.join(".git").exists() {
        "local_repo"
    } else {
        "local_folder"
    };
    Ok((ImportSource::Local(path), kind.to_string()))
}

// ─── Analysis ────────────────────────────────────────────────────────────────

/// Classify, register a job, and run the analysis in the background. Returns
/// immediately — the client polls the job until `ready`.
pub async fn start_analysis(
    control: Arc<ControlState>,
    config: Arc<AppConfig>,
    ledger: Option<Arc<crate::billing::CreditLedger>>,
    workspace_id: String,
    principal_id: String,
    req: AnalyzeImportRequest,
) -> Result<AnalyzeImportResponse, StatusCode> {
    let (source, source_kind) = classify_source(&req.source)?;
    let job_id = Uuid::new_v4().to_string();
    // A target entity must exist and be visible before minutes of analysis are
    // spent on it; its snapshot later seeds whatever the analysis leaves blank.
    let target_entity = match req
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        Some(entity_id) => {
            let entities = control.entities.read().await;
            Some(
                entities
                    .iter()
                    .find(|e| {
                        e.id == entity_id
                            && is_entity_visible(e, &workspace_id, Some(principal_id.as_str()))
                    })
                    .cloned()
                    .ok_or(StatusCode::NOT_FOUND)?,
            )
        }
        None => None,
    };
    let name_hint = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
        .or_else(|| target_entity.as_ref().map(|e| e.name.clone()));
    let links: Vec<String> = req
        .links
        .iter()
        .map(|l| l.trim().to_string())
        .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
        .take(5)
        .collect();
    let source_display = match &source {
        ImportSource::Local(path) => path.display().to_string(),
        ImportSource::Url(url) => url.clone(),
    };
    let context = req
        .context
        .as_ref()
        .map(ImportContext::normalized)
        .filter(|c| !c.is_empty());
    insert_job(ImportJob {
        id: job_id.clone(),
        workspace_id: workspace_id.clone(),
        source: source_display,
        source_kind: source_kind.clone(),
        status: "running".to_string(),
        draft: None,
        error: None,
        entity_id: target_entity.as_ref().map(|e| e.id.clone()),
        context: context.clone(),
        started_at: Utc::now().to_rfc3339(),
        completed_at: None,
    })
    .await;

    // Resolved before the spawn: the token lookup needs the workspace's
    // connections, and doing it here keeps the background task self-contained.
    let github_token = match &source {
        ImportSource::Url(_) => {
            github_token_for_source_ingest(control.as_ref(), &workspace_id, None).await
        }
        ImportSource::Local(_) => None,
    };

    let job_for_task = job_id.clone();
    tokio::spawn(async move {
        let outcome = match source {
            ImportSource::Local(path) => {
                analyze_local(
                    control.clone(),
                    &job_for_task,
                    path,
                    name_hint.as_deref(),
                    &links,
                    context.as_ref(),
                )
                .await
            }
            ImportSource::Url(url) => {
                analyze_remote(
                    control.clone(),
                    config,
                    ledger,
                    &workspace_id,
                    &url,
                    github_token.as_deref(),
                    name_hint.as_deref(),
                    &links,
                    context.as_ref(),
                )
                .await
            }
        };
        match outcome {
            Ok(mut draft) => {
                // Operator first, then the entity's existing values fill what
                // is still blank — so what the operator just typed is never
                // shadowed by an older answer.
                if let Some(ctx) = context.as_ref() {
                    apply_operator_context(&mut draft, ctx);
                }
                if let Some(entity) = target_entity.as_ref() {
                    seed_draft_from_entity(&mut draft, entity);
                }
                finish_job(&job_for_task, Some(draft), None).await;
            }
            Err(message) => finish_job(&job_for_task, None, Some(message)).await,
        }
    });

    Ok(AnalyzeImportResponse {
        job_id,
        source_kind,
    })
}

/// Review shows the union: analysis findings win where present, the entity's
/// existing values fill everything the analysis left blank. Nothing the
/// operator already wrote disappears from the screen they are about to approve.
fn seed_draft_from_entity(draft: &mut EntityImportDraft, entity: &Entity) {
    fn fill(slot: &mut Option<String>, value: &Option<String>) {
        if slot.as_deref().map(str::trim).unwrap_or("").is_empty()
            && let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty())
        {
            *slot = Some(v.to_string());
        }
    }
    if draft.name.trim().is_empty() {
        draft.name = entity.name.clone();
    }
    fill(&mut draft.description, &entity.description);
    fill(&mut draft.entity_type, &entity.entity_type);
    fill(&mut draft.stage, &entity.stage);
    fill(&mut draft.profile, &entity.profile);
    fill(&mut draft.goals, &entity.goals);
    fill(&mut draft.active_context, &entity.active_context);
    fill(&mut draft.operating_rules, &entity.operating_rules);
    fill(&mut draft.setup.what_it_is, &entity.setup.what_it_is);
    fill(&mut draft.setup.primary_goal, &entity.setup.primary_goal);
    fill(
        &mut draft.setup.success_criteria,
        &entity.setup.success_criteria,
    );
    fill(
        &mut draft.setup.primary_domain,
        &entity.setup.primary_domain,
    );
    fill(&mut draft.setup.primary_email, &entity.setup.primary_email);
    fill(&mut draft.setup.repo_path, &entity.setup.repo_path);
    fill(&mut draft.setup.backend_stack, &entity.setup.backend_stack);
    fill(
        &mut draft.setup.frontend_stack,
        &entity.setup.frontend_stack,
    );
    fill(
        &mut draft.setup.database_stack,
        &entity.setup.database_stack,
    );
}

/// Operator-supplied companion resources, rendered for either prompt.
fn links_block(links: &[String]) -> String {
    if links.is_empty() {
        return String::new();
    }
    format!(
        "\n\nThe operator also provided these related resources — factor them in (fetch them if you are able to; otherwise use them as naming/context hints):\n{}",
        links
            .iter()
            .map(|l| format!("- {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

/// The JSON contract both engines are asked to satisfy. Shared so the local
/// agent and the remote completion cannot drift apart.
fn draft_schema_instructions() -> &'static str {
    r#"Reply with ONLY a JSON object — no prose before or after, no code fences — with exactly these fields (use null when unknown):
{
  "name": "short project name",
  "description": "2-3 plain-language sentences on what this project is",
  "entityType": "product | content | service | website | tool | venture",
  "stage": "idea | prototype | building | launched | growing | maintenance",
  "profile": "what this project is, written for long-term operational memory (3-5 sentences)",
  "goals": "the project's apparent goals",
  "activeContext": "what is being worked on right now",
  "operatingRules": "conventions or constraints an operator should respect",
  "setup": {
    "whatItIs": "one sentence",
    "primaryGoal": "one sentence",
    "successCriteria": "what success looks like",
    "primaryDomain": "domain name if one is evident, else null",
    "backendStack": "e.g. Rust, Axum",
    "frontendStack": "e.g. SvelteKit",
    "databaseStack": "e.g. SurrealDB"
  },
  "summary": "a CEO-level briefing: what this is, its current state, what happened recently, and what needs attention (5-8 sentences)",
  "suggestedTasks": [
    { "title": "…", "description": "…", "priority": "low | medium | high", "kind": "task" },
    { "title": "…", "kind": "reminder", "dueAt": "RFC3339 timestamp within the next two weeks" }
  ]
}
Suggest at most 6 items: concrete next steps evident from the source (unfinished work, missing tests, TODO markers, stale docs). Use kind "reminder" with "dueAt" only for time-based follow-ups; everything actionable is kind "task"."#
}

// The prompt handed to the read-only Claude session in `analyze_local`,
// combining the name hint, the operator's extra links, and the shared draft
// JSON contract.
fn local_analysis_prompt(
    name_hint: Option<&str>,
    links: &[String],
    context: Option<&ImportContext>,
) -> String {
    let hint = name_hint
        .map(|n| format!("The operator calls this project \"{n}\".\n\n"))
        .unwrap_or_default();
    format!(
        "{hint}You are onboarding an existing project into an operations platform. Analyze the repository in the current working directory: read the README and any docs, the dependency manifests (package.json, Cargo.toml, pyproject.toml, …), the top-level file tree, and — if this is a git repo — recent history via `git log --oneline -20`. This is a read-only analysis: do not create, modify, or delete any files.{}{}\n\n{}",
        context.map(ImportContext::prompt_block).unwrap_or_default(),
        links_block(links),
        draft_schema_instructions()
    )
}

/// Ground the draft in the actual working copy: a read-only Claude session run
/// inside the project directory. Falls back to a minimal name-and-README draft
/// rather than failing the job when the model's reply doesn't parse — the
/// operator can still edit their way to a useful entity.
async fn analyze_local(
    control: Arc<ControlState>,
    job_id: &str,
    path: PathBuf,
    name_hint: Option<&str>,
    links: &[String],
    context: Option<&ImportContext>,
) -> Result<EntityImportDraft, String> {
    let request = crate::agent_sessions::SessionRequest {
        run_id: format!("entity-import-{job_id}"),
        prompt: local_analysis_prompt(name_hint, links, context),
        cwd: path.display().to_string(),
        permission_mode: crate::agent_sessions::PermissionMode::BypassPermissions,
        max_turns: Some(LOCAL_MAX_TURNS),
        resume_session_id: None,
        stall_timeout_secs: LOCAL_STALL_TIMEOUT_SECS,
        mcp_config_path: None,
    };
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(LOCAL_ANALYSIS_TIMEOUT_SECS),
        crate::agent_sessions::run_claude_session(control, request),
    )
    .await
    .map_err(|_| {
        format!("Analysis timed out after {LOCAL_ANALYSIS_TIMEOUT_SECS} seconds. Try again, or create the entity manually and attach the repo path.")
    })?;

    if outcome.status != "completed" {
        // An import is a one-shot job: nothing resumes it when the window
        // reopens, so say when to come back instead of promising a retry.
        if outcome.rate_limited {
            let when = outcome
                .resets_at
                .and_then(|secs| chrono::DateTime::<Utc>::from_timestamp(secs, 0))
                .map(|at| format!(" It resets around {} UTC.", at.format("%H:%M")))
                .unwrap_or_default();
            return Err(format!(
                "Claude's usage window is spent, so the analysis could not run.{when} Try the import again then, or import from a GitHub URL, which uses the operations model instead."
            ));
        }
        return Err(outcome.error.unwrap_or_else(|| {
            format!(
                "The analysis agent did not finish (status: {}).",
                outcome.status
            )
        }));
    }
    let mut draft = parse_import_draft(&outcome.output)
        .unwrap_or_else(|| fallback_local_draft(&path, name_hint));
    // The canonical path wins over whatever the model echoed back: it is what
    // the workspace gate and the executor will actually use.
    draft.setup.repo_path = Some(path.display().to_string());
    if draft.name.trim().is_empty() {
        draft.name = name_hint
            .map(str::to_string)
            .unwrap_or_else(|| folder_name(&path));
    }
    draft.suggested_tasks.truncate(MAX_SUGGESTED_TASKS);
    Ok(draft)
}

/// No checkout to walk, so: fetch what the source publishes (GitHub metadata +
/// README, or page text) and ask the operations model for the same draft JSON.
#[allow(clippy::too_many_arguments)]
async fn analyze_remote(
    control: Arc<ControlState>,
    config: Arc<AppConfig>,
    ledger: Option<Arc<crate::billing::CreditLedger>>,
    workspace_id: &str,
    url: &str,
    github_token: Option<&str>,
    name_hint: Option<&str>,
    links: &[String],
    operator: Option<&ImportContext>,
) -> Result<EntityImportDraft, String> {
    let context = fetch_entity_source_with_token(url, github_token)
        .await
        .map_err(|status| format!("Could not fetch {url} (HTTP {status})."))?;
    if context.text.trim().is_empty() {
        return Err(format!("{url} returned no readable content."));
    }
    let Some(resolved) = resolve_operations_llm(config.as_ref(), control.as_ref()).await else {
        return Err("No LLM is configured for analysis. Add a provider in Settings.".to_string());
    };
    let api_key = credentials_resolver::resolve_llm_api_key(&resolved.api_key_env);
    let provider = build_llm_client(
        config.as_ref(),
        &resolved.base_url,
        &api_key,
        &resolved.api_key_env,
    );
    let messages = vec![
        ChatMessage {
            role: Role::System,
            content: format!(
                "You are onboarding an existing project into an operations platform from its published source material. {}",
                draft_schema_instructions()
            ),
            tool_calls: None,
            tool_call_id: None,
        },
        ChatMessage {
            role: Role::User,
            content: format!(
                "Name hint: {}\nSource URL: {}\nSource kind: {}\nTitle: {}{}{}\n\nSource text:\n{}",
                name_hint.unwrap_or(""),
                context.url,
                context.kind,
                context.title.as_deref().unwrap_or(""),
                operator
                    .map(ImportContext::prompt_block)
                    .unwrap_or_default(),
                links_block(links),
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
        max_tokens: Some(1400),
        temperature: Some(0.2),
        stream: false,
    };
    let response = provider
        .complete(request)
        .await
        .map_err(|e| format!("Analysis failed: {e}"))?;
    if let Some(ledger) = ledger.as_ref() {
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
    let mut draft =
        parse_import_draft(&raw).unwrap_or_else(|| fallback_remote_draft(&context, name_hint));
    if draft.setup.repo_path.is_none() && context.kind == "github_repo" {
        draft.setup.repo_path = Some(context.url.clone());
    }
    if draft.name.trim().is_empty() {
        draft.name = name_hint
            .map(str::to_string)
            .or_else(|| context.title.clone())
            .unwrap_or_else(|| url.to_string());
    }
    draft.suggested_tasks.truncate(MAX_SUGGESTED_TASKS);
    Ok(draft)
}

/// Pull the JSON object out of a model reply that may be wrapped in prose or
/// fences. Tolerant on purpose: the reply is advisory, the operator reviews it.
fn parse_import_draft(raw: &str) -> Option<EntityImportDraft> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end <= start {
        return None;
    }
    serde_json::from_str::<EntityImportDraft>(&raw[start..=end]).ok()
}

fn folder_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "Imported project".to_string())
}

// Used when the local agent's reply doesn't parse as the draft JSON: a bare
// draft built from the folder name and a README excerpt, so the job still
// reaches `ready` instead of `failed`.
fn fallback_local_draft(path: &std::path::Path, name_hint: Option<&str>) -> EntityImportDraft {
    let readme = ["README.md", "README", "readme.md"]
        .iter()
        .find_map(|candidate| std::fs::read_to_string(path.join(candidate)).ok())
        .map(|text| text.chars().take(2000).collect::<String>());
    let description = readme.as_deref().map(|text| {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .take(4)
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(420)
            .collect::<String>()
    });
    EntityImportDraft {
        name: name_hint
            .map(str::to_string)
            .unwrap_or_else(|| folder_name(path)),
        description: description.filter(|v| !v.is_empty()),
        summary: Some(
            "Automatic analysis did not return a usable result; this draft holds only the folder name and README excerpt. Edit it before committing."
                .to_string(),
        ),
        ..Default::default()
    }
}

// Same idea as `fallback_local_draft`, but built from the fetched source text
// when the remote completion's reply doesn't parse.
fn fallback_remote_draft(
    context: &crate::control_api::SourceContext,
    name_hint: Option<&str>,
) -> EntityImportDraft {
    let excerpt = context
        .text
        .lines()
        .map(str::trim)
        .filter(|line| line.len() > 12)
        .take(8)
        .collect::<Vec<_>>()
        .join(" ");
    EntityImportDraft {
        name: name_hint
            .map(str::to_string)
            .or_else(|| context.title.clone())
            .unwrap_or_default(),
        description: Some(excerpt.chars().take(420).collect::<String>()).filter(|v| !v.is_empty()),
        summary: Some(
            "Automatic analysis did not return a usable result; this draft holds only an excerpt of the source. Edit it before committing."
                .to_string(),
        ),
        ..Default::default()
    }
}

// ─── Commit ──────────────────────────────────────────────────────────────────

/// Turn a reviewed draft into entity state plus the kept tasks/reminders.
///
/// Two targets, one contract. **New entity**: creation goes through
/// [`internal_create_entity`] so the import path gets everything manual
/// creation gets — vault folder, default heartbeats, enrichment queueing.
/// **Existing entity** (`entity_id` set): the reviewed draft is applied in
/// place — non-empty draft values overwrite (the operator just approved them),
/// blank draft values leave the entity untouched. Task creation is best-effort
/// per row: one bad suggestion should not roll back an otherwise good import.
pub async fn commit_import(
    state: &ServerState,
    control: &ControlState,
    workspace_id: &str,
    principal_id: &str,
    req: CommitImportRequest,
) -> Result<CommitImportResponse, StatusCode> {
    fn override_field(target: &mut Option<String>, value: &Option<String>) {
        if let Some(v) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            *target = Some(v.to_string());
        }
    }

    let name = req.draft.name.trim().to_string();
    if name.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut setup = req.draft.setup.clone();
    // Record the workspace decision when the draft points at a real local
    // directory — this is what un-gates the task executor for the entity.
    if setup.workspace_choice.is_none()
        && let Some(path) = existing_local_path(setup.repo_path.as_deref()).filter(|p| p.is_dir())
    {
        setup.workspace_choice = Some(if path.join(".git").exists() {
            crate::entity_workspace::CHOICE_REPO.to_string()
        } else {
            crate::entity_workspace::CHOICE_LOCAL_FOLDER.to_string()
        });
    }

    let target_entity_id = req
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let created = target_entity_id.is_none();

    let entity = match target_entity_id {
        Some(entity_id) => {
            let mut entities = control.entities.write().await;
            let stored = entities
                .iter_mut()
                .find(|e| {
                    e.id == entity_id && is_entity_visible(e, workspace_id, Some(principal_id))
                })
                .ok_or(StatusCode::NOT_FOUND)?;
            stored.name = name;
            override_field(&mut stored.description, &req.draft.description);
            override_field(&mut stored.entity_type, &req.draft.entity_type);
            override_field(&mut stored.stage, &req.draft.stage);
            override_field(&mut stored.profile, &req.draft.profile);
            override_field(&mut stored.goals, &req.draft.goals);
            override_field(&mut stored.active_context, &req.draft.active_context);
            override_field(&mut stored.operating_rules, &req.draft.operating_rules);
            override_field(&mut stored.notes, &req.draft.notes);
            override_field(&mut stored.setup.what_it_is, &setup.what_it_is);
            override_field(&mut stored.setup.primary_goal, &setup.primary_goal);
            override_field(&mut stored.setup.success_criteria, &setup.success_criteria);
            override_field(&mut stored.setup.primary_domain, &setup.primary_domain);
            override_field(&mut stored.setup.primary_email, &setup.primary_email);
            override_field(&mut stored.setup.repo_path, &setup.repo_path);
            override_field(&mut stored.setup.backend_stack, &setup.backend_stack);
            override_field(&mut stored.setup.frontend_stack, &setup.frontend_stack);
            override_field(&mut stored.setup.database_stack, &setup.database_stack);
            // A recorded decision is the operator's; only fill silence.
            if stored.setup.workspace_choice.is_none() {
                stored.setup.workspace_choice = setup.workspace_choice.clone();
            }
            refresh_entity_readiness(stored);
            stored.clone()
        }
        None => {
            let created_entity = internal_create_entity(
                state,
                control,
                workspace_id,
                CreateEntityRequest {
                    name,
                    description: req.draft.description.clone(),
                    department: Some("General".to_string()),
                    owner: None,
                    status: Some("Active".to_string()),
                    entity_type: req.draft.entity_type.clone(),
                    default_workspace_root: None,
                    stage: req.draft.stage.clone(),
                    setup: Some(setup),
                },
            )
            .await?;
            let mut entities = control.entities.write().await;
            match entities.iter_mut().find(|e| e.id == created_entity.id) {
                Some(stored) => {
                    override_field(&mut stored.profile, &req.draft.profile);
                    override_field(&mut stored.goals, &req.draft.goals);
                    override_field(&mut stored.active_context, &req.draft.active_context);
                    override_field(&mut stored.operating_rules, &req.draft.operating_rules);
                    override_field(&mut stored.notes, &req.draft.notes);
                    refresh_entity_readiness(stored);
                    stored.clone()
                }
                None => created_entity,
            }
        }
    };

    let mut created_tasks = 0usize;
    let mut created_reminders = 0usize;
    for suggestion in req.draft.suggested_tasks.iter().filter(|t| t.include) {
        let title = suggestion.title.trim();
        if title.is_empty() {
            continue;
        }
        let is_reminder = suggestion.kind.trim().eq_ignore_ascii_case("reminder");
        let due_at = suggestion
            .due_at
            .as_deref()
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|d| d.to_utc().to_rfc3339());
        let metadata = if is_reminder {
            due_at.as_ref().map(|due| {
                serde_json::json!({
                    crate::reminders::REMINDER_DUE_AT_KEY: due,
                    crate::reminders::REMINDER_FIRED_KEY: false,
                })
            })
        } else {
            None
        };
        let request = CreateTaskRequest {
            title: title.to_string(),
            dept: Some("General".to_string()),
            roles: None,
            entity_id: Some(entity.id.clone()),
            mission_id: None,
            priority: suggestion
                .priority
                .as_deref()
                .map(|p| p.trim().to_ascii_lowercase())
                .filter(|p| is_valid_priority(p)),
            requires_human_approval: None,
            task_kind: Some(if is_reminder { "reminder" } else { "task" }.to_string()),
            content: suggestion.description.clone(),
            director_id: if is_reminder {
                Some("operations".to_string())
            } else {
                None
            },
            agent_id: None,
            connection_id: None,
            external_source: Some("entity-import".to_string()),
            external_id: None,
            metadata,
            due_at,
        };
        match internal_create_task(control, workspace_id, None, request).await {
            Ok(_) => {
                if is_reminder {
                    created_reminders += 1;
                } else {
                    created_tasks += 1;
                }
            }
            Err(status) => {
                tracing::warn!(entity_id = %entity.id, title, %status, "import task creation failed");
            }
        }
    }

    control.operator_feed.write().await.push(OperatorFeedEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        kind: OperatorFeedKind::EntitySourceImported,
        task_id: None,
        mission_id: None,
        entity_id: Some(entity.id.clone()),
        connection_id: None,
        title: if created {
            format!("Entity imported: {}", entity.name)
        } else {
            format!("Entity filled from sources: {}", entity.name)
        },
        detail: Some(match req.draft.summary.as_deref() {
            Some(summary) => summary.chars().take(500).collect(),
            None => {
                format!("Imported with {created_tasks} tasks and {created_reminders} reminders.")
            }
        }),
        approval_id: None,
    });

    let (job_source, job_context) = match req.job_id.as_deref() {
        Some(job_id) => {
            let mut map = jobs().write().await;
            match map
                .get_mut(job_id)
                .filter(|j| j.workspace_id == workspace_id)
            {
                Some(job) => {
                    job.status = "committed".to_string();
                    job.entity_id = Some(entity.id.clone());
                    (Some(job.source.clone()), job.context.clone())
                }
                None => (None, None),
            }
        }
        None => (None, None),
    };

    // Indexing: what the analysis read once, the entity keeps. The analyzed
    // source wins; otherwise whatever the entity records. Runs in the
    // background — commit returns as soon as the entity exists.
    let brief = crate::entity_knowledge::ImportBrief {
        summary: req.draft.summary.clone(),
        operator_context: job_context
            .as_ref()
            .map(ImportContext::labelled)
            .unwrap_or_default(),
        notes: req.draft.notes.clone(),
        suggested: req
            .draft
            .suggested_tasks
            .iter()
            .filter(|t| t.include && !t.title.trim().is_empty())
            .map(|t| t.title.trim().to_string())
            .collect(),
    };
    let (entity, indexing) =
        match crate::entity_knowledge::resolve_index_source(&entity, job_source.as_deref()) {
            Some(source) => {
                let status = crate::entity_knowledge::begin_index(
                    state.clone(),
                    workspace_id,
                    &entity.id,
                    source,
                    Some(brief),
                )
                .await;
                let mut entity = entity;
                entity.knowledge_index = Some(status);
                (entity, true)
            }
            None => {
                let status = crate::entity_knowledge::mark_skipped(
                    control,
                    &entity.id,
                    "No source to index: the draft has no repo path, folder, or URL.",
                )
                .await;
                let mut entity = entity;
                entity.knowledge_index = Some(status);
                (entity, false)
            }
        };

    Ok(CommitImportResponse {
        entity,
        created,
        created_tasks,
        created_reminders,
        indexing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_draft_tolerates_fences_and_prose() {
        let raw = "Here is the analysis:\n```json\n{\"name\":\"Acme\",\"suggestedTasks\":[{\"title\":\"Ship it\",\"kind\":\"task\"}]}\n```\nDone.";
        let draft = parse_import_draft(raw).expect("draft should parse");
        assert_eq!(draft.name, "Acme");
        assert_eq!(draft.suggested_tasks.len(), 1);
        assert!(draft.suggested_tasks[0].include, "include defaults to true");
    }

    #[test]
    fn parse_draft_accepts_snake_case_aliases() {
        let raw = r#"{"name":"Acme","active_context":"building","suggested_tasks":[{"title":"Remind","kind":"reminder","due_at":"2026-08-20T09:00:00Z"}]}"#;
        let draft = parse_import_draft(raw).expect("draft should parse");
        assert_eq!(draft.active_context.as_deref(), Some("building"));
        assert_eq!(
            draft.suggested_tasks[0].due_at.as_deref(),
            Some("2026-08-20T09:00:00Z")
        );
    }

    #[test]
    fn parse_draft_rejects_garbage() {
        assert!(parse_import_draft("no json here").is_none());
        assert!(parse_import_draft("{not valid").is_none());
    }

    #[test]
    fn seeding_fills_only_what_analysis_left_blank() {
        let mut draft = EntityImportDraft {
            name: String::new(),
            goals: Some("Ship v1".to_string()),
            ..Default::default()
        };
        let entity = Entity {
            id: "e1".to_string(),
            name: "NGOreality".to_string(),
            profile: Some("Existing profile".to_string()),
            goals: Some("Old goal".to_string()),
            ..Default::default()
        };
        seed_draft_from_entity(&mut draft, &entity);
        assert_eq!(draft.name, "NGOreality", "blank name comes from the entity");
        assert_eq!(
            draft.profile.as_deref(),
            Some("Existing profile"),
            "blank draft field is seeded"
        );
        assert_eq!(
            draft.goals.as_deref(),
            Some("Ship v1"),
            "analysis finding is never overwritten by the old value"
        );
    }

    #[test]
    fn operator_context_overrides_analysis_but_keeps_found_conventions() {
        let mut draft = EntityImportDraft {
            name: "Widget".to_string(),
            goals: Some("Whatever the README said".to_string()),
            stage: Some("prototype".to_string()),
            operating_rules: Some("Use pnpm.".to_string()),
            ..Default::default()
        };
        let ctx = ImportContext {
            what_it_is: Some("  A widget marketplace  ".to_string()),
            primary_goal: Some("First paying customer".to_string()),
            stage: Some("Launched".to_string()),
            operating_rules: Some("No marketing spend.".to_string()),
            notes: Some("Founder is solo.".to_string()),
            ..Default::default()
        };
        apply_operator_context(&mut draft, &ctx);
        assert_eq!(
            draft.setup.what_it_is.as_deref(),
            Some("A widget marketplace")
        );
        assert_eq!(
            draft.description.as_deref(),
            Some("A widget marketplace"),
            "a blank description takes the operator's one-liner"
        );
        assert_eq!(
            draft.setup.primary_goal.as_deref(),
            Some("First paying customer")
        );
        assert_eq!(
            draft.goals.as_deref(),
            Some("Whatever the README said"),
            "a goals field the analysis filled is left for review, not clobbered"
        );
        assert_eq!(
            draft.stage.as_deref(),
            Some("launched"),
            "stage is normalised"
        );
        let rules = draft.operating_rules.unwrap();
        assert!(
            rules.starts_with("No marketing spend."),
            "operator's rules lead: {rules}"
        );
        assert!(
            rules.contains("Use pnpm."),
            "found conventions are kept: {rules}"
        );
        assert_eq!(draft.notes.as_deref(), Some("Founder is solo."));

        let block = ctx.prompt_block();
        assert!(block.contains("authoritative"));
        assert!(block.contains("- Main goal: First paying customer"));
        assert!(ImportContext::default().prompt_block().is_empty());
        assert!(
            ImportContext {
                what_it_is: Some("   ".to_string()),
                ..Default::default()
            }
            .is_empty()
        );
    }

    #[test]
    fn analyze_request_accepts_context_in_camel_case() {
        let raw = r#"{"source":"https://github.com/acme/widget","context":{"whatItIs":"A widget","primaryGoal":"Ship","stage":"building"}}"#;
        let req: AnalyzeImportRequest = serde_json::from_str(raw).unwrap();
        let ctx = req.context.expect("context parsed");
        assert_eq!(ctx.what_it_is.as_deref(), Some("A widget"));
        assert_eq!(ctx.labelled().len(), 3);
    }

    #[test]
    fn classify_rejects_missing_paths_and_accepts_urls() {
        assert!(classify_source("/definitely/not/a/real/path/xyz").is_err());
        assert!(classify_source("   ").is_err());
        let (source, kind) = classify_source("https://github.com/acme/widget").unwrap();
        assert!(matches!(source, ImportSource::Url(_)));
        assert_eq!(kind, "github_repo");
        let (_, kind) = classify_source("https://acme.example.com").unwrap();
        assert_eq!(kind, "website");
    }
}
