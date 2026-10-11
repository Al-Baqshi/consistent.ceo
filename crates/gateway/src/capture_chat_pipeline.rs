//! Conversational layer for `POST /api/capture/chat`: router + actions, then LLM natural reply.

use crate::capture::{
    CaptureChatMessage, CaptureChatRequest, CaptureChatResponse, ChatAction, ChatAnalysis,
    build_llm_client, entity_workspace_id, extract_key_points, resolve_operations_llm,
};
use crate::config::AppConfig;
use crate::control_api::{CreateTaskRequest, internal_create_task};
use crate::credentials_resolver::resolve_llm_api_key;
use crate::state::{ControlState, Entity};
use axum::http::StatusCode;
use consistent_world_llm::client::{ChatMessage, CompletionRequest, Role};
use consistent_world_llm::cost::CostCalculator;

/// Outcome of routing + backend actions before natural-language synthesis.
#[derive(Debug, Clone)]
pub(crate) struct ChatPipelineOutcome {
    pub session_id: String,
    pub interpreter_context: String,
    pub entity_context: Option<Entity>,
    pub intake_decision: crate::intake_policy::IntakeDecision,
    pub routing_result: crate::routing_engine::RoutingResult,
    pub key_points: Vec<String>,
    pub readiness_note: Option<String>,
    pub actions: Vec<ChatAction>,
    pub machine_message: String,
    pub suggestions: Vec<String>,
    pub needs_followup: bool,
    pub user_message: String,
    pub history: Vec<CaptureChatMessage>,
    pub workspace_context: Option<String>,
}

/// Runs intake → routing → task/note creation (machine layer).
pub(crate) async fn run_chat_pipeline(
    state: &crate::state::ServerState,
    control: &ControlState,
    workspace_id: &str,
    req: CaptureChatRequest,
) -> Result<ChatPipelineOutcome, StatusCode> {
    use crate::intake_policy::{IntakeRequest, make_intake_decision};
    use crate::memory_buffer::{self, EntitySnapshot};
    use crate::routing_engine::{RoutingRequest, make_routing_decision};

    let session_id = req
        .session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let user_message = req.message.clone();
    let history = req.history.clone();
    let workspace_context = req.workspace_context.clone();
    let entity_id = req.entity_id.clone();

    let entity_context = if let Some(eid) = &entity_id {
        let entities = control.entities.read().await;
        entities
            .iter()
            .find(|e| e.id == *eid && entity_workspace_id(e) == workspace_id && !e.is_personal)
            .cloned()
    } else {
        None
    };

    let snapshot = if let Some(ref entity) = entity_context {
        EntitySnapshot {
            entity_id: entity.id.clone(),
            name: entity.name.clone(),
            persona_narrative: entity
                .profile
                .clone()
                .unwrap_or_else(|| "You are a helpful assistant.".to_string()),
            current_goal: entity
                .goals
                .clone()
                .unwrap_or_else(|| "Assist the user.".to_string()),
            tone_constraints: vec![
                "Conversational".to_string(),
                "Helpful".to_string(),
                "Direct".to_string(),
            ],
        }
    } else {
        EntitySnapshot {
            entity_id: String::new(),
            name: "Workspace".to_string(),
            persona_narrative: "You are Consistent's workspace assistant — conversational like Claude Code or a capable agent IDE companion: pair with the user, explain clearly, no robotic ticketing tone.".to_string(),
            current_goal: "Answer naturally; when there's no project hub selected, gently orient users on how Consistent works so nothing feels mysterious.".to_string(),
            tone_constraints: vec![
                "Collaborative".to_string(),
                "Clear".to_string(),
                "Efficient".to_string(),
            ],
        }
    };

    memory_buffer::ensure_session(&state.session_cache, &session_id, snapshot, 30);
    if let Some(mut buf) = state.session_cache.get_mut(&session_id) {
        buf.push_turn("user", &user_message);
    }

    let interpreter_context = state
        .session_cache
        .get(&session_id)
        .map(|b| b.to_interpreter_prompt())
        .unwrap_or_default();

    let intake_req = IntakeRequest {
        text: user_message.clone(),
        entity_id: entity_id.clone(),
        intent_hint: None,
        workspace_id: workspace_id.to_string(),
    };

    let tasks = control.tasks.read().await.clone();
    let missions = control.missions.read().await.clone();
    let intake_decision = make_intake_decision(&intake_req, &tasks, &missions);

    let routing_req = RoutingRequest {
        entity_id: entity_id.clone(),
        intake_decision: intake_decision.clone(),
        task_kind: match &intake_decision.classification {
            crate::intake_policy::IntakeClassification::Note => "note".to_string(),
            crate::intake_policy::IntakeClassification::Task => "task".to_string(),
            crate::intake_policy::IntakeClassification::Reminder => "reminder".to_string(),
            crate::intake_policy::IntakeClassification::Mission => "mission".to_string(),
            _ => "general".to_string(),
        },
        workspace_id: workspace_id.to_string(),
    };

    let routing_result = make_routing_decision(&routing_req, entity_context.as_ref());

    let key_points = extract_key_points(&user_message);
    let readiness_note = entity_context.as_ref().map(|e| {
        format!(
            "Entity readiness: {} ({:.0}%)",
            e.readiness.label, e.readiness.percent
        )
    });

    let synthetic_req = CaptureChatRequest {
        message: user_message.clone(),
        entity_id,
        scope: req.scope.clone(),
        timezone_offset_minutes: req.timezone_offset_minutes,
        workspace_context: workspace_context.clone(),
        history: history.clone(),
        session_id: Some(session_id.clone()),
    };

    let (actions, machine_message, suggestions, needs_followup) = execute_chat_action(
        control,
        workspace_id,
        &synthetic_req,
        &routing_result,
        &intake_decision,
    )
    .await?;

    Ok(ChatPipelineOutcome {
        session_id,
        interpreter_context,
        entity_context,
        intake_decision,
        routing_result,
        key_points,
        readiness_note,
        actions,
        machine_message,
        suggestions,
        needs_followup,
        user_message,
        history,
        workspace_context,
    })
}

pub(crate) fn chat_analysis_from_pipeline(pipeline: &ChatPipelineOutcome) -> ChatAnalysis {
    ChatAnalysis {
        intent: intake_classification_label(&pipeline.intake_decision.classification),
        confidence: pipeline.intake_decision.confidence,
        entity_context: pipeline.entity_context.as_ref().map(|e| e.name.clone()),
        key_points: pipeline.key_points.clone(),
        readiness_note: pipeline.readiness_note.clone(),
    }
}

pub(crate) fn capture_response_from_pipeline(
    pipeline: &ChatPipelineOutcome,
    message: String,
) -> CaptureChatResponse {
    CaptureChatResponse {
        message,
        analysis: chat_analysis_from_pipeline(pipeline),
        actions: pipeline.actions.clone(),
        suggestions: pipeline.suggestions.clone(),
        needs_followup: pipeline.needs_followup,
        session_id: Some(pipeline.session_id.clone()),
        routing: None,
        // This path reports the actions it took rather than the tools it
        // called, so `Observed` is inferred from whether anything was actually
        // done this turn.
        evidence: Some(crate::evidence::ChatEvidence::from_tool_calls(
            pipeline
                .actions
                .iter()
                .map(|a| (a.action_type.as_str(), false)),
        )),
    }
}

fn intake_classification_label(c: &crate::intake_policy::IntakeClassification) -> String {
    use crate::intake_policy::IntakeClassification;
    match c {
        IntakeClassification::Note => "note".to_string(),
        IntakeClassification::Task => "task".to_string(),
        IntakeClassification::Reminder => "reminder".to_string(),
        IntakeClassification::Mission => "mission".to_string(),
        IntakeClassification::FollowUp { .. } => "follow_up".to_string(),
        IntakeClassification::Update { .. } => "update".to_string(),
        IntakeClassification::Clarify { .. } => "clarify".to_string(),
    }
}

fn routing_decision_summary(d: &crate::routing_engine::RoutingDecision) -> String {
    use crate::routing_engine::RoutingDecision;
    match d {
        RoutingDecision::Execute {
            director_id,
            task_kind,
            confidence,
        } => format!(
            "execute via director {}, task_kind={}, routing_confidence={:.2}",
            director_id, task_kind, confidence
        ),
        RoutingDecision::Queue { reason, .. } => format!("queued: {}", reason),
        RoutingDecision::Review { reason, .. } => format!("needs_review: {}", reason),
    }
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect::<String>() + "…"
}

/// Core persona — expanded below with product + UI-aware guidance.
fn chat_natural_system_prompt() -> &'static str {
    "You are Consistent's assistant in chat form — continuous dialogue like ChatGPT or a pairing assistant such as Claude Code: reasoning aloud briefly when helpful, no jargon dumps unless asked."
}

// System-prompt fragments assembled conditionally in
// build_synthesis_messages_system below.
const PRODUCT_AND_CAPABILITY_SUMMARY: &str = r#"What Consistent is (explain naturally when the user seems lost or asks meta questions):
Consistent is an AI workspace. Users chat freely here; when something reads like a note, task, or reminder, the system may quietly capture it. Entities (projects / hubs) tie chats and execution to a specific brand or initiative — creating or selecting one unlocks richer context and scoped workflows."#;

const PAIR_ASSISTANT_STYLE: &str = r#"Conversation vibe:
Collaborative pair programmer / ops partner — crisp paragraphs; bullets only when listing steps or options; acknowledge uncertainty briefly instead of hedging forever."#;

const NO_ENTITY_SELECTED_GUIDANCE: &str = r#"Session shape — NO ENTITY SELECTED:
The user has not picked a project hub yet (perfectly okay). Continue fluid ChatGPT-style help: brainstorming, explanations, planning.
Orient softly once when they're clearly new or ask what this is: Consistent = workspace chat plus lightweight capture; entities anchor deeper work.
Suggest picking/creating an entity only when they want persistent project memory or scoped tasks — never preach every single reply."#;

// Compose the synthesis system prompt: persona + style + product blurb, plus
// session context and a no-entity hint when relevant.
fn build_synthesis_messages_system(pipeline: &ChatPipelineOutcome) -> String {
    let mut sys = chat_natural_system_prompt().to_string();
    sys.push_str("\n\n");
    sys.push_str(PAIR_ASSISTANT_STYLE);
    sys.push_str("\n\n");
    sys.push_str(PRODUCT_AND_CAPABILITY_SUMMARY);
    if pipeline.entity_context.is_none() {
        sys.push_str("\n\n");
        sys.push_str(NO_ENTITY_SELECTED_GUIDANCE);
    }
    if !pipeline.interpreter_context.is_empty() {
        sys.push_str("\n\n--- Session context ---\n");
        sys.push_str(&truncate_chars(&pipeline.interpreter_context, 6000));
    }
    sys
}

// Build the full message list for the natural-language synthesis call:
// system prompt, trimmed history, then the pipeline's synthesis payload.
fn build_chat_synthesis_messages(pipeline: &ChatPipelineOutcome) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let sys = build_synthesis_messages_system(pipeline);
    messages.push(ChatMessage {
        role: Role::System,
        content: sys,
        tool_calls: None,
        tool_call_id: None,
    });

    let start = pipeline.history.len().saturating_sub(20);
    for m in &pipeline.history[start..] {
        let role = match m.role.as_str() {
            "assistant" => Role::Assistant,
            "user" => Role::User,
            _ => continue,
        };
        if m.content.trim().is_empty() {
            continue;
        }
        messages.push(ChatMessage {
            role,
            content: m.content.clone(),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    messages.push(ChatMessage {
        role: Role::User,
        content: build_synthesis_user_payload(pipeline),
        tool_calls: None,
        tool_call_id: None,
    });
    messages
}

// Assemble the user-turn payload that hands the LLM everything it needs to
// narrate this turn: the message, classifier/router reasoning, actions
// taken, and a closing style instruction.
fn build_synthesis_user_payload(pipeline: &ChatPipelineOutcome) -> String {
    let first_turn_in_thread = pipeline.history.is_empty();
    let mut parts = vec![format!(
        "The user's latest message:\n{}",
        pipeline.user_message.trim()
    )];
    if let Some(ws) = pipeline
        .workspace_context
        .as_ref()
        .filter(|s| !s.trim().is_empty())
    {
        parts.push(format!("Workspace / UI context:\n{}", ws.trim()));
    }
    if pipeline.entity_context.is_none() {
        parts.push(
            "Internal UI flag (do not quote verbatim): No entity selected — answers still behave like a full conversational assistant; mention hubs only when it genuinely helps."
                .to_string(),
        );
    }
    if first_turn_in_thread && pipeline.entity_context.is_none() {
        parts.push(
            "This appears to be an early message in the thread with no entity hub selected — if the user's intent is vague, one brief welcoming orientation sentence about Consistent is acceptable alongside answering."
                .to_string(),
        );
    }
    parts.push(format!(
        "Classifier intent: {}\nClassifier reasoning:\n{}",
        intake_classification_label(&pipeline.intake_decision.classification),
        pipeline.intake_decision.reasoning
    ));
    parts.push(format!(
        "Router reasoning:\n{}\nRouter decision: {}",
        pipeline.routing_result.reasoning,
        routing_decision_summary(&pipeline.routing_result.decision)
    ));
    if !pipeline.actions.is_empty() {
        let aj =
            serde_json::to_string_pretty(&pipeline.actions).unwrap_or_else(|_| "[]".to_string());
        parts.push(format!(
            "Actions the backend performed (summarize outcomes for the user; do not read JSON verbatim):\n{}",
            aj
        ));
    }
    parts.push(format!(
        "Backend status summary (internal wording — rewrite as natural speech):\n{}",
        pipeline.machine_message
    ));
    if !pipeline.suggestions.is_empty() {
        parts.push(format!(
            "Suggested next steps (optional):\n{}",
            pipeline.suggestions.join("\n")
        ));
    }
    if pipeline.needs_followup {
        parts.push(
            "The system flagged that follow-up may be needed; guide the user clearly.".to_string(),
        );
    }
    if !pipeline.intake_decision.duplicate_warnings.is_empty() {
        let dw = serde_json::to_string_pretty(&pipeline.intake_decision.duplicate_warnings)
            .unwrap_or_default();
        parts.push(format!("Possible duplicate items detected:\n{}", dw));
    }
    parts.push(
        "Write your reply to the user now. Sound human and concise — same tier as ChatGPT / Claude for continuity and reasoning. \
If they asked a general or technical question, answer it directly (don't force product lecture into unrelated answers). \
If something was queued or blocked, explain gently and say what they can do next. \
Do not use leading symbols like ✓ ⏸ ❓ ⚠ unless the user did. \
Do not expose raw JSON, internal IDs, or field names unless the user needs them."
            .to_string(),
    );
    parts.join("\n\n")
}

// Call the LLM to turn the pipeline's outcome into a natural reply, falling
// back to a humanized machine summary when the model returns nothing.
async fn synthesize_natural_reply(
    state: &crate::state::ServerState,
    config: &AppConfig,
    control: &ControlState,
    workspace_id: &str,
    pipeline: &ChatPipelineOutcome,
) -> Result<String, StatusCode> {
    let messages = build_chat_synthesis_messages(pipeline);
    let resolved = resolve_operations_llm(config, control)
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let api_key = resolve_llm_api_key(&resolved.api_key_env);
    let provider = build_llm_client(config, &resolved.base_url, &api_key, &resolved.api_key_env);

    let request = CompletionRequest {
        model: resolved.model,
        messages,
        tools: None,
        max_tokens: Some(1024),
        temperature: Some(0.65),
        stream: false,
    };

    let response = provider.complete(request).await.map_err(|e| {
        tracing::warn!(error = %e, "capture chat synthesis LLM failed");
        StatusCode::BAD_GATEWAY
    })?;

    if let Some(ledger) = state.credit_ledger.as_ref() {
        let calc = CostCalculator::new();
        let raw_cost = calc.calculate(
            &response.model,
            response.usage.input_tokens,
            response.usage.output_tokens,
        );
        if raw_cost > 0.0 {
            let _ = ledger.debit_for_cost(workspace_id, raw_cost).await;
        }
    }

    let text = response.content.unwrap_or_default().trim().to_string();
    if text.is_empty() {
        Ok(humanize_machine_summary(&pipeline.machine_message))
    } else {
        Ok(text)
    }
}

/// Strip status glyphs (✓⏸❓⚠) from a machine-generated summary so it reads
/// like prose, for use when natural-language synthesis is unavailable.
pub(crate) fn humanize_machine_summary(raw: &str) -> String {
    let cleaned = raw
        .replace(['✓', '⏸', '❓', '⚠'], "")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if cleaned.trim().is_empty() {
        "I've handled that on my side — tell me if you want more detail.".to_string()
    } else {
        cleaned
    }
}

/// Full `/api/capture/chat` pipeline: run intake + routing + actions, then
/// synthesize the natural-language reply.
pub(crate) async fn chat_capture_full(
    state: &crate::state::ServerState,
    control: &ControlState,
    workspace_id: &str,
    req: CaptureChatRequest,
) -> Result<CaptureChatResponse, StatusCode> {
    let cfg = state
        .app_config
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let pipeline = run_chat_pipeline(state, control, workspace_id, req).await?;
    let natural = synthesize_natural_reply(state, cfg, control, workspace_id, &pipeline).await?;

    if let Some(mut buf) = state.session_cache.get_mut(&pipeline.session_id) {
        buf.push_turn("assistant", &natural);
    }

    let _ = crate::memory_buffer::detect_intent_flag(&natural);

    Ok(capture_response_from_pipeline(&pipeline, natural))
}

// Apply the router's decision: create a note/task for Execute (or a
// Queue-routed note), otherwise just narrate the queue/review outcome.
async fn execute_chat_action(
    control: &ControlState,
    workspace_id: &str,
    req: &CaptureChatRequest,
    routing_result: &crate::routing_engine::RoutingResult,
    intake_decision: &crate::intake_policy::IntakeDecision,
) -> Result<(Vec<ChatAction>, String, Vec<String>, bool), StatusCode> {
    use crate::routing_engine::RoutingDecision;

    let mut actions = Vec::new();
    let mut suggestions = Vec::new();
    let mut needs_followup = false;
    let mut response_parts: Vec<String> = Vec::new();

    match &routing_result.decision {
        RoutingDecision::Execute {
            director_id,
            task_kind,
            confidence,
        } => match &intake_decision.classification {
            crate::intake_policy::IntakeClassification::Note => {
                let title = req.message.chars().take(100).collect::<String>();
                let task_req = CreateTaskRequest {
                    title,
                    dept: Some("General".to_string()),
                    roles: None,
                    entity_id: req.entity_id.clone(),
                    mission_id: None,
                    priority: None,
                    requires_human_approval: None,
                    task_kind: Some("note".to_string()),
                    content: Some(req.message.clone()),
                    director_id: Some(director_id.clone()),
                    agent_id: None,
                    connection_id: None,
                    external_source: None,
                    external_id: None,
                    metadata: None,
                    due_at: None,
                };

                match internal_create_task(control, workspace_id, None, task_req).await {
                    Ok(task) => {
                        actions.push(ChatAction {
                            action_type: "create_note".to_string(),
                            description: "Note captured and stored".to_string(),
                            result_id: Some(task.id.clone()),
                            status: "completed".to_string(),
                        });
                        response_parts.push("Note captured (stored as a note task).".to_string());
                    }
                    Err(_) => {
                        response_parts.push(
                                "Could not save the note automatically — please try again or create it from Tasks."
                                    .to_string(),
                            );
                        needs_followup = true;
                    }
                }
            }
            crate::intake_policy::IntakeClassification::Task
            | crate::intake_policy::IntakeClassification::Reminder
            | crate::intake_policy::IntakeClassification::Mission => {
                let title = req.message.chars().take(100).collect::<String>();
                let task_req = CreateTaskRequest {
                    title,
                    dept: Some("General".to_string()),
                    roles: None,
                    entity_id: req.entity_id.clone(),
                    mission_id: None,
                    priority: None,
                    requires_human_approval: None,
                    task_kind: Some(task_kind.clone()),
                    content: Some(req.message.clone()),
                    director_id: Some(director_id.clone()),
                    agent_id: None,
                    connection_id: None,
                    external_source: None,
                    external_id: None,
                    metadata: None,
                    due_at: None,
                };

                match internal_create_task(control, workspace_id, None, task_req).await {
                    Ok(task) => {
                        actions.push(ChatAction {
                            action_type: "create_task".to_string(),
                            description: format!("Task queued for {}", director_id),
                            result_id: Some(task.id.clone()),
                            status: "queued".to_string(),
                        });
                        response_parts.push(format!(
                            "Task created and queued (routing confidence {:.0}%).",
                            confidence * 100.0
                        ));
                        suggestions.push("Track task progress in the dashboard.".to_string());
                    }
                    Err(_) => {
                        response_parts.push("Task creation failed.".to_string());
                        needs_followup = true;
                    }
                }
            }
            _ => {
                response_parts
                    .push("Received your message; no automatic task type matched.".to_string());
                needs_followup = true;
            }
        },
        RoutingDecision::Queue {
            reason,
            missing_fields,
            current_readiness,
            required_readiness,
            suggested_action,
        } => {
            if matches!(
                intake_decision.classification,
                crate::intake_policy::IntakeClassification::Note
            ) {
                let title = req.message.chars().take(100).collect::<String>();
                let task_req = CreateTaskRequest {
                    title,
                    dept: Some("General".to_string()),
                    roles: None,
                    entity_id: req.entity_id.clone(),
                    mission_id: None,
                    priority: None,
                    requires_human_approval: None,
                    task_kind: Some("note".to_string()),
                    content: Some(req.message.clone()),
                    director_id: Some("operations".to_string()),
                    agent_id: None,
                    connection_id: None,
                    external_source: None,
                    external_id: None,
                    metadata: None,
                    due_at: None,
                };

                match internal_create_task(control, workspace_id, None, task_req).await {
                    Ok(task) => {
                        actions.push(ChatAction {
                            action_type: "create_note".to_string(),
                            description: "Note saved to workspace".to_string(),
                            result_id: Some(task.id.clone()),
                            status: "completed".to_string(),
                        });
                        response_parts.push(format!("Note recorded (router note: {}).", reason));
                    }
                    Err(_) => {
                        response_parts.push("Could not save the note automatically.".to_string());
                        needs_followup = true;
                    }
                }
            } else {
                response_parts.push(format!("Queued: {}", reason));

                if !missing_fields.is_empty() {
                    response_parts.push(format!(
                        "Entity readiness: {:.0}% (need {:.0}%)",
                        current_readiness, required_readiness
                    ));
                    response_parts.push(format!("Missing: {}", missing_fields.join(", ")));
                    suggestions.push(suggested_action.clone());
                }

                needs_followup = true;
            }
        }
        RoutingDecision::Review {
            reason,
            ambiguities,
        } => {
            response_parts.push("Needs clarification.".to_string());
            response_parts.push(format!("Reason: {}", reason));

            for ambiguity in ambiguities {
                suggestions.push(format!("Clarify: {}", ambiguity));
            }

            needs_followup = true;
        }
    }

    if !intake_decision.duplicate_warnings.is_empty() {
        response_parts.push("Similar existing items found:".to_string());
        for dup in &intake_decision.duplicate_warnings {
            response_parts.push(format!(
                "  - {} ({}% similar)",
                dup.existing_title,
                (dup.similarity * 100.0) as u8
            ));
        }
        suggestions.push("Review existing items to avoid duplication.".to_string());
    }

    let response_message = if response_parts.is_empty() {
        "Processing complete.".to_string()
    } else {
        response_parts.join("\n")
    };

    Ok((actions, response_message, suggestions, needs_followup))
}

#[cfg(test)]
mod tests {
    use super::humanize_machine_summary;

    #[test]
    fn humanize_strips_console_symbols() {
        let cleaned = humanize_machine_summary("✓ Note captured\n⏸ Queued: busy");
        assert!(!cleaned.contains('✓'));
        assert!(!cleaned.contains('⏸'));
        assert!(cleaned.contains("Note captured"));
    }
}
