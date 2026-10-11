//! Conversational entity creation: LLM-driven onboarding that builds entity narrative progressively.
//! Session-persisted, auto-fills multiple fields from user answers, supports skip.

use crate::config::AppConfig;
use crate::credentials_resolver::resolve_llm_api_key;
use crate::state::{ControlState, Entity, EntityReadiness, EntitySetup, EntityVisibility};
use axum::http::StatusCode;
use consistent_world_llm::client::{ChatMessage, CompletionRequest, LlmProvider, Role};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// Narrative fields collected during entity creation
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NarrativeDraft {
    pub profile: Option<String>,
    pub goals: Option<String>,
    pub active_context: Option<String>,
    pub operating_rules: Option<String>,
    pub notes: Option<String>,
    pub content_pipeline: Option<String>,
    pub local_path: Option<String>,
}

/// Conversational creation session
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityCreationSession {
    pub id: String,
    pub entity_name: String,
    pub workspace_id: String,
    pub collected: NarrativeDraft,
    pub answered_fields: Vec<String>,
    pub current_step: usize,
    pub complete: bool,
    pub created_at: String,
    pub updated_at: String,
    /// Colour suggested by the system (deterministic palette pick) or
    /// overridden by the user during the creation flow. Settled into the
    /// final Entity at complete_creation time.
    #[serde(default)]
    pub proposed_colour: Option<String>,
    /// The principal that started the session. Only that principal (in
    /// `workspace_id`) may answer, recolour, complete, list or cancel it.
    /// `None` on sessions saved before ownership was recorded, which fall back
    /// to a workspace match.
    #[serde(default)]
    pub owner_principal_id: Option<String>,
}

/// The caller acting on a creation session.
#[derive(Debug, Clone, Copy)]
pub struct SessionOwner<'a> {
    pub workspace_id: &'a str,
    pub principal_id: &'a str,
}

impl EntityCreationSession {
    /// Whether `owner` may act on this session.
    pub fn is_owned_by(&self, owner: SessionOwner<'_>) -> bool {
        self.workspace_id == owner.workspace_id
            && self
                .owner_principal_id
                .as_deref()
                .is_none_or(|p| p == owner.principal_id)
    }
}

/// One swatch in the curated entity-colour palette.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ColourSwatch {
    pub hex: String,
    pub name: String,
}

/// Base questions for entity creation (LLM-enhanced)
pub const BASE_QUESTIONS: &[(&str, &str)] = &[
    ("profile", "What is {entity_name}? What does it do?"),
    ("goals", "What are the main goals for {entity_name}?"),
    (
        "active_context",
        "What's the current situation or stage for {entity_name}?",
    ),
    (
        "operating_rules",
        "Are there any rules, constraints, or principles for working with {entity_name}?",
    ),
    ("notes", "Anything else I should know about {entity_name}?"),
    (
        "content_pipeline",
        "How does {entity_name} communicate outwardly? Any content strategy?",
    ),
    (
        "local_path",
        "Where should {entity_name}'s files live on this machine? (e.g. ~/projects/{entity_name})",
    ),
];

/// Request to start conversational creation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartCreationRequest {
    pub entity_name: String,
    pub workspace_id: Option<String>,
}

/// Response with first question
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartCreationResponse {
    pub session_id: String,
    pub question: String,
    pub help: String,
    pub progress: CreationProgress,
    /// System-suggested accent colour for this entity. The user may override
    /// via `update_session_colour` before completion.
    pub proposed_colour: String,
    /// The full curated swatch list the UI should render so the user can
    /// pick a different colour.
    pub palette: Vec<ColourSwatch>,
}

/// Request to override the proposed colour for an in-progress creation session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateColourRequest {
    pub session_id: String,
    pub colour: String,
}

/// Response after updating the colour.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateColourResponse {
    pub colour: String,
}

/// Request to answer a question
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerQuestionRequest {
    pub session_id: String,
    pub answer: String,
    #[serde(default)]
    pub skip: bool,
}

/// Response with next question
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AnswerQuestionResponse {
    pub next_question: Option<String>,
    pub help: Option<String>,
    pub progress: CreationProgress,
    pub complete: bool,
    pub fields_auto_filled: Vec<String>,
}

/// Request to complete creation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteCreationRequest {
    pub session_id: String,
    /// If true, finish creation even if the full BASE_QUESTIONS state machine
    /// hasn't been walked. Used by the slim 3-step chat onboarding so users
    /// aren't blocked on the longer optional fields (notes, content_pipeline,
    /// local_path) — those can be filled in later from the entity page.
    #[serde(default)]
    pub force_complete: bool,
}

/// Response with created entity
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompleteCreationResponse {
    pub entity: Entity,
}

/// List sessions response
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSessionsResponse {
    pub sessions: Vec<SessionSummary>,
}

/// One row in [`ListSessionsResponse`] — enough for the UI to list in-progress
/// and finished creation sessions without loading each full session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub entity_name: String,
    pub created_at: String,
    pub complete: bool,
    pub fields_filled: usize,
    pub total_fields: usize,
}

/// How far through [`BASE_QUESTIONS`] a creation session has gotten.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreationProgress {
    pub answered: usize,
    pub total: usize,
    pub percent: u8,
}

/// Session storage directory
fn sessions_dir() -> String {
    app_data_dir()
        .join("creation_sessions")
        .to_string_lossy()
        .to_string()
}

/// Whether `session_id` is a session id this module could have minted: a
/// canonical hyphenated UUID (`start_creation` uses `Uuid::new_v4()`).
///
/// The id arrives in a URL path segment and in JSON bodies, and becomes a file
/// name. axum percent-decodes path segments, so `..%2Fvaults%2F<id>` reached
/// `format!("{dir}/{id}.json")` as `../vaults/<id>` — and `DELETE` removed a
/// file outside the sessions directory. A UUID has no separators and no dots.
pub fn is_valid_session_id(session_id: &str) -> bool {
    session_id.len() == 36 && Uuid::try_parse(session_id).is_ok()
}

/// The on-disk path for a session, or 400 for an id that is not one of ours.
fn session_path(session_id: &str) -> Result<PathBuf, StatusCode> {
    if !is_valid_session_id(session_id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    Ok(PathBuf::from(sessions_dir()).join(format!("{session_id}.json")))
}

/// Save session to disk
fn save_session(session: &EntityCreationSession) -> Result<(), String> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create sessions dir: {}", e))?;
    let path = session_path(&session.id).map_err(|_| "invalid session id".to_string())?;
    let bytes = serde_json::to_vec_pretty(session)
        .map_err(|e| format!("Failed to serialize session: {}", e))?;
    std::fs::write(&path, bytes).map_err(|e| format!("Failed to write session: {}", e))?;
    Ok(())
}

/// Load a session from disk: 400 for a malformed id, 404 when it does not
/// exist. Does not check ownership — see [`load_owned_session`].
fn load_session(session_id: &str) -> Result<EntityCreationSession, StatusCode> {
    let path = session_path(session_id)?;
    let bytes = std::fs::read(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    serde_json::from_slice(&bytes).map_err(|_| StatusCode::NOT_FOUND)
}

/// Load a session the caller owns. Someone else's session is a 404, exactly
/// like a missing one.
fn load_owned_session(
    session_id: &str,
    owner: SessionOwner<'_>,
) -> Result<EntityCreationSession, StatusCode> {
    let session = load_session(session_id)?;
    if !session.is_owned_by(owner) {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(session)
}

/// The caller's own sessions, in progress or finished.
pub fn load_sessions_for(owner: SessionOwner<'_>) -> Vec<EntityCreationSession> {
    load_all_sessions()
        .into_iter()
        .filter(|s| s.is_owned_by(owner))
        .collect()
}

/// Load all sessions from disk, every owner's. Callers serving a request want
/// [`load_sessions_for`].
pub fn load_all_sessions() -> Vec<EntityCreationSession> {
    let dir = sessions_dir();
    let mut sessions = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json")
                && let Ok(bytes) = std::fs::read(&path)
                && let Ok(session) = serde_json::from_slice(&bytes)
            {
                sessions.push(session);
            }
        }
    }
    sessions
}

/// Delete session from disk. Only ever called with a validated id.
fn delete_session(session_id: &str) {
    if let Ok(path) = session_path(session_id) {
        let _ = std::fs::remove_file(&path);
    }
}

/// Public cancel hook for the HTTP DELETE endpoint: 400 for a malformed id,
/// 404 for someone else's session, `Ok` once the caller's session is gone
/// (including when it was already gone — cancelling is idempotent).
pub fn cancel_session(session_id: &str, owner: SessionOwner<'_>) -> Result<(), StatusCode> {
    match load_owned_session(session_id, owner) {
        Ok(_) => {
            delete_session(session_id);
            Ok(())
        }
        Err(StatusCode::NOT_FOUND) if !session_path(session_id)?.exists() => Ok(()),
        Err(status) => Err(status),
    }
}

/// Start conversational entity creation. The session belongs to `owner`; a
/// `workspaceId` in the request body is ignored, since the caller's
/// workspace comes from their credentials, not from what they claim.
pub async fn start_creation(
    _config: &AppConfig,
    owner: SessionOwner<'_>,
    req: StartCreationRequest,
) -> Result<StartCreationResponse, StatusCode> {
    let session_id = Uuid::new_v4().to_string();
    let proposed_colour = pick_entity_colour(&session_id);
    let session = EntityCreationSession {
        id: session_id,
        entity_name: req.entity_name.clone(),
        workspace_id: owner.workspace_id.to_string(),
        owner_principal_id: Some(owner.principal_id.to_string()),
        collected: NarrativeDraft::default(),
        answered_fields: Vec::new(),
        current_step: 0,
        complete: false,
        created_at: chrono::Utc::now().to_rfc3339(),
        updated_at: chrono::Utc::now().to_rfc3339(),
        proposed_colour: Some(proposed_colour.clone()),
    };

    save_session(&session).map_err(|e| {
        tracing::error!("Failed to save session: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let (question, help) = get_question(&session);

    Ok(StartCreationResponse {
        session_id: session.id,
        question,
        help,
        progress: CreationProgress {
            answered: 0,
            total: BASE_QUESTIONS.len(),
            percent: 0,
        },
        proposed_colour,
        palette: entity_colour_palette(),
    })
}

/// User-driven override of the proposed colour for an in-progress session.
/// The supplied colour must belong to the curated palette.
pub async fn update_session_colour(
    owner: SessionOwner<'_>,
    req: UpdateColourRequest,
) -> Result<UpdateColourResponse, StatusCode> {
    let mut session = load_owned_session(&req.session_id, owner)?;
    let normalized = req.colour.trim().to_ascii_lowercase();
    let palette = entity_colour_palette();
    let valid = palette
        .iter()
        .any(|s| s.hex.to_ascii_lowercase() == normalized);
    if !valid {
        return Err(StatusCode::BAD_REQUEST);
    }
    session.proposed_colour = Some(normalized.clone());
    session.updated_at = chrono::Utc::now().to_rfc3339();
    save_session(&session).map_err(|e| {
        tracing::error!("Failed to save session: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(UpdateColourResponse { colour: normalized })
}

/// Answer a question and get next one
pub async fn answer_question(
    config: &AppConfig,
    owner: SessionOwner<'_>,
    req: AnswerQuestionRequest,
) -> Result<AnswerQuestionResponse, StatusCode> {
    let mut session = load_owned_session(&req.session_id, owner)?;

    if session.complete {
        return Ok(AnswerQuestionResponse {
            next_question: None,
            help: None,
            progress: CreationProgress {
                answered: BASE_QUESTIONS.len(),
                total: BASE_QUESTIONS.len(),
                percent: 100,
            },
            complete: true,
            fields_auto_filled: Vec::new(),
        });
    }

    if !req.skip && !req.answer.trim().is_empty() {
        // Use LLM to extract info into all relevant fields
        let auto_filled = extract_fields_with_llm(config, &mut session, &req.answer).await?;
        session.answered_fields.extend(auto_filled.clone());

        // Also store the raw answer in the current step's field
        if session.current_step < BASE_QUESTIONS.len() {
            let field = BASE_QUESTIONS[session.current_step].0;
            if !session.answered_fields.iter().any(|f| f == field) {
                session.answered_fields.push(field.to_string());
                store_answer(&mut session, field, &req.answer);
            }
        }
    } else if req.skip {
        // Skip current field
        if session.current_step < BASE_QUESTIONS.len() {
            let field = BASE_QUESTIONS[session.current_step].0;
            session.answered_fields.push(format!("{}:skipped", field));
        }
    }

    session.current_step += 1;
    session.updated_at = chrono::Utc::now().to_rfc3339();

    // Check if complete
    let is_complete = session.current_step >= BASE_QUESTIONS.len();
    if is_complete {
        session.complete = true;
    }

    save_session(&session).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let (next_question, help) = if is_complete {
        (None, None)
    } else {
        let (q, h) = get_question(&session);
        (Some(q), Some(h))
    };

    let answered_count = session
        .answered_fields
        .iter()
        .filter(|f| !f.ends_with(":skipped"))
        .count();

    Ok(AnswerQuestionResponse {
        next_question,
        help,
        progress: CreationProgress {
            answered: answered_count,
            total: BASE_QUESTIONS.len(),
            percent: ((answered_count as f32 / BASE_QUESTIONS.len() as f32) * 100.0) as u8,
        },
        complete: is_complete,
        fields_auto_filled: Vec::new(),
    })
}

/// Complete creation and create entity
pub async fn complete_creation(
    control: &ControlState,
    owner: SessionOwner<'_>,
    req: CompleteCreationRequest,
) -> Result<CompleteCreationResponse, StatusCode> {
    let workspace_id = owner.workspace_id;
    let session = load_owned_session(&req.session_id, owner)?;

    if !session.complete && !req.force_complete {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Determine local_path
    let local_path = session.collected.local_path.clone().unwrap_or_else(|| {
        let slug = slugify(&session.entity_name);
        format!("~/.consistent-world/entities/{}/", slug)
    });

    // Auto-create directory in a safe location. Relative paths are always resolved
    // under the app entity root to avoid polluting the caller's working directory.
    let normalized_local_path = normalize_local_path_input(&local_path, &session.entity_name);
    std::fs::create_dir_all(&normalized_local_path).map_err(|e| {
        tracing::error!("Failed to create entity directory: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Create entity. Prefer the session's proposed_colour (set at start_creation,
    // possibly overridden by the user via update_session_colour); fall back to a
    // deterministic pick if somehow missing or no longer in the palette.
    let id = Uuid::new_v4().to_string();
    let palette = entity_colour_palette();
    let colour = session
        .proposed_colour
        .as_ref()
        .filter(|c| {
            let normalized = c.to_ascii_lowercase();
            palette
                .iter()
                .any(|s| s.hex.to_ascii_lowercase() == normalized)
        })
        .cloned()
        .unwrap_or_else(|| pick_entity_colour(&id));
    let setup = EntitySetup {
        what_it_is: session.collected.profile.clone(),
        primary_goal: session.collected.goals.clone(),
        ..Default::default()
    };
    let mut entity = Entity {
        id,
        name: session.entity_name.clone(),
        description: session.collected.profile.clone(),
        department: None,
        owner: Some(workspace_id.to_string()),
        workspace_id: Some(workspace_id.to_string()),
        visibility: EntityVisibility::Workspace,
        status: "Learning".to_string(),
        entity_type: None,
        dept: "General".to_string(),
        file_count: 0,
        path: None,
        files: None,
        stage: session.collected.active_context.clone(),
        domains: vec![],
        markets: vec![],
        profile: session.collected.profile.clone(),
        goals: session.collected.goals.clone(),
        active_context: session.collected.active_context.clone(),
        operating_rules: session.collected.operating_rules.clone(),
        notes: session.collected.notes.clone(),
        content_pipeline: session.collected.content_pipeline.clone(),
        relationships: vec![],
        enrichment: None,
        resources: vec![],
        execution_policies: None,
        default_workspace_root: Some(normalized_local_path.clone()),
        memory_scope: None,
        preferred_model_tier: None,
        allowed_agent_groups: vec![],
        local_path: Some(normalized_local_path),
        setup,
        readiness: EntityReadiness::default(),
        entity_runtime: None,
        colour: Some(colour),
        ..Default::default()
    };
    refresh_creation_readiness(&mut entity);
    entity.local_path = crate::local_vault::ensure_entity_folder(&entity, workspace_id)
        .await
        .map(|p| p.to_string_lossy().to_string());

    control.entities.write().await.push(entity.clone());

    // Clean up session
    delete_session(&req.session_id);

    Ok(CompleteCreationResponse { entity })
}

/// The curated, accessible palette the entity-creation UI offers the user.
/// Single source of truth: `pick_entity_colour` draws from this list, and
/// `update_session_colour` validates overrides against it.
pub fn entity_colour_palette() -> Vec<ColourSwatch> {
    [
        ("#a78bfa", "Violet"),
        ("#22d3ee", "Cyan"),
        ("#34d399", "Emerald"),
        ("#f472b6", "Pink"),
        ("#fb923c", "Amber"),
        ("#facc15", "Yellow"),
        ("#60a5fa", "Sky"),
        ("#c084fc", "Purple"),
    ]
    .into_iter()
    .map(|(hex, name)| ColourSwatch {
        hex: hex.to_string(),
        name: name.to_string(),
    })
    .collect()
}

/// Deterministic accent-colour pick from the curated palette.
/// Same seed always returns the same colour so reloads stay stable.
pub(crate) fn pick_entity_colour(seed: &str) -> String {
    let palette = entity_colour_palette();
    let sum: u32 = seed.bytes().map(u32::from).sum();
    palette[(sum as usize) % palette.len()].hex.clone()
}

/// Get current question (with LLM enhancement if available)
fn get_question(session: &EntityCreationSession) -> (String, String) {
    if session.current_step >= BASE_QUESTIONS.len() {
        return (
            "Creation complete!".to_string(),
            "Your entity is ready.".to_string(),
        );
    }

    let (field, base_question) = BASE_QUESTIONS[session.current_step];
    let question = base_question.replace("{entity_name}", &session.entity_name);

    let help = match field {
        "profile" => "Describe what this entity is, what it does, and who it's for.",
        "goals" => "What should this entity achieve? Short and long-term goals.",
        "active_context" => "Current stage, challenges, opportunities. Where are you now?",
        "operating_rules" => "Guidelines, constraints, principles. How should work be done?",
        "notes" => "Anything else relevant. Can skip if nothing to add.",
        "content_pipeline" => {
            "How does it communicate? Newsletter, blog, social media? Or 'none yet'."
        }
        "local_path" => "Local path for files. E.g. ~/projects/myentity or ~/content/myentity.",
        _ => "",
    };

    (question, help.to_string())
}

/// Extract fields from user answer using LLM
async fn extract_fields_with_llm(
    config: &AppConfig,
    session: &mut EntityCreationSession,
    answer: &str,
) -> Result<Vec<String>, StatusCode> {
    let (base_url, api_key_env, model_id) =
        crate::config::resolve_llm(config, &config.llm_provider, &config.default_model)
            .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let api_key = resolve_llm_api_key(&api_key_env);
    let provider = build_provider(config, &base_url, &api_key, &api_key_env);

    let system_prompt = format!(
        r#"You are extracting entity narrative fields from a user's answer during entity creation.
Entity: {}
Current step: {} of {}

Extract information into these fields where applicable:
- profile: What is this entity? What does it do?
- goals: What should it achieve?
- active_context: Current situation, stage, challenges
- operating_rules: Rules, constraints, principles
- notes: Additional context
- content_pipeline: Communication strategy
- local_path: Local filesystem path

Return ONLY a JSON object with the fields you found. Empty fields should be omitted.
Example: {{"profile": "...", "goals": "..."}}"#,
        session.entity_name,
        session.current_step + 1,
        BASE_QUESTIONS.len(),
    );

    let messages = vec![
        ChatMessage {
            role: Role::System,
            content: system_prompt,
            tool_calls: None,
            tool_call_id: None,
        },
        ChatMessage {
            role: Role::User,
            content: answer.to_string(),
            tool_calls: None,
            tool_call_id: None,
        },
    ];

    let request = CompletionRequest {
        model: model_id,
        messages,
        tools: None,
        max_tokens: Some(1024),
        temperature: Some(0.2),
        stream: false,
    };

    match provider.complete(request).await {
        Ok(response) => {
            let content = response.content.unwrap_or_default();
            let mut auto_filled = Vec::new();

            // Try to parse JSON response
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
                && let Some(obj) = json.as_object()
            {
                for (key, value) in obj {
                    if let Some(s) = value.as_str()
                        && !s.trim().is_empty()
                    {
                        store_answer_for_field(&mut session.collected, key, s);
                        auto_filled.push(key.clone());
                    }
                }
            }

            Ok(auto_filled)
        }
        Err(e) => {
            tracing::warn!("LLM extraction failed: {}", e);
            // Fall back to storing answer in current field
            Ok(Vec::new())
        }
    }
}

// Thin wrapper so callers holding a whole session don't have to reach into
// `.collected` themselves.
fn store_answer(session: &mut EntityCreationSession, field: &str, answer: &str) {
    store_answer_for_field(&mut session.collected, field, answer);
}

// Field name -> draft slot. Unknown field names are silently ignored, since
// `field` always comes from `BASE_QUESTIONS` or the LLM extraction step.
fn store_answer_for_field(draft: &mut NarrativeDraft, field: &str, answer: &str) {
    match field {
        "profile" => draft.profile = Some(answer.to_string()),
        "goals" => draft.goals = Some(answer.to_string()),
        "active_context" => draft.active_context = Some(answer.to_string()),
        "operating_rules" => draft.operating_rules = Some(answer.to_string()),
        "notes" => draft.notes = Some(answer.to_string()),
        "content_pipeline" => draft.content_pipeline = Some(answer.to_string()),
        "local_path" => draft.local_path = Some(answer.to_string()),
        _ => {}
    }
}

// Score how execution-ready an entity is (0-100) and note the highest-value
// missing field. Mirrors what an operator would ask next; used both to label
// the entity in the UI and to pick the question `entity_heartbeat_defaults`
// asks when the daily beat becomes a setup question instead of a pulse.
fn refresh_creation_readiness(entity: &mut Entity) {
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
        || entity
            .profile
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
        || entity
            .active_context
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
            .is_some()
        || entity
            .local_path
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
        "asset" => {
            "Does this entity have a domain, email, codebase, or working folder yet?".to_string()
        }
        "director_context" => {
            "If this entity involves product or code, what stack or tools does it use?".to_string()
        }
        _ => "What should we know next about this entity?".to_string(),
    });
    entity.readiness = EntityReadiness {
        percent: score.min(100),
        label,
        next_question,
        missing_fields: missing,
    };
}

fn build_provider(
    config: &AppConfig,
    base_url: &str,
    api_key: &str,
    api_key_env: &str,
) -> Box<dyn LlmProvider> {
    // One resolver for every execution path. This used to be a local copy that
    // predated the claude-code provider, so background runs (tasks, enrichment,
    // missions) tried to POST chat completions to `claude-code://` as if it
    // were an OpenAI server and died with "failed to call OpenAI chat
    // completions" while interactive chat worked fine. Delegating keeps the
    // provider matrix in exactly one place: `capture::build_llm_client`.
    crate::capture::build_llm_client(config, base_url, api_key, api_key_env)
}

fn slugify(input: &str) -> String {
    input
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join("-")
}

fn expand_tilde(path: &str) -> String {
    consistent_world_common::platform::expand_tilde(path)
}

fn app_data_dir() -> PathBuf {
    // No home directory at all is a broken environment; the temp dir keeps the
    // gateway usable rather than writing into whatever the cwd happens to be.
    consistent_world_common::platform::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".consistent-world")
}

fn app_entities_root() -> PathBuf {
    app_data_dir().join("entities")
}

/// Resolve whatever the operator typed for `local_path` into an absolute
/// directory: blank -> a slugified folder under the app's entities root,
/// absolute -> kept as-is, relative -> re-rooted under the app's entities
/// root rather than the process's working directory.
fn normalize_local_path_input(local_path: &str, entity_name: &str) -> String {
    let trimmed = local_path.trim();
    if trimmed.is_empty() {
        return app_entities_root()
            .join(slugify(entity_name))
            .to_string_lossy()
            .to_string();
    }

    let expanded = expand_tilde(trimmed);
    let as_path = Path::new(&expanded);
    if as_path.is_absolute() {
        return as_path.to_string_lossy().to_string();
    }

    // Relative inputs are interpreted as logical names under app-owned storage.
    app_entities_root()
        .join(slugify(trimmed))
        .to_string_lossy()
        .to_string()
}

/// Load sessions into control state on startup (migration)
pub fn load_sessions_into_control(control: &ControlState) {
    let _ = control;
    let sessions = load_all_sessions();
    for session in sessions {
        if !session.complete {
            // Keep incomplete sessions available for resume
            tracing::info!(
                session_id = %session.id,
                entity_name = %session.entity_name,
                "Loaded incomplete creation session"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_minted_session_ids_are_accepted() {
        assert!(is_valid_session_id("6f1c1d2e-3b4a-4c5d-8e9f-0a1b2c3d4e5f"));
        for bad in [
            "../vaults/abc",
            "..%2Fvaults%2Fabc",
            "..",
            "",
            "6f1c1d2e3b4a4c5d8e9f0a1b2c3d4e5f",
            "{6f1c1d2e-3b4a-4c5d-8e9f-0a1b2c3d4e5f}",
            "urn:uuid:6f1c1d2e-3b4a-4c5d-8e9f-0a1b2c3d4e5f",
            "6f1c1d2e-3b4a-4c5d-8e9f-0a1b2c3d4e5/",
            "6f1c1d2e-3b4a-4c5d-8e9f/../../../../x",
        ] {
            assert!(!is_valid_session_id(bad), "{bad:?} must be refused");
            assert_eq!(session_path(bad).unwrap_err(), StatusCode::BAD_REQUEST);
        }
        let path = session_path("6f1c1d2e-3b4a-4c5d-8e9f-0a1b2c3d4e5f").unwrap();
        assert_eq!(path.parent().unwrap(), Path::new(&sessions_dir()));
    }

    #[test]
    fn a_creation_session_belongs_to_whoever_started_it() {
        let session = EntityCreationSession {
            id: "6f1c1d2e-3b4a-4c5d-8e9f-0a1b2c3d4e5f".into(),
            entity_name: "Acme".into(),
            workspace_id: "ws-a".into(),
            collected: NarrativeDraft::default(),
            answered_fields: Vec::new(),
            current_step: 0,
            complete: false,
            created_at: String::new(),
            updated_at: String::new(),
            proposed_colour: None,
            owner_principal_id: Some("alice".into()),
        };
        let alice = SessionOwner {
            workspace_id: "ws-a",
            principal_id: "alice",
        };
        let bob_same_ws = SessionOwner {
            workspace_id: "ws-a",
            principal_id: "bob",
        };
        let mallory = SessionOwner {
            workspace_id: "ws-m",
            principal_id: "alice",
        };
        assert!(session.is_owned_by(alice));
        assert!(!session.is_owned_by(bob_same_ws));
        assert!(!session.is_owned_by(mallory));

        // Legacy sessions with no recorded principal fall back to the workspace.
        let legacy = EntityCreationSession {
            owner_principal_id: None,
            ..session
        };
        assert!(legacy.is_owned_by(bob_same_ws));
        assert!(!legacy.is_owned_by(mallory));
    }

    #[test]
    fn normalize_relative_path_under_app_entities_root() {
        let normalized = normalize_local_path_input("acme-launch", "Acme Launch");
        assert!(normalized.contains("/entities/acme-launch"));
        assert!(Path::new(&normalized).is_absolute());
    }

    #[test]
    fn normalize_keeps_absolute_path() {
        let input = "/tmp/entity-work";
        let normalized = normalize_local_path_input(input, "Entity");
        assert_eq!(normalized, input);
    }

    #[test]
    fn readiness_counts_chat_creation_foundation() {
        let mut entity = Entity {
            id: "entity-1".to_string(),
            name: "Builder Product".to_string(),
            profile: Some("A full-stack product being actively built.".to_string()),
            goals: Some("Ship a usable first version.".to_string()),
            active_context: Some("Core harness is being prepared.".to_string()),
            local_path: Some("/tmp/builder-product".to_string()),
            setup: EntitySetup {
                what_it_is: Some("A full-stack product being actively built.".to_string()),
                primary_goal: Some("Ship a usable first version.".to_string()),
                ..Default::default()
            },
            status: "Learning".to_string(),
            dept: "General".to_string(),
            ..Default::default()
        };

        refresh_creation_readiness(&mut entity);

        assert!(entity.readiness.percent >= 70);
        assert_eq!(entity.readiness.label, "execution ready");
        assert!(
            !entity
                .readiness
                .missing_fields
                .contains(&"what_it_is".to_string())
        );
        assert!(
            !entity
                .readiness
                .missing_fields
                .contains(&"primary_goal".to_string())
        );
        assert!(
            !entity
                .readiness
                .missing_fields
                .contains(&"asset".to_string())
        );
    }

    #[test]
    fn store_answer_for_field_mutates_the_session_draft() {
        let mut session = EntityCreationSession {
            id: "session-1".to_string(),
            entity_name: "Client Account".to_string(),
            workspace_id: "default".to_string(),
            collected: NarrativeDraft::default(),
            answered_fields: Vec::new(),
            current_step: 0,
            complete: false,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            proposed_colour: None,
            owner_principal_id: None,
        };

        store_answer(&mut session, "goals", "Keep every client next step warm.");

        assert_eq!(
            session.collected.goals.as_deref(),
            Some("Keep every client next step warm.")
        );
    }
}
