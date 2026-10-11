//! Local Markdown mirror for user-visible data.
//!
//! SurrealDB remains the canonical store. This module writes an inspectable,
//! Obsidian-style folder tree so chats and captured content also exist as files
//! on the user's machine.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;

use crate::config::{AppConfig, expand_tilde_path};
use crate::state::{
    ActionEvent, ControlState, Entity, OperatorFeedEvent, RawCapture, RawCaptureStatus, Task,
};

const DEFAULT_VAULT_ROOT: &str = "~/.consistent-world/vault";
const WORKSPACE_KNOWLEDGE_DIRS: &[&str] = &[
    "raw",
    "working",
    "canonical",
    "narrative",
    "rules",
    "lessons",
    "skills",
    "audit",
];
const WORKSPACE_ACTIVITY_DIRS: &[&str] = &["actions", "missions", "recurring", "outputs", "runs"];
const ENTITY_KNOWLEDGE_DIRS: &[&str] = &[
    "raw",
    "working",
    "canonical",
    "narrative",
    "rules",
    "lessons",
    "skills",
    "audit",
];
const ENTITY_ACTIVITY_DIRS: &[&str] = &["actions", "missions", "recurring", "outputs", "runs"];

/// One role/content turn to append to a chat session's Markdown mirror.
#[derive(Debug, Clone)]
pub(crate) struct ChatTurn<'a> {
    pub workspace_id: &'a str,
    pub session_id: &'a str,
    pub role: &'a str,
    pub content: &'a str,
    pub entity: Option<&'a Entity>,
}

/// A heartbeat's director summary, mirrored as its own file per firing.
#[derive(Debug, Clone)]
pub(crate) struct ConsolidationSummary<'a> {
    pub workspace_id: &'a str,
    pub heartbeat_id: &'a str,
    pub director_id: &'a str,
    pub fired_at: &'a str,
    pub summary: &'a str,
    pub entity: Option<&'a Entity>,
}

/// One completed run of an external agent (e.g. the `claude` CLI), mirrored
/// under the entity's `runs/` folder.
#[derive(Debug, Clone)]
pub(crate) struct ExternalAgentRun<'a> {
    pub workspace_id: &'a str,
    pub run_id: &'a str,
    pub provider: &'a str,
    pub prompt: &'a str,
    pub output: &'a str,
    pub cwd: Option<&'a str>,
    pub status: &'a str,
    pub task_id: Option<&'a str>,
    pub started_at: &'a str,
    pub completed_at: &'a str,
    pub entity: &'a Entity,
}

pub(crate) fn vault_root() -> PathBuf {
    let root = std::env::var("CONSISTENT_WORLD_VAULT_ROOT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_VAULT_ROOT.to_string());
    PathBuf::from(expand_tilde_path(&root))
}

// The `mirror_*` wrappers below are fire-and-forget: the local vault is a
// convenience mirror, not the source of truth, so a write failure here is
// logged and swallowed rather than bubbled up to fail the caller's request.
pub(crate) async fn mirror_chat_turn(turn: ChatTurn<'_>) {
    if let Err(err) = mirror_chat_turn_inner(turn).await {
        tracing::warn!(error = %err, "local vault: failed to mirror chat turn");
    }
}

pub(crate) async fn mirror_task(task: &Task, entity: Option<&Entity>, workspace_id: &str) {
    if let Err(err) = mirror_task_inner(task, entity, workspace_id).await {
        tracing::warn!(task_id = %task.id, error = %err, "local vault: failed to mirror task");
    }
}

pub(crate) async fn mirror_action_event(
    event: &ActionEvent,
    entity: Option<&Entity>,
    workspace_id: &str,
) {
    if let Err(err) = mirror_action_event_inner(event, entity, workspace_id).await {
        tracing::warn!(event_id = %event.id, error = %err, "local vault: failed to mirror action event");
    }
}

pub(crate) async fn mirror_operator_feed_event(
    event: &OperatorFeedEvent,
    entity: Option<&Entity>,
    workspace_id: &str,
) {
    if let Err(err) = mirror_operator_feed_event_inner(event, entity, workspace_id).await {
        tracing::warn!(event_id = %event.id, error = %err, "local vault: failed to mirror operator feed event");
    }
}

/// Mirror a raw capture into the local vault. Entity captures land in the
/// entity's `raw/` folder; inbox-level captures land in the workspace
/// `inbox/raw/` folder. The file path is deterministic by capture id, so
/// dismiss/promote status changes overwrite the same file cleanly.
pub(crate) async fn mirror_raw_capture(capture: &RawCapture, entity: Option<&Entity>) {
    if let Err(err) = mirror_raw_capture_inner(capture, entity).await {
        tracing::warn!(capture_id = %capture.id, error = %err, "local vault: failed to mirror raw capture");
    }
}

/// Rebuild raw-capture mirror files from canonical control state. This is used
/// as a catch-up path on startup and periodic persistence so the local vault
/// converges even if a request-time mirror write was missed.
pub(crate) async fn mirror_raw_captures_from_state(state: &ControlState) {
    let captures = state.raw_captures.read().await.clone();
    if captures.is_empty() {
        return;
    }
    let entities = state.entities.read().await.clone();
    for capture in captures {
        let entity = capture
            .entity_id
            .as_deref()
            .and_then(|id| entities.iter().find(|entity| entity.id == id));
        mirror_raw_capture(&capture, entity).await;
    }
}

/// Append a reviewed narrative note into the entity's `narrative/` folder. The
/// narrative log is a single append-only file per entity so reviewed context
/// accumulates in reading order. Returns the file path written. Unlike the
/// best-effort mirror helpers, this is the primary side effect of a review
/// action, so failures are surfaced to the caller.
pub(crate) async fn append_entity_narrative_note(
    entity: &Entity,
    workspace_id: &str,
    note: &str,
    capture_id: &str,
) -> anyhow::Result<PathBuf> {
    ensure_workspace_layout_inner(workspace_id).await?;
    ensure_entity_folder_inner(entity, workspace_id).await?;
    let dir = entity_dir(workspace_id, entity).join("narrative");
    tokio::fs::create_dir_all(&dir).await?;
    let file = dir.join("reviewed-updates.md");
    let exists = tokio::fs::try_exists(&file).await.unwrap_or(false);
    let mut handle = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .await?;
    if !exists {
        handle
            .write_all(narrative_log_header(entity, workspace_id).as_bytes())
            .await?;
    }
    handle
        .write_all(narrative_log_entry(note, capture_id, Utc::now()).as_bytes())
        .await?;
    Ok(file)
}

/// Write a durable memory note under the entity's (or workspace inbox's)
/// `canonical/` or `lessons/` folder. Returns the file path written. This is a
/// primary review side effect, so failures are surfaced to the caller.
pub(crate) async fn write_memory_note(
    entity: Option<&Entity>,
    workspace_id: &str,
    kind: &str,
    title: &str,
    body: &str,
    capture: &RawCapture,
) -> anyhow::Result<PathBuf> {
    ensure_workspace_layout_inner(workspace_id).await?;
    let dir = match entity {
        Some(entity) => {
            ensure_entity_folder_inner(entity, workspace_id).await?;
            entity_dir(workspace_id, entity).join(kind)
        }
        None => workspace_root(workspace_id).join(kind),
    };
    tokio::fs::create_dir_all(&dir).await?;
    let filename = format!(
        "{}-{}.md",
        safe_date_prefix(&capture.created_at),
        safe_segment(title)
    );
    let path = unique_path(&dir, &filename).await;
    tokio::fs::write(
        &path,
        memory_note_markdown(entity, workspace_id, kind, title, body, capture),
    )
    .await?;
    Ok(path)
}

/// Create (or fill in) a workspace's folder tree, best-effort.
pub(crate) async fn ensure_workspace_layout(workspace_id: &str) {
    if let Err(err) = ensure_workspace_layout_inner(workspace_id).await {
        tracing::warn!(workspace_id, error = %err, "local vault: failed to create workspace layout");
    }
}

pub(crate) async fn mirror_runtime_environment(config_path: &str, cfg: &AppConfig) {
    if let Err(err) = mirror_runtime_environment_inner(config_path, cfg).await {
        tracing::warn!(error = %err, "local vault: failed to mirror runtime environment");
    }
}

pub(crate) async fn mirror_wallet_balance(workspace_id: &str, balance_cents: i64, markup: f64) {
    if let Err(err) = mirror_wallet_balance_inner(workspace_id, balance_cents, markup).await {
        tracing::warn!(workspace_id, error = %err, "local vault: failed to mirror wallet");
    }
}

pub(crate) async fn mirror_consolidation_summary(summary: ConsolidationSummary<'_>) {
    if let Err(err) = mirror_consolidation_summary_inner(summary).await {
        tracing::warn!(error = %err, "local vault: failed to mirror consolidation summary");
    }
}

pub(crate) async fn mirror_external_agent_run(run: ExternalAgentRun<'_>) {
    if let Err(err) = mirror_external_agent_run_inner(run).await {
        tracing::warn!(error = %err, "local vault: failed to mirror external agent run");
    }
}

/// Rebuild external-agent run mirror files from canonical control state. Run
/// files are deterministic by run id, so this is safe to call periodically.
pub(crate) async fn mirror_external_agent_runs_from_state(state: &ControlState) {
    let runs = state.external_agent_runs.read().await.clone();
    if runs.is_empty() {
        return;
    }
    let entities = state.entities.read().await.clone();
    for run in runs {
        let Some(entity) = entities.iter().find(|entity| entity.id == run.entity_id) else {
            continue;
        };
        mirror_external_agent_run(ExternalAgentRun {
            workspace_id: &run.workspace_id,
            run_id: &run.id,
            provider: &run.provider,
            prompt: &run.prompt,
            output: &run.output,
            cwd: Some(&run.cwd),
            status: &run.status,
            task_id: run.task_id.as_deref(),
            started_at: &run.started_at,
            completed_at: run.completed_at.as_deref().unwrap_or(""),
            entity,
        })
        .await;
    }
}

pub(crate) async fn ensure_entity_folder(entity: &Entity, workspace_id: &str) -> Option<PathBuf> {
    match ensure_entity_folder_inner(entity, workspace_id).await {
        Ok(path) => Some(path),
        Err(err) => {
            tracing::warn!(entity_id = %entity.id, error = %err, "local vault: failed to create entity folder");
            None
        }
    }
}

async fn ensure_workspace_layout_inner(workspace_id: &str) -> anyhow::Result<()> {
    let root = workspace_root(workspace_id);
    tokio::fs::create_dir_all(root.join("chats")).await?;
    tokio::fs::create_dir_all(root.join("entities")).await?;
    tokio::fs::create_dir_all(root.join("consolidations")).await?;
    tokio::fs::create_dir_all(root.join("environments")).await?;
    tokio::fs::create_dir_all(root.join("inbox").join("notes")).await?;
    tokio::fs::create_dir_all(root.join("inbox").join("tasks")).await?;
    tokio::fs::create_dir_all(root.join("inbox").join("reminders")).await?;
    tokio::fs::create_dir_all(root.join("wallet")).await?;
    ensure_named_dirs(&root, WORKSPACE_ACTIVITY_DIRS).await?;
    ensure_knowledge_layout(&root, WORKSPACE_KNOWLEDGE_DIRS).await?;
    let readme = root.join("README.md");
    if !tokio::fs::try_exists(&readme).await.unwrap_or(false) {
        tokio::fs::write(readme, workspace_readme_markdown(workspace_id)).await?;
    }
    Ok(())
}

async fn mirror_chat_turn_inner(turn: ChatTurn<'_>) -> anyhow::Result<()> {
    let now = Utc::now();
    let mut path = workspace_root(turn.workspace_id).join("chats");
    if let Some(entity) = turn.entity {
        path = entity_dir(turn.workspace_id, entity).join("chats");
    }
    tokio::fs::create_dir_all(&path).await?;
    let file = path.join(format!("{}.md", safe_segment(turn.session_id)));
    let exists = tokio::fs::try_exists(&file).await.unwrap_or(false);
    let mut handle = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&file)
        .await?;
    if !exists {
        handle
            .write_all(chat_header(turn.workspace_id, turn.session_id, turn.entity, now).as_bytes())
            .await?;
    }
    handle
        .write_all(chat_entry(turn.role, turn.content, now).as_bytes())
        .await?;
    Ok(())
}

async fn mirror_task_inner(
    task: &Task,
    entity: Option<&Entity>,
    workspace_id: &str,
) -> anyhow::Result<()> {
    let kind = task.task_kind.as_deref().unwrap_or("task");
    let dir_name = match kind {
        "note" => "notes",
        "reminder" => "reminders",
        "mission" | "mission-next-step" => "missions",
        _ => "tasks",
    };
    let dir = match entity {
        Some(entity) => entity_dir(workspace_id, entity).join(dir_name),
        None => workspace_root(workspace_id).join("inbox").join(dir_name),
    };
    tokio::fs::create_dir_all(&dir).await?;
    let filename = format!(
        "{}-{}.md",
        safe_date_prefix(&task.created_at),
        safe_segment(&task.title)
    );
    let path = unique_path(&dir, &filename).await;
    tokio::fs::write(path, task_markdown(task, entity, workspace_id)).await?;
    Ok(())
}

async fn mirror_action_event_inner(
    event: &ActionEvent,
    entity: Option<&Entity>,
    workspace_id: &str,
) -> anyhow::Result<()> {
    ensure_workspace_layout_inner(workspace_id).await?;
    let dir = match entity {
        Some(entity) => {
            ensure_entity_folder_inner(entity, workspace_id).await?;
            entity_dir(workspace_id, entity).join("actions")
        }
        None => workspace_root(workspace_id).join("actions"),
    };
    tokio::fs::create_dir_all(&dir).await?;
    let filename = format!(
        "{}-{}.md",
        safe_date_prefix(&event.timestamp),
        safe_segment(&event.id)
    );
    let path = unique_path(&dir, &filename).await;
    tokio::fs::write(path, action_event_markdown(event, entity, workspace_id)).await?;
    Ok(())
}

async fn mirror_operator_feed_event_inner(
    event: &OperatorFeedEvent,
    entity: Option<&Entity>,
    workspace_id: &str,
) -> anyhow::Result<()> {
    ensure_workspace_layout_inner(workspace_id).await?;
    let dir = match entity {
        Some(entity) => {
            ensure_entity_folder_inner(entity, workspace_id).await?;
            entity_dir(workspace_id, entity).join("audit")
        }
        None => workspace_root(workspace_id).join("audit"),
    };
    tokio::fs::create_dir_all(&dir).await?;
    let filename = format!(
        "{}-{}.md",
        safe_date_prefix(&event.timestamp),
        safe_segment(&event.id)
    );
    let path = unique_path(&dir, &filename).await;
    tokio::fs::write(
        path,
        operator_feed_event_markdown(event, entity, workspace_id),
    )
    .await?;
    Ok(())
}

async fn mirror_raw_capture_inner(
    capture: &RawCapture,
    entity: Option<&Entity>,
) -> anyhow::Result<()> {
    let workspace_id = &capture.workspace_id;
    ensure_workspace_layout_inner(workspace_id).await?;
    let dir = match entity {
        Some(entity) => {
            ensure_entity_folder_inner(entity, workspace_id).await?;
            entity_dir(workspace_id, entity).join("raw")
        }
        None => workspace_root(workspace_id).join("inbox").join("raw"),
    };
    tokio::fs::create_dir_all(&dir).await?;
    // Deterministic path keyed by capture id so status changes overwrite in place.
    let filename = format!(
        "{}-{}.md",
        safe_date_prefix(&capture.created_at),
        safe_segment(&capture.id)
    );
    tokio::fs::write(dir.join(filename), raw_capture_markdown(capture, entity)).await?;
    Ok(())
}

async fn mirror_runtime_environment_inner(
    config_path: &str,
    cfg: &AppConfig,
) -> anyhow::Result<()> {
    ensure_workspace_layout_inner("default").await?;
    let dir = workspace_root("default").join("environments");
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(dir.join("local.env"), environment_file(config_path, cfg)).await?;
    tokio::fs::write(
        dir.join("README.md"),
        environment_readme_markdown(config_path, cfg),
    )
    .await?;
    Ok(())
}

async fn mirror_wallet_balance_inner(
    workspace_id: &str,
    balance_cents: i64,
    markup: f64,
) -> anyhow::Result<()> {
    ensure_workspace_layout_inner(workspace_id).await?;
    let dir = workspace_root(workspace_id).join("wallet");
    tokio::fs::create_dir_all(&dir).await?;
    tokio::fs::write(
        dir.join("consistent-world-wallet.md"),
        wallet_markdown(workspace_id, balance_cents, markup),
    )
    .await?;
    Ok(())
}

async fn mirror_consolidation_summary_inner(
    summary: ConsolidationSummary<'_>,
) -> anyhow::Result<()> {
    ensure_workspace_layout_inner(summary.workspace_id).await?;
    let dir = match summary.entity {
        Some(entity) => entity_dir(summary.workspace_id, entity).join("consolidations"),
        None => workspace_root(summary.workspace_id).join("consolidations"),
    };
    tokio::fs::create_dir_all(&dir).await?;
    let filename = format!(
        "{}-{}.md",
        safe_date_prefix(summary.fired_at),
        safe_segment(summary.heartbeat_id)
    );
    let path = unique_path(&dir, &filename).await;
    tokio::fs::write(path, consolidation_markdown(summary)).await?;
    Ok(())
}

async fn mirror_external_agent_run_inner(run: ExternalAgentRun<'_>) -> anyhow::Result<()> {
    ensure_workspace_layout_inner(run.workspace_id).await?;
    ensure_entity_folder_inner(run.entity, run.workspace_id).await?;
    let dir = entity_dir(run.workspace_id, run.entity).join("runs");
    tokio::fs::create_dir_all(&dir).await?;
    let filename = format!(
        "{}-{}-{}.md",
        safe_date_prefix(run.started_at),
        safe_segment(run.provider),
        safe_segment(run.run_id)
    );
    tokio::fs::write(dir.join(filename), external_agent_run_markdown(run)).await?;
    Ok(())
}

async fn ensure_entity_folder_inner(
    entity: &Entity,
    workspace_id: &str,
) -> anyhow::Result<PathBuf> {
    let dir = entity_dir(workspace_id, entity);
    tokio::fs::create_dir_all(dir.join("notes")).await?;
    tokio::fs::create_dir_all(dir.join("tasks")).await?;
    tokio::fs::create_dir_all(dir.join("chats")).await?;
    tokio::fs::create_dir_all(dir.join("consolidations")).await?;
    ensure_named_dirs(&dir, ENTITY_ACTIVITY_DIRS).await?;
    ensure_knowledge_layout(&dir, ENTITY_KNOWLEDGE_DIRS).await?;
    let index = dir.join("README.md");
    if !tokio::fs::try_exists(&index).await.unwrap_or(false) {
        tokio::fs::write(&index, entity_index_markdown(entity, workspace_id)).await?;
    }
    let mut brief_entity = entity.clone();
    brief_entity.local_path = Some(dir.to_string_lossy().to_string());
    tokio::fs::write(
        dir.join("consistent_world.md"),
        entity_operating_brief_markdown(&brief_entity, workspace_id),
    )
    .await?;
    Ok(dir)
}

async fn ensure_named_dirs(root: &Path, dirs: &[&str]) -> anyhow::Result<()> {
    for dir in dirs {
        tokio::fs::create_dir_all(root.join(dir)).await?;
    }
    Ok(())
}

async fn ensure_knowledge_layout(root: &Path, dirs: &[&str]) -> anyhow::Result<()> {
    for dir in dirs {
        let path = root.join(dir);
        tokio::fs::create_dir_all(&path).await?;
        let index = path.join("README.md");
        if !tokio::fs::try_exists(&index).await.unwrap_or(false) {
            tokio::fs::write(index, knowledge_dir_readme(dir)).await?;
        }
    }
    Ok(())
}

// ---- Markdown templates ------------------------------------------------
// Everything below renders one record type into the file body written to
// disk. Each returns a String rather than writing directly, so the mirror
// functions above can decide the path and the write mode.

fn workspace_readme_markdown(workspace_id: &str) -> String {
    format!(
        "---\ntype: workspace\nworkspace: {}\ncreated_at: {}\n---\n\n# Consistent Workspace\n\nThis folder is the local Markdown mirror for workspace `{}`.\n\nSurrealDB remains the canonical database. These files exist so you can inspect chats, notes, tasks, reminders, heartbeat consolidations, wallet state, local runtime settings, and the knowledge promotion pipeline directly on disk.\n\n## Folders\n\n- `chats/` general chat transcripts\n- `entities/` entity/project folders with notes, tasks, entity-scoped chats, actions, and consolidations\n- `actions/` workspace-level action records\n- `missions/` workspace-level mission task records\n- `recurring/` recurring job notes and generated task traces\n- `outputs/` generated deliverables that do not belong in source projects\n- `consolidations/` workspace-level heartbeat consolidation summaries\n- `inbox/notes/` saved notes without an entity\n- `inbox/tasks/` saved tasks without an entity\n- `inbox/reminders/` saved reminders without an entity\n- `environments/` local runtime env files without secrets\n- `wallet/` Consistent wallet mirror\n\n## Knowledge Pipeline\n\n- `raw/` captured material before trust decisions\n- `working/` temporary plans, drafts, command output, and scratch context\n- `canonical/` durable workspace truths that can be promoted into model context\n- `narrative/` story-so-far summaries and strategic continuity\n- `rules/` standing instructions, constraints, and operating principles\n- `lessons/` reusable corrections and behavior learned from previous work\n- `skills/` repeatable procedures agents/directors can reuse\n- `audit/` evidence of actions, model choices, approvals, and verification\n\nPromotion should be deliberate: raw material can become a note, narrative update, rule, lesson, skill, or canonical reference only after classification or user/director approval.\n",
        yaml_escape(workspace_id),
        Utc::now().to_rfc3339(),
        workspace_id
    )
}

fn workspace_root(workspace_id: &str) -> PathBuf {
    vault_root()
        .join("workspaces")
        .join(safe_segment(non_empty(workspace_id, "default")))
}

/// Where this entity's notes live. Pure path arithmetic — it creates nothing,
/// so callers that only need to *report* the folder (e.g. the workspace status
/// endpoint) can use it without the side effects of `ensure_entity_folder`.
pub(crate) fn entity_dir(workspace_id: &str, entity: &Entity) -> PathBuf {
    workspace_root(workspace_id).join("entities").join(format!(
        "{}--{}",
        safe_segment(&entity.name),
        safe_segment(&entity.id)
    ))
}

fn environment_file(config_path: &str, cfg: &AppConfig) -> String {
    let expected_secret_vars = expected_secret_vars(cfg).join(",");
    format!(
        "# Consistent local runtime environment mirror\n# Generated by the gateway. Safe to inspect: secrets are intentionally not written here.\n# Source config: {}\n\nCONSISTENT_WORLD_CONFIG={}\nCONSISTENT_WORLD_VAULT_ROOT={}\nCONSISTENT_WORLD_BIND={}\nCONSISTENT_WORLD_PORT={}\nSURREAL_URL={}\nSURREAL_NS={}\nSURREAL_DB={}\nCONSISTENT_WORLD_MEMORY_BACKEND={}\nCONSISTENT_WORLD_LLM_PROVIDER={}\nCONSISTENT_WORLD_LLM_BASE_URL={}\nCONSISTENT_WORLD_LLM_API_KEY_ENV={}\nCONSISTENT_WORLD_DEFAULT_MODEL={}\nCONSISTENT_WORLD_EXPECTED_SECRET_VARS={}\nCONSISTENT_WORLD_CREDENTIALS_BACKEND={}\n",
        config_path,
        shell_escape(config_path),
        shell_escape(&vault_root().to_string_lossy()),
        shell_escape(&cfg.gateway_bind),
        cfg.gateway_port,
        shell_escape(&cfg.surreal_url),
        shell_escape(&cfg.surreal_ns),
        shell_escape(&cfg.surreal_db),
        shell_escape(&cfg.memory_backend),
        shell_escape(&cfg.llm_provider),
        shell_escape(&cfg.llm_base_url),
        shell_escape(&cfg.llm_api_key_env),
        shell_escape(&cfg.default_model),
        shell_escape(&expected_secret_vars),
        shell_escape(&cfg.credentials_backend)
    )
}

fn environment_readme_markdown(config_path: &str, cfg: &AppConfig) -> String {
    let mut providers = cfg
        .llm_providers_map
        .iter()
        .map(|(name, (base, env))| format!("- `{name}`: `{base}` via `{env}`"))
        .collect::<Vec<_>>();
    providers.sort();
    if providers.is_empty() {
        providers.push("- No named providers loaded.".to_string());
    }
    let mut models = cfg
        .llm_models_map
        .iter()
        .map(|(alias, (provider, model))| format!("- `{alias}` -> `{provider}` / `{model}`"))
        .collect::<Vec<_>>();
    models.sort();
    if models.is_empty() {
        models.push("- No model aliases loaded.".to_string());
    }
    format!(
        "---\ntype: environment\nworkspace: \"default\"\nupdated_at: {}\n---\n\n# Local Environment\n\n`local.env` mirrors the local runtime shape without storing secrets.\n\n- Config: `{}`\n- Database: `{}` / namespace `{}` / database `{}`\n- Credentials backend: `{}`\n- Default LLM: `{}` / `{}` / `{}`\n\n## Providers\n\n{}\n\n## Model Aliases\n\n{}\n\nAPI keys belong in the OS keyring, encrypted credentials vault, or your shell environment. This folder records which variables are expected, not their values.\n",
        Utc::now().to_rfc3339(),
        config_path,
        cfg.surreal_url,
        cfg.surreal_ns,
        cfg.surreal_db,
        cfg.credentials_backend,
        cfg.llm_provider,
        cfg.default_model,
        cfg.llm_api_key_env,
        providers.join("\n"),
        models.join("\n")
    )
}

fn chat_header(
    workspace_id: &str,
    session_id: &str,
    entity: Option<&Entity>,
    now: DateTime<Utc>,
) -> String {
    let entity_id = entity.map(|e| e.id.as_str()).unwrap_or("");
    let entity_name = entity.map(|e| e.name.as_str()).unwrap_or("");
    format!(
        "---\ntype: chat-session\nworkspace: {}\nsession_id: {}\nentity_id: {}\nentity_name: {}\npromotion_status: \"archived\"\nread_policy: \"searchable\"\ncanonical: false\ncreated_at: {}\n---\n\n# Chat {}\n\n",
        yaml_escape(workspace_id),
        yaml_escape(session_id),
        yaml_escape(entity_id),
        yaml_escape(entity_name),
        now.to_rfc3339(),
        session_id
    )
}

fn chat_entry(role: &str, content: &str, now: DateTime<Utc>) -> String {
    format!(
        "## {} · {}\n\n{}\n\n",
        role.trim(),
        now.to_rfc3339(),
        content.trim()
    )
}

fn wallet_markdown(workspace_id: &str, balance_cents: i64, markup: f64) -> String {
    let dollars = balance_cents as f64 / 100.0;
    format!(
        "---\ntype: consistent-world-wallet\nworkspace: {}\nupdated_at: {}\n---\n\n# Consistent Wallet\n\n- Balance: ${:.2}\n- Balance cents: {}\n- LLM markup: {:.2}x\n\nThis is a local mirror of the SurrealDB `credit_balances` row for this workspace.\n",
        yaml_escape(workspace_id),
        Utc::now().to_rfc3339(),
        dollars,
        balance_cents,
        markup
    )
}

fn task_markdown(task: &Task, entity: Option<&Entity>, workspace_id: &str) -> String {
    let kind = task.task_kind.as_deref().unwrap_or("task");
    let entity_id = entity.map(|e| e.id.as_str()).unwrap_or("");
    let entity_name = entity.map(|e| e.name.as_str()).unwrap_or("");
    let body = task.result.as_deref().unwrap_or("");
    format!(
        "---\ntype: {}\nid: {}\nworkspace: {}\nentity_id: {}\nentity_name: {}\nmission_id: {}\ndirector_id: {}\nagent_id: {}\npromotion_status: \"working\"\nread_policy: \"searchable\"\ncanonical: false\nstatus: {:?}\ncreated_at: {}\n---\n\n# {}\n\n{}\n",
        yaml_escape(kind),
        yaml_escape(&task.id),
        yaml_escape(workspace_id),
        yaml_escape(entity_id),
        yaml_escape(entity_name),
        yaml_escape(task.mission_id.as_deref().unwrap_or("")),
        yaml_escape(task.assigned_director_id.as_deref().unwrap_or("")),
        yaml_escape(task.assigned_agent_id.as_deref().unwrap_or("")),
        task.status,
        yaml_escape(&task.created_at),
        task.title.trim(),
        body.trim()
    )
}

fn action_event_markdown(
    event: &ActionEvent,
    entity: Option<&Entity>,
    workspace_id: &str,
) -> String {
    let entity_id = event
        .entity_id
        .as_deref()
        .or_else(|| entity.map(|e| e.id.as_str()))
        .unwrap_or("");
    let entity_name = entity.map(|e| e.name.as_str()).unwrap_or("");
    format!(
        "---\ntype: action-event\nid: {}\nworkspace: {}\nentity_id: {}\nentity_name: {}\ntask_id: {}\nagent_id: {}\npromotion_status: \"archived\"\nread_policy: \"archive_only\"\ncanonical: false\ncapability: {}\nexecution_level: {}\noutcome: {}\ntarget: {}\ncreated_at: {}\n---\n\n# Action {}\n\n- Capability: `{}`\n- Execution level: `{}`\n- Outcome: `{}`\n- Target: `{}`\n\n{}\n",
        yaml_escape(&event.id),
        yaml_escape(workspace_id),
        yaml_escape(entity_id),
        yaml_escape(entity_name),
        yaml_escape(event.task_id.as_deref().unwrap_or("")),
        yaml_escape(event.agent_id.as_deref().unwrap_or("")),
        yaml_escape(&debug_string(&event.capability)),
        yaml_escape(&debug_string(&event.execution_level)),
        yaml_escape(&debug_string(&event.outcome)),
        yaml_escape(event.target.as_deref().unwrap_or("")),
        yaml_escape(&event.timestamp),
        event.id,
        debug_string(&event.capability),
        debug_string(&event.execution_level),
        debug_string(&event.outcome),
        event.target.as_deref().unwrap_or(""),
        event.reason.as_deref().unwrap_or("").trim()
    )
}

fn operator_feed_event_markdown(
    event: &OperatorFeedEvent,
    entity: Option<&Entity>,
    workspace_id: &str,
) -> String {
    let entity_id = event
        .entity_id
        .as_deref()
        .or_else(|| entity.map(|e| e.id.as_str()))
        .unwrap_or("");
    let entity_name = entity.map(|e| e.name.as_str()).unwrap_or("");
    format!(
        "---\ntype: operator-feed-event\nid: {}\nworkspace: {}\nentity_id: {}\nentity_name: {}\ntask_id: {}\nmission_id: {}\npromotion_status: \"archived\"\nread_policy: \"archive_only\"\ncanonical: false\nkind: {}\ncreated_at: {}\n---\n\n# {}\n\n- Kind: `{}`\n- Task: `{}`\n- Mission: `{}`\n\n{}\n",
        yaml_escape(&event.id),
        yaml_escape(workspace_id),
        yaml_escape(entity_id),
        yaml_escape(entity_name),
        yaml_escape(event.task_id.as_deref().unwrap_or("")),
        yaml_escape(event.mission_id.as_deref().unwrap_or("")),
        yaml_escape(&debug_string(&event.kind)),
        yaml_escape(&event.timestamp),
        event.title.trim(),
        debug_string(&event.kind),
        event.task_id.as_deref().unwrap_or(""),
        event.mission_id.as_deref().unwrap_or(""),
        event.detail.as_deref().unwrap_or("").trim()
    )
}

fn raw_capture_status_str(status: RawCaptureStatus) -> &'static str {
    match status {
        RawCaptureStatus::Raw => "raw",
        RawCaptureStatus::Dismissed => "dismissed",
        RawCaptureStatus::PromotedToTask => "promoted_to_task",
        RawCaptureStatus::PromotedToMemory => "promoted_to_memory",
        RawCaptureStatus::NarrativeUpdated => "narrative_updated",
    }
}

fn raw_capture_markdown(capture: &RawCapture, entity: Option<&Entity>) -> String {
    let entity_id = capture
        .entity_id
        .as_deref()
        .or_else(|| entity.map(|e| e.id.as_str()))
        .unwrap_or("");
    let entity_name = entity.map(|e| e.name.as_str()).unwrap_or("");
    let status = raw_capture_status_str(capture.status);
    let summary = capture.summary.as_deref().map(str::trim).unwrap_or("");
    let summary_block = if summary.is_empty() {
        String::new()
    } else {
        format!("## Summary\n\n{summary}\n\n")
    };
    format!(
        "---\ntype: raw-capture\nid: {}\nworkspace: {}\nentity_id: {}\nentity_name: {}\nsource: {}\nsource_ref: {}\nsession_id: {}\nprovider: {}\nstatus: {}\npromoted_task_id: {}\npromotion_status: \"raw\"\nread_policy: \"searchable\"\ncanonical: false\ncreated_at: {}\nupdated_at: {}\n---\n\n# Raw Capture\n\n- Source: `{}`\n- Status: `{}`\n- Provider: `{}`\n- Promoted task: `{}`\n\n{}## Captured Text\n\n{}\n",
        yaml_escape(&capture.id),
        yaml_escape(&capture.workspace_id),
        yaml_escape(entity_id),
        yaml_escape(entity_name),
        yaml_escape(&capture.source),
        yaml_escape(capture.source_ref.as_deref().unwrap_or("")),
        yaml_escape(capture.session_id.as_deref().unwrap_or("")),
        yaml_escape(capture.provider.as_deref().unwrap_or("")),
        yaml_escape(status),
        yaml_escape(capture.promoted_task_id.as_deref().unwrap_or("")),
        yaml_escape(&capture.created_at),
        yaml_escape(&capture.updated_at),
        capture.source,
        status,
        capture.provider.as_deref().unwrap_or(""),
        capture.promoted_task_id.as_deref().unwrap_or(""),
        summary_block,
        capture.raw_text.trim()
    )
}

fn narrative_log_header(entity: &Entity, workspace_id: &str) -> String {
    format!(
        "---\ntype: entity-narrative-log\nworkspace: {}\nentity_id: {}\nentity_name: {}\npromotion_status: \"narrative\"\nread_policy: \"read_by_default\"\ncanonical: false\ncreated_at: {}\n---\n\n# Narrative Updates\n\nReviewed notes promoted from raw captures. Newest entries are appended below in reading order. These are compact, human-approved narrative updates — not raw material.\n\n",
        yaml_escape(workspace_id),
        yaml_escape(&entity.id),
        yaml_escape(&entity.name),
        Utc::now().to_rfc3339(),
    )
}

fn narrative_log_entry(note: &str, capture_id: &str, now: DateTime<Utc>) -> String {
    format!(
        "## {}\n\n{}\n\n_Source capture: `{}`_\n\n",
        now.to_rfc3339(),
        note.trim(),
        capture_id
    )
}

fn memory_note_markdown(
    entity: Option<&Entity>,
    workspace_id: &str,
    kind: &str,
    title: &str,
    body: &str,
    capture: &RawCapture,
) -> String {
    let entity_id = capture
        .entity_id
        .as_deref()
        .or_else(|| entity.map(|e| e.id.as_str()))
        .unwrap_or("");
    let entity_name = entity.map(|e| e.name.as_str()).unwrap_or("");
    let is_canonical = kind == "canonical";
    let (note_type, promotion_status, read_policy, canonical) = if is_canonical {
        ("canonical-note", "canonical", "read_by_default", "true")
    } else {
        ("lesson", "lessons", "searchable", "false")
    };
    format!(
        "---\ntype: {}\nworkspace: {}\nentity_id: {}\nentity_name: {}\nsource_capture_id: {}\npromotion_status: \"{}\"\nread_policy: \"{}\"\ncanonical: {}\ncreated_at: {}\n---\n\n# {}\n\n{}\n\n_Promoted from raw capture `{}` ({})._\n",
        note_type,
        yaml_escape(workspace_id),
        yaml_escape(entity_id),
        yaml_escape(entity_name),
        yaml_escape(&capture.id),
        promotion_status,
        read_policy,
        canonical,
        Utc::now().to_rfc3339(),
        title.trim(),
        body.trim(),
        capture.id,
        yaml_escape(&capture.source),
    )
}

fn consolidation_markdown(summary: ConsolidationSummary<'_>) -> String {
    let entity_id = summary.entity.map(|e| e.id.as_str()).unwrap_or("");
    let entity_name = summary.entity.map(|e| e.name.as_str()).unwrap_or("");
    format!(
        "---\ntype: heartbeat-consolidation\nworkspace: {}\nheartbeat_id: {}\ndirector_id: {}\nentity_id: {}\nentity_name: {}\nfired_at: {}\n---\n\n# Consolidation\n\n{}\n",
        yaml_escape(summary.workspace_id),
        yaml_escape(summary.heartbeat_id),
        yaml_escape(summary.director_id),
        yaml_escape(entity_id),
        yaml_escape(entity_name),
        yaml_escape(summary.fired_at),
        summary.summary.trim()
    )
}

fn external_agent_run_markdown(run: ExternalAgentRun<'_>) -> String {
    format!(
        "---\ntype: external-agent-run\nid: {}\nworkspace: {}\nentity_id: {}\nentity_name: {}\nprovider: {}\nstatus: {}\ntask_id: {}\ncwd: {}\nstarted_at: {}\ncompleted_at: {}\npromotion_status: \"working\"\nread_policy: \"searchable\"\ncanonical: false\n---\n\n# {} Run\n\n## Prompt\n\n{}\n\n## Output\n\n{}\n",
        yaml_escape(run.run_id),
        yaml_escape(run.workspace_id),
        yaml_escape(&run.entity.id),
        yaml_escape(&run.entity.name),
        yaml_escape(run.provider),
        yaml_escape(run.status),
        yaml_escape(run.task_id.unwrap_or("")),
        yaml_escape(run.cwd.unwrap_or("")),
        yaml_escape(run.started_at),
        yaml_escape(run.completed_at),
        run.provider,
        run.prompt.trim(),
        run.output.trim()
    )
}

fn entity_index_markdown(entity: &Entity, workspace_id: &str) -> String {
    format!(
        "---\ntype: entity\nid: {}\nworkspace: {}\npromotion_status: \"canonical\"\nread_policy: \"read_by_default\"\ncanonical: true\n---\n\n# {}\n\n{}\n\n## Project Operating Picture\n\nUse this entity folder as the project-local operating layer. Record the goal, narrative, stack, local roots, deployment targets, chats, actions, recurring jobs, and director handoffs here as human-readable files.\n\n## Knowledge Pipeline\n\nThis entity has the same promotion layout as the workspace:\n\n- `raw/` for untrusted captures and possible junk\n- `working/` for temporary execution material\n- `canonical/` for durable entity truth\n- `narrative/` for story-so-far and strategy continuity\n- `rules/` for entity-specific operating rules\n- `lessons/` for director/agent learnings scoped to this entity\n- `skills/` for reusable entity procedures\n- `audit/` for action evidence and verification\n\nActivity folders:\n\n- `actions/` for action records created by agents/tools\n- `missions/` for mission-scoped work records\n- `recurring/` for heartbeat and recurring-job traces\n- `outputs/` for generated deliverables\n\nKeep promotion deliberate. The agent loop should not treat raw files as truth until they are classified and promoted.\n",
        yaml_escape(&entity.id),
        yaml_escape(workspace_id),
        entity.name.trim(),
        entity.description.as_deref().unwrap_or("").trim()
    )
}

fn optional_line(label: &str, value: Option<&str>) -> String {
    let clean = value.map(str::trim).filter(|v| !v.is_empty()).unwrap_or("");
    format!("- {label}: {clean}\n")
}

fn entity_operating_brief_markdown(entity: &Entity, workspace_id: &str) -> String {
    let repo_path = entity.setup.repo_path.as_deref();
    let what_it_is = entity.setup.what_it_is.as_deref();
    let primary_goal = entity
        .setup
        .primary_goal
        .as_deref()
        .or(entity.goals.as_deref());
    let success_criteria = entity.setup.success_criteria.as_deref();
    let active_context = entity.active_context.as_deref();
    let operating_rules = entity.operating_rules.as_deref();
    format!(
        "---\ntype: entity-operating-brief\nid: {}\nworkspace: {}\ncanonical: false\nsource_of_truth: database\nupdated_at: {}\n---\n\n# {}\n\nThis is the local operating brief for the entity. SurrealDB remains the canonical structured source of truth; this file is the readable working layer for humans and agents.\n\n## Identity\n\n{}{}{}{}{}{}\n## Working Roots\n\n{}{}{}{}\n## Current Context\n\n{}\n\n## Operating Rules\n\n{}\n\n## Memory And Execution\n\n- Keep durable facts in canonical entity fields, mission notes, or promoted docs.\n- Store raw captures in `raw/` until reviewed.\n- Store working plans and command output in `working/`.\n- Store reusable corrections in `lessons/`.\n- Store evidence and approvals in `audit/`.\n- Generated deliverables belong in `outputs/` unless they are part of the source project.\n",
        yaml_escape(&entity.id),
        yaml_escape(workspace_id),
        Utc::now().to_rfc3339(),
        entity.name.trim(),
        optional_line("Type", entity.entity_type.as_deref()),
        optional_line("Description", entity.description.as_deref()),
        optional_line("What it is", what_it_is),
        optional_line("Primary goal", primary_goal),
        optional_line("Success criteria", success_criteria),
        optional_line("Status", Some(entity.status.as_str())),
        optional_line("Local path", entity.local_path.as_deref()),
        optional_line(
            "Default workspace root",
            entity.default_workspace_root.as_deref()
        ),
        optional_line("Repo path", repo_path),
        optional_line("Memory scope", entity.memory_scope.as_deref()),
        active_context.unwrap_or("No active context has been captured yet."),
        operating_rules.unwrap_or("No entity-specific operating rules have been captured yet.")
    )
}

fn knowledge_dir_readme(dir: &str) -> String {
    let (title, purpose, prompt_policy) = match dir {
        "raw" => (
            "Raw Capture",
            "Untrusted captured material, uploads, transcripts, imports, and possible junk.",
            "Do not inject by default. Classify first.",
        ),
        "working" => (
            "Working Memory",
            "Temporary plans, drafts, command output, and execution scratch space.",
            "Use only for the active operation or when explicitly referenced.",
        ),
        "canonical" => (
            "Canonical Truth",
            "Durable facts and references the AI may treat as trusted context after promotion.",
            "Eligible for prompt injection through controlled entity docs or mission notes.",
        ),
        "narrative" => (
            "Narrative",
            "Story-so-far summaries, strategic continuity, and current direction.",
            "Inject compact summaries, not every underlying raw event.",
        ),
        "rules" => (
            "Rules",
            "Standing instructions, constraints, preferences, and operating principles.",
            "High priority context when scoped to the active workspace/entity/director.",
        ),
        "lessons" => (
            "Lessons",
            "Reusable corrections and learned behavior from previous runs.",
            "Recall selectively by director, entity, mission, or task type.",
        ),
        "skills" => (
            "Skills",
            "Repeatable procedures that agents and directors can reuse.",
            "Load only the relevant skill for the current operation.",
        ),
        "audit" => (
            "Audit",
            "Evidence of actions, approvals, model choices, tool calls, and verification.",
            "Use for review and accountability, not as creative context by default.",
        ),
        _ => (
            "Knowledge Area",
            "Consistent knowledge pipeline area.",
            "Use according to the local promotion policy.",
        ),
    };

    format!(
        "---\ntype: knowledge-area\narea: {}\ncreated_at: {}\n---\n\n# {}\n\n{}\n\n## Prompt Policy\n\n{}\n",
        yaml_escape(dir),
        Utc::now().to_rfc3339(),
        title,
        purpose,
        prompt_policy
    )
}

async fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let mut candidate = dir.join(filename);
    if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
        return candidate;
    }
    let stem = filename.trim_end_matches(".md");
    for i in 2..1000 {
        candidate = dir.join(format!("{stem}-{i}.md"));
        if !tokio::fs::try_exists(&candidate).await.unwrap_or(false) {
            return candidate;
        }
    }
    dir.join(format!("{stem}-{}.md", Utc::now().timestamp_millis()))
}

fn safe_date_prefix(value: &str) -> String {
    value
        .get(0..10)
        .filter(|s| s.chars().all(|c| c.is_ascii_digit() || c == '-'))
        .unwrap_or("undated")
        .to_string()
}

// Collapse a title/id into a lowercase, hyphen-separated filename segment:
// runs of whitespace/punctuation become one dash, everything else is
// dropped, and the result is capped so filenames stay filesystem-friendly.
fn safe_segment(input: &str) -> String {
    let mut out = String::new();
    for ch in input.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == '_' {
            out.push(ch);
        } else if (ch.is_whitespace() || matches!(ch, '/' | '\\' | ':' | '.' | ','))
            && !out.ends_with('-')
        {
            out.push('-');
        }
        if out.len() >= 80 {
            break;
        }
    }
    let segment = out.trim_matches('-').to_string();
    if segment.is_empty() {
        "untitled".to_string()
    } else {
        segment
    }
}

// A JSON-quoted string is also a valid YAML scalar, so this doubles as a
// correct YAML frontmatter escape without a YAML-specific dependency.
fn yaml_escape(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

// Double-quoted shell escaping for values written into `local.env`.
fn shell_escape(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn debug_string<T: std::fmt::Debug>(value: &T) -> String {
    format!("{value:?}")
}

// Which env var names the local runtime expects a secret in — for the
// README only, so it never lists Ollama's placeholder (it takes no key).
fn expected_secret_vars(cfg: &AppConfig) -> Vec<String> {
    let mut vars = vec![cfg.llm_api_key_env.clone()];
    vars.extend(cfg.llm_providers_map.values().map(|(_, env)| env.clone()));
    vars.retain(|v| !v.trim().is_empty() && v != "OLLAMA_API_KEY");
    vars.sort();
    vars.dedup();
    vars
}

fn non_empty<'a>(value: &'a str, fallback: &'a str) -> &'a str {
    if value.trim().is_empty() {
        fallback
    } else {
        value.trim()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn knowledge_layout_creates_promotion_folders_with_indexes() {
        let root = std::env::temp_dir().join(format!(
            "consistent-world-local-vault-test-{}",
            uuid::Uuid::new_v4()
        ));

        ensure_named_dirs(&root, WORKSPACE_ACTIVITY_DIRS)
            .await
            .expect("create activity layout");
        ensure_knowledge_layout(&root, WORKSPACE_KNOWLEDGE_DIRS)
            .await
            .expect("create knowledge layout");

        for dir in WORKSPACE_ACTIVITY_DIRS {
            let path = root.join(dir);
            assert!(path.is_dir(), "missing activity dir: {dir}");
        }
        for dir in WORKSPACE_KNOWLEDGE_DIRS {
            let path = root.join(dir);
            assert!(path.is_dir(), "missing knowledge dir: {dir}");
            let readme = path.join("README.md");
            assert!(readme.is_file(), "missing readme for: {dir}");
            let content = tokio::fs::read_to_string(readme)
                .await
                .expect("read area readme");
            assert!(content.contains("## Prompt Policy"));
        }

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[test]
    fn workspace_readme_documents_knowledge_promotion_policy() {
        let readme = workspace_readme_markdown("default");

        assert!(readme.contains("## Knowledge Pipeline"));
        assert!(readme.contains("`raw/` captured material before trust decisions"));
        assert!(readme.contains("`actions/` workspace-level action records"));
        assert!(readme.contains("Promotion should be deliberate"));
    }

    #[test]
    fn entity_index_documents_raw_vs_canonical_boundary() {
        let entity = Entity {
            id: "ent_test".to_string(),
            name: "Test Entity".to_string(),
            description: Some("A test entity".to_string()),
            ..Entity::default()
        };

        let readme = entity_index_markdown(&entity, "default");

        assert!(readme.contains("`raw/` for untrusted captures and possible junk"));
        assert!(readme.contains("`canonical/` for durable entity truth"));
        assert!(readme.contains("should not treat raw files as truth"));
    }

    #[test]
    fn chat_header_defaults_to_searchable_archive_not_canonical() {
        let header = chat_header("default", "session-1", None, Utc::now());

        assert!(header.contains("promotion_status: \"archived\""));
        assert!(header.contains("read_policy: \"searchable\""));
        assert!(header.contains("canonical: false"));
    }

    #[test]
    fn raw_capture_markdown_is_searchable_and_carries_status() {
        let capture = RawCapture {
            id: "cap-1".to_string(),
            workspace_id: "default".to_string(),
            entity_id: None,
            source: "chat".to_string(),
            source_ref: None,
            session_id: Some("session-1".to_string()),
            provider: None,
            raw_text: "ship the capture loop".to_string(),
            summary: None,
            status: RawCaptureStatus::PromotedToTask,
            promoted_task_id: Some("task-9".to_string()),
            metadata: serde_json::Value::Null,
            created_at: "2026-07-02T00:00:00Z".to_string(),
            updated_at: "2026-07-02T00:00:00Z".to_string(),
        };

        let markdown = raw_capture_markdown(&capture, None);

        assert!(markdown.contains("type: raw-capture"));
        assert!(markdown.contains("read_policy: \"searchable\""));
        assert!(markdown.contains("status: \"promoted_to_task\""));
        assert!(markdown.contains("promoted_task_id: \"task-9\""));
        assert!(markdown.contains("ship the capture loop"));
    }

    #[test]
    fn action_event_markdown_defaults_to_archive_only() {
        let event = ActionEvent {
            id: "action-1".to_string(),
            timestamp: "2026-06-21T00:00:00Z".to_string(),
            scope_type: consistent_world_common::types::ScopeType::Entity,
            scope_id: "entity-1".to_string(),
            entity_id: Some("entity-1".to_string()),
            task_id: Some("task-1".to_string()),
            agent_id: Some("agent-1".to_string()),
            capability: consistent_world_common::capabilities::CapabilityId::MessageInternal,
            execution_level: consistent_world_common::capabilities::ExecutionLevel::Coordinate,
            target: Some("test.target".to_string()),
            outcome: crate::state::ActionOutcome::Allowed,
            reason: Some("ok".to_string()),
        };

        let markdown = action_event_markdown(&event, None, "default");

        assert!(markdown.contains("type: action-event"));
        assert!(markdown.contains("promotion_status: \"archived\""));
        assert!(markdown.contains("read_policy: \"archive_only\""));
        assert!(markdown.contains("canonical: false"));
        assert!(markdown.contains("test.target"));
    }
}
