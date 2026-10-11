//! Runs a single task with the assigned agent: builds prompt from agent + entity + task,
//! resolves provider/model from config, calls LLM, updates task result and status.
//! For task_kind "entity-enrichment", uses a structured JSON prompt and updates entity.enrichment.
//! For task_kind "prompt-interpretation", uses a structured JSON prompt and stores a pretty-printed
//! `PromptInterpretationBrief` JSON object in `task.result`.

use crate::config::{self, AppConfig};
use crate::credentials_resolver::resolve_llm_api_key_for_entity;
use crate::interpreter::{
    build_interpretation_system_prompt, build_interpretation_user_prompt,
    parse_prompt_interpretation_brief,
};
use crate::state::{
    ActionEvent, ActionOutcome, Agent, AgentGroup, ControlState, EnrichmentDraft, Entity,
    ExecutionContext, OperatorFeedKind, ResolvedScope, Task, TaskOutcome, TaskStatus,
};
use consistent_world_common::capabilities::{CapabilityId, ExecutionLevel};
use consistent_world_common::types::{ScopeType, ToolCall, ToolResult};
use consistent_world_llm::client::{
    ChatMessage, CompletionRequest, LlmProvider, Role, ToolDefinition,
};
use consistent_world_memory::store::MemoryStore;
use consistent_world_tools::ToolRegistry;
use consistent_world_tools::browser::BrowserTool;
use consistent_world_tools::filesystem::{FileSystemPolicy, FileSystemTool};
use consistent_world_tools::http::HttpTool;
use consistent_world_tools::shell::{self, ShellPolicy, ShellTool};
use consistent_world_tools::web_search::WebSearchTool;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info};

/// Hard ceiling on a single provider.complete() call. Without this, an LLM
/// that hangs (network partition, dead local model, etc.) would leave the
/// task `InProgress` forever and any sync code-paths waiting on the task's
/// completion would never unblock.
const LLM_CALL_TIMEOUT: Duration = Duration::from_secs(240);

/// Outcome of one `run_agent` call: what happened to the task, and an
/// optional human-readable message to store alongside it.
#[derive(Debug, Clone)]
pub struct RunAgentResult {
    pub outcome: TaskOutcome,
    pub message: Option<String>,
}

// One tool call's result, paired with the audit outcome it should be logged as.
#[derive(Debug, Clone)]
struct ExecutedToolCall {
    result: ToolResult,
    outcome: ActionOutcome,
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

// One resolved LLM target to try: provider/model plus its optional tier
// label and mode preset (temperature/max_tokens).
#[derive(Debug, Clone)]
struct LlmAttempt {
    tier: Option<String>,
    provider_label: String,
    base_url: String,
    api_key_env: String,
    model_id: String,
    mode_preset: Option<config::ModePreset>,
}

// Merges a per-entity/task-kind routing override onto the base entry, field
// by field; an override field wins only when it is actually set.
fn merge_task_kind_routing_entry(
    base: Option<config::TaskKindRoutingEntry>,
    ov: Option<config::TaskKindRoutingEntry>,
) -> Option<config::TaskKindRoutingEntry> {
    match (base, ov) {
        (None, None) => None,
        (Some(b), None) => Some(b),
        (None, Some(o)) => Some(o),
        (Some(mut b), Some(o)) => {
            if o.director_id.is_some() {
                b.director_id = o.director_id;
            }
            if o.provider.is_some() {
                b.provider = o.provider;
            }
            if o.model.is_some() {
                b.model = o.model;
            }
            if o.mode.is_some() {
                b.mode = o.mode;
            }
            if o.generation_tier.is_some() {
                b.generation_tier = o.generation_tier;
            }
            if o.review_tier.is_some() {
                b.review_tier = o.review_tier;
            }
            if o.fallback_tiers.is_some() {
                b.fallback_tiers = o.fallback_tiers;
            }
            Some(b)
        }
    }
}

/// Runs the given task: loads agent and optional entity, resolves LLM from config,
/// builds prompt, calls LLM, then updates task to Completed with result.
pub async fn run_agent(
    control: Arc<ControlState>,
    config: &AppConfig,
    task_id: String,
    memory: Option<Arc<dyn MemoryStore>>,
) -> Result<RunAgentResult, String> {
    // Snapshot everything we need up-front, releasing each read lock immediately
    // after the clone. Holding any of these read guards across the LLM call
    // would block writers (including the orchestrator's next tick) and starve
    // all readers of /api/tasks if the LLM hangs.
    let task = {
        let tasks = control.tasks.read().await;
        tasks
            .iter()
            .find(|t| t.id == task_id.as_str())
            .cloned()
            .ok_or_else(|| format!("task not found: {}", task_id))?
    };
    let agent_id = task
        .assigned_agent_id
        .clone()
        .ok_or_else(|| format!("task {} has no assigned agent", task_id))?;
    let agent = {
        let agents = control.agents.read().await;
        agents
            .iter()
            .find(|a| a.id.as_str() == agent_id.as_str())
            .cloned()
            .ok_or_else(|| format!("agent not found: {}", agent_id))?
    };
    let director_group: Option<AgentGroup> = {
        let groups = control.agent_groups.read().await;
        groups.iter().find(|g| g.id == agent.group_id).cloned()
    };
    let entity = match &task.entity_id {
        Some(eid) => {
            let entities = control.entities.read().await;
            entities
                .iter()
                .find(|e| e.id.as_str() == eid.as_str())
                .cloned()
        }
        None => None,
    };
    // Phase A2 gate: no execution proceeds without a derived ExecutionContext.
    let execution_context = match build_execution_context(&task, &agent, entity.as_ref()) {
        Ok(ctx) => ctx,
        Err(e) => {
            control
                .append_action_event(ActionEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    scope_type: if task.entity_id.is_some() {
                        ScopeType::Entity
                    } else {
                        ScopeType::Principal
                    },
                    scope_id: task
                        .entity_id
                        .clone()
                        .unwrap_or_else(|| format!("task:{}", task.id)),
                    entity_id: task.entity_id.clone(),
                    task_id: Some(task.id.clone()),
                    agent_id: Some(agent.id.clone()),
                    capability: CapabilityId::MessageInternal,
                    execution_level: ExecutionLevel::Coordinate,
                    target: Some("run_agent.execution_context".to_string()),
                    outcome: ActionOutcome::Denied,
                    reason: Some(e.clone()),
                })
                .await;
            return Ok(RunAgentResult {
                outcome: TaskOutcome::BlockedByPolicy,
                message: Some(e),
            });
        }
    };
    info!(
        task_id = %task_id,
        scope_type = ?execution_context.scope.scope_type,
        scope_id = %execution_context.scope.scope_id,
        effective_workspace = ?execution_context.effective_workspace,
        "run_agent: execution context resolved"
    );
    control
        .append_action_event(ActionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            scope_type: execution_context.scope.scope_type.clone(),
            scope_id: execution_context.scope.scope_id.clone(),
            entity_id: execution_context.entity_id.clone(),
            task_id: Some(task.id.clone()),
            agent_id: Some(agent.id.clone()),
            capability: CapabilityId::MessageInternal,
            execution_level: ExecutionLevel::Coordinate,
            target: execution_context.effective_workspace.clone(),
            outcome: ActionOutcome::Allowed,
            reason: execution_context.scope.resolution_reason.clone(),
        })
        .await;

    let is_enrichment = task.task_kind.as_deref() == Some("entity-enrichment");
    let is_interpretation = task.task_kind.as_deref() == Some("prompt-interpretation");

    let lane = crate::planner::lane_for_entity(entity.as_ref());
    let tools_enabled = crate::planner::allow_llm_tool_calls(lane) && !is_interpretation;
    let planner_decision =
        crate::planner::planner_decision_for_run(lane, tools_enabled, task.task_kind.as_deref());
    debug!(
        task_id = %task_id,
        planner = %planner_decision.detail_str(),
        "run_agent: planner decision"
    );

    let (memory_snippet, long_memory_snippet): (Option<String>, Option<String>) =
        if let (Some(eid), Some(mem)) = (task.entity_id.as_ref(), memory.as_ref()) {
            let short = crate::entity_memory::recall_entity_short(mem.clone(), eid, 4).await;
            let long = crate::entity_memory::recall_entity_long(mem.clone(), eid, 4).await;
            let join = |lines: Vec<String>| {
                if lines.is_empty() {
                    None
                } else {
                    Some(lines.join(" · "))
                }
            };
            (join(short), join(long))
        } else {
            (None, None)
        };
    if is_enrichment && let Some(ref eid) = task.entity_id {
        let mut entities = control.entities.write().await;
        if let Some(e) = entities.iter_mut().find(|e| e.id == *eid)
            && let Some(ref mut en) = e.enrichment
        {
            en.status = "running".to_string();
        }
    }

    let llm_mode = control.llm_mode.read().await.clone();
    let task_kind = task.task_kind.as_deref().unwrap_or("");
    let tk_override = {
        let overrides = control.routing_task_kind_overrides.read().await;
        overrides.get(task_kind).cloned()
    };
    let task_kind_entry = merge_task_kind_routing_entry(
        config.task_kind_routing.get(task_kind).cloned(),
        tk_override,
    );
    // Resolution order for the generation tier:
    //   1. Explicit `generation_tier` on this task-kind's routing entry.
    //   2. Per-entity `preferred_model_tier`.
    //   3. Semantic default (operator/interpreter) when those tiers exist.
    //   4. Mode-based legacy preference (set inside apply_mode_to_tier_resolution).
    let tier_resolution = config::TierResolution {
        generation_tier: task_kind_entry
            .as_ref()
            .and_then(|e| e.generation_tier.clone())
            .or_else(|| execution_context.preferred_model_tier.clone())
            .or_else(|| config::semantic_tier_for_task(config, task.task_kind.as_deref())),
        review_tier: task_kind_entry.as_ref().and_then(|e| e.review_tier.clone()),
        fallback_tiers: task_kind_entry
            .as_ref()
            .and_then(|e| e.fallback_tiers.clone())
            .unwrap_or_default(),
    };
    let tier_resolution = config::apply_mode_to_tier_resolution(
        config,
        &llm_mode,
        task.task_kind.as_deref(),
        tier_resolution,
    );

    let runtime_provider = config.llm_provider.as_str();
    let runtime_model = config.default_model.as_str();

    // Three-way resolution: an explicit tier wins first, then a task-kind's
    // own provider/model entry, then the deployment defaults.
    let (llm_resolved, mode_preset, selected_tier) = if let Some(ref tier) =
        tier_resolution.generation_tier
    {
        let tier_override = {
            let overrides = control.routing_model_tier_overrides.read().await;
            overrides.get(tier).cloned()
        };
        // An operator may have repointed this tier at a specific provider/model.
        if let Some(route) = tier_override {
            if let (Some(provider), Some(model)) =
                (route.provider.as_deref(), route.model.as_deref())
            {
                let (resolved, mode) = config::resolve_llm_for_task(config, None, provider, model);
                let mode = route
                    .mode
                    .as_deref()
                    .and_then(|m| config.mode_presets.get(m))
                    .cloned()
                    .or(mode);
                (resolved, mode, Some(tier.clone()))
            } else {
                let (resolved, mode) =
                    config::resolve_llm_for_tier(config, tier, runtime_provider, runtime_model);
                (resolved, mode, Some(tier.clone()))
            }
        } else {
            let (resolved, mode) =
                config::resolve_llm_for_tier(config, tier, runtime_provider, runtime_model);
            (resolved, mode, Some(tier.clone()))
        }
    } else if let Some(ref entry) = task_kind_entry {
        // No tier resolved; fall back to this task-kind's routing entry, if any.
        if let (Some(provider), Some(model)) = (entry.provider.as_deref(), entry.model.as_deref()) {
            let (resolved, mode) = config::resolve_llm_for_task(config, None, provider, model);
            let mode = entry
                .mode
                .as_deref()
                .and_then(|m| config.mode_presets.get(m))
                .cloned()
                .or(mode);
            (resolved, mode, None)
        } else {
            let (resolved, mode) = config::resolve_llm_for_task(
                config,
                task.task_kind.as_deref(),
                runtime_provider,
                runtime_model,
            );
            (resolved, mode, None)
        }
    } else {
        // Neither a tier nor a task-kind entry resolved; use deployment defaults.
        let (resolved, mode) = config::resolve_llm_for_task(
            config,
            task.task_kind.as_deref(),
            runtime_provider,
            runtime_model,
        );
        (resolved, mode, None)
    };
    let (base_url, api_key_env, model_id) =
        llm_resolved.ok_or_else(|| "LLM resolution failed (no provider/config)".to_string())?;

    // Build the ordered list of attempts: the primary resolution first, then
    // each fallback tier that resolves to a distinct provider/model.
    let mut llm_attempts: Vec<LlmAttempt> = Vec::new();
    llm_attempts.push(LlmAttempt {
        tier: selected_tier.clone(),
        provider_label: runtime_provider.to_string(),
        base_url,
        api_key_env,
        model_id: model_id.clone(),
        mode_preset: mode_preset.clone(),
    });
    for fallback_tier in &tier_resolution.fallback_tiers {
        let (resolved, fallback_mode) = {
            let overrides = control.routing_model_tier_overrides.read().await;
            if let Some(route) = overrides.get(fallback_tier).cloned() {
                if let (Some(provider), Some(model)) =
                    (route.provider.as_deref(), route.model.as_deref())
                {
                    let (resolved, mode) =
                        config::resolve_llm_for_task(config, None, provider, model);
                    let mode = route
                        .mode
                        .as_deref()
                        .and_then(|m| config.mode_presets.get(m))
                        .cloned()
                        .or(mode);
                    (resolved, mode)
                } else {
                    config::resolve_llm_for_tier(
                        config,
                        fallback_tier,
                        runtime_provider,
                        runtime_model,
                    )
                }
            } else {
                config::resolve_llm_for_tier(config, fallback_tier, runtime_provider, runtime_model)
            }
        };
        if let Some((fb_base_url, fb_api_key_env, fb_model_id)) = resolved
            && !llm_attempts.iter().any(|attempt| {
                attempt.base_url == fb_base_url
                    && attempt.api_key_env == fb_api_key_env
                    && attempt.model_id == fb_model_id
            })
        {
            llm_attempts.push(LlmAttempt {
                tier: Some(fallback_tier.clone()),
                provider_label: runtime_provider.to_string(),
                base_url: fb_base_url,
                api_key_env: fb_api_key_env,
                model_id: fb_model_id,
                mode_preset: fallback_mode,
            });
        }
    }

    let (prompt_system, prompt_user) = if is_enrichment {
        (
            build_enrichment_system_prompt(),
            build_enrichment_user_prompt(entity.as_ref()),
        )
    } else if is_interpretation {
        (
            build_interpretation_system_prompt(),
            build_interpretation_user_prompt(&task, entity.as_ref()),
        )
    } else {
        (
            build_system_prompt(&agent, director_group.as_ref()),
            build_user_prompt(&task, director_group.as_ref(), entity.as_ref()),
        )
    };

    let prompt_system = augment_system_with_entity_memory(
        prompt_system,
        task.entity_id.is_some(),
        memory_snippet.as_deref(),
        long_memory_snippet.as_deref(),
    );
    let prompt_system = augment_system_with_entity_narrative(prompt_system, entity.as_ref());

    let messages = vec![
        ChatMessage {
            role: Role::System,
            content: prompt_system,
            tool_calls: None,
            tool_call_id: None,
        },
        ChatMessage {
            role: Role::User,
            content: prompt_user,
            tool_calls: None,
            tool_call_id: None,
        },
    ];

    let tool_registry = build_scoped_tool_registry(&execution_context);
    let tool_defs = tool_definitions(&tool_registry);

    let director_id = task.assigned_director_id.as_deref().unwrap_or("—");
    let mut last_error: Option<String> = None;
    let mut result_text: Option<String> = None;

    // Try each attempt in order; a call/provider error moves to the next one
    // via `continue 'attempts`, so a bad primary target doesn't fail the task.
    'attempts: for attempt in llm_attempts.clone() {
        // Entity-scoped: an entity holding its own key for this provider uses it,
        // and falls back to the deployment's shared one when it does not.
        let api_key =
            resolve_llm_api_key_for_entity(&attempt.api_key_env, task.entity_id.as_deref());
        let provider: Box<dyn LlmProvider> =
            build_provider(config, &attempt.base_url, &api_key, &attempt.api_key_env);
        *control.current_model_display.write().await = if let Some(ref tier) = attempt.tier {
            format!("{} · {} · {}", llm_mode, tier, attempt.model_id)
        } else {
            format!("{} · {}", llm_mode, attempt.model_id)
        };

        let (temperature, max_tokens) = attempt
            .mode_preset
            .clone()
            .map(|p| (p.temperature.unwrap_or(0.3), p.max_tokens.unwrap_or(4096)))
            .unwrap_or((0.3, 4096));
        let provider_supports_tools = attempt.api_key_env != "DASHSCOPE_API_KEY";
        let mut request = CompletionRequest {
            model: attempt.model_id.clone(),
            messages: messages.clone(),
            tools: if tools_enabled && provider_supports_tools {
                tool_defs.clone()
            } else {
                None
            },
            max_tokens: Some(max_tokens),
            temperature: Some(temperature),
            stream: false,
        };

        info!(
            task_id = %task_id,
            task_kind = ?task.task_kind,
            director_id = %director_id,
            agent = %agent.name,
            model = %attempt.model_id,
            tier = ?attempt.tier,
            "run_agent: calling LLM"
        );

        // Call the LLM, then execute any tool calls it requested, feeding
        // results back until it replies with plain text (no more tool calls).
        let current_result = loop {
            let llm_call =
                tokio::time::timeout(LLM_CALL_TIMEOUT, provider.complete(request.clone()));
            let response = match llm_call.await {
                Ok(Ok(r)) => r,
                Err(_) => {
                    let err_text = format!(
                        "LLM call exceeded {}s timeout — provider may be down or model overloaded.",
                        LLM_CALL_TIMEOUT.as_secs()
                    );
                    control
                        .append_action_event(ActionEvent {
                            id: uuid::Uuid::new_v4().to_string(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            scope_type: execution_context.scope.scope_type.clone(),
                            scope_id: execution_context.scope.scope_id.clone(),
                            entity_id: execution_context.entity_id.clone(),
                            task_id: Some(task.id.clone()),
                            agent_id: Some(agent.id.clone()),
                            capability: CapabilityId::MessageInternal,
                            execution_level: ExecutionLevel::Draft,
                            target: Some("llm.complete".to_string()),
                            outcome: ActionOutcome::Failed,
                            reason: Some(err_text.clone()),
                        })
                        .await;
                    error!(task_id = %task.id, "run_agent: {}", err_text);
                    last_error = Some(format!(
                        "{} [{}] {}",
                        attempt
                            .tier
                            .clone()
                            .unwrap_or_else(|| "default".to_string()),
                        attempt.model_id,
                        err_text
                    ));
                    continue 'attempts;
                }
                Ok(Err(e)) => {
                    let err_text = e.to_string();
                    control
                        .append_action_event(ActionEvent {
                            id: uuid::Uuid::new_v4().to_string(),
                            timestamp: chrono::Utc::now().to_rfc3339(),
                            scope_type: execution_context.scope.scope_type.clone(),
                            scope_id: execution_context.scope.scope_id.clone(),
                            entity_id: execution_context.entity_id.clone(),
                            task_id: Some(task.id.clone()),
                            agent_id: Some(agent.id.clone()),
                            capability: CapabilityId::MessageInternal,
                            execution_level: ExecutionLevel::Draft,
                            target: Some("llm.complete".to_string()),
                            outcome: ActionOutcome::Failed,
                            reason: Some(err_text.clone()),
                        })
                        .await;
                    error!(task_id = %task.id, error = %e, "run_agent: LLM call failed");
                    if is_enrichment
                        && crate::enrichment_llm::is_enrichment_immediate_terminal_llm_error(
                            &err_text,
                        )
                    {
                        if let Some(ref eid) = task.entity_id {
                            let mut entities = control.entities.write().await;
                            if let Some(ent) = entities.iter_mut().find(|x| x.id == *eid)
                                && let Some(ref mut en) = ent.enrichment
                            {
                                en.status = "provider_invalid".to_string();
                                en.error = Some(format!(
                                    "The LLM provider rejected this request (check API key, base URL, and model in Settings). {err_text}"
                                ));
                            }
                        }
                        return Ok(RunAgentResult {
                            outcome: TaskOutcome::Failed,
                            message: Some(format!("Enrichment failed: {err_text}")),
                        });
                    }
                    last_error = Some(format!(
                        "{} [{}] {}",
                        attempt
                            .tier
                            .clone()
                            .unwrap_or_else(|| "default".to_string()),
                        attempt.model_id,
                        err_text
                    ));
                    continue 'attempts;
                }
            };
            control
                .append_action_event(ActionEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    scope_type: execution_context.scope.scope_type.clone(),
                    scope_id: execution_context.scope.scope_id.clone(),
                    entity_id: execution_context.entity_id.clone(),
                    task_id: Some(task.id.clone()),
                    agent_id: Some(agent.id.clone()),
                    capability: CapabilityId::MessageInternal,
                    execution_level: ExecutionLevel::Draft,
                    target: Some("llm.complete".to_string()),
                    outcome: ActionOutcome::Allowed,
                    reason: Some("llm completion succeeded".to_string()),
                })
                .await;

            if response.tool_calls.is_empty() {
                break response
                    .content
                    .unwrap_or_else(|| "[No content returned]".to_string());
            }

            let tool_calls = response.tool_calls;
            request.messages.push(ChatMessage {
                role: Role::Assistant,
                content: response.content.unwrap_or_default(),
                tool_calls: Some(tool_calls.clone()),
                tool_call_id: None,
            });

            for tool_call in tool_calls {
                let executed = execute_tool_call(
                    &tool_registry,
                    &execution_context,
                    &control,
                    &task,
                    &agent,
                    &tool_call,
                )
                .await;
                // A tool that needs a human decision stops the task here rather
                // than looping forever waiting for an approval that never comes.
                if matches!(executed.outcome, ActionOutcome::ApprovalRequired) {
                    let approval_message = format!(
                        "Tool call '{}' requires human approval before task can continue. {}",
                        tool_call.name, executed.result.output
                    );
                    let mut tasks = control.tasks.write().await;
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == task.id) {
                        t.requires_human_approval = true;
                        t.result = Some(approval_message.clone());
                    }
                    return Ok(RunAgentResult {
                        outcome: TaskOutcome::NeedsHumanApproval,
                        message: Some(approval_message),
                    });
                }
                request.messages.push(ChatMessage {
                    role: Role::Tool,
                    content: executed.result.output,
                    tool_calls: None,
                    tool_call_id: Some(executed.result.call_id.clone()),
                });
            }
        };

        result_text = Some(current_result);
        break;
    }

    let result_text = result_text.ok_or_else(|| {
        last_error.unwrap_or_else(|| "All LLM attempts failed without a recorded error".to_string())
    })?;

    debug!(task_id = %task_id, len = result_text.len(), "run_agent: got response");

    // If the task was cancelled while this run was in flight, do not overwrite its
    // terminal state. The orchestrator's outcome handling also guards on this.
    if control
        .tasks
        .read()
        .await
        .iter()
        .any(|t| t.id == task_id && t.status == TaskStatus::Cancelled)
    {
        return Ok(RunAgentResult {
            outcome: TaskOutcome::Completed,
            message: Some("Task was cancelled during the run.".to_string()),
        });
    }

    if is_interpretation {
        let (stored_result, ok) = match parse_prompt_interpretation_brief(&result_text) {
            Ok(brief) => match serde_json::to_string_pretty(&brief) {
                Ok(json) => (json, true),
                Err(e) => (format!("Interpretation failed (serialize): {}", e), false),
            },
            Err(e) => (format!("Interpretation failed: {}", e), false),
        };
        let done = {
            let mut tasks = control.tasks.write().await;
            if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
                // Keep the result either way; only claim completion if the
                // operator has not cancelled the task out from under the run.
                if crate::state::set_task_status_by_loop(t, TaskStatus::Completed) {
                    t.progress = 100;
                    t.outcome = Some(if ok {
                        TaskOutcome::Completed
                    } else {
                        TaskOutcome::NeedsReview
                    });
                }
                t.result = Some(stored_result.clone());
                info!(
                    task_id = %task_id,
                    task_kind = "prompt-interpretation",
                    model = %model_id,
                    director_id = %director_id,
                    parsed_ok = ok,
                    "run_agent: interpretation task completed"
                );
                Some(t.clone())
            } else {
                None
            }
        };
        if let Some(ref t) = done {
            control
                .push_operator_feed(OperatorFeedKind::TaskCompleted, t, None)
                .await;
        }
        return Ok(RunAgentResult {
            outcome: if ok {
                TaskOutcome::Completed
            } else {
                TaskOutcome::NeedsReview
            },
            message: None,
        });
    }

    if is_enrichment {
        let (task_result, enrichment_ready) = match parse_enrichment_draft(&result_text) {
            Ok(draft) => {
                if let Some(ref eid) = task.entity_id {
                    let mut entities = control.entities.write().await;
                    if let Some(e) = entities.iter_mut().find(|e| e.id == *eid)
                        && let Some(ref mut en) = e.enrichment
                    {
                        en.status = "ready".to_string();
                        en.draft = Some(draft);
                        en.model = Some(model_id.clone());
                        en.generated_at = Some(chrono::Utc::now().to_rfc3339());
                        en.generated_by = task.assigned_agent_id.clone();
                        en.error = None;
                    }
                }
                (result_text, true)
            }
            Err(e) => {
                if let Some(ref eid) = task.entity_id {
                    let mut entities = control.entities.write().await;
                    if let Some(ent) = entities.iter_mut().find(|x| x.id == *eid)
                        && let Some(ref mut en) = ent.enrichment
                    {
                        en.status = "failed".to_string();
                        en.error = Some(e.clone());
                    }
                }
                (format!("Enrichment failed: {}", e), false)
            }
        };
        let done = {
            let mut tasks = control.tasks.write().await;
            if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
                if crate::state::set_task_status_by_loop(t, TaskStatus::Completed) {
                    t.progress = 100;
                    t.outcome = Some(if enrichment_ready {
                        TaskOutcome::Completed
                    } else {
                        TaskOutcome::NeedsReview
                    });
                }
                t.result = Some(task_result);
                info!(
                    task_id = %task_id,
                    task_kind = "entity-enrichment",
                    model = %model_id,
                    director_id = %director_id,
                    ready = enrichment_ready,
                    "run_agent: enrichment task completed"
                );
                Some(t.clone())
            } else {
                None
            }
        };
        if let Some(ref t) = done {
            control
                .push_operator_feed(OperatorFeedKind::TaskCompleted, t, None)
                .await;
        }
        Ok(RunAgentResult {
            outcome: if enrichment_ready {
                TaskOutcome::Completed
            } else {
                TaskOutcome::NeedsReview
            },
            message: None,
        })
    } else {
        // Adversarial gate: grade the output before accepting `Completed`. A
        // needs-review verdict downgrades the outcome (loop-safe: the task still
        // completes) and surfaces a VerificationRequested feed event for a human.
        let verdict = if crate::verification::enabled() {
            crate::verification::verify_task_output(&result_text)
        } else {
            crate::verification::VerificationVerdict::pass()
        };
        let outcome = if verdict.needs_review() {
            TaskOutcome::NeedsReview
        } else {
            TaskOutcome::Completed
        };
        let feed_kind = if verdict.needs_review() {
            OperatorFeedKind::VerificationNeedsReview
        } else {
            OperatorFeedKind::TaskCompleted
        };
        let stored_result = verdict.annotate_result(result_text);
        let done = {
            let mut tasks = control.tasks.write().await;
            if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
                if crate::state::set_task_status_by_loop(t, TaskStatus::Completed) {
                    t.progress = 100;
                    t.outcome = Some(outcome);
                }
                t.result = Some(stored_result);
                match t.metadata {
                    Some(serde_json::Value::Object(ref mut m)) => {
                        m.insert("verification".to_string(), verdict.to_metadata_value());
                    }
                    _ => {
                        t.metadata = Some(
                            serde_json::json!({ "verification": verdict.to_metadata_value() }),
                        );
                    }
                }
                info!(
                    task_id = %task_id,
                    needs_review = verdict.needs_review(),
                    "run_agent: task completed"
                );
                Some(t.clone())
            } else {
                None
            }
        };
        if let Some(ref t) = done {
            control
                .push_operator_feed(feed_kind, t, verdict.feed_detail())
                .await;
        }
        Ok(RunAgentResult {
            outcome,
            message: None,
        })
    }
}

// Appends short-term then long-term entity-memory context to the system
// prompt; a no-op for tasks that aren't entity-scoped.
fn augment_system_with_entity_memory(
    mut prompt: String,
    entity_scoped: bool,
    memory_snippet: Option<&str>,
    long_memory_snippet: Option<&str>,
) -> String {
    if !entity_scoped {
        return prompt;
    }
    if let Some(m) = memory_snippet.filter(|s| !s.trim().is_empty()) {
        prompt.push_str("\n\n[Entity memory context — stay consistent with these notes]:\n");
        prompt.push_str(m);
    }
    if let Some(m) = long_memory_snippet.filter(|s| !s.trim().is_empty()) {
        prompt.push_str("\n\n[Entity long-term memory — durable facts]:\n");
        prompt.push_str(m);
    }
    prompt
}

// Appends one labeled narrative field to `prompt`, only if it has content.
fn append_narrative_section(prompt: &mut String, label: &str, value: &Option<String>) {
    if let Some(v) = value.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        prompt.push('\n');
        prompt.push_str(label);
        prompt.push_str(": ");
        prompt.push_str(v);
    }
}

// Appends the entity's profile/goals/active-context/operating-rules/content-pipeline
// narrative fields to the system prompt, if any are set.
fn augment_system_with_entity_narrative(mut prompt: String, entity: Option<&Entity>) -> String {
    let Some(ent) = entity else {
        return prompt;
    };
    let any_set = ent.profile.is_some()
        || ent.goals.is_some()
        || ent.active_context.is_some()
        || ent.operating_rules.is_some()
        || ent.content_pipeline.is_some();
    if !any_set {
        return prompt;
    }
    prompt.push_str("\n\n[Entity narrative — long-term purpose; bias work toward these]:");
    append_narrative_section(&mut prompt, "Profile", &ent.profile);
    append_narrative_section(&mut prompt, "Goals", &ent.goals);
    append_narrative_section(&mut prompt, "Active context", &ent.active_context);
    append_narrative_section(&mut prompt, "Operating rules", &ent.operating_rules);
    append_narrative_section(&mut prompt, "Content pipeline", &ent.content_pipeline);
    prompt
}

// Builds the tool registry scoped to this execution context: no workspace
// means no filesystem/shell tools; http/browser require an explicit
// HttpFetch capability grant.
fn build_scoped_tool_registry(execution_context: &ExecutionContext) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    // No workspace means no jail root: never fall back to the gateway's own
    // cwd — a context without a directory gets no filesystem/shell tools.
    // An absolute path, or no tools at all. A relative workspace ("." , "..",
    // "work") resolves against the *gateway's* cwd, which is the very thing
    // this guard exists to prevent — rejecting only the empty string left the
    // hole open for every other relative form.
    let Some(workspace) = execution_context
        .effective_workspace
        .clone()
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty() && std::path::Path::new(w).is_absolute())
    else {
        if let Some(search) = WebSearchTool::from_env() {
            registry.register(Box::new(search));
        }
        return registry;
    };

    // Entities without explicit execution policies keep jailed file/shell
    // work, but nothing privileged or outward-capable (no http/browser).
    let allowed_capabilities = if execution_context.allowed_capabilities.is_empty() {
        vec![
            CapabilityId::FsRead,
            CapabilityId::FsWrite,
            CapabilityId::ShellExecSafe,
        ]
    } else {
        execution_context.allowed_capabilities.clone()
    };

    registry.register(Box::new(FileSystemTool {
        workspace_root: workspace.clone(),
        policy: Some(FileSystemPolicy {
            deny_by_default: true,
            allowed_capabilities: allowed_capabilities.clone(),
            read_roots: if execution_context.allowed_resource_roots.is_empty() {
                vec![workspace.clone()]
            } else {
                execution_context.allowed_resource_roots.clone()
            },
            write_roots: if execution_context.allowed_resource_roots.is_empty() {
                vec![workspace.clone()]
            } else {
                execution_context.allowed_resource_roots.clone()
            },
            list_roots: if execution_context.allowed_resource_roots.is_empty() {
                vec![workspace.clone()]
            } else {
                execution_context.allowed_resource_roots.clone()
            },
            search_roots: if execution_context.allowed_resource_roots.is_empty() {
                vec![workspace.clone()]
            } else {
                execution_context.allowed_resource_roots.clone()
            },
        }),
    }));

    registry.register(Box::new(ShellTool {
        allowed_commands: None,
        denied_commands: shell::default_hard_deny_patterns(),
        working_dir: workspace.clone(),
        policy: Some(ShellPolicy {
            deny_by_default: true,
            allowed_capabilities: allowed_capabilities.clone(),
            approval_required_patterns: shell::default_approval_required_patterns(),
            hard_deny_patterns: shell::default_hard_deny_patterns(),
            allowed_cwd_roots: if execution_context.allowed_resource_roots.is_empty() {
                vec![workspace.clone()]
            } else {
                execution_context.allowed_resource_roots.clone()
            },
        }),
    }));

    // Http/browser reach the whole internet from the user's machine: only
    // entities whose execution policy grants http.fetch get them, and the
    // loopback denylist keeps the gateway's own API out of reach.
    if allowed_capabilities.contains(&CapabilityId::HttpFetch) {
        registry.register(Box::new(HttpTool::new(
            None,
            vec!["localhost".to_string(), "127.0.0.1".to_string()],
        )));
        registry.register(Box::new(BrowserTool::new(
            None,
            None,
            vec!["localhost".to_string(), "127.0.0.1".to_string()],
        )));
    }
    if let Some(search) = WebSearchTool::from_env() {
        registry.register(Box::new(search));
    }

    registry
}

fn tool_definitions(registry: &ToolRegistry) -> Option<Vec<ToolDefinition>> {
    let defs = registry
        .list_definitions()
        .into_iter()
        .map(|t| ToolDefinition {
            name: t["name"].as_str().unwrap_or_default().to_string(),
            description: t["description"].as_str().unwrap_or_default().to_string(),
            parameters: t["parameters"].clone(),
        })
        .collect::<Vec<_>>();
    if defs.is_empty() { None } else { Some(defs) }
}

// Injects a `_policy` object into a tool call's arguments so each tool
// enforces this task's execution-context scoping (workspace root, allowed
// capabilities, resource roots) rather than trusting the LLM's arguments alone.
fn enrich_tool_arguments_for_execution_context(
    execution_context: &ExecutionContext,
    tool_name: &str,
    arguments: &Value,
) -> Value {
    let mut enriched = arguments.clone();
    // Without a workspace there is no jail root to scope the override to (and
    // no filesystem/shell tool registered); never point policies at the
    // gateway's own cwd.
    let Some(workspace) = execution_context
        .effective_workspace
        .clone()
        .map(|w| w.trim().to_string())
        .filter(|w| !w.is_empty() && std::path::Path::new(w).is_absolute())
    else {
        return enriched;
    };
    let allowed_roots = if execution_context.allowed_resource_roots.is_empty() {
        vec![workspace.clone()]
    } else {
        execution_context.allowed_resource_roots.clone()
    };
    let policy_json = match tool_name {
        "filesystem" => json!({
            "workspaceRoot": workspace,
            "allowedCapabilities": execution_context.allowed_capabilities.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            "readRoots": allowed_roots.clone(),
            "writeRoots": allowed_roots.clone(),
            "listRoots": allowed_roots.clone(),
            "searchRoots": allowed_roots,
        }),
        "shell" => json!({
            "workingDir": workspace,
            "allowedCapabilities": execution_context.allowed_capabilities.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            "allowedCwdRoots": allowed_roots,
        }),
        // Screenshots land inside the task's workspace instead of the system temp dir, so the
        // agent can hand the file to the filesystem tool afterwards. Domain allow/deny stays with
        // the tool's own configuration unless a context tightens it.
        "browser" => json!({
            "screenshotDir": workspace,
        }),
        _ => return enriched,
    };
    if let Some(obj) = enriched.as_object_mut() {
        obj.insert("_policy".to_string(), policy_json);
    }
    enriched
}

// Executes one tool call, classifies it into a capability + execution level
// for the audit log, runs it, and records the outcome as an ActionEvent.
async fn execute_tool_call(
    registry: &ToolRegistry,
    execution_context: &ExecutionContext,
    control: &Arc<ControlState>,
    task: &Task,
    agent: &Agent,
    tool_call: &ToolCall,
) -> ExecutedToolCall {
    let Some(tool) = registry.get(&tool_call.name) else {
        return ExecutedToolCall {
            result: ToolResult {
                call_id: tool_call.id.clone(),
                output: format!("unknown tool: {}", tool_call.name),
                is_error: true,
            },
            outcome: ActionOutcome::Denied,
        };
    };

    let capability = match tool_call.name.as_str() {
        "filesystem" => {
            if tool_call.arguments.get("action").and_then(|v| v.as_str()) == Some("write") {
                CapabilityId::FsWrite
            } else {
                CapabilityId::FsRead
            }
        }
        "shell" => tool_call
            .arguments
            .get("capability")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<CapabilityId>().ok())
            .unwrap_or(CapabilityId::ShellExecSafe),
        _ => CapabilityId::MessageInternal,
    };
    let execution_level = match capability {
        CapabilityId::FsRead => ExecutionLevel::Observe,
        CapabilityId::FsWrite => ExecutionLevel::InternalAct,
        CapabilityId::ShellExecSafe | CapabilityId::ShellExecPrivileged => {
            ExecutionLevel::SensitiveExec
        }
        _ => ExecutionLevel::Draft,
    };

    let arguments = enrich_tool_arguments_for_execution_context(
        execution_context,
        &tool_call.name,
        &tool_call.arguments,
    );
    let result = match tool.execute(arguments).await {
        Ok(mut r) => {
            r.call_id = tool_call.id.clone();
            r
        }
        Err(e) => ToolResult {
            call_id: tool_call.id.clone(),
            output: e.to_string(),
            is_error: true,
        },
    };

    let target = tool_target(&tool_call.name, &tool_call.arguments);
    let (outcome, reason) = if !result.is_error {
        (
            ActionOutcome::Allowed,
            Some(format!(
                "tool execution succeeded{}",
                target
                    .as_ref()
                    .map(|t| format!(" ({t})"))
                    .unwrap_or_default()
            )),
        )
    } else if result.output.contains("NEEDS_HUMAN_APPROVAL") {
        (
            ActionOutcome::ApprovalRequired,
            extract_policy_message(&result.output).or_else(|| Some(result.output.clone())),
        )
    } else {
        (
            ActionOutcome::Denied,
            extract_policy_message(&result.output).or_else(|| Some(result.output.clone())),
        )
    };

    control
        .append_action_event(ActionEvent {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            scope_type: execution_context.scope.scope_type.clone(),
            scope_id: execution_context.scope.scope_id.clone(),
            entity_id: execution_context.entity_id.clone(),
            task_id: Some(task.id.clone()),
            agent_id: Some(agent.id.clone()),
            capability,
            execution_level,
            target,
            outcome: outcome.clone(),
            reason,
        })
        .await;

    ExecutedToolCall { result, outcome }
}

fn extract_policy_message(raw: &str) -> Option<String> {
    serde_json::from_str::<Value>(raw).ok().and_then(|v| {
        v.get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string)
    })
}

// Human-readable target string for the audit log, per tool type.
fn tool_target(tool_name: &str, arguments: &Value) -> Option<String> {
    match tool_name {
        "filesystem" => arguments
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| format!("filesystem:{}", p)),
        "shell" => arguments
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(|cwd| format!("shell:{}", cwd))
            .or_else(|| {
                arguments
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(|cmd| format!("shell:{}", cmd))
            }),
        _ => Some(tool_name.to_string()),
    }
}

/// Derives the [`ExecutionContext`] for a task: an entity-scoped task
/// inherits the entity's resource roots and execution policy (and is
/// rejected up front if the agent's group isn't allowed for that entity); a
/// task without an entity gets a minimal principal-scoped context with no
/// filesystem/shell access.
fn build_execution_context(
    task: &Task,
    agent: &Agent,
    entity: Option<&Entity>,
) -> Result<ExecutionContext, String> {
    match (task.entity_id.as_ref(), entity) {
        (Some(entity_id), Some(entity)) => {
            if !entity.allowed_agent_groups.is_empty()
                && !entity
                    .allowed_agent_groups
                    .iter()
                    .any(|g| g == &agent.group_id)
            {
                return Err(format!(
                    "agent group '{}' is not allowed for entity '{}'",
                    agent.group_id, entity_id
                ));
            }

            let allowed_resource_roots: Vec<String> =
                entity.resources.iter().map(|r| r.root.clone()).collect();
            let effective_workspace = entity
                .default_workspace_root
                .clone()
                .or_else(|| allowed_resource_roots.first().cloned())
                .or_else(|| entity.path.clone());
            let (allowed_capabilities, allowed_execution_levels, approval_required_capabilities) =
                if let Some(policy) = &entity.execution_policies {
                    (
                        policy.allowed_capabilities.clone(),
                        policy.allowed_execution_levels.clone(),
                        policy.approval_required_capabilities.clone(),
                    )
                } else {
                    (Vec::new(), Vec::new(), Vec::new())
                };

            Ok(ExecutionContext {
                scope: ResolvedScope {
                    scope_type: ScopeType::Entity,
                    scope_id: entity_id.clone(),
                    resolution_reason: Some("task-entity-id".to_string()),
                },
                entity_id: Some(entity_id.clone()),
                allowed_resource_roots,
                allowed_capabilities,
                allowed_execution_levels,
                approval_required_capabilities,
                effective_workspace,
                memory_scope: entity.memory_scope.clone(),
                preferred_model_tier: entity.preferred_model_tier.clone(),
            })
        }
        (Some(entity_id), None) => Err(format!(
            "task {} references missing entity '{}'",
            task.id, entity_id
        )),
        (None, _) => Ok(ExecutionContext {
            scope: ResolvedScope {
                scope_type: ScopeType::Principal,
                scope_id: format!("task:{}", task.id),
                resolution_reason: Some("task-principal-fallback".to_string()),
            },
            entity_id: None,
            allowed_resource_roots: Vec::new(),
            allowed_capabilities: Vec::new(),
            allowed_execution_levels: Vec::new(),
            approval_required_capabilities: Vec::new(),
            effective_workspace: None,
            memory_scope: None,
            preferred_model_tier: None,
        }),
    }
}

// Strips an optional ```json fence before parsing the enrichment draft.
fn parse_enrichment_draft(text: &str) -> Result<EnrichmentDraft, String> {
    let json_str = text.trim();
    let json_str = json_str
        .strip_prefix("```json")
        .or_else(|| json_str.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```").map(|s| s.trim()))
        .unwrap_or(json_str);
    serde_json::from_str(json_str).map_err(|e| format!("invalid enrichment JSON: {}", e))
}

fn build_enrichment_system_prompt() -> String {
    r#"You are the Entity Intake Assistant. Given minimal metadata about an entity, create a realistic editable draft.
Do not invent specific real-world claims unless directly supported by the input.
Prefer defaults, assumptions, and placeholders. Return a single JSON object with exactly these string fields (use empty string if no value): profile, goals, activeContext, operatingRules, notes, contentPipeline.
No markdown, no explanation outside the JSON. Example: {"profile":"...","goals":"...","activeContext":"...","operatingRules":"...","notes":"...","contentPipeline":"..."}"#.to_string()
}

fn build_enrichment_user_prompt(entity: Option<&Entity>) -> String {
    let e = match entity {
        Some(x) => x,
        None => return "No entity context.".to_string(),
    };
    let mut parts = vec![format!("Entity name: {}", e.name)];
    if let Some(ref d) = e.description {
        parts.push(format!("Description: {}", d));
    }
    if !e.dept.is_empty() {
        parts.push(format!("Department: {}", e.dept));
    }
    if let Some(ref o) = e.owner {
        parts.push(format!("Owner: {}", o));
    }
    parts.push("Return JSON only.".to_string());
    parts.join("\n")
}

fn build_system_prompt(agent: &Agent, director_group: Option<&AgentGroup>) -> String {
    let mut lines = Vec::new();
    if let Some(group) = director_group {
        lines.push(format!(
            "You are part of the {group_name} director group ({group_desc}).",
            group_name = group.name,
            group_desc = group.description
        ));
    }
    lines.push(format!(
        "You are {name}, a {role} ({kind}).",
        name = agent.name,
        role = agent.role,
        kind = agent.kind,
    ));
    lines.push(format!(
        "Preferred provider/model: {provider} / {model}.",
        provider = agent.preferred_provider,
        model = agent.preferred_model
    ));
    lines.push(format!("Mission: {}", agent.mission));
    lines.push(format!("Profile: {}", agent.profile));
    lines.join("\n\n").to_string()
}

fn build_user_prompt(
    task: &Task,
    director_group: Option<&AgentGroup>,
    entity: Option<&Entity>,
) -> String {
    let mut parts = vec![
        format!("Task: {}", task.title),
        format!("Department: {}", task.dept),
    ];
    if let Some(group) = director_group {
        parts.push(format!(
            "Director: {name} — {desc}",
            name = group.name,
            desc = group.description
        ));
    }
    if !task.roles.is_empty() {
        parts.push(format!("Roles: {}", task.roles.join(", ")));
    }
    if let Some(e) = entity {
        parts.push(format!("Entity: {}", e.name));
        if let Some(ref stage) = e.stage {
            parts.push(format!("Entity stage: {}", stage));
        }
        if !e.domains.is_empty() {
            parts.push(format!("Entity domains: {}", e.domains.join(", ")));
        }
        if !e.markets.is_empty() {
            parts.push(format!("Entity markets: {}", e.markets.join(", ")));
        }
        if let Some(ref p) = e.profile {
            parts.push(format!("Entity profile: {}", p));
        }
        if let Some(ref g) = e.goals {
            parts.push(format!("Entity goals: {}", g));
        }
        if let Some(ref r) = e.operating_rules {
            parts.push(format!("Operating rules: {}", r));
        }
        if let Some(ref c) = e.active_context {
            parts.push(format!("Active context: {}", c));
        }
    }
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_task() -> Task {
        Task {
            id: "task-1".to_string(),
            title: "test".to_string(),
            dept: "General".to_string(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some("ent-1".to_string()),
            mission_id: None,
            priority: None,
            requires_human_approval: false,
            assigned_director_id: None,
            assigned_agent_id: Some("agent-1".to_string()),
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
        }
    }

    fn base_agent() -> Agent {
        Agent {
            id: "agent-1".to_string(),
            group_id: "dir-1".to_string(),
            name: "Agent".to_string(),
            role: "role".to_string(),
            kind: "kind".to_string(),
            preferred_provider: "openai".to_string(),
            preferred_model: "gpt".to_string(),
            mission: "mission".to_string(),
            profile: "profile".to_string(),
        }
    }

    fn base_entity() -> Entity {
        Entity {
            id: "ent-1".to_string(),
            name: "Entity".to_string(),
            status: "Active".to_string(),
            dept: "General".to_string(),
            path: Some("/tmp/entity".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn execution_context_blocks_disallowed_agent_group() {
        let task = base_task();
        let agent = base_agent();
        let mut entity = base_entity();
        entity.allowed_agent_groups = vec!["other-dir".to_string()];

        let err = build_execution_context(&task, &agent, Some(&entity)).unwrap_err();
        assert!(err.contains("not allowed"));
    }

    #[test]
    fn execution_context_uses_entity_workspace_defaults() {
        let task = base_task();
        let agent = base_agent();
        let mut entity = base_entity();
        entity.default_workspace_root = Some("/workspace/entity".to_string());
        entity.memory_scope = Some("entity-memory".to_string());
        entity.preferred_model_tier = Some("local_fast".to_string());

        let ctx = build_execution_context(&task, &agent, Some(&entity)).unwrap();
        assert!(matches!(ctx.scope.scope_type, ScopeType::Entity));
        assert_eq!(ctx.scope.scope_id, "ent-1");
        assert_eq!(
            ctx.effective_workspace.as_deref(),
            Some("/workspace/entity")
        );
        assert_eq!(ctx.memory_scope.as_deref(), Some("entity-memory"));
    }

    #[test]
    fn augment_memory_appends_short_and_long_sections() {
        let out = augment_system_with_entity_memory(
            "base".to_string(),
            true,
            Some("prefers metric units"),
            Some("HQ is in Kuwait City"),
        );
        assert!(out.starts_with("base"));
        let short_idx = out
            .find("[Entity memory context — stay consistent with these notes]:")
            .expect("short section present");
        let long_idx = out
            .find("[Entity long-term memory — durable facts]:")
            .expect("long section present");
        assert!(short_idx < long_idx, "short-term renders before long-term");
        assert!(out.contains("prefers metric units"));
        assert!(out.contains("HQ is in Kuwait City"));
    }

    #[test]
    fn augment_memory_long_only_and_not_entity_scoped() {
        let out = augment_system_with_entity_memory(
            "base".to_string(),
            true,
            None,
            Some("HQ is in Kuwait City"),
        );
        assert!(!out.contains("[Entity memory context"));
        assert!(out.contains("[Entity long-term memory — durable facts]:"));

        let untouched = augment_system_with_entity_memory(
            "base".to_string(),
            false,
            Some("ignored"),
            Some("ignored"),
        );
        assert_eq!(untouched, "base");
    }
}
