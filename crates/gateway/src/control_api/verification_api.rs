//! Adversarial verification runs over task output.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Body for running a verification action (a shell test or a URL check).
#[derive(Debug, Deserialize)]
pub struct RunVerificationRequest {
    /// `shell` (test command) or `browser` (URL check).
    pub kind: String,
    /// Shell command to run (required when kind is `shell`).
    #[serde(default)]
    pub command: Option<String>,
    /// URL to check (required when kind is `browser`).
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub entity_id: Option<String>,
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub mission_id: Option<String>,
    /// Working directory for a shell verification (defaults to the entity root).
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Query-string filters for listing verification runs.
#[derive(Debug, Deserialize)]
pub(super) struct VerificationQuery {
    pub(super) entity_id: Option<String>,
    pub(super) task_id: Option<String>,
    pub(super) mission_id: Option<String>,
    pub(super) status: Option<String>,
    pub(super) kind: Option<String>,
    pub(super) limit: Option<usize>,
}

/// Who is asking for a verification, and whether they may run a shell one.
pub(crate) struct VerificationCaller<'a> {
    pub(crate) workspace_id: &'a str,
    pub(crate) principal_id: &'a str,
    /// `sh -c <command>` on this machine. True only for an instance admin on a
    /// gateway that is the user's own machine (`ServerState::local_files` is
    /// set only for a loopback-bound gateway). On a hosted gateway a shell
    /// verification is remote code execution for anyone who can sign up.
    /// The same trust lets a URL check reach private addresses; without it a
    /// URL check is limited to the public internet (no SSRF into the host).
    pub(crate) may_run_shell: bool,
}

/// Execute a verification action (shell command or browser URL check), store the
/// result, and — on failure — flag the linked task `NeedsReview` and create a
/// follow-up "next action" fix task. Reusable from HTTP and internal flows.
pub(crate) async fn internal_run_verification(
    s: &crate::state::ControlState,
    caller: VerificationCaller<'_>,
    body: RunVerificationRequest,
) -> Result<VerificationRun, StatusCode> {
    let workspace_id = caller.workspace_id;
    let viewer = Some(caller.principal_id);
    let kind = crate::verification_actions::VerificationKind::parse(&body.kind)
        .ok_or(StatusCode::BAD_REQUEST)?;
    // Validate entity visibility and resolve a working directory when scoped.
    // The viewer is passed so a Personal Space can be verified by its owner.
    let entity = if let Some(eid) = body
        .entity_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        let entities = s.entities.read().await;
        let entity = entities
            .iter()
            .find(|e| e.id == eid)
            .ok_or(StatusCode::BAD_REQUEST)?;
        if !is_entity_visible(entity, workspace_id, viewer) {
            return Err(StatusCode::FORBIDDEN);
        }
        Some(entity.clone())
    } else {
        None
    };

    // The linked task is about to be flagged `NeedsReview` on failure, so it
    // must be one the caller can see; an unchecked id let anyone mark any
    // tenant's task. Checked before anything runs.
    let task_id = body
        .task_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    if let Some(ref tid) = task_id {
        let entity_ids = {
            let entities = s.entities.read().await;
            entity_ids_for_workspace(&entities, workspace_id, viewer)
        };
        let visible = s
            .tasks
            .read()
            .await
            .iter()
            .any(|t| t.id == *tid && task_visible_in_workspace(t, &entity_ids, workspace_id));
        if !visible {
            return Err(StatusCode::NOT_FOUND);
        }
    }

    let command = body
        .command
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
    let url = body
        .url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
    use crate::verification_actions::VerificationKind;
    match kind {
        VerificationKind::Shell if command.is_none() => return Err(StatusCode::BAD_REQUEST),
        VerificationKind::Browser if url.is_none() => return Err(StatusCode::BAD_REQUEST),
        _ => {}
    }
    // A shell verification runs in the entity's own folder, never in a
    // directory the caller names: `cwd` may only narrow to a path inside the
    // entity's resolved roots (the same jail coding-agent runs use), and an
    // entity-less shell run has no jail to run in.
    let shell_cwd = if kind == VerificationKind::Shell {
        if !caller.may_run_shell {
            return Err(StatusCode::FORBIDDEN);
        }
        let entity = entity.as_ref().ok_or(StatusCode::BAD_REQUEST)?;
        let (cwd, _roots) = resolve_entity_agent_run_cwd(entity, body.cwd.as_deref()).await?;
        Some(cwd.to_string_lossy().to_string())
    } else {
        None
    };
    let timeout = body.timeout_secs.unwrap_or(120);

    let now = chrono::Utc::now().to_rfc3339();
    let id = Uuid::new_v4().to_string();
    let mut record = VerificationRun {
        id: id.clone(),
        workspace_id: workspace_id.to_string(),
        entity_id: entity.as_ref().map(|e| e.id.clone()),
        task_id: task_id.clone(),
        mission_id: body.mission_id.clone(),
        kind: kind.as_str().to_string(),
        command: command.clone(),
        url: url.clone(),
        status: "running".to_string(),
        exit_code: None,
        logs: String::new(),
        artifacts: Vec::new(),
        follow_up_task_id: None,
        created_at: now,
        completed_at: None,
    };
    s.verifications.write().await.push(record.clone());

    let (passed, exit_code, logs) = match kind {
        VerificationKind::Shell => {
            let out = crate::verification_actions::run_shell(
                command.as_deref().unwrap_or(""),
                shell_cwd.as_deref(),
                timeout,
            )
            .await;
            (out.passed, out.exit_code, out.logs)
        }
        VerificationKind::Browser => {
            let url = url.as_deref().unwrap_or("");
            // The machine's owner may check anything their own machine can
            // reach (a dev server on localhost included); anyone else's check
            // is a server-side request and only reaches the public internet.
            let out = if caller.may_run_shell {
                crate::verification_actions::run_url_check(url, timeout).await
            } else {
                crate::verification_actions::run_public_url_check(url, timeout).await
            };
            (out.passed, out.status_code.map(i64::from), out.logs)
        }
    };
    record.status = if passed { "passed" } else { "failed" }.to_string();
    record.exit_code = exit_code;
    record.logs = logs;
    record.completed_at = Some(chrono::Utc::now().to_rfc3339());

    // On failure: flag the linked task for review and create the next action.
    if !passed {
        if let Some(ref tid) = record.task_id {
            let mut tasks = s.tasks.write().await;
            if let Some(t) = tasks.iter_mut().find(|t| t.id == *tid) {
                t.outcome = Some(TaskOutcome::NeedsReview);
            }
        }
        let label: String = record
            .command
            .clone()
            .or_else(|| record.url.clone())
            .unwrap_or_default()
            .chars()
            .take(80)
            .collect();
        let follow = internal_create_task(
            s,
            workspace_id,
            viewer,
            CreateTaskRequest {
                title: format!("Fix failing verification: {label}"),
                dept: None,
                roles: None,
                entity_id: record.entity_id.clone(),
                mission_id: record.mission_id.clone(),
                priority: Some("high".to_string()),
                requires_human_approval: None,
                task_kind: Some("verification-fix".to_string()),
                content: Some(format!(
                    "Verification `{}` failed.\n\n{}",
                    record.kind, record.logs
                )),
                director_id: None,
                agent_id: None,
                connection_id: None,
                external_source: Some("verification".to_string()),
                external_id: Some(record.id.clone()),
                metadata: Some(serde_json::json!({ "verificationId": record.id })),
                due_at: None,
            },
        )
        .await
        .ok();
        record.follow_up_task_id = follow.map(|t| t.id);
    }

    {
        let mut runs = s.verifications.write().await;
        if let Some(r) = runs.iter_mut().find(|r| r.id == id) {
            *r = record.clone();
        }
    }
    Ok(record)
}

/// `POST` handler: runs a verification action (see [`internal_run_verification`]).
pub(super) async fn run_verification(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<RunVerificationRequest>,
) -> Result<Json<VerificationRun>, StatusCode> {
    let s = control(&state)?;
    let may_run_shell = state.local_files.is_some()
        && crate::server::is_instance_admin(&auth, crate::server::gateway_is_self_hosted(&state));
    let caller = VerificationCaller {
        workspace_id: &workspace.id,
        principal_id: &workspace.principal_id,
        may_run_shell,
    };
    let record = internal_run_verification(s, caller, body).await?;
    Ok(Json(record))
}

/// `GET` handler: lists verification runs, filtered and newest-first-capped.
pub(super) async fn list_verifications(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<VerificationQuery>,
) -> Result<Json<Vec<VerificationRun>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let mut out: Vec<VerificationRun> = s
        .verifications
        .read()
        .await
        .iter()
        .filter(|v| v.workspace_id == workspace_id)
        .filter(|v| match q.entity_id.as_deref() {
            Some(e) => v.entity_id.as_deref() == Some(e),
            None => true,
        })
        .filter(|v| match q.task_id.as_deref() {
            Some(t) => v.task_id.as_deref() == Some(t),
            None => true,
        })
        .filter(|v| match q.mission_id.as_deref() {
            Some(m) => v.mission_id.as_deref() == Some(m),
            None => true,
        })
        .filter(|v| match q.status.as_deref() {
            Some(st) => v.status == st,
            None => true,
        })
        .filter(|v| match q.kind.as_deref() {
            Some(k) => v.kind == k,
            None => true,
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    out.truncate(q.limit.unwrap_or(100));
    Ok(Json(out))
}

/// `GET` handler: lists operator feed events visible in the workspace,
/// filtered and paginated.
pub(super) async fn list_operator_feed(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Query(q): Query<OperatorFeedQuery>,
) -> Result<Json<PaginatedResponse<OperatorFeedEvent>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    let entity_ids = {
        let entities = s.entities.read().await;
        entity_ids_for_workspace(
            &entities,
            &workspace_id,
            Some(workspace.principal_id.as_str()),
        )
    };
    let mut out = s.operator_feed.read().await.clone();
    out.retain(|e| feed_visible_in_workspace(e, &entity_ids, &workspace_id));
    if let Some(task_id) = q.task_id.as_deref() {
        out.retain(|e| e.task_id.as_deref() == Some(task_id));
    }
    if let Some(mission_id) = q.mission_id.as_deref() {
        out.retain(|e| e.mission_id.as_deref() == Some(mission_id));
    }
    if let Some(entity_id) = q.entity_id.as_deref() {
        out.retain(|e| e.entity_id.as_deref() == Some(entity_id));
    }
    if let Some(connection_id) = q.connection_id.as_deref() {
        out.retain(|e| e.connection_id.as_deref() == Some(connection_id));
    }
    let total = out.len();
    out.reverse();
    let pag = apply_pagination(
        out,
        total,
        &PaginationQuery {
            limit: q.limit,
            offset: q.offset,
        },
    );
    Ok(Json(pag))
}
