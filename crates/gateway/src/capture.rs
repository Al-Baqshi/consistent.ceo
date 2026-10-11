//! Capture interpreter facade: `POST /api/capture/interpret` (LLM → structured intent) and
//! `POST /api/capture/commit` (apply via existing `control_api::internal_create_*` paths).
//! Uses the Operations director's configured agent preferences as the capture default.
//!
//! Enhanced with chat feedback: `POST /api/capture/chat` for bidirectional conversation
//! with analysis, recommendations, and execution confirmations.

use crate::config::AppConfig;
use crate::control_api::{
    CommandResponse, CreateCommandRequest, CreateEntityRequest, CreateTaskRequest,
    internal_create_command, internal_create_entity, internal_create_task,
};
use crate::credentials_resolver::resolve_llm_api_key;
use crate::state::{ControlState, Entity};
use async_trait::async_trait;
use axum::http::StatusCode;
use consistent_world_llm::anthropic::AnthropicProvider;
use consistent_world_llm::client::{
    ChatMessage, CompletionRequest, CompletionResponse, LlmProvider, Role,
};
use consistent_world_llm::cost::CostCalculator;
use consistent_world_llm::google::GoogleProvider;
use consistent_world_llm::ollama::OllamaProvider;
use consistent_world_llm::openai::OpenAiProvider;
use consistent_world_llm::streaming::TokenStream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

const DEFAULT_WORKSPACE_ID: &str = "default";
pub(crate) const DEFAULT_CAPTURE_DIRECTOR_ID: &str = "operations";
pub(crate) const DEFAULT_CAPTURE_AGENT_ID: &str = "ops-coordinator";
const DEFAULT_CAPTURE_MODEL: &str = "brain-default";

/// Resolved LLM provider + model for the Operations director, alongside the
/// originally preferred provider/model (kept for logging and UI display).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResolvedOperationsLlm {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: String,
    pub preferred_provider: String,
    pub preferred_model: String,
}

// Falls back to the default workspace when an entity has no explicit owner.
pub(crate) fn entity_workspace_id(entity: &Entity) -> &str {
    entity
        .owner
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or(DEFAULT_WORKSPACE_ID)
}

// Provider dispatch: match base_url/api_key_env hints to the right
// LlmProvider implementation. `cfg` is only consulted when a requested CLI
// provider turns out to be missing and a registered local lane can stand in.
pub(crate) fn build_llm_client(
    cfg: &AppConfig,
    base_url: &str,
    api_key: &str,
    api_key_env: &str,
) -> Box<dyn LlmProvider> {
    // The locally-installed `claude` CLI, driven as a provider. Checked first
    // because it is the only one that needs no key at all — it rides the
    // operator's existing subscription, so a director assigned this muscle
    // costs nothing per call.
    let is_claude_code = base_url.starts_with("claude-code://")
        || api_key_env == "CLAUDE_CODE_CLI"
        || base_url == "claude-code";
    if is_claude_code {
        if let Some(p) = consistent_world_llm::claude_code::ClaudeCodeProvider::new(None) {
            return Box::new(p);
        }
        // The old fall-through hit the heuristic chain below, where
        // "claude-code://" matches nothing and became an OpenAI client whose
        // every call died with a transport error — silently.
        if let Some(local) = cli_missing_fallback(cfg, "claude-code") {
            return local;
        }
    }
    let is_codex =
        base_url.starts_with("codex://") || api_key_env == "CODEX_CLI" || base_url == "codex";
    if is_codex {
        if let Some(p) = consistent_world_llm::codex::CodexProvider::new(None) {
            return Box::new(p);
        }
        // Same trap as claude-code above: falling through turns "codex://"
        // into an OpenAI client whose every call dies with a transport error.
        if let Some(local) = cli_missing_fallback(cfg, "codex") {
            return local;
        }
    }
    let is_ollama = is_ollama_target(base_url, api_key_env);
    let is_google = api_key_env == "GOOGLE_API_KEY"
        || api_key_env == "GEMINI_API_KEY"
        || base_url.contains("generativelanguage.googleapis.com");
    let is_anthropic = api_key_env == "ANTHROPIC_API_KEY" || base_url.contains("api.anthropic.com");
    if is_ollama {
        Box::new(OllamaProvider::new(Some(base_url.to_string())))
    } else if is_google {
        Box::new(GoogleProvider::new(
            api_key.to_string(),
            Some(base_url.to_string()),
        ))
    } else if is_anthropic {
        Box::new(AnthropicProvider::new(
            api_key.to_string(),
            Some(base_url.to_string()),
        ))
    } else {
        Box::new(OpenAiProvider::new(
            api_key.to_string(),
            Some(base_url.to_string()),
        ))
    }
}

// Shared "is this the local Ollama lane" heuristic — also the heartbeat retry
// path's test for "the run that just failed was already local".
pub(crate) fn is_ollama_target(base_url: &str, api_key_env: &str) -> bool {
    api_key_env == "OLLAMA_API_KEY" || base_url.contains(":11434") || base_url.contains("ollama")
}

// Substitute client for "CLI provider requested, CLI missing": the registered
// Ollama lane with every request's model pinned to a local id — the caller's
// model id belongs to the missing provider and would 404 against Ollama.
// `None` (plus a loud warn) when no local lane is registered either.
fn cli_missing_fallback(cfg: &AppConfig, provider: &str) -> Option<Box<dyn LlmProvider>> {
    match crate::config::resolve_local_ollama_lane(cfg) {
        Some((base_url, _api_key_env, model)) => {
            tracing::warn!(
                model = %model,
                base_url = %base_url,
                "{provider} provider requested but its CLI was not found; \
                 using the local Ollama muscle instead"
            );
            Some(Box::new(PinnedModelProvider {
                inner: Box::new(OllamaProvider::new(Some(base_url))),
                model,
            }))
        }
        None => {
            tracing::warn!(
                "{provider} provider requested but its CLI was not found, and no \
                 `ollama` provider is registered to fall back to — LLM calls through this \
                 client WILL fail until one or the other is fixed"
            );
            None
        }
    }
}

/// Wraps a substitute provider and pins every request's model id — used when
/// the requested provider is unavailable and the substitute cannot serve the
/// caller's model id.
struct PinnedModelProvider {
    inner: Box<dyn LlmProvider>,
    model: String,
}

#[async_trait]
impl LlmProvider for PinnedModelProvider {
    async fn complete(&self, mut request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
        request.model = self.model.clone();
        self.inner.complete(request).await
    }

    async fn stream_complete(&self, mut request: CompletionRequest) -> anyhow::Result<TokenStream> {
        request.model = self.model.clone();
        self.inner.stream_complete(request).await
    }

    fn provider_name(&self) -> &'static str {
        self.inner.provider_name()
    }
}

// System prompt for the Capture Interpreter: classifies a message into
// exactly one structured intent.
fn capture_system_prompt() -> String {
    r#"You are the Capture Interpreter. Classify the user's message into exactly one intent and return one JSON object only.

When `requestedAction` is `note`, `task`, or `reminder`, treat the request as task creation unless the only blocker is ambiguity that requires `clarify`. Do not downgrade those to `chat`.

Allowed intent values (string, use exactly these spellings):
- "chat" — general question or conversation; no structural create.
- "create_entity" — user wants a new entity/hub (brand, product, project, etc.).
- "create_space" — user wants a workspace/namespace (v1: treat as an entity container; use space name).
- "create_entity_and_space" — both an entity and a named workspace root.
- "create_task" — user wants a task/todo/work item.
- "clarify" — ambiguous (e.g. multiple matching entities); ask one short question.

Rules:
- Use the provided entity catalog (id + name). If the user likely refers to one entity, set "entityMatch" to that entity id.
- If multiple plausible entities, set intent to "clarify" and list their ids in "candidateEntityIds" with a clear "clarifyingQuestion".
- "confidence" is a number from 0 to 1.
- "draft" object may include: entityName, entityType, entitySummary, spaceName, taskTitle, taskBody, chatMessage (all optional strings).
- Use null for optional fields when absent (not empty string), except intent and confidence are always set.

Return JSON only with these keys: intent, confidence, entityMatch, candidateEntityIds, clarifyingQuestion, draft.
No markdown fences, no commentary."#
        .to_string()
}

// Build the user-turn prompt: raw text, the entity catalog, and any hints
// (requested action, selected entity, workspace context) supplied by the caller.
fn capture_user_prompt(
    text: &str,
    requested_action: Option<&str>,
    selected_entity_id: Option<&str>,
    workspace_context: Option<&str>,
    catalog: &[(String, String)],
) -> String {
    let catalog_json: Vec<_> = catalog
        .iter()
        .map(|(id, name)| json!({"id": id, "name": name}))
        .collect();
    let mut parts = vec![
        format!("User text:\n{}", text.trim()),
        format!(
            "Entity catalog (workspace):\n{}",
            serde_json::to_string_pretty(&catalog_json).unwrap_or_else(|_| "[]".to_string())
        ),
    ];
    if let Some(action) = requested_action.filter(|s| !s.trim().is_empty()) {
        parts.push(format!("requestedAction: {}", action.trim()));
    }
    if let Some(eid) = selected_entity_id.filter(|s| !s.trim().is_empty()) {
        parts.push(format!("Selected entity id (UI context): {}", eid));
    }
    if let Some(ctx) = workspace_context.filter(|s| !s.trim().is_empty()) {
        parts.push(format!("Workspace / user context:\n{}", ctx));
    }
    parts.push("Return JSON only.".to_string());
    parts.join("\n\n")
}

fn strip_json_fence(text: &str) -> &str {
    let json_str = text.trim();

    (json_str
        .strip_prefix("```json")
        .or_else(|| json_str.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```").map(|s| s.trim()))
        .unwrap_or(json_str)) as _
}

// Scan for the first balanced {...} object, respecting quoted strings and
// escapes, in case the model wrapped its JSON in surrounding prose.
fn extract_json_object(text: &str) -> Option<String> {
    let s = strip_json_fence(text).trim();
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escape = false;

    for (offset, ch) in s[start..].char_indices() {
        if in_string {
            if escape {
                escape = false;
                continue;
            }
            match ch {
                '\\' => escape = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    let end = start + offset + ch.len_utf8();
                    return Some(s[start..end].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_capture_response(raw_text: &str) -> Option<LlmCaptureRaw> {
    let primary = strip_json_fence(raw_text).trim();
    if let Ok(v) = serde_json::from_str::<LlmCaptureRaw>(primary) {
        return Some(v);
    }
    let extracted = extract_json_object(raw_text)?;
    serde_json::from_str::<LlmCaptureRaw>(&extracted).ok()
}

/// Optional fields an LLM (or the fallback path) may populate when drafting a
/// capture — entity, task, space, or chat content, depending on intent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureDraft {
    #[serde(default)]
    pub entity_name: Option<String>,
    #[serde(default)]
    pub entity_type: Option<String>,
    #[serde(default)]
    pub entity_summary: Option<String>,
    #[serde(default)]
    pub space_name: Option<String>,
    #[serde(default)]
    pub task_title: Option<String>,
    #[serde(default)]
    pub task_body: Option<String>,
    #[serde(default)]
    pub chat_message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LlmCaptureRaw {
    #[serde(default)]
    intent: String,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    entity_match: Option<String>,
    #[serde(default)]
    candidate_entity_ids: Vec<String>,
    #[serde(default)]
    clarifying_question: Option<String>,
    #[serde(default)]
    draft: Option<CaptureDraft>,
}

/// Request body for `POST /api/capture/interpret`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureInterpretRequest {
    pub text: String,
    #[serde(default)]
    pub entity_id: Option<String>,
    #[serde(default)]
    pub workspace_context: Option<String>,
    #[serde(default)]
    pub requested_action: Option<String>,
}

/// Structured intent produced by the Capture Interpreter.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureInterpretResponse {
    pub intent: String,
    pub confidence: f64,
    pub entity_match: Option<String>,
    pub candidate_entity_ids: Vec<String>,
    pub clarifying_question: Option<String>,
    pub draft: CaptureDraft,
}

/// Request body for `POST /api/capture/commit`: apply a previously
/// interpreted (or clarified) intent via the existing create paths.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCommitRequest {
    pub intent: String,
    #[serde(default)]
    pub draft: CaptureDraft,
    #[serde(default)]
    pub entity_id: Option<String>,
    #[serde(default)]
    pub director_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub mission_id: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
    /// When committing via command path (chat): note | task | reminder
    #[serde(default)]
    pub capture_intent: Option<String>,
    /// Echo from interpret for clarify-shaped commits (`ok: false` responses).
    #[serde(default)]
    pub clarifying_question: Option<String>,
    #[serde(default)]
    pub candidate_entity_ids: Vec<String>,
    /// When intent is `create_task`: note | task | reminder (default task).
    #[serde(default)]
    pub task_kind: Option<String>,
}

/// Result of committing a capture intent — either a created record, or a
/// clarify response when `ok` is false.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureCommitResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Set when `ok` is false (e.g. clarify — not an HTTP error).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clarifying_question: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_entity_ids: Vec<String>,
}

fn normalize_commit_task_kind(raw: Option<&str>) -> String {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("note") => "note".into(),
        Some("reminder") | Some("followup") | Some("follow-up") => "reminder".into(),
        Some(s) if !s.is_empty() => s.to_string(),
        _ => "task".into(),
    }
}

fn normalize_intent(raw: &str) -> String {
    let s = raw.trim().to_ascii_lowercase().replace(' ', "_");
    match s.as_str() {
        "create-entity" => "create_entity".into(),
        "create-space" => "create_space".into(),
        "create_entity_and_space" | "create-entity-and-space" | "create_entity_space" => {
            "create_entity_and_space".into()
        }
        "create-task" | "create_task" => "create_task".into(),
        "clarify" | "ambiguous" => "clarify".into(),
        "chat" | "message" | "conversation" => "chat".into(),
        _ => s,
    }
}

fn clarify_response(question: impl Into<String>) -> CaptureInterpretResponse {
    CaptureInterpretResponse {
        intent: "clarify".into(),
        confidence: 0.0,
        entity_match: None,
        candidate_entity_ids: vec![],
        clarifying_question: Some(question.into()),
        draft: CaptureDraft::default(),
    }
}

// Best-effort task draft used when the LLM's structured response could not be parsed.
fn fallback_task_response(text: &str, requested_action: Option<&str>) -> CaptureInterpretResponse {
    let trimmed = text.trim();
    let title = trimmed
        .lines()
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(140).collect::<String>());
    let task_body = if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    };
    let draft = CaptureDraft {
        task_title: title,
        task_body,
        ..CaptureDraft::default()
    };
    let clarifying_question = requested_action.and_then(|action| match action {
        "note" | "task" | "reminder" => None,
        _ => Some(
            "I could not confidently interpret that. Please rephrase or pick an entity."
                .to_string(),
        ),
    });
    CaptureInterpretResponse {
        intent: "create_task".into(),
        confidence: 0.35,
        entity_match: None,
        candidate_entity_ids: vec![],
        clarifying_question,
        draft,
    }
}

/// Resolve the Operations director's preferred provider + model, falling back
/// through the seeded agent-group default, then the config's own defaults.
pub(crate) async fn operations_capture_preferences(
    config: &AppConfig,
    control: &ControlState,
) -> (String, String) {
    let agents = control.agents.read().await;
    if let Some(agent) = agents
        .iter()
        .find(|a| a.id == DEFAULT_CAPTURE_AGENT_ID)
        .or_else(|| {
            agents
                .iter()
                .find(|a| a.group_id == DEFAULT_CAPTURE_DIRECTOR_ID)
        })
    {
        if !agent.preferred_provider.trim().is_empty() {
            return (
                agent.preferred_provider.clone(),
                if agent.preferred_model.trim().is_empty() {
                    DEFAULT_CAPTURE_MODEL.to_string()
                } else {
                    agent.preferred_model.clone()
                },
            );
        }
        // Agent exists but is provider-agnostic (empty preference): fall
        // through to the config defaults, keeping any explicit model.
        if !agent.preferred_model.trim().is_empty() {
            let provider = if config.llm_provider.trim().is_empty() {
                "openai".to_string()
            } else {
                config.llm_provider.clone()
            };
            return (provider, agent.preferred_model.clone());
        }
    }
    // No seeded agent (fresh deploy) or agnostic agent: honor the config's
    // defaults — a hard-coded "openai" here silently overrode cloud configs
    // whose default is a different provider.
    let provider = if config.llm_provider.trim().is_empty() {
        "openai".to_string()
    } else {
        config.llm_provider.clone()
    };
    let model = if config.default_model.trim().is_empty() {
        DEFAULT_CAPTURE_MODEL.to_string()
    } else {
        config.default_model.clone()
    };
    (provider, model)
}

fn fallback_api_key_env(provider_name: &str, provider_type: &str) -> String {
    let name = provider_name.trim().to_ascii_lowercase();
    let kind = provider_type.trim().to_ascii_lowercase();
    if kind == "ollama" || kind == "local" || kind == "lmstudio" || name == "ollama" {
        "OLLAMA_API_KEY".to_string()
    } else if kind == "anthropic" || name == "anthropic" || name == "claude" {
        "ANTHROPIC_API_KEY".to_string()
    } else if kind == "google"
        || kind == "google_ai_studio"
        || kind == "gemini"
        || name.contains("google")
    {
        "GOOGLE_API_KEY".to_string()
    } else if kind == "deepseek" || name == "deepseek" {
        "DEEPSEEK_API_KEY".to_string()
    } else {
        "OPENAI_API_KEY".to_string()
    }
}

/// Resolve the Operations director's LLM into a ready-to-call configuration.
pub(crate) async fn resolve_operations_llm(
    config: &AppConfig,
    control: &ControlState,
) -> Option<ResolvedOperationsLlm> {
    let (preferred_provider, preferred_model) =
        operations_capture_preferences(config, control).await;
    resolve_llm_for_preference(config, control, &preferred_provider, &preferred_model).await
}

pub(crate) async fn resolve_llm_for_preference(
    config: &AppConfig,
    control: &ControlState,
    preferred_provider: &str,
    preferred_model: &str,
) -> Option<ResolvedOperationsLlm> {
    // `resolve_llm` never answers None: a provider the TOML has never heard of
    // silently collapses to the global default (`[llm]`, i.e. Ollama). Left
    // unguarded that makes the runtime-registered branch below unreachable, so
    // every provider connected through the UI resolved back to Ollama and
    // choosing a model appeared to do nothing at all. Ask first whether the
    // config genuinely knows this provider; if it does not, the answer has to
    // come from what was registered at runtime.
    let config_knows_provider =
        crate::config::resolve_llm_provider_from_config(config, preferred_provider).is_some();
    if config_knows_provider
        && let Some((mut base_url, mut api_key_env, mut model_id)) =
            crate::config::resolve_llm(config, preferred_provider, preferred_model)
    {
        let mut provider = preferred_provider.to_string();
        let mut hops = 0;
        while let Some((prov, real)) = config.llm_models_map.get(&model_id).cloned() {
            if real == model_id {
                break;
            }
            model_id = real;
            provider = prov.clone();
            if let Some((b, a)) = config.llm_providers_map.get(&prov) {
                base_url = b.clone();
                api_key_env = a.clone();
            }
            hops += 1;
            if hops > 4 {
                break;
            }
        }
        return Some(ResolvedOperationsLlm {
            provider,
            model: model_id,
            base_url,
            api_key_env,
            preferred_provider: preferred_provider.to_string(),
            preferred_model: preferred_model.to_string(),
        });
    }

    let providers = control.llm_providers.read().await;
    let provider = providers
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(preferred_provider))?;
    let model = preferred_model.trim();
    if model.is_empty() {
        return None;
    }
    Some(ResolvedOperationsLlm {
        provider: provider.name.clone(),
        model: model.to_string(),
        base_url: provider.base_url.clone().unwrap_or_default(),
        // Rows saved before the allowlist existed could name any environment
        // variable; such a name falls back to the provider's standard key var
        // rather than being read.
        api_key_env: provider
            .api_key_env
            .clone()
            .filter(|env| crate::credentials_resolver::is_allowed_llm_key_env(env))
            .unwrap_or_else(|| fallback_api_key_env(&provider.name, &provider.provider_type)),
        preferred_provider: preferred_provider.to_string(),
        preferred_model: preferred_model.to_string(),
    })
}

/// Run structured capture interpretation using the Operations director's preferred AI.
pub async fn interpret_capture(
    state: &crate::state::ServerState,
    control: &ControlState,
    config: &AppConfig,
    workspace_id: &str,
    req: CaptureInterpretRequest,
) -> Result<CaptureInterpretResponse, StatusCode> {
    let text = req.text.trim();
    if text.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let catalog: Vec<(String, String)> = {
        let entities = control.entities.read().await;
        entities
            .iter()
            // Routing candidates are business entities only — a capture is never
            // disambiguated into a user's private Personal Space.
            .filter(|e| entity_workspace_id(e) == workspace_id && !e.is_personal)
            .map(|e| (e.id.clone(), e.name.clone()))
            .collect()
    };

    let requested_action = req.requested_action.as_deref().map(str::trim);
    let user_prompt = capture_user_prompt(
        text,
        requested_action,
        req.entity_id.as_deref(),
        req.workspace_context.as_deref(),
        &catalog,
    );

    let resolved = resolve_operations_llm(config, control)
        .await
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let api_key = resolve_llm_api_key(&resolved.api_key_env);
    let provider = build_llm_client(config, &resolved.base_url, &api_key, &resolved.api_key_env);

    let messages = vec![
        ChatMessage {
            role: Role::System,
            content: capture_system_prompt(),
            tool_calls: None,
            tool_call_id: None,
        },
        ChatMessage {
            role: Role::User,
            content: user_prompt,
            tool_calls: None,
            tool_call_id: None,
        },
    ];
    let request = CompletionRequest {
        model: resolved.model,
        messages,
        tools: None,
        max_tokens: Some(1024),
        temperature: Some(0.2),
        stream: false,
    };

    let response = provider
        .complete(request)
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    if let Some(ledger) = state.credit_ledger.as_ref() {
        let calc = CostCalculator::new();
        let raw_cost = calc.calculate(
            &response.model,
            response.usage.input_tokens,
            response.usage.output_tokens,
        );
        if raw_cost > 0.0 {
            let billed = ledger.debit_for_cost(workspace_id, raw_cost).await;
            tracing::debug!(
                workspace_id,
                model = %response.model,
                input_tokens = response.usage.input_tokens,
                output_tokens = response.usage.output_tokens,
                raw_cost_usd = raw_cost,
                billed_cents = billed,
                "debited capture/interpret LLM call"
            );
        }
    }

    let raw_text = response.content.unwrap_or_default();
    let parsed: LlmCaptureRaw = match parse_capture_response(&raw_text) {
        Some(v) => v,
        None => {
            warn!(requested_action = ?requested_action, raw_output = %raw_text, "capture interpret parse failed; using fallback");
            if matches!(requested_action, Some("note" | "task" | "reminder")) {
                return Ok(fallback_task_response(text, requested_action));
            }
            return Ok(clarify_response(
                "I could not parse the interpreter output. Please rephrase or try again.",
            ));
        }
    };

    let intent = normalize_intent(&parsed.intent);
    let allowed = [
        "chat",
        "create_entity",
        "create_space",
        "create_entity_and_space",
        "create_task",
        "clarify",
    ];
    let intent = if allowed.contains(&intent.as_str()) {
        intent
    } else {
        "clarify".into()
    };

    let mut intent = intent;
    if matches!(requested_action, Some("note" | "task" | "reminder")) && intent == "chat" {
        intent = "create_task".to_string();
    }

    let draft = parsed.draft.unwrap_or_default();
    Ok(CaptureInterpretResponse {
        intent,
        confidence: parsed.confidence.clamp(0.0, 1.0),
        entity_match: parsed.entity_match.filter(|s| !s.trim().is_empty()),
        candidate_entity_ids: parsed.candidate_entity_ids,
        clarifying_question: parsed.clarifying_question.filter(|s| !s.trim().is_empty()),
        draft,
    })
}

async fn default_director_id(control: &ControlState) -> Option<String> {
    let groups = control.agent_groups.read().await;
    groups
        .iter()
        .find(|g| g.id == DEFAULT_CAPTURE_DIRECTOR_ID)
        .or_else(|| groups.first())
        .map(|g| g.id.clone())
}

/// Apply a confirmed capture intent using existing create paths.
pub async fn commit_capture(
    state: &crate::state::ServerState,
    control: &ControlState,
    workspace_id: &str,
    // The caller, so their own Personal Space counts as visible when a task
    // or command targets it (a `None` viewer never sees a personal entity).
    principal_id: Option<&str>,
    req: CaptureCommitRequest,
) -> Result<CaptureCommitResponse, StatusCode> {
    let intent = normalize_intent(&req.intent);
    match intent.as_str() {
        "clarify" => Ok(CaptureCommitResponse {
            ok: false,
            entity_id: None,
            task_id: None,
            command: None,
            message: Some("Nothing to commit — clarify first, then interpret again.".into()),
            intent: Some("clarify".into()),
            clarifying_question: req.clarifying_question.clone(),
            candidate_entity_ids: req.candidate_entity_ids.clone(),
        }),
        "create_entity" => {
            let name = req
                .draft
                .entity_name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or(StatusCode::BAD_REQUEST)?;
            let body = CreateEntityRequest {
                name,
                description: req.draft.entity_summary.clone(),
                department: None,
                owner: None,
                status: None,
                entity_type: req.draft.entity_type.clone(),
                default_workspace_root: None,
                stage: None,
                setup: None,
            };
            let entity = internal_create_entity(state, control, workspace_id, body).await?;
            Ok(CaptureCommitResponse {
                ok: true,
                entity_id: Some(entity.id),
                task_id: None,
                command: None,
                message: None,
                intent: None,
                clarifying_question: None,
                candidate_entity_ids: vec![],
            })
        }
        "create_space" => {
            let name = req
                .draft
                .space_name
                .clone()
                .or_else(|| req.draft.entity_name.clone())
                .filter(|s| !s.trim().is_empty())
                .ok_or(StatusCode::BAD_REQUEST)?;
            let body = CreateEntityRequest {
                name: name.clone(),
                description: req.draft.entity_summary.clone(),
                department: None,
                owner: None,
                status: None,
                entity_type: None,
                default_workspace_root: Some(name),
                stage: None,
                setup: None,
            };
            let entity = internal_create_entity(state, control, workspace_id, body).await?;
            Ok(CaptureCommitResponse {
                ok: true,
                entity_id: Some(entity.id),
                task_id: None,
                command: None,
                message: None,
                intent: None,
                clarifying_question: None,
                candidate_entity_ids: vec![],
            })
        }
        "create_entity_and_space" => {
            let name = req
                .draft
                .entity_name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or(StatusCode::BAD_REQUEST)?;
            let space = req
                .draft
                .space_name
                .clone()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| name.clone());
            let body = CreateEntityRequest {
                name,
                description: req.draft.entity_summary.clone(),
                department: None,
                owner: None,
                status: None,
                entity_type: req.draft.entity_type.clone(),
                default_workspace_root: Some(space),
                stage: None,
                setup: None,
            };
            let entity = internal_create_entity(state, control, workspace_id, body).await?;
            Ok(CaptureCommitResponse {
                ok: true,
                entity_id: Some(entity.id),
                task_id: None,
                command: None,
                message: None,
                intent: None,
                clarifying_question: None,
                candidate_entity_ids: vec![],
            })
        }
        "create_task" => {
            let title = req
                .draft
                .task_title
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| {
                    req.draft.task_body.as_deref().and_then(|b| {
                        let line = b.trim().lines().next()?.trim();
                        if line.is_empty() {
                            None
                        } else {
                            Some(line.chars().take(140).collect())
                        }
                    })
                })
                .ok_or(StatusCode::BAD_REQUEST)?;
            let body = CreateTaskRequest {
                title,
                dept: None,
                roles: None,
                entity_id: req.entity_id.clone(),
                mission_id: req.mission_id.clone(),
                priority: None,
                requires_human_approval: None,
                task_kind: Some(normalize_commit_task_kind(req.task_kind.as_deref())),
                content: req.draft.task_body.clone(),
                director_id: Some(DEFAULT_CAPTURE_DIRECTOR_ID.to_string()),
                agent_id: Some(DEFAULT_CAPTURE_AGENT_ID.to_string()),
                connection_id: None,
                external_source: None,
                external_id: None,
                metadata: None,
                due_at: None,
            };
            let task = internal_create_task(control, workspace_id, principal_id, body).await?;
            Ok(CaptureCommitResponse {
                ok: true,
                entity_id: None,
                task_id: Some(task.id),
                command: None,
                message: None,
                intent: None,
                clarifying_question: None,
                candidate_entity_ids: vec![],
            })
        }
        "chat" => {
            let prompt = req
                .draft
                .chat_message
                .clone()
                .or(req.draft.task_body.clone())
                .filter(|s| !s.trim().is_empty())
                .ok_or(StatusCode::BAD_REQUEST)?;
            let director_id = match req.director_id.clone().filter(|s| !s.trim().is_empty()) {
                Some(d) => d,
                None => default_director_id(control)
                    .await
                    .ok_or(StatusCode::BAD_REQUEST)?,
            };
            let cmd = CreateCommandRequest {
                director_id,
                agent_id: req.agent_id.clone(),
                entity_id: req.entity_id.clone(),
                mission_id: req.mission_id.clone(),
                prompt,
                mode: req.mode.clone(),
                capture_intent: req.capture_intent.clone(),
            };
            let command = internal_create_command(control, workspace_id, principal_id, cmd).await?;
            Ok(CaptureCommitResponse {
                ok: true,
                entity_id: command.entity_id.clone(),
                task_id: Some(command.task_id.clone()),
                command: Some(command),
                message: None,
                intent: None,
                clarifying_question: None,
                candidate_entity_ids: vec![],
            })
        }
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

// ─── Chat Feedback (Bidirectional Capture) ────────────────────────────────────

/// Chat-based capture with analysis and feedback
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureChatRequest {
    /// User's message
    pub message: String,
    /// Optional entity context
    pub entity_id: Option<String>,
    /// Explicit chat scope. `workspace` means all visible entities; `entity`
    /// means the selected entity. Missing preserves legacy Personal Space
    /// behavior for older clients.
    #[serde(default)]
    pub scope: Option<String>,
    /// Current local offset in minutes east of UTC, supplied by the client for
    /// wall-clock reminders. The stored instant remains UTC.
    #[serde(default)]
    pub timezone_offset_minutes: Option<i32>,
    /// Optional workspace context
    pub workspace_context: Option<String>,
    /// Conversation history (optional)
    #[serde(default)]
    pub history: Vec<CaptureChatMessage>,
    /// Session ID for short memory buffer
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Chat message in conversation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureChatMessage {
    pub role: String, // "user" or "assistant"
    pub content: String,
}

/// Smart-routing outcome for a loose chat turn that happened in the user's
/// Personal Space. Tells the UI whether the message was auto-filed into a
/// business entity (with an undo affordance) or merely suggested.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRouting {
    /// `auto_filed` | `suggest` | `personal`.
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_name: Option<String>,
    /// Created capture id for an auto-file, so the UI can offer "Undo".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undo_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Chat response with analysis and actions
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureChatResponse {
    /// Response to show user
    pub message: String,
    /// Analysis of user's request
    pub analysis: ChatAnalysis,
    /// Actions taken (if any)
    pub actions: Vec<ChatAction>,
    /// Suggestions for next steps
    pub suggestions: Vec<String>,
    /// Whether follow-up is needed
    pub needs_followup: bool,
    /// Session ID for short memory buffer
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Smart-routing outcome when the turn happened in the user's Personal Space.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing: Option<ChatRouting>,
    /// What this answer rests on: `observed` when a tool produced something
    /// during the turn, `inferred` when it is the model's own reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<crate::evidence::ChatEvidence>,
}

/// Analysis of user's request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAnalysis {
    /// Detected intent
    pub intent: String,
    /// Confidence score
    pub confidence: f32,
    /// Entity context (if any)
    pub entity_context: Option<String>,
    /// Key entities/actions detected
    pub key_points: Vec<String>,
    /// Readiness assessment (if entity involved)
    pub readiness_note: Option<String>,
}

/// Action taken by capture
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatAction {
    /// Action type
    pub action_type: String,
    /// What was done
    pub description: String,
    /// Result ID (task/entity/mission ID)
    pub result_id: Option<String>,
    /// Status
    pub status: String,
}

/// Chat-based capture with bidirectional feedback (natural LLM reply + actions).
pub async fn chat_capture(
    state: &crate::state::ServerState,
    control: &ControlState,
    workspace_id: &str,
    req: CaptureChatRequest,
) -> Result<CaptureChatResponse, StatusCode> {
    crate::capture_chat_pipeline::chat_capture_full(state, control, workspace_id, req).await
}

/// Extract key points from message (used by capture chat pipeline).
pub(crate) fn extract_key_points(message: &str) -> Vec<String> {
    let mut points = Vec::new();
    let message_lower = message.to_lowercase();

    // Extract entities mentioned (simplified - would use NLP in production)
    if message_lower.contains("project") || message_lower.contains("product") {
        points.push("Entity/project mentioned".to_string());
    }

    // Extract actions
    if message_lower.contains("create")
        || message_lower.contains("make")
        || message_lower.contains("build")
    {
        points.push("Action: create/build".to_string());
    }
    if message_lower.contains("remind") || message_lower.contains("remember") {
        points.push("Action: reminder".to_string());
    }
    if message_lower.contains("note") || message_lower.contains("remember") {
        points.push("Action: note-taking".to_string());
    }

    // Extract time references
    if message_lower.contains("tomorrow")
        || message_lower.contains("next week")
        || message_lower.contains("at ")
    {
        points.push("Time-sensitive".to_string());
    }

    if points.is_empty() {
        points.push("General request".to_string());
    }

    points
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_intent() {
        assert_eq!(normalize_intent("Create-Entity"), "create_entity");
        assert_eq!(normalize_intent("chat"), "chat");
    }

    #[test]
    fn extracts_key_points() {
        let points = extract_key_points("Remind me to create the project tomorrow");
        assert!(points.iter().any(|p| p.contains("reminder"))); // "Action: reminder"
        assert!(points.iter().any(|p| p.contains("create"))); // "Action: create/build"
        assert!(points.iter().any(|p| p.contains("Time"))); // "Time-sensitive"
    }

    #[test]
    fn claude_cli_missing_falls_back_to_registered_ollama() {
        let mut cfg = AppConfig::default();
        cfg.llm_providers_map.insert(
            "ollama".to_string(),
            (
                "http://127.0.0.1:11434".to_string(),
                "OLLAMA_API_KEY".to_string(),
            ),
        );
        let client = cli_missing_fallback(&cfg, "claude-code").expect("registered local lane");
        assert_eq!(client.provider_name(), "ollama");
    }

    #[test]
    fn claude_cli_missing_without_ollama_keeps_old_fall_through() {
        let cfg = AppConfig::default();
        assert!(
            cli_missing_fallback(&cfg, "claude-code").is_none(),
            "no registered local lane means no substitute client"
        );
    }

    struct ModelRecorder {
        seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl LlmProvider for ModelRecorder {
        async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
            *self.seen.lock().unwrap() = Some(request.model);
            anyhow::bail!("recorded")
        }
        fn provider_name(&self) -> &'static str {
            "mock"
        }
    }

    #[tokio::test]
    async fn pinned_model_provider_rewrites_the_request_model() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let pinned = PinnedModelProvider {
            inner: Box::new(ModelRecorder { seen: seen.clone() }),
            model: "gemma4:latest".to_string(),
        };
        let _ = pinned
            .complete(CompletionRequest {
                model: "claude-opus-4-8".to_string(),
                messages: vec![],
                tools: None,
                max_tokens: None,
                temperature: None,
                stream: false,
            })
            .await;
        assert_eq!(
            seen.lock().unwrap().as_deref(),
            Some("gemma4:latest"),
            "a fallback lane must not be asked for the missing provider's model"
        );
    }
}
