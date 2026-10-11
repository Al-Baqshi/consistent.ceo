//! Observe a repo, folder or URL, draft an entity from it, let the operator
//! review the draft, then commit it.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Handler: kicks off analysis of a would-be entity import (repo/folder/URL),
/// returning a job to poll (see `crate::entity_import::start_analysis`).
pub(super) async fn analyze_entity_import(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::entity_import::AnalyzeImportRequest>,
) -> Result<Json<crate::entity_import::AnalyzeImportResponse>, StatusCode> {
    let control_state = state
        .control_state
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let cfg = state
        .app_config
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let ledger = state.credit_ledger.clone();
    let out = crate::entity_import::start_analysis(
        control_state,
        cfg,
        ledger,
        workspace.id,
        workspace.principal_id,
        body,
    )
    .await?;
    Ok(Json(out))
}

/// `GET` handler: polls the status of an entity-import analysis job.
pub(super) async fn get_entity_import_job(
    State(_state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(job_id): Path<String>,
) -> Result<Json<crate::entity_import::ImportJob>, StatusCode> {
    crate::entity_import::get_job(&workspace.id, &job_id)
        .await
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// Handler: commits a finished entity-import job into a real entity.
pub(super) async fn commit_entity_import(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::entity_import::CommitImportRequest>,
) -> Result<Json<crate::entity_import::CommitImportResponse>, StatusCode> {
    let s = control(&state)?;
    let out = crate::entity_import::commit_import(
        &state,
        s,
        &workspace.id,
        &workspace.principal_id,
        body,
    )
    .await?;
    Ok(Json(out))
}

/// Handler: fetches a source URL and either merges the extracted narrative
/// into an existing entity, or creates a new entity seeded from it.
pub(super) async fn ingest_entity_source(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<IngestEntitySourceRequest>,
) -> Result<Json<IngestEntitySourceResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let cfg = state
        .app_config
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let github_token = github_token_for_source_ingest(
        s,
        &workspace_id,
        body.entity_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty()),
    )
    .await;
    let context = fetch_entity_source_with_token(&body.url, github_token.as_deref()).await?;
    if context.text.trim().is_empty() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    let narrative = extract_source_narrative(
        &state,
        cfg,
        s,
        &workspace_id,
        &context,
        body.name.as_deref(),
    )
    .await;

    if let Some(entity_id) = body
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let mut entity = {
            let mut entities = s.entities.write().await;
            let entity = entities
                .iter_mut()
                .find(|e| {
                    e.id == entity_id
                        && is_entity_visible(
                            e,
                            &workspace_id,
                            Some(workspace.principal_id.as_str()),
                        )
                })
                .ok_or(StatusCode::NOT_FOUND)?;
            merge_source_into_entity(entity, &narrative, &context);
            entity.clone()
        };
        entity.local_path = crate::local_vault::ensure_entity_folder(&entity, &workspace_id)
            .await
            .map(|p| p.to_string_lossy().to_string());
        {
            let mut entities = s.entities.write().await;
            if let Some(stored) = entities.iter_mut().find(|e| e.id == entity.id) {
                stored.local_path = entity.local_path.clone();
                entity = stored.clone();
            }
        }
        s.operator_feed.write().await.push(OperatorFeedEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: Utc::now().to_rfc3339(),
            kind: OperatorFeedKind::EntitySourceImported,
            task_id: None,
            mission_id: None,
            entity_id: Some(entity.id.clone()),
            connection_id: None,
            title: format!("Source attached: {}", entity.name),
            detail: Some(format!("Imported {} as entity context.", context.url)),
            approval_id: None,
        });
        if let Some(source) = crate::entity_knowledge::IndexSource::from_hint(&context.url) {
            entity.knowledge_index = Some(
                crate::entity_knowledge::begin_index(
                    state.clone(),
                    &workspace_id,
                    &entity.id,
                    source,
                    None,
                )
                .await,
            );
        }
        return Ok(Json(IngestEntitySourceResponse {
            entity,
            created: false,
            attached: true,
            source_url: context.url,
            summary: narrative
                .summary
                .or(narrative.profile)
                .unwrap_or_else(|| "Source attached to existing entity.".to_string()),
        }));
    }

    let entity_name = non_empty_string(body.name)
        .or_else(|| non_empty_string(narrative.entity_name.clone()))
        .or_else(|| context.title.clone())
        .unwrap_or_else(|| source_title_from_url(&context.url));
    let setup = EntitySetup {
        what_it_is: non_empty_string(narrative.profile.clone())
            .or_else(|| non_empty_string(narrative.summary.clone())),
        primary_goal: non_empty_string(narrative.goals.clone()),
        success_criteria: non_empty_string(narrative.success_criteria.clone()),
        primary_domain: non_empty_string(narrative.primary_domain.clone()),
        primary_email: None,
        repo_path: non_empty_string(narrative.repo_path.clone()).or_else(|| {
            if context.kind == "github_repo" {
                Some(context.url.clone())
            } else {
                None
            }
        }),
        backend_stack: non_empty_string(narrative.backend_stack.clone()),
        frontend_stack: non_empty_string(narrative.frontend_stack.clone()),
        database_stack: non_empty_string(narrative.database_stack.clone()),
        ..EntitySetup::default()
    };
    let mut entity = internal_create_entity(
        &state,
        s,
        &workspace_id,
        CreateEntityRequest {
            name: entity_name,
            description: Some(source_brief(&narrative, &context)),
            department: Some("General".to_string()),
            owner: None,
            status: Some("Learning".to_string()),
            entity_type: non_empty_string(narrative.entity_type.clone()),
            default_workspace_root: None,
            stage: non_empty_string(narrative.active_context.clone()),
            setup: Some(setup),
        },
    )
    .await?;
    {
        let mut refreshed = entity.clone();
        merge_source_into_entity(&mut refreshed, &narrative, &context);
        refreshed.local_path = crate::local_vault::ensure_entity_folder(&refreshed, &workspace_id)
            .await
            .map(|p| p.to_string_lossy().to_string());
        let mut entities = s.entities.write().await;
        if let Some(stored) = entities.iter_mut().find(|e| e.id == entity.id) {
            *stored = refreshed.clone();
            entity = stored.clone();
        }
    }
    s.operator_feed.write().await.push(OperatorFeedEvent {
        id: Uuid::new_v4().to_string(),
        timestamp: Utc::now().to_rfc3339(),
        kind: OperatorFeedKind::EntitySourceImported,
        task_id: None,
        mission_id: None,
        entity_id: Some(entity.id.clone()),
        connection_id: None,
        title: format!("Entity created from source: {}", entity.name),
        detail: Some(format!(
            "Imported {} as the initial narrative source.",
            context.url
        )),
        approval_id: None,
    });
    if let Some(source) = crate::entity_knowledge::IndexSource::from_hint(&context.url) {
        entity.knowledge_index = Some(
            crate::entity_knowledge::begin_index(
                state.clone(),
                &workspace_id,
                &entity.id,
                source,
                None,
            )
            .await,
        );
    }
    Ok(Json(IngestEntitySourceResponse {
        entity,
        created: true,
        attached: false,
        source_url: context.url,
        summary: narrative
            .summary
            .or(narrative.profile)
            .unwrap_or_else(|| "Source imported into a new entity.".to_string()),
    }))
}

/// `GET` handler: fetches a single entity by id, scoped to the workspace.
pub(super) async fn get_entity(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<Entity>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entities = s.entities.read().await;
    entities
        .iter()
        .find(|e| {
            e.id == id && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        })
        .cloned()
        .map(Json)
        .ok_or(StatusCode::NOT_FOUND)
}

/// A narrative field from a `PATCH`: blank (empty or whitespace-only) clears
/// the field, anything else is stored as sent.
pub(super) fn non_empty_or_clear(value: String) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

/// `PATCH` handler: partial-updates an entity's fields, refusing identity
/// edits (name/type/status) on the fixed Personal Space entity. For
/// description/profile/goals/activeContext/operatingRules/notes, `""` clears
/// the field (`null` leaves it unchanged).
pub(super) async fn update_entity(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<UpdateEntityRequest>,
) -> Result<Json<Entity>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let mut entities = s.entities.write().await;
    let e = entities
        .iter_mut()
        .find(|e| {
            e.id == id && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        })
        .ok_or(StatusCode::NOT_FOUND)?;
    // The Personal Space is a fixed per-user primitive: cosmetic edits (notes,
    // description, colour) are fine, but it can't be renamed, re-typed, or
    // deactivated — the default-chat and routing logic depend on it existing as
    // an Active personal entity named "Personal".
    if e.is_personal && (body.name.is_some() || body.entity_type.is_some() || body.status.is_some())
    {
        return Err(StatusCode::FORBIDDEN);
    }
    if let Some(n) = body.name {
        e.name = n;
    }
    // Narrative fields: `null` means "unchanged" (the request fields are plain
    // `Option<String>`), so an empty string is how a client clears one.
    if let Some(d) = body.description {
        e.description = non_empty_or_clear(d);
    }
    if let Some(d) = body.department {
        e.department = Some(d.clone());
        e.dept = d;
    }
    if let Some(st) = body.status {
        e.status = st;
    }
    if let Some(t) = body.entity_type {
        e.entity_type = Some(t);
    }
    if let Some(stage) = body.stage {
        e.stage = Some(stage);
    }
    if let Some(goals) = body.goals {
        e.goals = non_empty_or_clear(goals);
    }
    if let Some(profile) = body.profile {
        e.profile = non_empty_or_clear(profile);
    }
    if let Some(active_context) = body.active_context {
        e.active_context = non_empty_or_clear(active_context);
    }
    if let Some(operating_rules) = body.operating_rules {
        e.operating_rules = non_empty_or_clear(operating_rules);
    }
    if let Some(notes) = body.notes {
        e.notes = non_empty_or_clear(notes);
    }
    if let Some(content_pipeline) = body.content_pipeline {
        e.content_pipeline = Some(content_pipeline);
    }
    if let Some(setup) = body.setup {
        e.setup = setup;
    }
    if let Some(rt) = body.entity_runtime {
        e.entity_runtime = Some(rt);
    }
    if let Some(tier) = body.preferred_model_tier {
        e.preferred_model_tier = Some(tier);
    }
    if let Some(lp) = body.local_path {
        e.local_path = lp;
    }
    if let Some(fd) = body.flow_diagrams {
        e.flow_diagrams = Some(fd);
    }
    refresh_entity_readiness(e);
    Ok(Json(e.clone()))
}

/// A configured value that is actually a directory on this machine.
///
/// An http(s) value is a website someone typed into a path field, and a path
/// that is not on disk is a promise. Both are rejected here so every caller
/// agrees on what "has a workspace" means.
pub(crate) fn existing_local_path(value: Option<&str>) -> Option<PathBuf> {
    let value = value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .filter(|v| !v.starts_with("http://") && !v.starts_with("https://"))?;
    let expanded = crate::config::expand_tilde_path(value);
    let path = PathBuf::from(expanded);
    path.exists().then_some(path)
}

/// Whether `path` is contained within any of `roots`.
pub(super) fn path_is_under(path: &StdPath, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| path.starts_with(root))
}

/// Picks and validates a coding-agent run's working directory: collects the
/// entity's known local roots (local path, workspace root, repo path,
/// falling back to the local vault folder), then confirms the requested cwd
/// (or the first root) is actually inside one of them.
pub(crate) async fn resolve_entity_agent_run_cwd(
    entity: &Entity,
    requested_cwd: Option<&str>,
) -> Result<(PathBuf, Vec<PathBuf>), StatusCode> {
    let mut roots = Vec::new();
    if let Some(path) = existing_local_path(entity.local_path.as_deref()) {
        roots.push(path);
    }
    if let Some(path) = existing_local_path(entity.default_workspace_root.as_deref()) {
        roots.push(path);
    }
    if let Some(path) = existing_local_path(entity.setup.repo_path.as_deref()) {
        roots.push(path);
    }
    if roots.is_empty()
        && let Some(vault_path) =
            crate::local_vault::ensure_entity_folder(entity, entity_workspace_id(entity)).await
    {
        roots.push(vault_path);
    }
    let roots = roots
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect::<Vec<_>>();
    if roots.is_empty() {
        return Err(StatusCode::FAILED_DEPENDENCY);
    }
    let cwd = if let Some(cwd) = requested_cwd.map(str::trim).filter(|v| !v.is_empty()) {
        let expanded = crate::config::expand_tilde_path(cwd);
        PathBuf::from(expanded)
            .canonicalize()
            .map_err(|_| StatusCode::BAD_REQUEST)?
    } else {
        roots[0].clone()
    };
    if !path_is_under(&cwd, &roots) {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok((cwd, roots))
}

// ---------------------------------------------------------------------------
// Entity workspace: "does this thing have a repo?"
// ---------------------------------------------------------------------------

/// Find a visible entity, cloned. The workspace endpoints read then write, so
/// they cannot hold the read guard across the answer.
pub(super) async fn visible_entity(
    s: &ControlState,
    id: &str,
    workspace: &WorkspaceCtx,
) -> Option<Entity> {
    s.entities
        .read()
        .await
        .iter()
        .find(|e| {
            e.id == id && is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
        })
        .cloned()
}

/// Builds the `{ ok: false, error }` JSON body used by the entity-workspace
/// endpoints' error responses.
pub(super) fn workspace_err(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "ok": false, "error": message.into() })),
    )
}

/// `GET /api/entities/{id}/workspace` — where does this entity's work live, and
/// has anybody actually said so?
pub(super) async fn get_entity_workspace(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<crate::entity_workspace::WorkspaceStatus>, StatusCode> {
    let s = control(&state)?;
    let entity = visible_entity(s, &id, &workspace)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(crate::entity_workspace::workspace_status(&entity)))
}

/// The answer, plus the state it produced.
/// Result of a workspace-setup decision (e.g. `git init`, or picking a
/// folder), alongside the workspace state it produced.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkspaceDecisionResponse {
    pub(super) ok: bool,
    pub(super) message: String,
    pub(super) git_initialized: bool,
    /// Set for choices this endpoint cannot finish alone — GitHub's device-code
    /// flow, which already exists and is driven by the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) next_endpoint: Option<String>,
    pub(super) status: crate::entity_workspace::WorkspaceStatus,
}

/// `POST /api/entities/{id}/workspace` — record what the operator chose.
///
/// Everything that can fail is decided by [`crate::entity_workspace::plan_decision`]
/// before a single directory is created, so a rejected answer changes nothing.
pub(super) async fn set_entity_workspace(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<crate::entity_workspace::WorkspaceDecision>,
) -> Result<Json<WorkspaceDecisionResponse>, (StatusCode, Json<serde_json::Value>)> {
    use crate::entity_workspace as ws;

    let s = control(&state).map_err(|status| workspace_err(status, "control state unavailable"))?;
    let entity = visible_entity(s, &id, &workspace)
        .await
        .ok_or_else(|| workspace_err(StatusCode::NOT_FOUND, "entity not found"))?;

    let plan = ws::plan_decision(&id, &body).map_err(|e| {
        let status = if e.is_client_error() {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::FAILED_DEPENDENCY
        };
        workspace_err(status, e.message())
    })?;

    // Side effects, in the order that makes a partial failure harmless: create
    // the directory, then git, then persist. Nothing is recorded about a folder
    // that could not be made.
    let mut git_initialized = false;
    if let Some(dir) = plan.ensure_dir.as_deref() {
        tokio::fs::create_dir_all(dir).await.map_err(|e| {
            workspace_err(
                StatusCode::FAILED_DEPENDENCY,
                ws::WorkspaceError::Create(e.to_string()).message(),
            )
        })?;
        if plan.git_init {
            ws::git_init(dir)
                .await
                .map_err(|e| workspace_err(StatusCode::FAILED_DEPENDENCY, e.message()))?;
            git_initialized = true;
        }
    }
    // "Notes only" is a real answer, so the folder it names has to exist.
    if plan.choice.as_deref() == Some(ws::CHOICE_VAULT_ONLY) {
        crate::local_vault::ensure_entity_folder(&entity, entity_workspace_id(&entity)).await;
    }

    let updated = {
        let mut entities = s.entities.write().await;
        let e = entities
            .iter_mut()
            .find(|e| e.id == id)
            .ok_or_else(|| workspace_err(StatusCode::NOT_FOUND, "entity not found"))?;
        if let Some(local) = plan.local_path.as_deref() {
            e.local_path = Some(local.to_string());
            // A checkout is a repo whether or not the operator said the word,
            // and directors read `repoPath` when deciding where code lives.
            let is_repo = plan.repo_path.is_some() || StdPath::new(local).join(".git").exists();
            if is_repo {
                e.setup.repo_path = Some(local.to_string());
            }
        } else if let Some(repo) = plan.repo_path.as_deref() {
            e.setup.repo_path = Some(repo.to_string());
        }
        if let Some(choice) = plan.choice.as_deref() {
            let settled = if choice == ws::CHOICE_LOCAL_FOLDER
                && e.setup.repo_path.as_deref() == plan.local_path.as_deref()
                && plan.local_path.is_some()
            {
                ws::CHOICE_REPO
            } else {
                choice
            };
            e.setup.workspace_choice = Some(settled.to_string());
            // Answered: the executor may ask again if this ever breaks.
            e.setup.workspace_asked_at = None;
        }
        refresh_entity_readiness(e);
        e.clone()
    };
    // Write-through so a restart before the periodic sync does not lose the
    // one answer this whole flow exists to collect.
    if let Err(e) = s.sync_entities_to_store().await {
        tracing::warn!(entity_id = %id, error = %e, "workspace decision write-through failed; periodic sync will retry");
    }

    Ok(Json(WorkspaceDecisionResponse {
        ok: true,
        message: plan.message,
        git_initialized,
        next_endpoint: plan.next_endpoint,
        status: ws::workspace_status(&updated),
    }))
}

/// One row per entity that is waiting on the question, for `/today`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PendingWorkspaceQuestion {
    pub(super) entity_id: String,
    pub(super) entity_name: String,
    pub(super) question: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) asked_at: Option<String>,
    pub(super) status: crate::entity_workspace::WorkspaceStatus,
}

/// `GET /api/entity-workspace/pending` — every entity that needs an answer.
///
/// Only entities that actually have queued work are listed: an archived idea
/// nobody is building does not need a directory, and asking about it would
/// bury the ones that do.
pub(super) async fn list_pending_entity_workspaces(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<Vec<PendingWorkspaceQuestion>>, StatusCode> {
    let s = control(&state)?;
    let entities = s.entities.read().await.clone();
    let waiting: std::collections::HashSet<String> = {
        let tasks = s.tasks.read().await;
        tasks
            .iter()
            .filter(|t| matches!(t.status, TaskStatus::Queued | TaskStatus::Backlog))
            .filter_map(|t| t.entity_id.clone())
            .collect()
    };
    let mut out = Vec::new();
    for entity in entities.iter().filter(|e| {
        is_entity_visible(e, &workspace.id, Some(workspace.principal_id.as_str()))
            && waiting.contains(&e.id)
    }) {
        let status = crate::entity_workspace::workspace_status(entity);
        if !status.needs_decision {
            continue;
        }
        out.push(PendingWorkspaceQuestion {
            entity_id: entity.id.clone(),
            entity_name: entity.name.clone(),
            question: status
                .question
                .clone()
                .unwrap_or_else(|| crate::entity_workspace::workspace_question(entity)),
            asked_at: status.asked_at.clone(),
            status,
        });
    }
    Ok(Json(out))
}

/// Converts a stored [`ExternalAgentRun`] to its wire [`EntityAgentRunResponse`].
pub(super) fn entity_agent_run_response(run: ExternalAgentRun) -> EntityAgentRunResponse {
    EntityAgentRunResponse {
        run_id: run.id,
        provider: run.provider,
        entity_id: run.entity_id,
        task_id: run.task_id,
        status: run.status,
        cwd: run.cwd,
        output: run.output,
        error: run.error,
        started_at: run.started_at,
        completed_at: run.completed_at,
        review_policy: run.review_policy,
        review_status: run.review_status,
        review_verdict: run.review_verdict,
        review_run_id: run.review_run_id,
        reviewed_run_id: run.reviewed_run_id,
    }
}

/// `GET` handler: fetches a single coding-agent run by id.
pub(super) async fn get_entity_agent_run(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path((entity_id, run_id)): Path<(String, String)>,
) -> Result<Json<EntityAgentRunResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    // The run's output can carry the entity's private context, so the caller
    // must be able to see the entity itself — which also keeps a teammate out
    // of someone else's Personal Space runs in a shared workspace.
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == entity_id
                && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let run = s
        .external_agent_runs
        .read()
        .await
        .iter()
        .find(|run| {
            run.id == run_id && run.entity_id == entity_id && run.workspace_id == workspace_id
        })
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(entity_agent_run_response(run)))
}

/// `POST` handler: launches a coding-agent run (Claude/OpenCode/Cursor)
/// jailed to the entity's resolved cwd. Creates a backing task, mirrors the
/// prompt into a capture, then spawns the run itself — Claude goes through
/// the streaming session runner (live output, resumable session id, killable
/// child), the other providers use the one-shot buffered [`CodingAgentTool`].
pub(super) async fn create_entity_agent_run(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(entity_id): Path<String>,
    Json(body): Json<CreateEntityAgentRunRequest>,
) -> Result<Json<EntityAgentRunResponse>, StatusCode> {
    let control_state = state
        .control_state
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let s = control_state.as_ref();
    let workspace_id = workspace.id;
    let entity = {
        let entities = s.entities.read().await;
        entities
            .iter()
            .find(|e| {
                e.id == entity_id
                    && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
            })
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };
    let kind = body.kind.trim().to_ascii_lowercase();
    if !matches!(kind.as_str(), "claude" | "opencode" | "cursor-agent") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let review_policy = crate::agent_review::ReviewPolicy::parse(body.review_policy.as_deref());
    // Reviewer agent kind for auto review; falls back to the run's own kind.
    let reviewer_provider = body
        .reviewer_provider
        .as_deref()
        .map(|v| v.trim().to_ascii_lowercase())
        .filter(|v| matches!(v.as_str(), "claude" | "opencode" | "cursor-agent"))
        .unwrap_or_else(|| kind.clone());
    let prompt = body.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let (cwd, allowed_roots) = resolve_entity_agent_run_cwd(&entity, body.cwd.as_deref()).await?;
    let run_id = Uuid::new_v4().to_string();
    let started_at = Utc::now().to_rfc3339();
    let title = body
        .title
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| prompt.lines().next().unwrap_or("External agent run"))
        .chars()
        .take(120)
        .collect::<String>();
    let mut task_id = None;
    if body.create_task.unwrap_or(true) {
        let task = internal_create_task(
            s,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
            CreateTaskRequest {
                title: title.clone(),
                dept: Some("Engineering".to_string()),
                roles: Some(vec!["External agent".to_string()]),
                entity_id: Some(entity.id.clone()),
                mission_id: None,
                priority: Some("medium".to_string()),
                requires_human_approval: Some(false),
                task_kind: Some("external-agent-run".to_string()),
                content: Some(prompt.clone()),
                director_id: Some("engineering".to_string()),
                agent_id: None,
                connection_id: None,
                external_source: Some(kind.clone()),
                external_id: Some(run_id.clone()),
                metadata: Some(serde_json::json!({
                    "cwd": cwd.to_string_lossy(),
                    "runId": run_id.clone(),
                    "provider": kind.clone()
                })),
                due_at: None,
            },
        )
        .await?;
        let created_task_id = task.id.clone();
        task_id = Some(created_task_id.clone());
        let mut tasks = s.tasks.write().await;
        if let Some(task) = tasks.iter_mut().find(|t| t.id == created_task_id) {
            crate::state::set_task_status_by_human(task, TaskStatus::InProgress);
            task.progress = 5;
        }
    }
    let cwd_string = cwd.to_string_lossy().to_string();
    let allowed_root_strings = allowed_roots
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    let run = ExternalAgentRun {
        id: run_id.clone(),
        workspace_id: workspace_id.clone(),
        entity_id: entity.id.clone(),
        provider: kind.clone(),
        prompt: prompt.clone(),
        cwd: cwd_string.clone(),
        status: "running".to_string(),
        task_id: task_id.clone(),
        output: String::new(),
        error: None,
        started_at: started_at.clone(),
        completed_at: None,
        metadata: serde_json::json!({
            "timeoutSecs": body.timeout_secs,
            "allowedRoots": allowed_root_strings,
        }),
        review_policy: Some(review_policy.as_str().to_string()),
        review_status: None,
        review_verdict: None,
        review_run_id: None,
        reviewed_run_id: None,
    };
    s.external_agent_runs.write().await.push(run.clone());
    crate::local_vault::mirror_external_agent_run(crate::local_vault::ExternalAgentRun {
        workspace_id: &workspace_id,
        run_id: &run_id,
        provider: &kind,
        prompt: &prompt,
        output: "",
        cwd: Some(&cwd_string),
        status: "running",
        task_id: task_id.as_deref(),
        started_at: &started_at,
        completed_at: "",
        entity: &entity,
    })
    .await;

    // Feed the external run prompt into the raw capture inbox before execution.
    // Best-effort: capture failure must not prevent the run from starting.
    let _ = internal_create_capture(
        s,
        &workspace_id,
        Some(workspace.principal_id.as_str()),
        CreateCaptureRequest {
            raw_text: prompt.clone(),
            entity_id: Some(entity.id.clone()),
            source: Some("external_agent".to_string()),
            source_ref: Some(run_id.clone()),
            session_id: None,
            provider: Some(kind.clone()),
            summary: Some(format!("{kind} run prompt")),
            metadata: Some(serde_json::json!({
                "runId": run_id.clone(),
                "kind": "prompt",
                "taskId": task_id.clone(),
            })),
        },
    )
    .await;

    let state_for_run = control_state.clone();
    let workspace_for_run = workspace_id.clone();
    // The viewer, so the output capture below can file into a Personal Space
    // (a `None` viewer never sees one, and the capture silently failed).
    let principal_for_run = workspace.principal_id.clone();
    let entity_for_run = entity.clone();
    let run_id_for_run = run_id.clone();
    let kind_for_run = kind.clone();
    let prompt_for_run = prompt.clone();
    let cwd_for_run = cwd_string.clone();
    let task_id_for_run = task_id.clone();
    let started_at_for_run = started_at.clone();
    let timeout_secs = body.timeout_secs;
    let allowed_roots_for_review = allowed_root_strings.clone();
    let title_for_feed = title.clone();
    let permission_mode =
        crate::agent_sessions::PermissionMode::parse(body.permission_mode.as_deref());
    let max_turns = body.max_turns;
    let resume_session_id = body.resume_session_id.clone();
    tokio::spawn(async move {
        // Claude runs go through the streaming session runner: live output, a
        // captured session id (so the work can be resumed), and a killable
        // child. The other CLIs keep the one-shot buffered path until they get
        // an equivalent stream format.
        let session_outcome = if kind_for_run == "claude" {
            Some(
                crate::agent_sessions::run_claude_session(
                    state_for_run.clone(),
                    crate::agent_sessions::SessionRequest {
                        run_id: run_id_for_run.clone(),
                        prompt: prompt_for_run.clone(),
                        cwd: cwd_for_run.clone(),
                        permission_mode,
                        max_turns,
                        resume_session_id,
                        stall_timeout_secs: timeout_secs
                            .unwrap_or(crate::agent_sessions::DEFAULT_STALL_TIMEOUT_SECS),
                        mcp_config_path: None,
                    },
                )
                .await,
            )
        } else {
            None
        };

        let (status, output, error, session_id, num_turns, cost_usd) = match session_outcome {
            Some(o) => (
                o.status,
                o.output,
                o.error,
                o.session_id,
                o.num_turns,
                o.cost_usd,
            ),
            None => {
                let tool = CodingAgentTool {
                    working_dir: cwd_for_run.clone(),
                    allowed_cwd_roots: allowed_root_strings,
                    ..CodingAgentTool::default()
                };
                let tool_result = tool
                    .execute(serde_json::json!({
                        "kind": kind_for_run.clone(),
                        "prompt": prompt_for_run.clone(),
                        "cwd": cwd_for_run.clone(),
                        "timeout_secs": timeout_secs
                    }))
                    .await;
                match tool_result {
                    Ok(result) if result.is_error => (
                        "failed".to_string(),
                        result.output,
                        Some("external agent returned a non-zero result".to_string()),
                        None,
                        None,
                        None,
                    ),
                    Ok(result) => (
                        "completed".to_string(),
                        result.output,
                        None,
                        None,
                        None,
                        None,
                    ),
                    Err(err) => (
                        "failed".to_string(),
                        String::new(),
                        Some(err.to_string()),
                        None,
                        None,
                        None,
                    ),
                }
            }
        };
        let completed_at = Utc::now().to_rfc3339();
        {
            let mut runs = state_for_run.external_agent_runs.write().await;
            if let Some(run) = runs.iter_mut().find(|run| run.id == run_id_for_run) {
                run.status = status.clone();
                run.output = output.clone();
                run.error = error.clone();
                run.completed_at = Some(completed_at.clone());
                if !run.metadata.is_object() {
                    run.metadata = serde_json::json!({});
                }
                if let Some(obj) = run.metadata.as_object_mut() {
                    if let Some(sid) = session_id.as_deref() {
                        obj.insert("sessionId".to_string(), serde_json::json!(sid));
                    }
                    if let Some(n) = num_turns {
                        obj.insert("numTurns".to_string(), serde_json::json!(n));
                    }
                    if let Some(c) = cost_usd {
                        obj.insert("costUsd".to_string(), serde_json::json!(c));
                    }
                    obj.insert(
                        "permissionMode".to_string(),
                        serde_json::json!(permission_mode.as_str()),
                    );
                }
            }
        }

        // Put the run's ending on the operator feed, which is what carries it
        // to the phone and the desktop notification centre.
        state_for_run
            .push_operator_feed_surface(
                if status == "completed" {
                    crate::state::OperatorFeedKind::AgentRunCompleted
                } else {
                    crate::state::OperatorFeedKind::AgentRunFailed
                },
                title_for_feed.clone(),
                Some(match (&error, output.trim()) {
                    (Some(e), _) => e.clone(),
                    (None, "") => "Finished with no output.".to_string(),
                    (None, text) => text.chars().take(300).collect(),
                }),
                Some(entity_for_run.id.clone()),
                None,
            )
            .await;
        if let Some(ref task_id) = task_id_for_run {
            let task_for_vault = {
                let mut tasks = state_for_run.tasks.write().await;
                tasks
                    .iter_mut()
                    .find(|task| task.id == *task_id)
                    .map(|task| {
                        // Routed through the guarded setter for two reasons: a
                        // task the operator cancelled while this run was in
                        // flight must stay cancelled, and the direct assignment
                        // this replaced skipped `completed_at` — the stamp the
                        // streak and per-day counts are computed from.
                        let next = if status == "completed" {
                            TaskStatus::Completed
                        } else {
                            TaskStatus::Failed
                        };
                        if crate::state::set_task_status_by_loop(task, next) {
                            task.progress = if status == "completed" { 100 } else { 0 };
                        }
                        task.result = Some(if output.trim().is_empty() {
                            error
                                .clone()
                                .unwrap_or_else(|| "No output returned.".to_string())
                        } else {
                            output.clone()
                        });
                        task.clone()
                    })
            };
            if let Some(task) = task_for_vault {
                crate::local_vault::mirror_task(&task, Some(&entity_for_run), &workspace_for_run)
                    .await;
            }
        }
        crate::local_vault::mirror_external_agent_run(crate::local_vault::ExternalAgentRun {
            workspace_id: &workspace_for_run,
            run_id: &run_id_for_run,
            provider: &kind_for_run,
            prompt: &prompt_for_run,
            output: &output,
            cwd: Some(&cwd_for_run),
            status: &status,
            task_id: task_id_for_run.as_deref(),
            started_at: &started_at_for_run,
            completed_at: &completed_at,
            entity: &entity_for_run,
        })
        .await;
        if !output.trim().is_empty() {
            let _ = internal_create_capture(
                state_for_run.as_ref(),
                &workspace_for_run,
                Some(principal_for_run.as_str()),
                CreateCaptureRequest {
                    raw_text: output.clone(),
                    entity_id: Some(entity_for_run.id.clone()),
                    source: Some("external_agent".to_string()),
                    source_ref: Some(run_id_for_run.clone()),
                    session_id: None,
                    provider: Some(kind_for_run.clone()),
                    summary: Some(format!("{kind_for_run} run output ({status})")),
                    metadata: Some(serde_json::json!({
                        "runId": run_id_for_run,
                        "kind": "output",
                        "taskId": task_id_for_run,
                        "status": status,
                    })),
                },
            )
            .await;
        }

        // Agent review loop: after the run completes, apply the review policy.
        match review_policy {
            crate::agent_review::ReviewPolicy::Auto if status == "completed" => {
                if let Some(reviewer) = crate::agent_review::start_review(
                    state_for_run.as_ref(),
                    &run_id_for_run,
                    &reviewer_provider,
                )
                .await
                {
                    // Execute the reviewer agent, then attach its verdict.
                    let review_state = state_for_run.clone();
                    let review_id = reviewer.id.clone();
                    let review_kind = reviewer.provider.clone();
                    let review_prompt_text = reviewer.prompt.clone();
                    let review_cwd = reviewer.cwd.clone();
                    tokio::spawn(async move {
                        let tool = CodingAgentTool {
                            working_dir: review_cwd.clone(),
                            allowed_cwd_roots: allowed_roots_for_review,
                            ..CodingAgentTool::default()
                        };
                        let result = tool
                            .execute(serde_json::json!({
                                "kind": review_kind,
                                "prompt": review_prompt_text,
                                "cwd": review_cwd,
                                "timeout_secs": timeout_secs
                            }))
                            .await;
                        let (rstatus, routput) = match result {
                            Ok(r) if r.is_error => ("failed", r.output),
                            Ok(r) => ("completed", r.output),
                            Err(e) => ("failed", e.to_string()),
                        };
                        crate::agent_review::finish_review(
                            review_state.as_ref(),
                            &review_id,
                            &routput,
                            rstatus,
                        )
                        .await;
                    });
                }
            }
            crate::agent_review::ReviewPolicy::Manual if status == "completed" => {
                crate::agent_review::mark_pending_review(state_for_run.as_ref(), &run_id_for_run)
                    .await;
            }
            _ => {}
        }
    });

    Ok(Json(entity_agent_run_response(run)))
}

/// Handler: deletes an entity and everything in-memory that references it
/// (tasks, missions, approvals, feed, connections), refusing to delete a
/// Personal Space. See [`cascade_entity_cloud_delete`] for the store-side pass.
pub(super) async fn delete_entity(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;

    {
        let entities = s.entities.read().await;
        let Some(entity) = entities.iter().find(|e| {
            e.id == id && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) else {
            return Err(StatusCode::NOT_FOUND);
        };
        // A user's Personal Space is a permanent primitive — never deletable.
        if entity.is_personal {
            return Err(StatusCode::FORBIDDEN);
        }
    }

    s.entities.write().await.retain(|e| e.id != id);
    // Keep the ids of what is removed here: the store-side cascade tombstones
    // them, so a copy some loop cloned a moment ago (the persist loop's task
    // Vec, the orchestrator's mission sync) cannot write them back. Each guard
    // is taken and released on its own (lock-order rule 2 on `ControlState`).
    let removed_task_ids: Vec<String> = {
        let mut tasks = s.tasks.write().await;
        let ids = tasks
            .iter()
            .filter(|t| task_references_entity(t, &id))
            .map(|t| t.id.clone())
            .collect();
        tasks.retain(|t| !task_references_entity(t, &id));
        ids
    };
    let removed_mission_ids: Vec<String> = {
        let mut missions = s.missions.write().await;
        let ids = missions
            .iter()
            .filter(|m| mission_references_entity(m, &id))
            .map(|m| m.id.clone())
            .collect();
        missions.retain(|m| !mission_references_entity(m, &id));
        ids
    };
    s.approvals
        .write()
        .await
        .retain(|a| !approval_references_entity(a, &id));
    s.operator_feed
        .write()
        .await
        .retain(|e| !feed_references_entity(e, &id));
    s.entity_connections
        .write()
        .await
        .retain(|c| !connection_references_entity(c, &id));
    s.entity_connection_events
        .write()
        .await
        .retain(|e| !connection_event_references_entity(e, &id));
    s.entity_connection_sessions
        .write()
        .await
        .retain(|session| !connection_session_references_entity(session, &id));
    s.entity_files.write().await.retain(|f| f.entity_id != id);

    cascade_entity_cloud_delete(&state, s, &id, &removed_task_ids, &removed_mission_ids).await;

    Ok(StatusCode::NO_CONTENT)
}

/// Best-effort removal of everything in SurrealDB that references a deleted
/// entity. The in-memory removal in [`delete_entity`] is what the running
/// process sees; without this pass, orphaned heartbeats keep firing (the
/// dispatcher synthesises a placeholder entity named after the raw id) and
/// merge-hydrating stores resurrect the records on the next boot. Failures
/// are logged, not surfaced — the entity is already gone from live state, so
/// a half-clean cascade must not turn the 204 into an error.
///
/// Deliberately untouched: the local vault folder and uploaded files on disk.
/// The database is canonical; local files stay local.
///
/// `removed_task_ids` / `removed_mission_ids` are the in-memory rows
/// [`delete_entity`] just dropped. They are deleted (and so tombstoned) in
/// their stores alongside whatever the stores themselves list for the entity,
/// which covers rows that never reached the table but sit in a loop's stale
/// clone waiting to be synced.
pub(super) async fn cascade_entity_cloud_delete(
    state: &ServerState,
    s: &crate::state::ControlState,
    id: &str,
    removed_task_ids: &[String],
    removed_mission_ids: &[String],
) {
    // Entity row: syncs no longer infer deletes from a missing id, so delete
    // (and tombstone) it explicitly. Doing it now closes the window where a
    // crash would resurrect the entity from Surreal on the next boot.
    if let Some(store) = s.entity_store.get()
        && let Err(e) = store.delete(id).await
    {
        warn!(entity_id = %id, error = %e, "entity delete: entity row delete failed; the next sync retries it");
    }

    // Heartbeats targeting the entity — orphans would keep firing forever.
    if let Some(store) = state.heartbeat_store.as_ref() {
        use crate::heartbeat_store::HeartbeatStore;
        match store.list_all().await {
            Ok(beats) => {
                for hb in beats
                    .iter()
                    .filter(|hb| hb.target_kind == "entity" && hb.target_id.as_deref() == Some(id))
                {
                    if let Err(e) = store.delete(&hb.id).await {
                        warn!(entity_id = %id, heartbeat_id = %hb.id, error = %e, "entity delete: heartbeat delete failed");
                    }
                }
            }
            Err(e) => warn!(entity_id = %id, error = %e, "entity delete: heartbeat list failed"),
        }
    }

    // The beat result log the chat UI surfaces.
    if let Some(store) = state.heartbeat_results_store.as_ref()
        && let Err(e) = store.delete_for_entity(id).await
    {
        warn!(entity_id = %id, error = %e, "entity delete: heartbeat results delete failed");
    }

    // Task rows merge-hydrate at boot, so store-side rows must go too — not
    // just the in-memory ones retained away above. Deleting tombstones the id,
    // which is what stops the persist loop's clone from writing it back.
    if let Some(store) = s.task_store.get() {
        let mut task_ids: std::collections::BTreeSet<String> =
            removed_task_ids.iter().cloned().collect();
        match store.list_all().await {
            Ok(tasks) => task_ids.extend(
                tasks
                    .into_iter()
                    .filter(|t| task_references_entity(t, id))
                    .map(|t| t.id),
            ),
            Err(e) => warn!(entity_id = %id, error = %e, "entity delete: task list failed"),
        }
        for task_id in &task_ids {
            if let Err(e) = store.delete(task_id).await {
                warn!(entity_id = %id, task_id = %task_id, error = %e, "entity delete: task row delete failed");
            }
        }
    }

    // Missions: the same, through the mission store (one instance shared by
    // `ServerState` and `ControlState`, so its tombstones cover both paths).
    let mission_store = state
        .mission_store
        .as_ref()
        .or_else(|| s.mission_store.get());
    if let Some(store) = mission_store {
        let mut mission_ids: std::collections::BTreeSet<String> =
            removed_mission_ids.iter().cloned().collect();
        match store.list_by_entity(id).await {
            Ok(missions) => mission_ids.extend(missions.into_iter().map(|m| m.id)),
            Err(e) => warn!(entity_id = %id, error = %e, "entity delete: mission list failed"),
        }
        for mission_id in &mission_ids {
            if let Err(e) = store.delete(mission_id).await {
                warn!(entity_id = %id, mission_id = %mission_id, error = %e, "entity delete: mission delete failed");
            }
        }
    }

    // Canonical entity docs (the prompt-assembly layer).
    if let Some(store) = state.entity_docs_store.as_ref() {
        match store.list_for_entity(id).await {
            Ok(docs) => {
                for d in &docs {
                    if let Err(e) = store.delete(&d.id).await {
                        warn!(entity_id = %id, doc_id = %d.id, error = %e, "entity delete: entity doc delete failed");
                    }
                }
            }
            Err(e) => warn!(entity_id = %id, error = %e, "entity delete: entity doc list failed"),
        }
    }

    // Canvas nodes and edges.
    if let Some(store) = state.canvas_store.as_ref()
        && let Err(e) = store.delete_for_entity(id).await
    {
        warn!(entity_id = %id, error = %e, "entity delete: canvas delete failed");
    }

    // Feed + action log rows (mirrors the in-memory feed removal).
    if let Some(store) = s.feed_store.get()
        && let Err(e) = store.delete_for_entity(id).await
    {
        warn!(entity_id = %id, error = %e, "entity delete: feed rows delete failed");
    }

    // Scheduled tasks / reminders bound to the entity.
    if let Some(scheduler) = state.scheduler.as_ref() {
        match scheduler.list_tasks().await {
            Ok(tasks) => {
                for t in tasks.iter().filter(|t| t.entity_id.as_deref() == Some(id)) {
                    if let Err(e) = scheduler.remove_task(&t.id).await {
                        warn!(entity_id = %id, scheduled_task_id = %t.id, error = %e, "entity delete: scheduled task delete failed");
                    }
                }
            }
            Err(e) => {
                warn!(entity_id = %id, error = %e, "entity delete: scheduled task list failed")
            }
        }
    }

    // Short/long entity memory namespaces.
    if let Err(e) = crate::entity_memory::forget_entity(state.memory.clone(), id).await {
        warn!(entity_id = %id, error = %e, "entity delete: memory namespace delete failed");
    }
}

/// `GET` handler: resolves an entity's runtime execution context.
pub(super) async fn get_entity_runtime_context(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<EntityRuntimeContext>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entities = s.entities.read().await;
    let entity = entities
        .iter()
        .find(|e| {
            e.id == id && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        })
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(resolve_entity_runtime_context(entity)))
}

/// Response for the entity memory preview endpoint.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct EntityMemoryPreviewResponse {
    pub(super) short_term: Vec<String>,
    pub(super) long_term: Vec<String>,
}

/// Recent entity-scoped memory lines (for UX — same store as planner/orchestrator).
pub(super) async fn get_entity_memory_preview(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<EntityMemoryPreviewResponse>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    {
        let entities = s.entities.read().await;
        if !entities.iter().any(|e| {
            e.id == id && is_entity_visible(e, &workspace_id, Some(workspace.principal_id.as_str()))
        }) {
            return Err(StatusCode::NOT_FOUND);
        }
    }
    let short_term = crate::entity_memory::recall_entity_short(state.memory.clone(), &id, 20).await;
    let long_term = crate::entity_memory::recall_entity_long(state.memory.clone(), &id, 12).await;
    Ok(Json(EntityMemoryPreviewResponse {
        short_term,
        long_term,
    }))
}

/// Handler: applies a `ready` enrichment draft's fields (all, or the
/// requested subset) onto the entity, optionally guarded by `draft_version`
/// to reject a stale apply.
pub(super) async fn apply_enrichment(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<ApplyEnrichmentRequest>,
) -> Result<Json<Entity>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let mut entities = s.entities.write().await;
    let e = entities
        .iter_mut()
        .find(|x| {
            x.id == id && is_entity_visible(x, &workspace_id, Some(workspace.principal_id.as_str()))
        })
        .ok_or(StatusCode::NOT_FOUND)?;
    let enrichment = e.enrichment.as_mut().ok_or(StatusCode::BAD_REQUEST)?;
    if enrichment.status != "ready" {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(version) = body.draft_version
        && enrichment.draft_version != Some(version)
    {
        return Err(StatusCode::CONFLICT);
    }
    let draft = enrichment.draft.as_ref().ok_or(StatusCode::BAD_REQUEST)?;
    let fields_to_apply: Vec<&str> = if body.fields.is_empty() {
        vec![
            "profile",
            "goals",
            "active_context",
            "operating_rules",
            "notes",
            "content_pipeline",
        ]
    } else {
        body.fields.iter().map(String::as_str).collect()
    };
    // A field the draft left empty is "no suggestion", not "clear it": applying
    // all fields used to wipe whatever the user had written there.
    fn take(dst: &mut Option<String>, suggestion: &Option<String>) {
        if let Some(v) = suggestion.as_deref().filter(|v| !v.trim().is_empty()) {
            *dst = Some(v.to_string());
        }
    }
    for f in fields_to_apply {
        match f {
            "profile" => take(&mut e.profile, &draft.profile),
            "goals" => take(&mut e.goals, &draft.goals),
            "activeContext" | "active_context" => {
                take(&mut e.active_context, &draft.active_context)
            }
            "operatingRules" | "operating_rules" => {
                take(&mut e.operating_rules, &draft.operating_rules)
            }
            "contentPipeline" | "content_pipeline" => {
                take(&mut e.content_pipeline, &draft.content_pipeline)
            }
            "notes" => take(&mut e.notes, &draft.notes),
            _ => {}
        }
    }
    enrichment.status = "applied".to_string();
    Ok(Json(e.clone()))
}

/// Handler: marks an entity's pending enrichment draft as discarded.
pub(super) async fn discard_enrichment(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<Entity>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let mut entities = s.entities.write().await;
    let e = entities
        .iter_mut()
        .find(|x| {
            x.id == id && is_entity_visible(x, &workspace_id, Some(workspace.principal_id.as_str()))
        })
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(ref mut enrichment) = e.enrichment {
        enrichment.status = "discarded".to_string();
    }
    Ok(Json(e.clone()))
}

/// Handler: re-queues entity enrichment, or marks it `ai_unavailable` when
/// the LLM config hasn't finished loading yet.
pub(super) async fn regenerate_enrichment(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> Result<Json<Entity>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    if state.app_config.is_none() {
        let mut entities = s.entities.write().await;
        let e = entities
            .iter_mut()
            .find(|x| {
                x.id == id
                    && is_entity_visible(x, &workspace_id, Some(workspace.principal_id.as_str()))
            })
            .ok_or(StatusCode::NOT_FOUND)?;
        let msg = "LLM settings are not loaded yet. Try again after the gateway finishes loading configuration, or open Settings.";
        if let Some(ref mut enrichment) = e.enrichment {
            enrichment.status = "ai_unavailable".to_string();
            enrichment.error = Some(msg.to_string());
        } else {
            let snapshot = EnrichmentInputSnapshot {
                name: Some(e.name.clone()),
                description: e.description.clone(),
                department: e.department.clone(),
                owner: e.owner.clone(),
            };
            e.enrichment = Some(EntityEnrichment {
                status: "ai_unavailable".to_string(),
                error: Some(msg.to_string()),
                input_snapshot: Some(snapshot),
                draft: None,
                generated_by: None,
                model: None,
                generated_at: None,
                draft_version: Some(1),
            });
        }
        return Ok(Json(e.clone()));
    }
    let cfg = state.app_config.as_ref().unwrap();
    if !crate::config::llm_ready_for_entity_enrichment(cfg.as_ref()) {
        let mut entities = s.entities.write().await;
        let e = entities
            .iter_mut()
            .find(|x| {
                x.id == id
                    && is_entity_visible(x, &workspace_id, Some(workspace.principal_id.as_str()))
            })
            .ok_or(StatusCode::NOT_FOUND)?;
        let msg = "LLM is not configured for enrichment. Add your provider API key (or a keyless local provider) in Settings, then retry.";
        if let Some(ref mut enrichment) = e.enrichment {
            enrichment.status = "ai_unavailable".to_string();
            enrichment.error = Some(msg.to_string());
        } else {
            let snapshot = EnrichmentInputSnapshot {
                name: Some(e.name.clone()),
                description: e.description.clone(),
                department: e.department.clone(),
                owner: e.owner.clone(),
            };
            e.enrichment = Some(EntityEnrichment {
                status: "ai_unavailable".to_string(),
                error: Some(msg.to_string()),
                input_snapshot: Some(snapshot),
                draft: None,
                generated_by: None,
                model: None,
                generated_at: None,
                draft_version: Some(1),
            });
        }
        return Ok(Json(e.clone()));
    }

    {
        let tasks = s.tasks.read().await;
        let active_exists = tasks.iter().any(|t| {
            t.entity_id.as_deref() == Some(id.as_str())
                && t.task_kind.as_deref() == Some("entity-enrichment")
                && (t.status == TaskStatus::Queued || t.status == TaskStatus::InProgress)
        });
        if active_exists {
            return Err(StatusCode::CONFLICT);
        }
    }

    let mut entities = s.entities.write().await;
    let e = entities
        .iter_mut()
        .find(|x| x.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let next_version = e
        .enrichment
        .as_ref()
        .and_then(|en| en.draft_version)
        .unwrap_or(0)
        + 1;

    let snapshot = EnrichmentInputSnapshot {
        name: Some(e.name.clone()),
        description: e.description.clone(),
        department: e.department.clone(),
        owner: e.owner.clone(),
    };

    if let Some(ref mut enrichment) = e.enrichment {
        enrichment.status = "pending".to_string();
        enrichment.error = None;
        enrichment.input_snapshot = Some(snapshot);
        enrichment.draft = Some(EnrichmentDraft::default());
        enrichment.generated_by = None;
        enrichment.model = None;
        enrichment.generated_at = None;
        enrichment.draft_version = Some(next_version);
    } else {
        e.enrichment = Some(EntityEnrichment {
            status: "pending".to_string(),
            error: None,
            input_snapshot: Some(snapshot),
            draft: Some(EnrichmentDraft::default()),
            generated_by: None,
            model: None,
            generated_at: None,
            draft_version: Some(next_version),
        });
    }

    let task = Task {
        id: Uuid::new_v4().to_string(),
        title: format!("Enrich entity: {}", e.name),
        dept: e.dept.clone(),
        roles: vec![],
        status: TaskStatus::Queued,
        tries: "0".to_string(),
        progress: 0,
        entity_id: Some(e.id.clone()),
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
    let entity = e.clone();
    drop(entities);
    s.tasks.write().await.push(task.clone());
    s.push_operator_feed(OperatorFeedKind::TaskCreated, &task, None)
        .await;
    Ok(Json(entity))
}
