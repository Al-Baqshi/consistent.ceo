//! The AI page: what this computer can think with, and one button to use it.
//!
//! `GET /api/ai` looks at the machine (subscription CLIs, Ollama and its chat
//! models, saved API keys) and says, per provider, whether it is ready and if
//! not what one thing is missing. `POST /api/ai/connect` makes a provider the
//! AI everything runs on — but only after one real turn through it answers,
//! so "connected" on this page always means a model actually replied.
//! `DELETE /api/ai/choice` hands the decision back to detection.
//!
//! The older `/api/llm/*` endpoints stay for the advanced provider tools; this
//! is the path the settings page and onboarding use. See `ai_brain` for how
//! the choice reaches every LLM call.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers.
use super::*;
use crate::ai_brain::{self, Brain, BrainKind};

/// One provider on the AI page.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AiProviderView {
    id: &'static str,
    kind: BrainKind,
    label: &'static str,
    /// Can take a call right now.
    ready: bool,
    /// `ready`, `not_installed`, `signed_out`, `not_running`, `no_model`, `no_key`.
    status: &'static str,
    /// One plain sentence: what is true, and the next step if any.
    detail: String,
    models: Vec<String>,
    default_model: String,
    help_url: &'static str,
    /// Where the subscription CLI lives, so in-app sign-in can run the copy
    /// detection found (an editor's bundled `claude` is not on `PATH`).
    cli_path: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AiActiveView {
    provider: String,
    model: String,
    label: &'static str,
    kind: BrainKind,
    /// Picked by the owner, as opposed to found by detection.
    chosen: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AiStatus {
    /// What answers right now; `None` means nothing on this computer can.
    active: Option<AiActiveView>,
    /// The owner's pick when it is not what answers (it stopped working).
    broken_choice: Option<Brain>,
    providers: Vec<AiProviderView>,
}

fn ollama_base(state: &ServerState) -> String {
    match state.app_config.as_ref() {
        Some(cfg) => ai_brain::ollama_base(cfg),
        None => "http://127.0.0.1:11434".to_string(),
    }
}

fn provider_view(c: &'static ai_brain::Catalog, d: &ai_brain::Detection) -> AiProviderView {
    let (status, detail) = match c.id {
        "claude-code" | "codex" => {
            let cli = if c.id == "claude-code" {
                &d.claude
            } else {
                &d.codex
            };
            let (bin, sign_in, plan) = if c.id == "claude-code" {
                ("Claude Code", "claude", "Claude")
            } else {
                ("Codex", "codex login", "ChatGPT")
            };
            if !cli.installed {
                (
                    "not_installed",
                    format!(
                        "Uses your existing subscription. Install {bin} on this computer first, then connect."
                    ),
                )
            } else if !cli.signed_in {
                (
                    "signed_out",
                    format!(
                        "{bin} is installed but not signed in. Run `{sign_in}` once in Terminal and sign in."
                    ),
                )
            } else {
                (
                    "ready",
                    format!("Uses your {plan} plan. {bin} is installed; no API key needed."),
                )
            }
        }
        "ollama" => {
            if !d.ollama_running {
                (
                    "not_running",
                    "Free and private: runs on this computer. Install Ollama (or open it if it's installed).".to_string(),
                )
            } else if d.ollama_models.is_empty() {
                (
                    "no_model",
                    "Ollama is running but has no chat model yet. Download one (about 5–10 GB)."
                        .to_string(),
                )
            } else {
                (
                    "ready",
                    format!(
                        "Ollama is running with {} chat model{}.",
                        d.ollama_models.len(),
                        if d.ollama_models.len() == 1 { "" } else { "s" }
                    ),
                )
            }
        }
        _ => {
            if d.keys.iter().any(|k| k == c.id) {
                ("ready", "API key saved.".to_string())
            } else {
                (
                    "no_key",
                    "Paste an API key to connect. You pay the provider per use.".to_string(),
                )
            }
        }
    };
    let models = if c.id == "ollama" {
        d.ollama_models.clone()
    } else {
        c.models.iter().map(|m| m.to_string()).collect()
    };
    let default_model = if c.id == "ollama" {
        ai_brain::preferred_local_model(&d.ollama_models)
    } else {
        c.default_model.to_string()
    };
    AiProviderView {
        id: c.id,
        kind: c.kind,
        label: c.label,
        ready: d.usable(c.id),
        status,
        detail,
        models,
        default_model,
        help_url: c.help_url,
        cli_path: match c.id {
            "claude-code" => d.claude.path.clone(),
            "codex" => d.codex.path.clone(),
            _ => None,
        },
    }
}

fn active_view(brain: Brain, chosen: bool) -> Option<AiActiveView> {
    let c = brain.catalog()?;
    Some(AiActiveView {
        label: c.label,
        kind: c.kind,
        provider: brain.provider,
        model: brain.model,
        chosen,
    })
}

/// `GET /api/ai`: a fresh look at the machine, every time the page opens.
pub(super) async fn get_ai_status(State(state): State<ServerState>) -> Json<AiStatus> {
    let detection = ai_brain::refresh(&ollama_base(&state)).await;
    let chosen = ai_brain::chosen();
    let effective = ai_brain::effective();
    let chosen_works = chosen.is_some() && chosen == effective;
    Json(AiStatus {
        active: effective.and_then(|b| active_view(b, chosen_works)),
        broken_choice: chosen.filter(|_| !chosen_works),
        providers: ai_brain::CATALOG
            .iter()
            .map(|c| provider_view(c, &detection))
            .collect(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConnectAiBody {
    provider: String,
    #[serde(default)]
    model: Option<String>,
    /// For API-key providers: a new key to test and, if it works, save.
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConnectAiResult {
    ok: bool,
    provider: String,
    model: String,
    reply: Option<String>,
    latency_ms: Option<u64>,
    error: Option<String>,
}

impl ConnectAiResult {
    fn failed(provider: &str, model: &str, error: impl Into<String>) -> Json<Self> {
        Json(Self {
            ok: false,
            provider: provider.to_string(),
            model: model.to_string(),
            reply: None,
            latency_ms: None,
            error: Some(error.into()),
        })
    }
}

/// For a fresh API key, a model the account can actually call: the catalog
/// default when the provider lists it, else the newest listed of the same
/// family. A failed listing keeps the default; the real turn is the judge.
async fn pick_key_model(c: &ai_brain::Catalog, api_key: &str, requested: Option<&str>) -> String {
    if let Some(m) = requested {
        return m.to_string();
    }
    let Ok(ids) =
        crate::llm_provider_probe::fetch_model_ids(c.base_url, api_key, c.api_key_env).await
    else {
        return c.default_model.to_string();
    };
    if ids.iter().any(|id| id == c.default_model) {
        return c.default_model.to_string();
    }
    let family = match c.id {
        "anthropic" => "sonnet",
        "google" => "flash",
        _ => c.default_model,
    };
    ids.iter()
        .find(|id| id.contains(family) && !crate::llm_provider_probe::is_embedding_model(id))
        .cloned()
        .unwrap_or_else(|| c.default_model.to_string())
}

/// `POST /api/ai/connect`: test one real turn, then make it the AI.
pub(super) async fn connect_ai(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<ConnectAiBody>,
) -> Result<Json<ConnectAiResult>, (StatusCode, Json<serde_json::Value>)> {
    require_admin_json(&auth, &state)?;
    let Some(cfg) = state.app_config.clone() else {
        return Ok(ConnectAiResult::failed(
            &body.provider,
            "",
            "The gateway's configuration is unavailable.",
        ));
    };
    let Some(c) = ai_brain::catalog(&body.provider) else {
        return Ok(ConnectAiResult::failed(
            &body.provider,
            "",
            "Unknown provider.",
        ));
    };
    let requested = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty());
    let new_key = body
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_string);

    let detection = ai_brain::refresh(&ollama_base(&state)).await;
    let (api_key, model) = match c.kind {
        BrainKind::ApiKey => {
            let key = new_key
                .clone()
                .unwrap_or_else(|| credentials_resolver::resolve_llm_api_key(c.api_key_env));
            if key.trim().is_empty() {
                return Ok(ConnectAiResult::failed(
                    c.id,
                    "",
                    "Paste your API key first.",
                ));
            }
            let model = pick_key_model(c, &key, requested).await;
            (key, model)
        }
        BrainKind::Local => {
            if !detection.ollama_running {
                return Ok(ConnectAiResult::failed(
                    c.id,
                    "",
                    "Ollama isn't running. Install it from ollama.com, open it, then try again.",
                ));
            }
            if detection.ollama_models.is_empty() {
                return Ok(ConnectAiResult::failed(
                    c.id,
                    "",
                    "Ollama has no chat model yet. Download a model first.",
                ));
            }
            let model = requested
                .map(str::to_string)
                .unwrap_or_else(|| ai_brain::preferred_local_model(&detection.ollama_models));
            (String::new(), model)
        }
        BrainKind::Subscription => {
            let cli = if c.id == "claude-code" {
                &detection.claude
            } else {
                &detection.codex
            };
            if !cli.installed {
                return Ok(ConnectAiResult::failed(
                    c.id,
                    "",
                    format!("{} isn't installed on this computer yet.", c.label),
                ));
            }
            (
                String::new(),
                requested.unwrap_or(c.default_model).to_string(),
            )
        }
    };

    // The exact client: testing a provider must call that provider, never
    // the brain it would be replaced with.
    let base_url = match c.kind {
        BrainKind::Local => ollama_base(&state),
        _ => c.base_url.to_string(),
    };
    let client =
        crate::capture::build_llm_client_exact(cfg.as_ref(), &base_url, &api_key, c.api_key_env);
    let request = CompletionRequest {
        model: model.clone(),
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: "You are a connection test. Reply with exactly one word.".to_string(),
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: Role::User,
                content: "Reply with the single word: CONNECTED".to_string(),
                tool_calls: None,
                tool_call_id: None,
            },
        ],
        tools: None,
        max_tokens: Some(32),
        temperature: Some(0.0),
        stream: false,
    };
    let started = Instant::now();
    let outcome = tokio::time::timeout(VERIFY_TIMEOUT, client.complete(request)).await;
    let latency_ms = started.elapsed().as_millis() as u64;
    let reply = match outcome {
        Ok(Ok(response)) => response.content.unwrap_or_default().trim().to_string(),
        Ok(Err(err)) => {
            let raw = format!("{err:#}");
            tracing::warn!(provider = %c.id, error = %raw, "AI connect test failed");
            return Ok(ConnectAiResult::failed(
                c.id,
                &model,
                humanize_provider_error(c.api_key_env, &raw),
            ));
        }
        Err(_) => {
            return Ok(ConnectAiResult::failed(
                c.id,
                &model,
                format!(
                    "No answer within {}s. Try again in a moment.",
                    VERIFY_TIMEOUT.as_secs()
                ),
            ));
        }
    };
    if reply.is_empty() {
        return Ok(ConnectAiResult::failed(
            c.id,
            &model,
            "The provider answered with no text. Try a different model.",
        ));
    }

    // It works. Keep the key (only now, so a bad key never lands in the
    // keyring) and make this the AI.
    if let Some(key) = new_key.as_deref()
        && let Err(e) = credentials_resolver::store_llm_api_key(cfg.as_ref(), c.api_key_env, key)
    {
        return Ok(ConnectAiResult::failed(
            c.id,
            &model,
            format!("The key works, but saving it failed: {e}"),
        ));
    }
    let brain = Brain::new(c.id, &model);
    if let Err(e) = ai_brain::set_chosen(Some(brain)) {
        tracing::warn!(error = %e, "could not persist the AI choice; it holds until restart");
    }
    if let Ok(s) = control(&state) {
        record_choice_in_profile(s, c.id, &model).await;
    }
    ai_brain::refresh(&ollama_base(&state)).await;

    Ok(Json(ConnectAiResult {
        ok: true,
        provider: c.id.to_string(),
        model,
        reply: Some(reply.chars().take(160).collect()),
        latency_ms: Some(latency_ms),
        error: None,
    }))
}

/// Mirror the choice into the Operations agent's preference and the model
/// label the dashboard header shows, so every surface names the same AI.
async fn record_choice_in_profile(s: &crate::state::ControlState, provider: &str, model: &str) {
    {
        let mut agents = s.agents.write().await;
        if let Some(agent) = agents.iter_mut().find(|a| a.id == DEFAULT_CAPTURE_AGENT_ID) {
            agent.preferred_provider = provider.to_string();
            agent.preferred_model = model.to_string();
        }
    }
    *s.current_model_display.write().await = format!("{provider} · {model}");
}

/// `DELETE /api/ai/choice`: stop pinning one AI; detection picks again.
pub(super) async fn clear_ai_choice(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_admin_json(&auth, &state)?;
    ai_brain::set_chosen(None).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
        )
    })?;
    Ok(Json(serde_json::json!({ "ok": true })))
}
