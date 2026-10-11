//! Capture chat and conversational entity creation.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Handler: runs one non-streaming turn of the capture-chat agent.
pub(super) async fn capture_chat_handler(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::capture::CaptureChatRequest>,
) -> Result<Json<crate::capture::CaptureChatResponse>, StatusCode> {
    let control_arc = state
        .control_state
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let outcome = crate::chat_agent::run_chat_turn(
        &state,
        &control_arc,
        &workspace.id,
        &workspace.principal_id,
        body,
    )
    .await?;
    Ok(Json(crate::chat_agent::chat_response_from(&outcome)))
}

/// Handler: streams a capture-chat turn as newline-delimited JSON.
pub(super) async fn capture_chat_stream_handler(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::capture::CaptureChatRequest>,
) -> Result<Response<Body>, StatusCode> {
    let cfg = state
        .app_config
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let control_arc = state
        .control_state
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let stream = crate::chat_agent::stream_chat_turn_ndjson(
        state.clone(),
        cfg,
        control_arc,
        workspace.id.clone(),
        workspace.principal_id.clone(),
        body,
    )
    .await?;
    Response::builder()
        .header(
            axum::http::header::CONTENT_TYPE,
            "application/x-ndjson; charset=utf-8",
        )
        .header(axum::http::header::CACHE_CONTROL, "no-store")
        .body(Body::from_stream(stream))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

/// The caller as the owner of a conversational creation session.
fn creation_owner(workspace: &WorkspaceCtx) -> crate::entity_creation::SessionOwner<'_> {
    crate::entity_creation::SessionOwner {
        workspace_id: &workspace.id,
        principal_id: &workspace.principal_id,
    }
}

/// Handler: starts a conversational entity-creation session, owned by the
/// caller.
pub(super) async fn conversational_entity_start(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::entity_creation::StartCreationRequest>,
) -> Result<Json<crate::entity_creation::StartCreationResponse>, StatusCode> {
    let cfg = state
        .app_config
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let out = crate::entity_creation::start_creation(cfg, creation_owner(&workspace), body).await?;
    Ok(Json(out))
}

/// Handler: submits an answer within the caller's conversational
/// entity-creation session.
pub(super) async fn conversational_entity_answer(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::entity_creation::AnswerQuestionRequest>,
) -> Result<Json<crate::entity_creation::AnswerQuestionResponse>, StatusCode> {
    let cfg = state
        .app_config
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let out =
        crate::entity_creation::answer_question(cfg, creation_owner(&workspace), body).await?;
    Ok(Json(out))
}

/// Handler: finishes a conversational entity-creation session into a real
/// entity, provisioning its default heartbeats from the richer conversation
/// answers (profile/goals/stage) rather than a bare name.
pub(super) async fn conversational_entity_complete(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::entity_creation::CompleteCreationRequest>,
) -> Result<Json<crate::entity_creation::CompleteCreationResponse>, StatusCode> {
    let s = control(&state)?;
    let out =
        crate::entity_creation::complete_creation(s, creation_owner(&workspace), body).await?;
    // Same rhythm the direct-create path gets. This one is the richer case: the
    // conversation has already filled in profile / goals / stage, so the plan is
    // derived from real answers rather than a bare name.
    if let Some(hb_store) = state.heartbeat_store.as_ref()
        && let Err(e) = crate::entity_heartbeat_defaults::provision_default_heartbeats(
            hb_store.as_ref(),
            &out.entity,
            Some(&workspace.id),
        )
        .await
    {
        tracing::warn!(entity_id = %out.entity.id, error = %e, "default heartbeat provisioning failed");
    }
    Ok(Json(out))
}

/// Handler: cancels the caller's conversational entity-creation session. The
/// path id must be a session id this gateway minted (400 otherwise — it is a
/// file name), and the session must be the caller's (404 otherwise).
pub(super) async fn conversational_entity_cancel(
    Extension(workspace): Extension<WorkspaceCtx>,
    Path(session_id): Path<String>,
) -> StatusCode {
    match crate::entity_creation::cancel_session(&session_id, creation_owner(&workspace)) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(status) => status,
    }
}

/// Handler: updates the colour picked for the caller's in-progress creation
/// session.
pub(super) async fn conversational_entity_colour(
    State(_state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
    Json(body): Json<crate::entity_creation::UpdateColourRequest>,
) -> Result<Json<crate::entity_creation::UpdateColourResponse>, StatusCode> {
    let out =
        crate::entity_creation::update_session_colour(creation_owner(&workspace), body).await?;
    Ok(Json(out))
}

/// `GET` handler: lists the caller's in-progress and completed
/// entity-creation sessions — never anyone else's.
pub(super) async fn list_conversational_entity_sessions(
    State(_state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<crate::entity_creation::ListSessionsResponse>, StatusCode> {
    let sessions = crate::entity_creation::load_sessions_for(creation_owner(&workspace));
    let summaries = sessions
        .into_iter()
        .map(|s| crate::entity_creation::SessionSummary {
            session_id: s.id,
            entity_name: s.entity_name,
            created_at: s.created_at,
            complete: s.complete,
            fields_filled: s.answered_fields.len(),
            total_fields: crate::entity_creation::BASE_QUESTIONS.len(),
        })
        .collect();
    Ok(Json(crate::entity_creation::ListSessionsResponse {
        sessions: summaries,
    }))
}
