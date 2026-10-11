//! Skills API — browse, write, switch on or off, and import Claude-format
//! skills.
//!
//! A child module of `control_api` (`use super::*`). The library itself is
//! [`crate::skills`]; this layer adds the two checks it can't make alone:
//!
//! - **Ownership.** Every call is scoped to the caller's workspace, and an
//!   entity-scoped skill is reachable only by someone who can see that entity
//!   (`is_entity_visible`), so a project's skills never leak to a teammate who
//!   can't see the project.
//! - **Local disk.** Reading skill folders off the gateway's own disk — the
//!   user's `~/.claude/skills`, or a folder they name — is offered only when
//!   the gateway is the user's own machine (`state.local_files`), the same rule
//!   the chat file tools follow. Uploading a `SKILL.md` works everywhere.
//!
//! Errors carry a JSON `{ "error": ... }` body because the reasons ("a skill
//! named `x` already exists", "skill names use lowercase letters…") are what
//! the user needs to see.

use super::*;
use crate::skills::{
    ExternalSkill, ImportReport, ListView, NewSkill, SkillDetail, SkillError, SkillLibrary,
    SkillPatch, SkillScope, SkillSummary, UploadedFile,
};
use std::sync::Arc;

type ApiError = (StatusCode, Json<serde_json::Value>);
type ApiResult<T> = Result<Json<T>, ApiError>;

fn api_error(status: StatusCode, message: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": message.into() })))
}

fn skill_error(e: SkillError) -> ApiError {
    let status = match &e {
        SkillError::Invalid(_) => StatusCode::BAD_REQUEST,
        SkillError::NotFound => StatusCode::NOT_FOUND,
        SkillError::Conflict(_) => StatusCode::CONFLICT,
        SkillError::ReadOnly => StatusCode::FORBIDDEN,
        SkillError::Io(err) => {
            warn!(error = %err, "skills: storage error");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    };
    api_error(status, e.to_string())
}

fn library(state: &ServerState) -> Result<Arc<SkillLibrary>, ApiError> {
    state.skills.clone().ok_or_else(|| {
        api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "skills are not available on this gateway",
        )
    })
}

/// Run a library call off the async runtime (it does file I/O).
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, SkillError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| {
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("skills task failed: {e}"),
            )
        })?
        .map_err(skill_error)
}

/// The entities the caller can see, as `(id, name)` — the only entities whose
/// skills they may read or write.
async fn visible_entities(
    state: &ServerState,
    ws: &WorkspaceCtx,
) -> Result<Vec<(String, String)>, ApiError> {
    let s = control(state).map_err(|code| api_error(code, "workspace state is unavailable"))?;
    let entities = s.entities.read().await;
    Ok(entities
        .iter()
        .filter(|e| is_entity_visible(e, &ws.id, Some(ws.principal_id.as_str())))
        .map(|e| (e.id.clone(), e.name.clone()))
        .collect())
}

async fn require_visible_entity(
    state: &ServerState,
    ws: &WorkspaceCtx,
    entity_id: &str,
) -> Result<(), ApiError> {
    let visible = visible_entities(state, ws).await?;
    if visible.iter().any(|(id, _)| id == entity_id) {
        Ok(())
    } else {
        Err(api_error(StatusCode::NOT_FOUND, "entity not found"))
    }
}

/// An existing skill id is reachable when its scope is: built-in and global
/// always (within the caller's workspace), entity only for a visible entity.
async fn authorize_id(state: &ServerState, ws: &WorkspaceCtx, id: &str) -> Result<(), ApiError> {
    let (scope, _) = crate::skills::parse_skill_id(id).map_err(skill_error)?;
    if let Some(entity_id) = scope.entity_id() {
        require_visible_entity(state, ws, entity_id).await?;
    }
    Ok(())
}

async fn authorize_scope(
    state: &ServerState,
    ws: &WorkspaceCtx,
    scope: &str,
    entity_id: Option<&str>,
) -> Result<SkillScope, ApiError> {
    let scope = SkillScope::from_parts(scope, entity_id).map_err(skill_error)?;
    if let Some(entity_id) = scope.entity_id() {
        require_visible_entity(state, ws, entity_id).await?;
    }
    Ok(scope)
}

fn require_local(state: &ServerState) -> Result<(), ApiError> {
    if state.local_files.is_some() {
        Ok(())
    } else {
        Err(api_error(
            StatusCode::FORBIDDEN,
            "importing from disk is only available when the gateway runs on your own computer — upload the SKILL.md instead",
        ))
    }
}

// ─── List / read ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListSkillsQuery {
    /// Show what this entity's chat sees (built-in + global + its own). Omit
    /// for the management view: every scope, every visible entity.
    #[serde(default)]
    entity_id: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SkillEntityRef {
    id: String,
    name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListSkillsResponse {
    skills: Vec<SkillSummary>,
    /// Entities the caller can attach skills to, for grouping and pickers.
    entities: Vec<SkillEntityRef>,
    /// True when this gateway can import skill folders from its own disk.
    local_import: bool,
}

pub(super) async fn list_skills(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Query(q): Query<ListSkillsQuery>,
) -> ApiResult<ListSkillsResponse> {
    let lib = library(&state)?;
    let visible = visible_entities(&state, &ws).await?;
    let focus = q
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    if let Some(e) = focus.as_deref()
        && !visible.iter().any(|(id, _)| id == e)
    {
        return Err(api_error(StatusCode::NOT_FOUND, "entity not found"));
    }
    let workspace_id = ws.id.clone();
    let visible_ids: Vec<String> = visible.iter().map(|(id, _)| id.clone()).collect();
    let skills = blocking(move || {
        let view = match focus {
            Some(entity_id) => ListView::Context {
                entity_id: Some(entity_id),
            },
            None => ListView::All {
                entity_ids: lib
                    .entity_ids_with_skills(&workspace_id)
                    .into_iter()
                    .filter(|id| visible_ids.contains(id))
                    .collect(),
            },
        };
        Ok(lib.list(&workspace_id, &view))
    })
    .await?;
    Ok(Json(ListSkillsResponse {
        skills,
        entities: visible
            .into_iter()
            .map(|(id, name)| SkillEntityRef { id, name })
            .collect(),
        local_import: state.local_files.is_some(),
    }))
}

pub(super) async fn get_skill(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> ApiResult<SkillDetail> {
    authorize_id(&state, &ws, &id).await?;
    let lib = library(&state)?;
    let workspace_id = ws.id.clone();
    blocking(move || lib.get(&workspace_id, &id)).await.map(Json)
}

// ─── Write ───────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CreateSkillRequest {
    /// `global` or `entity` (also accepts `project`).
    scope: String,
    #[serde(default)]
    entity_id: Option<String>,
    name: String,
    description: String,
    #[serde(default)]
    body: String,
}

pub(super) async fn create_skill(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Json(body): Json<CreateSkillRequest>,
) -> ApiResult<SkillSummary> {
    let scope = authorize_scope(&state, &ws, &body.scope, body.entity_id.as_deref()).await?;
    let lib = library(&state)?;
    let workspace_id = ws.id.clone();
    blocking(move || {
        lib.create(
            &workspace_id,
            &scope,
            NewSkill {
                name: body.name,
                description: body.description,
                body: body.body,
            },
        )
    })
    .await
    .map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateSkillRequest {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    body: Option<String>,
    /// A whole replacement `SKILL.md` (the "edit raw" mode).
    #[serde(default)]
    raw: Option<String>,
}

pub(super) async fn update_skill(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<UpdateSkillRequest>,
) -> ApiResult<SkillSummary> {
    authorize_id(&state, &ws, &id).await?;
    let lib = library(&state)?;
    let workspace_id = ws.id.clone();
    blocking(move || {
        lib.update(
            &workspace_id,
            &id,
            SkillPatch {
                description: body.description,
                body: body.body,
                raw: body.raw,
            },
        )
    })
    .await
    .map(Json)
}

pub(super) async fn delete_skill(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
) -> ApiResult<serde_json::Value> {
    authorize_id(&state, &ws, &id).await?;
    let lib = library(&state)?;
    let workspace_id = ws.id.clone();
    blocking(move || lib.delete(&workspace_id, &id)).await?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Debug, Deserialize)]
pub(super) struct SetSkillEnabledRequest {
    enabled: bool,
}

pub(super) async fn set_skill_enabled(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<SetSkillEnabledRequest>,
) -> ApiResult<SkillSummary> {
    authorize_id(&state, &ws, &id).await?;
    let lib = library(&state)?;
    let workspace_id = ws.id.clone();
    blocking(move || lib.set_enabled(&workspace_id, &id, body.enabled))
        .await
        .map(Json)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct CopySkillRequest {
    scope: String,
    #[serde(default)]
    entity_id: Option<String>,
}

/// Copy a skill into another scope — "customize" a built-in into global, or
/// adapt a global skill for one project. The copy shadows the original there.
pub(super) async fn copy_skill(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Path(id): Path<String>,
    Json(body): Json<CopySkillRequest>,
) -> ApiResult<SkillSummary> {
    authorize_id(&state, &ws, &id).await?;
    let target = authorize_scope(&state, &ws, &body.scope, body.entity_id.as_deref()).await?;
    let lib = library(&state)?;
    let workspace_id = ws.id.clone();
    blocking(move || lib.copy_to(&workspace_id, &id, &target))
        .await
        .map(Json)
}

// ─── Import ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UploadSkillRequest {
    scope: String,
    #[serde(default)]
    entity_id: Option<String>,
    /// The full `SKILL.md`, frontmatter included.
    skill_md: String,
    /// Supporting text files, paths relative to the skill folder.
    #[serde(default)]
    files: Vec<UploadedFile>,
    #[serde(default)]
    overwrite: bool,
}

pub(super) async fn upload_skill(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Json(body): Json<UploadSkillRequest>,
) -> ApiResult<SkillSummary> {
    let scope = authorize_scope(&state, &ws, &body.scope, body.entity_id.as_deref()).await?;
    let lib = library(&state)?;
    let workspace_id = ws.id.clone();
    blocking(move || {
        lib.import_upload(
            &workspace_id,
            &scope,
            &body.skill_md,
            &body.files,
            body.overwrite,
        )
    })
    .await
    .map(Json)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LocalSkillSourcesResponse {
    /// `~/.claude/skills` on this machine, when it exists.
    claude_skills_dir: Option<String>,
    /// The skills found there, offered for import.
    skills: Vec<ExternalSkill>,
}

/// What `~/.claude/skills` holds — the user's Claude Code skills — so the
/// dashboard can offer "bring your Claude skills".
pub(super) async fn list_local_skill_sources(
    State(state): State<ServerState>,
) -> ApiResult<LocalSkillSourcesResponse> {
    require_local(&state)?;
    let found = blocking(|| {
        Ok(crate::skills::claude_skills_dir()
            .filter(|d| d.is_dir())
            .map(|d| {
                let skills = crate::skills::list_skill_folder(&d);
                (d.to_string_lossy().to_string(), skills)
            }))
    })
    .await?;
    let (claude_skills_dir, skills) = match found {
        Some((dir, skills)) => (Some(dir), skills),
        None => (None, Vec::new()),
    };
    Ok(Json(LocalSkillSourcesResponse {
        claude_skills_dir,
        skills,
    }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ImportLocalSkillsRequest {
    scope: String,
    #[serde(default)]
    entity_id: Option<String>,
    /// Absolute paths: a skill folder, or a folder of skill folders.
    paths: Vec<String>,
    #[serde(default)]
    overwrite: bool,
}

pub(super) async fn import_local_skills(
    State(state): State<ServerState>,
    Extension(ws): Extension<WorkspaceCtx>,
    Json(body): Json<ImportLocalSkillsRequest>,
) -> ApiResult<ImportReport> {
    require_local(&state)?;
    if body.paths.is_empty() {
        return Err(api_error(StatusCode::BAD_REQUEST, "pick at least one folder"));
    }
    let scope = authorize_scope(&state, &ws, &body.scope, body.entity_id.as_deref()).await?;
    let lib = library(&state)?;
    let workspace_id = ws.id.clone();
    blocking(move || {
        let mut report = ImportReport::default();
        for raw in &body.paths {
            let expanded = crate::config::expand_tilde_path(raw.trim());
            let path = PathBuf::from(&expanded);
            if !path.is_absolute() {
                report.skipped.push(crate::skills::SkippedSkill {
                    source: raw.clone(),
                    reason: "use an absolute path".to_string(),
                });
                continue;
            }
            match lib.import_dir(&workspace_id, &scope, &path, body.overwrite) {
                Ok(r) => {
                    report.imported.extend(r.imported);
                    report.skipped.extend(r.skipped);
                }
                Err(e) => report.skipped.push(crate::skills::SkippedSkill {
                    source: raw.clone(),
                    reason: e.to_string(),
                }),
            }
        }
        Ok(report)
    })
    .await
    .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_errors_map_to_honest_statuses() {
        assert_eq!(
            skill_error(SkillError::Invalid("x".into())).0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(skill_error(SkillError::NotFound).0, StatusCode::NOT_FOUND);
        assert_eq!(
            skill_error(SkillError::Conflict("x".into())).0,
            StatusCode::CONFLICT
        );
        assert_eq!(skill_error(SkillError::ReadOnly).0, StatusCode::FORBIDDEN);
        let (_, Json(body)) = skill_error(SkillError::Conflict("a skill named `x` already exists".into()));
        assert_eq!(body["error"], "a skill named `x` already exists");
    }
}
