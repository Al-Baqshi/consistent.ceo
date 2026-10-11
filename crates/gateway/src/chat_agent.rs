//! Tool-using chat loop driving `/api/capture/chat`.
//!
//! Replaces the previous "rules engine → write to DB → ask LLM to narrate" pipeline.
//! The LLM now decides whether to answer naturally, ask a clarifying question, or
//! call one of the workspace tools (`create_note`, `create_task`, `set_reminder`,
//! `list_entities`, `recall_recent`) declared in [`chat_tools`].

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::http::StatusCode;
use consistent_world_common::types::{ToolCall, ToolResult};
use consistent_world_llm::client::{
    ChatMessage, CompletionRequest, LlmProvider, Role, ToolDefinition,
};
use consistent_world_llm::cost::CostCalculator;
use consistent_world_llm::streaming::StreamEvent;
use consistent_world_tools::Tool;
use futures::StreamExt;
use serde_json::json;
use std::convert::Infallible;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

use crate::capture::{
    CaptureChatMessage, CaptureChatRequest, CaptureChatResponse, ChatAction, ChatAnalysis,
    build_llm_client, entity_workspace_id, resolve_operations_llm,
};
use crate::chat_tools::{build_workspace_tools, tool_effect_from_output};
use crate::config::AppConfig;
use crate::credentials_resolver::resolve_llm_api_key;
use crate::local_vault::{self, ChatTurn};
use crate::memory_buffer::{self, EntitySnapshot};
use crate::prompt_assembly::{CanonicalInputs, assemble_system_prompt};
use crate::state::{ControlState, Entity};

const MAX_TOOL_ITERATIONS: usize = 6;
const HISTORY_TURNS: usize = 20;

/// System prompt: lean, natural, capability-aware. Single coherent voice — no
/// 5-block concatenation of internal scaffolding like the old pipeline.
fn system_prompt(entity: Option<&Entity>) -> String {
    let mut s = String::new();
    s.push_str(
        "You are Consistent CEO — the user's chief of staff across every venture, project and person they run. Work like the best operator they could hire: calm, precise, proactive, and honest about what is and isn't done. Warm and direct; no filler, no robotic ticketing tone.\n\n\
         You can help with anything in plain conversation. When the user clearly wants something captured, scheduled, or actioned in their workspace, you have tools to do that:\n\
         - `create_note` — save an idea, observation, or piece of context the user wants kept.\n\
         - `create_task` — create an actionable task.\n\
         - `set_reminder` — set a reminder with optional natural-language time.\n\
         - `create_entity` — create a new entity (project/business/goal hub) when the user asks to start or set one up.\n\
         - `create_workspace` — create a whole new workspace/tenant; only on an explicit ask, never for a project.\n\
         - `review_today` — read the factual operating brief before answering what is due or important.\n\
         - `update_task` — complete, cancel, reopen, block, snooze, or reschedule an existing task.\n\
         - `update_entity` — update durable goals, context, rules, or notes.\n\
         - `list_heartbeats` / `manage_heartbeat` — inspect and explicitly manage recurring automations.\n\
         - `list_entities` — list the user's projects/hubs.\n\
         - `recall_recent` — recall their most recent items.\n\
         - `load_skill` — load one of your skills (listed under \"Skills\" when you have any) before doing the work it covers.\n\
         - `save_skill` — save a procedure the user approved as a reusable skill.\n\n\
         Rules of engagement:\n\
         1. Default to plain conversation. For priorities, status, what changed, or what is due, call `review_today` first so your answer is grounded in stored state. Only call mutation tools when intent is clear; for ambiguity, ask one concise question.\n\
         2. If the user says something ambiguous (\"this is interesting\" alone, with no context), ASK before saving.\n\
         3. After a tool succeeds, confirm briefly in human language. Don't recite IDs or JSON. One short sentence is enough.\n\
         4. Never invent IDs, links, or capabilities you don't have. If something fails, say so plainly.\n\
         5. Match the user's register. If they're casual, be casual. If they're shipping work, be crisp.\n\
         6. When an answer spans several ventures, lead each line with the venture's name so the user can scan across companies.",
    );
    if let Some(e) = entity {
        s.push_str("\n\nActive project context:\n");
        s.push_str(&format!("- Name: {}\n", e.name));
        if let Some(desc) = &e.description
            && !desc.trim().is_empty()
        {
            s.push_str(&format!("- Description: {}\n", desc.trim()));
        }
        if let Some(goals) = &e.goals
            && !goals.trim().is_empty()
        {
            s.push_str(&format!("- Current goal: {}\n", goals.trim()));
        }
        if let Some(profile) = &e.profile
            && !profile.trim().is_empty()
        {
            s.push_str(&format!("- Persona / voice: {}\n", profile.trim()));
        }
        s.push_str(
            "Anything the user asks to save defaults to this project unless they say otherwise.",
        );
    } else {
        s.push_str(
            "\n\nNo project is selected. That's fine — answer freely. Mention picking a project only if the user wants persistent scoped work, never as a preamble.",
        );
    }
    s
}

// Convert each registered Tool into the wire-format ToolDefinition the LLM API expects.
fn tool_defs(tools: &[Box<dyn Tool>]) -> Vec<ToolDefinition> {
    tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name().to_string(),
            description: t.description().to_string(),
            parameters: t.parameters_schema(),
        })
        .collect()
}

fn find_tool<'a>(tools: &'a [Box<dyn Tool>], name: &str) -> Option<&'a dyn Tool> {
    tools.iter().find(|t| t.name() == name).map(|t| t.as_ref())
}

// Read-only tools are safe to replay from the per-turn cache; everything else
// counts as a side effect and must run at most once per unique call.
fn tool_is_side_effecting(name: &str) -> bool {
    !matches!(
        name,
        "list_entities" | "recall_recent" | "review_today" | "list_heartbeats" | "load_skill"
    )
}

// Run one tool call, reusing the cached result when a side-effecting call
// with identical arguments already ran earlier this turn. Returns the result
// plus whether it was actually executed (vs. replayed from cache).
async fn execute_one_tool(
    tools: &[Box<dyn Tool>],
    call: &ToolCall,
    cache: &mut HashMap<String, ToolResult>,
) -> (ToolResult, bool) {
    let cache_key = if tool_is_side_effecting(&call.name) {
        Some(format!(
            "{}:{}",
            call.name,
            serde_json::to_string(&call.arguments).unwrap_or_default()
        ))
    } else {
        None
    };
    if let Some(key) = cache_key.as_deref()
        && let Some(cached) = cache.get(key)
    {
        let mut replay = cached.clone();
        replay.call_id = call.id.clone();
        return (replay, false);
    }
    let result = match find_tool(tools, &call.name) {
        Some(tool) => match tool.execute(call.arguments.clone()).await {
            Ok(mut r) => {
                r.call_id = call.id.clone();
                r
            }
            Err(e) => ToolResult {
                call_id: call.id.clone(),
                output: json!({ "ok": false, "error": format!("tool errored: {}", e) }).to_string(),
                is_error: true,
            },
        },
        None => ToolResult {
            call_id: call.id.clone(),
            output: json!({ "ok": false, "error": format!("unknown tool '{}'", call.name) })
                .to_string(),
            is_error: true,
        },
    };
    if let Some(key) = cache_key {
        cache.insert(key, result.clone());
    }
    (result, true)
}

// Assemble the message list sent to the LLM: system prompt(s) first, then the
// trimmed conversation history, then the new user turn.
fn build_initial_messages(
    history: &[CaptureChatMessage],
    user_message: &str,
    entity: Option<&Entity>,
    workspace_context: Option<&str>,
    canonical_block: Option<&str>,
    skills_block: Option<&str>,
) -> Vec<ChatMessage> {
    let mut msgs = Vec::with_capacity(history.len() + 5);
    msgs.push(ChatMessage {
        role: Role::System,
        content: system_prompt(entity),
        tool_calls: None,
        tool_call_id: None,
    });

    for block in [canonical_block, skills_block]
        .into_iter()
        .flatten()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        msgs.push(ChatMessage {
            role: Role::System,
            content: block.to_string(),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    if let Some(ws) = workspace_context.map(str::trim).filter(|s| !s.is_empty()) {
        msgs.push(ChatMessage {
            role: Role::System,
            content: format!("UI hint (do not quote): {}", ws),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    let start = history.len().saturating_sub(HISTORY_TURNS);
    for h in &history[start..] {
        let role = match h.role.as_str() {
            "assistant" => Role::Assistant,
            "user" => Role::User,
            _ => continue,
        };
        if h.content.trim().is_empty() {
            continue;
        }
        if role == Role::User && h.content.trim() == user_message.trim() {
            // Optimistic clients may include the current bubble in history;
            // append the current message below exactly once.
            continue;
        }
        msgs.push(ChatMessage {
            role,
            content: h.content.clone(),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    msgs.push(ChatMessage {
        role: Role::User,
        content: user_message.to_string(),
        tool_calls: None,
        tool_call_id: None,
    });
    msgs
}

/// Query embed budget for chat lesson recall — the turn must not stall on the
/// embedding round-trip; on timeout recall degrades to recency.
const CHAT_LESSON_EMBED_TIMEOUT_MS: u64 = 300;
const CHAT_LESSONS_TOP_K: usize = 6;

/// Phase 6: assemble the canonical-layer system block (active entity + active
/// mission narrative + pinned notes + recalled lessons) so every chat turn
/// for an entity carries the canonical layer in its system prompt. Lessons
/// come from the default "operations" director (the chat session doesn't
/// carry a "current director" concept until Phase 7/9), filtered to global
/// scope plus the active entity. Returns `None` when there is nothing to
/// inject.
async fn build_canonical_block(
    state: &crate::state::ServerState,
    entity: Option<&Entity>,
    user_message: &str,
) -> Option<String> {
    let entity = entity?;
    let mission_store = state.mission_store.as_ref()?;
    let active_missions = mission_store
        .list_active_for_entity(&entity.id)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, entity_id = %entity.id, "prompt-assembly: list_active_for_entity failed");
            Vec::new()
        });
    let mission = active_missions.into_iter().next();
    let lessons = recall_chat_lessons(state, entity, user_message).await;
    // Enabled knowledge docs (README, repo map, conventions — see
    // `entity_knowledge`) ride every turn; a store error just means none.
    let docs = match state.entity_docs_store.as_ref() {
        Some(store) => store
            .list_enabled_for_entity(&entity.id)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, entity_id = %entity.id, "prompt-assembly: entity docs listing failed");
                Vec::new()
            }),
        None => Vec::new(),
    };
    let block = assemble_system_prompt(&CanonicalInputs {
        soul: None,
        director: None,
        entity: Some(entity),
        mission: mission.as_ref(),
        lessons: &lessons,
        docs: &docs,
    });
    if block.trim().is_empty() {
        None
    } else {
        tracing::debug!(
            entity_id = %entity.id,
            chars = block.len(),
            with_mission = mission.is_some(),
            docs = docs.len(),
            "prompt-assembly: injected canonical block"
        );
        Some(block)
    }
}

/// The skills index for this turn: every active skill in scope (this entity's,
/// the workspace's, the built-ins), one line each. Unlike the canonical block
/// this rides turns with no entity selected too — global skills apply
/// everywhere. A storage error just means no skills this turn.
async fn build_skills_block(
    state: &crate::state::ServerState,
    workspace_id: &str,
    entity_id: Option<&str>,
) -> Option<String> {
    let library = state.skills.clone()?;
    let workspace_id = workspace_id.to_string();
    let entity_id = entity_id.map(str::to_string);
    let skills = tokio::task::spawn_blocking(move || {
        library.resolve_active(&workspace_id, entity_id.as_deref())
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "prompt-assembly: skills listing failed");
        Vec::new()
    });
    crate::skills::render_skills_index(&skills)
}

/// Recall lessons for the chat turn. Semantic (query = the user's message,
/// embed bounded by a short timeout) when a provider is wired; recency
/// otherwise. Scope-filtered to global + the active entity. Failures always
/// degrade to an empty slice — the prompt simply omits the lessons block.
async fn recall_chat_lessons(
    state: &crate::state::ServerState,
    entity: &Entity,
    user_message: &str,
) -> Vec<crate::state::DirectorLesson> {
    let Some(store) = state.director_lessons_store.as_ref() else {
        return Vec::new();
    };
    let embedder = state.embedding_provider.as_ref().map(|p| p.0.clone());
    let query_embedding = match tokio::time::timeout(
        std::time::Duration::from_millis(CHAT_LESSON_EMBED_TIMEOUT_MS),
        crate::lessons::embed_query(embedder.as_ref(), user_message),
    )
    .await
    {
        Ok(v) => v,
        Err(_) => {
            tracing::debug!("chat lesson recall: query embed timed out; using recency");
            None
        }
    };
    // Over-fetch so scope filtering still leaves up to top-k.
    let recalled = match store
        .recall_lessons(
            "operations",
            query_embedding.as_deref(),
            CHAT_LESSONS_TOP_K * 4,
        )
        .await
    {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, "chat lesson recall failed; continuing without lessons");
            return Vec::new();
        }
    };
    let entity_scope = format!("entity:{}", entity.id);
    let workspace_scope = format!("workspace:{}", entity_workspace_id(entity));
    // `global` is the legacy scope for entity-less chat lessons; new ones are
    // recorded as `workspace:<id>` so they never reach another tenant.
    let mut lessons: Vec<_> = recalled
        .into_iter()
        .filter(|l| l.scope == "global" || l.scope == workspace_scope || l.scope == entity_scope)
        .collect();
    lessons.truncate(CHAT_LESSONS_TOP_K);
    lessons
}

// Resolve which entity a chat turn is scoped to: an explicit id (ownership-
// checked when the entity is personal), or the caller's own Personal Space.
fn resolve_entity<'a>(
    entities: &'a [Entity],
    entity_id: Option<&str>,
    workspace_id: &str,
    principal_id: &str,
) -> Option<&'a Entity> {
    if let Some(eid) = entity_id {
        // A specific entity was chosen: resolve it within the workspace. A
        // personal entity is only resolvable by its own owner.
        return entities.iter().find(|e| {
            e.id == eid
                && entity_workspace_id(e) == workspace_id
                && (!e.is_personal || e.personal_user_id.as_deref() == Some(principal_id))
        });
    }
    // No entity selected: loose chat defaults to the caller's private Personal Space.
    entities.iter().find(|e| {
        e.is_personal
            && e.personal_user_id.as_deref() == Some(principal_id)
            && entity_workspace_id(e) == workspace_id
    })
}

/// Snapshot used by the session memory buffer (kept for compatibility with /reset
/// and the existing in-process session cache).
fn snapshot_for(entity: Option<&Entity>) -> EntitySnapshot {
    match entity {
        Some(e) => EntitySnapshot {
            entity_id: e.id.clone(),
            name: e.name.clone(),
            persona_narrative: e
                .profile
                .clone()
                .unwrap_or_else(|| "Helpful assistant.".to_string()),
            current_goal: e
                .goals
                .clone()
                .unwrap_or_else(|| "Assist the user.".to_string()),
            tone_constraints: vec!["Conversational".to_string(), "Direct".to_string()],
        },
        None => EntitySnapshot {
            entity_id: String::new(),
            name: "Workspace".to_string(),
            persona_narrative: "Consistent's general workspace assistant.".to_string(),
            current_goal: "Help the user think, plan, and capture.".to_string(),
            tone_constraints: vec!["Collaborative".to_string()],
        },
    }
}

/// Upper bound on a single tool result carried into the Architect's shadow
/// trace. The trace is serialised whole into `audit_trail.payload`.
const OBSERVED_OUTPUT_CHARS: usize = 4000;

/// Record what a tool returned, so Verify can tell a claim backed by a tool
/// result this turn (`Observed`) from one backed by workspace truth
/// (`Verified`).
///
/// Called at the tool loop rather than derived afterwards because `tool_cache`
/// is keyed by a cache key, not by tool name — a replayed result cannot be
/// mapped back to its tool after the fact.
fn note_observed(
    observed: &mut Vec<consistent_world_architect::ObservedFact>,
    call: &consistent_world_common::types::ToolCall,
    result: &consistent_world_common::types::ToolResult,
) {
    if result.is_error {
        return;
    }
    observed.push(consistent_world_architect::ObservedFact {
        tool: call.name.clone(),
        call_id: Some(result.call_id.clone()),
        output: result.output.chars().take(OBSERVED_OUTPUT_CHARS).collect(),
    });
}

/// Run the Architect over a finished turn, in the background.
///
/// Fire-and-forget by construction: the user's answer has already been sent,
/// and nothing here can change it. The crate has no LLM dependency, so this
/// cannot spend a token — it resolves the draft the gateway already produced
/// against the world Inspect just read, and writes the verdict to
/// `audit_trail`.
fn spawn_architect_shadow(
    state: &crate::state::ServerState,
    input: consistent_world_architect::TurnInput,
) {
    // Default-on: the shadow run is how the ASK-rate accumulates in normal
    // operation, and it is safe by construction (no LLM dependency, so no token
    // spend; fire-and-forget after the answer is already sent, so it cannot
    // change what the user saw). Set `ARCHITECT_SHADOW=0` to opt a deployment
    // out — e.g. to rule it out while chasing a performance regression.
    if std::env::var("ARCHITECT_SHADOW").ok().as_deref() == Some("0") {
        return;
    }
    let Some(architect) = state.architect.clone() else {
        return;
    };
    tokio::spawn(async move {
        let _ = architect.run(input).await;
    });
}

/// Outcome of a chat turn.
pub struct AgentTurnOutcome {
    pub session_id: String,
    pub message: String,
    pub actions: Vec<ChatAction>,
    pub entity_context: Option<Entity>,
    /// Smart-routing outcome when the turn happened in the Personal Space.
    pub routing: Option<crate::capture::ChatRouting>,
    /// What this answer rests on — tool provenance only. See
    /// `evidence::ChatEvidence`.
    pub evidence: crate::evidence::ChatEvidence,
}

/// Deterministic smart routing for a loose Personal-Space chat turn. Files the
/// message into a business entity when a high-precision signal points at one
/// (with an undo affordance), suggests when the signal is softer, else leaves it
/// in the Personal Space. Returns `None` unless the active entity is personal.
async fn compute_chat_routing(
    control: &ControlState,
    workspace_id: &str,
    principal_id: &str,
    active_entity: Option<&Entity>,
    message: &str,
) -> Option<crate::capture::ChatRouting> {
    if !active_entity?.is_personal {
        return None;
    }
    let business: Vec<Entity> = {
        let entities = control.entities.read().await;
        entities
            .iter()
            .filter(|e| {
                !e.is_personal
                    && entity_workspace_id(e) == workspace_id
                    && !e.status.eq_ignore_ascii_case("deleted")
            })
            .cloned()
            .collect()
    };
    if business.is_empty() {
        return None;
    }

    let capture = crate::state::RawCapture {
        id: String::new(),
        workspace_id: workspace_id.to_string(),
        entity_id: None,
        source: "chat".to_string(),
        source_ref: None,
        session_id: None,
        provider: None,
        raw_text: message.to_string(),
        summary: None,
        status: Default::default(),
        promoted_task_id: None,
        metadata: serde_json::Value::Null,
        created_at: String::new(),
        updated_at: String::new(),
    };
    let suggestion = crate::capture_routing::suggest_route(&capture, &business, None);

    // Auto-file when a strong structured signal (repo/domain) points at a
    // business entity: file a capture there and hand back an undo token.
    if suggestion.confidence >= crate::capture_routing::AUTO_APPLY_THRESHOLD
        && let Some(target_id) = suggestion.entity_id.clone()
    {
        let created = crate::control_api::internal_create_capture(
            control,
            workspace_id,
            Some(principal_id),
            crate::control_api::CreateCaptureRequest {
                raw_text: message.to_string(),
                entity_id: Some(target_id.clone()),
                source: Some("chat".to_string()),
                source_ref: None,
                session_id: None,
                provider: None,
                summary: None,
                metadata: None,
            },
        )
        .await
        .ok();
        return Some(crate::capture::ChatRouting {
            outcome: "auto_filed".to_string(),
            entity_id: Some(target_id),
            entity_name: suggestion.entity_name.clone(),
            undo_token: created.map(|c| c.id),
            reason: Some(suggestion.reason.clone()),
        });
    }

    // Softer structured signal (e.g. a repo-path basename): suggest, don't move.
    if suggestion.confidence >= 0.6
        && let Some(entity_id) = suggestion.entity_id.clone()
    {
        return Some(crate::capture::ChatRouting {
            outcome: "suggest".to_string(),
            entity_id: Some(entity_id),
            entity_name: suggestion.entity_name.clone(),
            undo_token: None,
            reason: Some(suggestion.reason.clone()),
        });
    }

    // Last resort: the user named a business entity in the message.
    if let Some(entity) = name_mention(&business, message) {
        return Some(crate::capture::ChatRouting {
            outcome: "suggest".to_string(),
            entity_id: Some(entity.id.clone()),
            entity_name: Some(entity.name.clone()),
            undo_token: None,
            reason: Some(format!("You mentioned {} by name.", entity.name)),
        });
    }
    None
}

/// Case-insensitive substring match of a business entity's name in the message.
/// Names shorter than 3 chars are ignored to avoid coincidental matches. Used
/// only to *suggest* (never to auto-file), so a loose match is acceptable.
fn name_mention<'a>(entities: &'a [Entity], message: &str) -> Option<&'a Entity> {
    let lower = message.to_lowercase();
    entities.iter().find(|e| {
        let name = e.name.trim().to_lowercase();
        name.len() >= 3 && lower.contains(&name)
    })
}

/// Non-streaming chat turn.
pub async fn run_chat_turn(
    state: &crate::state::ServerState,
    control: &Arc<ControlState>,
    workspace_id: &str,
    principal_id: &str,
    req: CaptureChatRequest,
) -> Result<AgentTurnOutcome, StatusCode> {
    let cfg = state
        .app_config
        .as_deref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    // Refuse before any LLM spend, not just at the capture-route middleware —
    // this entry point is also reached from surfaces outside `inference_routes`.
    if let Some(ledger) = state.credit_ledger.as_ref()
        && ledger.credit_exhausted(workspace_id).await
    {
        return Err(StatusCode::PAYMENT_REQUIRED);
    }

    let session_id = req
        .session_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let entity_context = match req.scope.as_deref() {
        Some("workspace") => None,
        Some("entity") if req.entity_id.is_none() => return Err(StatusCode::BAD_REQUEST),
        Some(other) if other != "entity" => return Err(StatusCode::BAD_REQUEST),
        _ => {
            let entities = control.entities.read().await;
            resolve_entity(
                &entities,
                req.entity_id.as_deref(),
                workspace_id,
                principal_id,
            )
            .cloned()
        }
    };

    memory_buffer::ensure_session(
        &state.session_cache,
        &session_id,
        snapshot_for(entity_context.as_ref()),
        30,
    );
    if let Some(mut buf) = state.session_cache.get_mut(&session_id) {
        buf.push_turn("user", &req.message);
    }
    local_vault::mirror_chat_turn(ChatTurn {
        workspace_id,
        session_id: &session_id,
        role: "user",
        content: &req.message,
        entity: entity_context.as_ref(),
    })
    .await;
    let provider = build_provider(cfg, control).await?;
    let model_id = provider.model.clone();
    // Scope tools to the RESOLVED entity (not the raw request), so a loose turn
    // that defaulted to the Personal Space saves notes/tasks there.
    let tools = build_workspace_tools(
        state.clone(),
        control.clone(),
        workspace_id,
        entity_context.as_ref().map(|e| e.id.as_str()),
        principal_id,
        req.timezone_offset_minutes,
    );
    let tool_definitions = tool_defs(&tools);

    let canonical_block = build_canonical_block(state, entity_context.as_ref(), &req.message).await;
    let skills_block = build_skills_block(
        state,
        workspace_id,
        entity_context.as_ref().map(|e| e.id.as_str()),
    )
    .await;
    let mut messages = build_initial_messages(
        &req.history,
        &req.message,
        entity_context.as_ref(),
        req.workspace_context.as_deref(),
        canonical_block.as_deref(),
        skills_block.as_deref(),
    );

    let mut actions: Vec<ChatAction> = Vec::new();
    // What tools returned this turn, for the Architect's shadow verification.
    let mut observed: Vec<consistent_world_architect::ObservedFact> = Vec::new();
    let mut tool_cache: HashMap<String, ToolResult> = HashMap::new();
    let mut final_text = String::new();

    for iteration in 0..MAX_TOOL_ITERATIONS {
        let request = CompletionRequest {
            model: model_id.clone(),
            messages: messages.clone(),
            tools: if tool_definitions.is_empty() {
                None
            } else {
                Some(tool_definitions.clone())
            },
            max_tokens: Some(1024),
            temperature: Some(0.65),
            stream: false,
        };

        let response = provider.client.complete(request).await.map_err(|e| {
            tracing::warn!(error = %e, "chat agent LLM call failed");
            StatusCode::BAD_GATEWAY
        })?;

        debit_cost_if_needed(
            state,
            workspace_id,
            &response.model,
            response.usage.input_tokens,
            response.usage.output_tokens,
        )
        .await;

        if response.tool_calls.is_empty() {
            final_text = response.content.unwrap_or_default().trim().to_string();
            tracing::debug!(iteration, "chat agent: final text produced");
            break;
        }

        // Append the assistant's tool-call message
        let assistant_content = response.content.clone().unwrap_or_default();
        messages.push(ChatMessage {
            role: Role::Assistant,
            content: assistant_content,
            tool_calls: Some(response.tool_calls.clone()),
            tool_call_id: None,
        });

        // Execute each tool, collect effects, append tool result messages
        for call in &response.tool_calls {
            let (result, executed) = execute_one_tool(&tools, call, &mut tool_cache).await;
            note_observed(&mut observed, call, &result);
            if executed && let Some(effect) = tool_effect_from_output(&call.name, &result.output) {
                actions.push(ChatAction {
                    action_type: effect.action_type,
                    description: effect.description,
                    result_id: effect.result_id,
                    status: effect.status,
                });
            }
            messages.push(ChatMessage {
                role: Role::Tool,
                content: result.output,
                tool_calls: None,
                tool_call_id: Some(result.call_id),
            });
        }

        if iteration + 1 == MAX_TOOL_ITERATIONS {
            tracing::warn!("chat agent: tool iteration cap reached");
            final_text = "Hit my internal iteration limit on that one — could you rephrase or break it into a smaller ask?".to_string();
        }
    }

    if final_text.is_empty() {
        final_text = "Hmm, I drew a blank on that — could you say a bit more?".to_string();
    }

    if let Some(mut buf) = state.session_cache.get_mut(&session_id) {
        buf.push_turn("assistant", &final_text);
    }
    local_vault::mirror_chat_turn(ChatTurn {
        workspace_id,
        session_id: &session_id,
        role: "assistant",
        content: &final_text,
        entity: entity_context.as_ref(),
    })
    .await;
    if let Some(entity) = entity_context.as_ref() {
        let assistant_tail: String = final_text.chars().take(600).collect();
        let user_tail: String = req.message.chars().take(400).collect();
        let line = format!("Operator: {user_tail}\nAssistant: {assistant_tail}");
        if let Err(e) =
            crate::entity_memory::append_entity_short_line(state.memory.clone(), &entity.id, &line)
                .await
        {
            tracing::debug!(entity_id = %entity.id, error = %e, "entity conversation memory append failed");
        }
    }

    // Bulletproof Architect shadow path. The user has their answer already;
    // this records what Verify *would* have said about it.
    //
    // `turn_id` is per-turn, not per-session. It used to be `session_id`, which
    // made `audit_turn_idx` group every turn of a conversation into one bucket
    // and left the trail unreadable per turn — the thing the index exists for.
    //
    // The entity is the *resolved* one: `resolve_entity` falls back to the
    // Personal Space when the request names none, and passing `req.entity_id`
    // through discarded that, so every loose chat turn inspected with
    // `entity_id: None` and took the shell-snapshot early return.
    // Derive the posture before `observed` is handed to the shadow run.
    // `note_observed` already skips failed calls, so everything left here
    // grounded something.
    let evidence = crate::evidence::ChatEvidence::from_tool_calls(
        observed.iter().map(|f| (f.tool.as_str(), false)),
    );
    spawn_architect_shadow(
        state,
        consistent_world_architect::TurnInput::chat(
            Uuid::new_v4().to_string(),
            Some(workspace_id.to_string()),
            entity_context.as_ref().map(|e| e.id.clone()),
            Some(session_id.clone()),
            Some(req.message.clone()),
        )
        .with_outcome(Some(final_text.clone()), observed),
    );

    let routing = compute_chat_routing(
        control,
        workspace_id,
        principal_id,
        entity_context.as_ref(),
        &req.message,
    )
    .await;

    Ok(AgentTurnOutcome {
        session_id,
        message: final_text,
        actions,
        entity_context,
        routing,
        evidence,
    })
}

// The LLM client plus the model id chosen for this chat turn.
struct ResolvedProvider {
    client: Box<dyn LlmProvider>,
    model: String,
}

// Resolve the Operations director's configured LLM into a ready-to-call provider.
async fn build_provider(
    cfg: &AppConfig,
    control: &ControlState,
) -> Result<ResolvedProvider, StatusCode> {
    let resolved = resolve_operations_llm(cfg, control)
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let api_key = resolve_llm_api_key(&resolved.api_key_env);
    Ok(ResolvedProvider {
        client: build_llm_client(cfg, &resolved.base_url, &api_key, &resolved.api_key_env),
        model: resolved.model,
    })
}

async fn debit_cost_if_needed(
    state: &crate::state::ServerState,
    workspace_id: &str,
    model: &str,
    input: u64,
    output: u64,
) {
    if let Some(ledger) = state.credit_ledger.as_ref() {
        let calc = CostCalculator::new();
        let cost = calc.calculate(model, input, output);
        if cost > 0.0 {
            let _ = ledger.debit_for_cost(workspace_id, cost).await;
        }
    }
}

/// Convert an [`AgentTurnOutcome`] into the HTTP response shape.
pub fn chat_response_from(outcome: &AgentTurnOutcome) -> CaptureChatResponse {
    CaptureChatResponse {
        message: outcome.message.clone(),
        analysis: ChatAnalysis {
            intent: classify_for_ui(&outcome.actions),
            confidence: if outcome.actions.is_empty() { 1.0 } else { 0.9 },
            entity_context: outcome.entity_context.as_ref().map(|e| e.name.clone()),
            key_points: Vec::new(),
            readiness_note: outcome.entity_context.as_ref().map(|e| {
                format!(
                    "Entity readiness: {} ({:.0}%)",
                    e.readiness.label, e.readiness.percent
                )
            }),
        },
        actions: outcome.actions.clone(),
        suggestions: Vec::new(),
        needs_followup: false,
        session_id: Some(outcome.session_id.clone()),
        routing: outcome.routing.clone(),
        evidence: Some(outcome.evidence.clone()),
    }
}

fn classify_for_ui(actions: &[ChatAction]) -> String {
    if let Some(a) = actions.first() {
        a.action_type.clone()
    } else {
        "chat".to_string()
    }
}

/// Streaming NDJSON variant. Runs the tool loop non-streamed; once the model has
/// no more tools to call, the FINAL natural-language reply is streamed to the
/// client as `token` events, followed by a `done` event with the full payload.
pub async fn stream_chat_turn_ndjson(
    state: crate::state::ServerState,
    config: Arc<AppConfig>,
    control: Arc<ControlState>,
    workspace_id: String,
    principal_id: String,
    req: CaptureChatRequest,
) -> Result<ReceiverStream<Result<Bytes, Infallible>>, StatusCode> {
    if let Some(ledger) = state.credit_ledger.as_ref()
        && ledger.credit_exhausted(&workspace_id).await
    {
        return Err(StatusCode::PAYMENT_REQUIRED);
    }

    let session_id = req
        .session_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let entity_context = match req.scope.as_deref() {
        Some("workspace") => None,
        Some("entity") if req.entity_id.is_none() => return Err(StatusCode::BAD_REQUEST),
        Some(other) if other != "entity" => return Err(StatusCode::BAD_REQUEST),
        _ => {
            let entities = control.entities.read().await;
            resolve_entity(
                &entities,
                req.entity_id.as_deref(),
                &workspace_id,
                &principal_id,
            )
            .cloned()
        }
    };

    memory_buffer::ensure_session(
        &state.session_cache,
        &session_id,
        snapshot_for(entity_context.as_ref()),
        30,
    );
    if let Some(mut buf) = state.session_cache.get_mut(&session_id) {
        buf.push_turn("user", &req.message);
    }
    local_vault::mirror_chat_turn(ChatTurn {
        workspace_id: &workspace_id,
        session_id: &session_id,
        role: "user",
        content: &req.message,
        entity: entity_context.as_ref(),
    })
    .await;

    let provider = build_provider(&config, &control).await?;
    let tools = build_workspace_tools(
        state.clone(),
        control.clone(),
        &workspace_id,
        entity_context.as_ref().map(|e| e.id.as_str()),
        &principal_id,
        req.timezone_offset_minutes,
    );
    let tool_definitions = tool_defs(&tools);

    let canonical_block =
        build_canonical_block(&state, entity_context.as_ref(), &req.message).await;
    let skills_block = build_skills_block(
        &state,
        &workspace_id,
        entity_context.as_ref().map(|e| e.id.as_str()),
    )
    .await;
    let initial_messages = build_initial_messages(
        &req.history,
        &req.message,
        entity_context.as_ref(),
        req.workspace_context.as_deref(),
        canonical_block.as_deref(),
        skills_block.as_deref(),
    );

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(128);
    let ctx = StreamingTurnCtx {
        state: state.clone(),
        control: control.clone(),
        workspace_id: workspace_id.clone(),
        principal_id,
        user_message: req.message.clone(),
        session_id,
        entity_context,
    };

    tokio::spawn(async move {
        run_streaming_task(ctx, provider, tools, tool_definitions, initial_messages, tx).await;
    });

    Ok(ReceiverStream::new(rx))
}

/// Everything the spawned streaming turn needs to finish with full parity to
/// the non-streaming path: session bookkeeping, entity conversation memory,
/// and smart routing on the final payload.
struct StreamingTurnCtx {
    state: crate::state::ServerState,
    control: Arc<ControlState>,
    workspace_id: String,
    principal_id: String,
    user_message: String,
    session_id: String,
    entity_context: Option<Entity>,
}

// Spawned task driving the streaming tool loop: stream tokens/tool-call
// deltas to the client, execute any requested tools, and repeat until the
// model stops calling them or the iteration cap is hit.
async fn run_streaming_task(
    ctx: StreamingTurnCtx,
    provider: ResolvedProvider,
    tools: Vec<Box<dyn Tool>>,
    tool_definitions: Vec<ToolDefinition>,
    initial_messages: Vec<ChatMessage>,
    tx: tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>,
) {
    let meta = json!({ "type": "meta", "sessionId": ctx.session_id });
    if tx
        .send(Ok(Bytes::from(meta.to_string() + "\n")))
        .await
        .is_err()
    {
        return;
    }

    let mut messages = initial_messages;
    let mut actions: Vec<ChatAction> = Vec::new();
    // What tools returned this turn, for the Architect's shadow verification.
    let mut observed: Vec<consistent_world_architect::ObservedFact> = Vec::new();
    let mut tool_cache: HashMap<String, ToolResult> = HashMap::new();
    let model_id = provider.model.clone();

    for iteration in 0..MAX_TOOL_ITERATIONS {
        // Stream the actual turn once. Streaming providers also emit tool-call
        // deltas, so a separate non-streamed probe would duplicate the model
        // request and could produce a different answer.
        let stream_req = CompletionRequest {
            model: model_id.clone(),
            messages: messages.clone(),
            tools: if tool_definitions.is_empty() {
                None
            } else {
                Some(tool_definitions.clone())
            },
            max_tokens: Some(1024),
            temperature: Some(0.65),
            stream: true,
        };
        let (assistant_content, tool_calls) = match provider
            .client
            .stream_complete(stream_req.clone())
            .await
        {
            Ok(mut stream) => {
                let mut content = String::new();
                let mut calls: HashMap<String, (String, String)> = HashMap::new();
                while let Some(event) = stream.next().await {
                    match event {
                        StreamEvent::Token(token) => {
                            content.push_str(&token);
                            let line = json!({ "type": "token", "text": token }).to_string() + "\n";
                            if tx.send(Ok(Bytes::from(line))).await.is_err() {
                                return;
                            }
                        }
                        StreamEvent::ToolCallStart { id, name } => {
                            calls.entry(id).or_insert((name, String::new()));
                        }
                        StreamEvent::ToolCallDelta {
                            id,
                            arguments_chunk,
                        } => {
                            calls
                                .entry(id)
                                .or_insert((String::new(), String::new()))
                                .1
                                .push_str(&arguments_chunk);
                        }
                        StreamEvent::ToolCallEnd { .. } | StreamEvent::Done => {}
                        StreamEvent::Error(error) => {
                            tracing::warn!(%error, "stream chat: provider stream error")
                        }
                    }
                }
                let parsed = calls
                    .into_iter()
                    .filter_map(|(id, (name, args))| {
                        serde_json::from_str(&args).ok().map(|arguments| ToolCall {
                            id,
                            name,
                            arguments,
                        })
                    })
                    .collect();
                (content, parsed)
            }
            Err(e) => {
                tracing::debug!(error = %e, "stream chat unavailable; falling back to one completion call");
                let fallback = CompletionRequest {
                    stream: false,
                    ..stream_req
                };
                match provider.client.complete(fallback).await {
                    Ok(response) => (response.content.unwrap_or_default(), response.tool_calls),
                    Err(error) => {
                        tracing::warn!(%error, "stream chat completion failed");
                        // Not a completed turn: skip the parity writes so a
                        // failed turn doesn't pollute entity memory or routing.
                        emit_done(
                            &ctx,
                            &tx,
                            actions,
                            "I couldn't reach an AI model just now. Open AI settings (the gear icon in the sidebar) to check the connected provider — Claude Code or Codex ride a subscription, Ollama runs fully local, or add an API key.".to_string(),
                            false,
                            observed,
                        )
                        .await;
                        return;
                    }
                }
            }
        };
        if tool_calls.is_empty() {
            emit_done(
                &ctx,
                &tx,
                actions,
                assistant_content.trim().to_string(),
                true,
                observed,
            )
            .await;
            return;
        }

        // Tools requested — execute, append, loop
        messages.push(ChatMessage {
            role: Role::Assistant,
            content: assistant_content,
            tool_calls: Some(tool_calls.clone()),
            tool_call_id: None,
        });
        for call in &tool_calls {
            let (result, executed) = execute_one_tool(&tools, call, &mut tool_cache).await;
            note_observed(&mut observed, call, &result);
            if executed && let Some(effect) = tool_effect_from_output(&call.name, &result.output) {
                actions.push(ChatAction {
                    action_type: effect.action_type,
                    description: effect.description,
                    result_id: effect.result_id,
                    status: effect.status,
                });
            }
            messages.push(ChatMessage {
                role: Role::Tool,
                content: result.output,
                tool_calls: None,
                tool_call_id: Some(result.call_id),
            });
        }

        if iteration + 1 == MAX_TOOL_ITERATIONS {
            emit_done(
                &ctx,
                &tx,
                actions,
                "I tried a few steps but hit my internal limit — could you break this into a smaller ask?".to_string(),
                true,
                observed,
            )
            .await;
            return;
        }
    }
}

/// Close out a streamed turn. `completed` marks a turn the model actually
/// finished (including the iteration-cap message); only those get the
/// non-streaming path's tail writes — entity conversation memory and smart
/// routing — so the streamed and blocking endpoints stay interchangeable.
async fn emit_done(
    ctx: &StreamingTurnCtx,
    tx: &tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>,
    actions: Vec<ChatAction>,
    final_text: String,
    completed: bool,
    observed: Vec<consistent_world_architect::ObservedFact>,
) {
    let evidence = crate::evidence::ChatEvidence::from_tool_calls(
        observed.iter().map(|f| (f.tool.as_str(), false)),
    );
    if let Some(mut buf) = ctx.state.session_cache.get_mut(&ctx.session_id) {
        buf.push_turn("assistant", &final_text);
    }
    local_vault::mirror_chat_turn(ChatTurn {
        workspace_id: &ctx.workspace_id,
        session_id: &ctx.session_id,
        role: "assistant",
        content: &final_text,
        entity: ctx.entity_context.as_ref(),
    })
    .await;

    let mut routing = None;
    if completed {
        if let Some(entity) = ctx.entity_context.as_ref() {
            let assistant_tail: String = final_text.chars().take(600).collect();
            let user_tail: String = ctx.user_message.chars().take(400).collect();
            let line = format!("Operator: {user_tail}\nAssistant: {assistant_tail}");
            if let Err(e) = crate::entity_memory::append_entity_short_line(
                ctx.state.memory.clone(),
                &entity.id,
                &line,
            )
            .await
            {
                tracing::debug!(entity_id = %entity.id, error = %e, "entity conversation memory append failed");
            }
        }
        routing = compute_chat_routing(
            &ctx.control,
            &ctx.workspace_id,
            &ctx.principal_id,
            ctx.entity_context.as_ref(),
            &ctx.user_message,
        )
        .await;
    }

    // The streaming path is what the desktop uses. Until now it had no
    // Architect hook at all, so the shadow run produced a near-empty trail and
    // looked broken when it was simply never invoked on this endpoint.
    if completed {
        // Same posture on the streamed path, so the two endpoints stay
        // interchangeable. Computed before `observed` moves into the shadow run.
        spawn_architect_shadow(
            &ctx.state,
            consistent_world_architect::TurnInput::chat(
                Uuid::new_v4().to_string(),
                Some(ctx.workspace_id.clone()),
                ctx.entity_context.as_ref().map(|e| e.id.clone()),
                Some(ctx.session_id.clone()),
                Some(ctx.user_message.clone()),
            )
            .with_outcome(Some(final_text.clone()), observed),
        );
    }

    let outcome = AgentTurnOutcome {
        session_id: ctx.session_id.clone(),
        evidence,
        message: final_text,
        actions,
        entity_context: ctx.entity_context.clone(),
        routing,
    };
    let payload = chat_response_from(&outcome);
    let line = json!({ "type": "done", "payload": payload }).to_string() + "\n";
    let _ = tx.send(Ok(Bytes::from(line))).await;
}

// Entity-resolution and name-mention behavior for chat scoping.
#[cfg(test)]
mod tests {
    use super::*;

    fn biz(id: &str, name: &str) -> Entity {
        Entity {
            id: id.into(),
            name: name.into(),
            owner: Some("ws".into()),
            status: "Active".into(),
            ..Default::default()
        }
    }

    fn personal(id: &str, user: &str) -> Entity {
        Entity {
            id: id.into(),
            name: "Personal".into(),
            owner: Some("ws".into()),
            status: "Active".into(),
            is_personal: true,
            personal_user_id: Some(user.into()),
            ..Default::default()
        }
    }

    #[test]
    fn loose_chat_defaults_to_callers_personal_space() {
        let ents = vec![
            biz("e1", "BitRegalo"),
            personal("p_a", "userA"),
            personal("p_b", "userB"),
        ];
        assert_eq!(
            resolve_entity(&ents, None, "ws", "userA").map(|e| e.id.as_str()),
            Some("p_a")
        );
        assert_eq!(
            resolve_entity(&ents, None, "ws", "userB").map(|e| e.id.as_str()),
            Some("p_b")
        );
    }

    #[test]
    fn cannot_resolve_another_users_personal_space_by_id() {
        let ents = vec![personal("p_a", "userA")];
        // A teammate can't target userA's personal space even with the raw id.
        assert!(resolve_entity(&ents, Some("p_a"), "ws", "userB").is_none());
        // The owner can.
        assert_eq!(
            resolve_entity(&ents, Some("p_a"), "ws", "userA").map(|e| e.id.as_str()),
            Some("p_a")
        );
    }

    #[test]
    fn name_mention_matches_business_entity() {
        let ents = vec![biz("e1", "BitRegalo"), biz("e2", "Skirting")];
        assert_eq!(
            name_mention(&ents, "some thoughts on bitregalo").map(|e| e.id.as_str()),
            Some("e1")
        );
        assert!(name_mention(&ents, "an unrelated message").is_none());
    }
}
