//! LLM provider status, credentials and live probes (TOML + keyring).
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Whether a provider's credential is configured, and where it came from.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LlmProviderStatusDto {
    pub(super) name: String,
    pub(super) base_url: String,
    pub(super) api_key_env: String,
    pub(super) keyless: bool,
    pub(super) secret_configured: bool,
    /// `keyless` | `environment` | `keyring` | `none`
    pub(super) secret_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) masked_secret_tail: Option<String>,
    pub(super) credentials_backend: String,
}

/// Checks a provider's credential across the keyless/env/keyring sources, in
/// that precedence order, and masks whatever secret it finds for display.
pub(super) fn compute_llm_provider_status(
    cfg: &crate::config::AppConfig,
    name: &str,
    base_url: &str,
    api_key_env: &str,
) -> LlmProviderStatusDto {
    let keyless = credentials_resolver::is_keyless_llm_env(api_key_env);
    let env_set = !keyless
        && std::env::var(api_key_env)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    let kr_secret = credentials_resolver::read_llm_keyring_secret(api_key_env);
    let kr_set = !keyless
        && kr_secret
            .as_ref()
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);

    let (secret_source, masked_secret_tail) = if keyless {
        ("keyless", None)
    } else if env_set {
        (
            "environment",
            std::env::var(api_key_env)
                .ok()
                .as_deref()
                .and_then(credentials_resolver::mask_secret_tail),
        )
    } else if kr_set {
        (
            "keyring",
            kr_secret
                .as_deref()
                .and_then(credentials_resolver::mask_secret_tail),
        )
    } else {
        ("none", None)
    };

    let secret_configured = keyless || env_set || kr_set;

    LlmProviderStatusDto {
        name: name.to_string(),
        base_url: base_url.to_string(),
        api_key_env: api_key_env.to_string(),
        keyless,
        secret_configured,
        secret_source: secret_source.to_string(),
        masked_secret_tail,
        credentials_backend: cfg.credentials_backend.clone(),
    }
}

/// Resolves a provider name to `(name, base_url, api_key_env)`, checking the
/// static config first and falling back to a DB-registered provider.
pub(super) async fn resolve_provider_runtime(
    state: &ServerState,
    provider_name: &str,
) -> Option<(String, String, String)> {
    if let Some(cfg) = state.app_config.as_ref()
        && let Some((name, base, env)) =
            crate::config::resolve_llm_provider_from_config(cfg.as_ref(), provider_name)
    {
        return Some((name, base, env));
    }
    let s = control(state).ok()?;
    let providers = s.llm_providers.read().await;
    let p = providers
        .iter()
        .find(|x| x.name.eq_ignore_ascii_case(provider_name))?;
    let base = p.base_url.clone().unwrap_or_default();
    let mut api_key_env = p
        .api_key_env
        .clone()
        .unwrap_or_else(|| "OLLAMA_API_KEY".to_string());
    // A row written before the allowlist existed may name any variable. Treat
    // it as keyless rather than resolve and ship that variable anywhere.
    if !credentials_resolver::is_allowed_llm_key_env(&api_key_env) {
        tracing::warn!(provider = %p.name, api_key_env = %api_key_env, "ignoring a non-LLM api_key_env on a registered provider");
        api_key_env = String::new();
    }
    Some((p.name.clone(), base, api_key_env))
}

/// The base URL a probe may use: the caller's override only when no secret
/// rides along. A resolved key is sent as a bearer token, so letting the
/// caller choose where it goes would hand them the key (`baseUrl` pointed at a
/// server they control). Keyless lanes (local Ollama) carry nothing worth
/// stealing, so overriding their URL to find a server is still allowed.
pub(super) fn probe_base_url<'a>(
    override_url: Option<&'a str>,
    configured: &'a str,
    api_key_env: &str,
) -> Result<&'a str, String> {
    match override_url.map(str::trim).filter(|u| !u.is_empty()) {
        None => Ok(configured),
        Some(url) if url == configured.trim() => Ok(configured),
        Some(url) if credentials_resolver::is_keyless_llm_env(api_key_env) => Ok(url),
        Some(_) => Err(
            "a custom base URL can only be tested for keyless providers; save the provider \
             with that URL first"
                .to_string(),
        ),
    }
}

/// `GET` handler: credential status for every known provider (config-defined
/// plus DB-registered), for the settings page.
pub(super) async fn list_llm_provider_status(
    State(state): State<ServerState>,
) -> Result<Json<Vec<LlmProviderStatusDto>>, StatusCode> {
    let cfg = state
        .app_config
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let mut names: Vec<String> = cfg.llm_providers_map.keys().cloned().collect();
    if !names
        .iter()
        .any(|n| n.eq_ignore_ascii_case(cfg.llm_provider.as_str()))
    {
        names.push(cfg.llm_provider.clone());
    }
    if let Ok(s) = control(&state) {
        let configured = s.llm_providers.read().await;
        for p in configured.iter() {
            if !names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(p.name.as_str()))
            {
                names.push(p.name.clone());
            }
        }
    }
    names.sort_by_key(|a| a.to_lowercase());
    let mut out = Vec::new();
    for n in names {
        if let Some((prov, base, env)) = resolve_provider_runtime(&state, &n).await {
            out.push(compute_llm_provider_status(cfg, &prov, &base, &env));
        }
    }
    Ok(Json(out))
}

/// Body for setting a provider's API key.
#[derive(Debug, Deserialize)]
pub(super) struct SetLlmProviderCredentialBody {
    #[serde(alias = "apiKey")]
    pub(super) api_key: String,
}

/// Phase 8: probe the candidate API key against the provider's `/v1/models` (or `/api/tags`)
/// **before** writing it to the credentials store. A bad key never lands in the keyring, so a
/// mid-session provider swap from the settings page is safe: the resolver re-reads env+keyring
/// on every chat turn, so the next outbound LLM request picks up the new key with no restart.
pub(super) async fn set_llm_provider_credential(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(name): Path<String>,
    Json(body): Json<SetLlmProviderCredentialBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // The keyring is the instance's, not the caller's: a key stored here is
    // the one every tenant's turns are billed to.
    require_admin_json(&auth, &state)?;
    let cfg = state.app_config.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "ok": false, "error": "app config unavailable" })),
    ))?;
    let Some((_prov, base, api_key_env)) = resolve_provider_runtime(&state, &name).await else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "ok": false, "error": "unknown provider" })),
        ));
    };
    let candidate = body.api_key.trim();
    if candidate.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "API key must not be empty" })),
        ));
    }
    // Sanity-check before persisting — bad keys never reach the keyring.
    match crate::llm_provider_probe::fetch_model_ids(&base, candidate, &api_key_env).await {
        Ok(ids) => {
            credentials_resolver::store_llm_api_key(cfg.as_ref(), &api_key_env, candidate)
                .map_err(|e| {
                    tracing::warn!(error = %e, provider = %name, "store_llm_api_key failed");
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
                    )
                })?;
            Ok(Json(
                serde_json::json!({ "ok": true, "modelCount": ids.len() }),
            ))
        }
        Err(e) => {
            tracing::warn!(provider = %name, error = %e, "credential probe failed; key not stored");
            Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "ok": false, "error": e })),
            ))
        }
    }
}

/// Body for probing a provider's reachability, optionally against an
/// override base URL instead of its configured one.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TestLlmProviderBody {
    #[serde(default)]
    pub(super) base_url: Option<String>,
}

/// Result of a provider reachability probe.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LlmProviderTestResult {
    pub(super) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model_count: Option<usize>,
}

/// Handler: probes a provider's reachability and returns model count, never
/// erroring the HTTP response itself (failure is reported inside the body).
pub(super) async fn test_llm_provider(
    State(state): State<ServerState>,
    Path(name): Path<String>,
    Json(body): Json<TestLlmProviderBody>,
) -> Json<LlmProviderTestResult> {
    let Some(_cfg) = state.app_config.as_ref() else {
        return Json(LlmProviderTestResult {
            ok: false,
            http_status: None,
            error: Some("app config unavailable".into()),
            model_count: None,
        });
    };
    let Some((_prov, base, api_key_env)) = resolve_provider_runtime(&state, &name).await else {
        return Json(LlmProviderTestResult {
            ok: false,
            http_status: None,
            error: Some("unknown provider".into()),
            model_count: None,
        });
    };
    // CLI-backed subscription providers have no HTTP models endpoint — probing
    // one over HTTP always fails. The installed CLI itself is the truth.
    if api_key_env == "CODEX_CLI" || api_key_env == "CLAUDE_CODE_CLI" {
        let kind = if api_key_env == "CODEX_CLI" {
            "codex"
        } else {
            "claude"
        };
        let status = crate::agent_cli_probe::probe(kind).await;
        return Json(LlmProviderTestResult {
            ok: status.installed,
            http_status: None,
            error: (!status.installed).then(|| {
                format!("the `{kind}` CLI is not installed on this machine — install it and sign in, then retry")
            }),
            model_count: status.installed.then_some(1),
        });
    }
    let base_url = match probe_base_url(body.base_url.as_deref(), &base, &api_key_env) {
        Ok(url) => url,
        Err(e) => {
            return Json(LlmProviderTestResult {
                ok: false,
                http_status: None,
                error: Some(e),
                model_count: None,
            });
        }
    };
    let api_key = credentials_resolver::resolve_llm_api_key(&api_key_env);
    match crate::llm_provider_probe::fetch_model_ids(base_url, &api_key, &api_key_env).await {
        Ok(ids) => Json(LlmProviderTestResult {
            ok: true,
            http_status: Some(200),
            error: None,
            model_count: Some(ids.len()),
        }),
        Err(e) => Json(LlmProviderTestResult {
            ok: false,
            http_status: None,
            error: Some(e),
            model_count: None,
        }),
    }
}

/// Body for a live end-to-end verification of a provider.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VerifyLlmProviderBody {
    /// Model to verify against. Optional — CLI subscription lanes take the
    /// `default` sentinel, which is what an omitted value falls back to.
    #[serde(default)]
    pub(super) model: Option<String>,
}

/// Proof that a provider answers: the lane that was actually resolved, the
/// round-trip time, and the words that came back.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LlmProviderVerifyResult {
    pub(super) ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reply: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) error: Option<String>,
}

impl LlmProviderVerifyResult {
    fn failed(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            provider: None,
            model: None,
            latency_ms: None,
            reply: None,
            error: Some(error.into()),
        }
    }
}

/// How long a single verification turn may take before we call it dead. The
/// CLI lanes spawn a process and wait on a subscription round trip, so this is
/// generous — but it must terminate, or the button spins forever.
pub(super) const VERIFY_TIMEOUT: Duration = Duration::from_secs(90);

/// The first line of a raw error that carries meaning, skipping the structured
/// log spew a CLI writes to stderr before it gets to the point.
pub(super) fn first_meaningful_line(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| {
            !l.is_empty()
                // `2026-09-05T22:02:40.749669Z ERROR codex_models_manager: …`
                && !l.contains(" ERROR ")
                && !l.contains(" WARN ")
                && !l.starts_with("20")
        })
        .unwrap_or_else(|| raw.trim());
    line.chars().take(200).collect()
}

/// Turn a provider's raw failure into one sentence an operator can act on.
///
/// The full text still goes to the logs; what reaches a settings card has to
/// fit on the card and name the next step. Without this, a revoked ChatGPT
/// token surfaced as forty lines of `rmcp::transport::worker` stack noise, and
/// the actual instruction — sign in again — was buried in the middle of it.
pub(super) fn humanize_provider_error(api_key_env: &str, raw: &str) -> String {
    let lower = raw.to_lowercase();
    let cli = match api_key_env {
        "CODEX_CLI" => Some(("codex", "codex login")),
        "CLAUDE_CODE_CLI" => Some(("claude", "claude")),
        _ => None,
    };

    if let Some((cli, sign_in)) = cli {
        // Installed and signed in, but the service refused the client for
        // being out of date: "Claude Code 2.1.231 does not support this
        // model; version 2.1.251 or newer is required". No amount of signing
        // in fixes that, so it goes ahead of the sign-in heuristics. The
        // wording deliberately avoids "in Terminal": the desktop card appends
        // its Sign in hint to that phrase.
        if lower.contains("does not support this model")
            || lower.contains("or newer is required")
        {
            let needed = lower
                .find("or newer is required")
                .and_then(|pos| lower[..pos].rsplit("version ").next())
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit() || c == '.'))
                .map(|v| format!(" — it needs {v} or newer"))
                .unwrap_or_default();
            let upgrade = match cli {
                "claude" => "`claude update`, or `brew upgrade claude-code` if Homebrew installed it",
                _ => "`npm install -g @openai/codex@latest`",
            };
            return format!(
                "Your `{cli}` CLI is too old for this model{needed}. Update it ({upgrade}), \
                 then press Test."
            );
        }
        if lower.contains("token_revoked")
            || lower.contains("refresh_token_invalidated")
            || lower.contains("sign in again")
            || lower.contains("session has ended")
            || lower.contains("not logged in")
            || lower.contains("/login")
            || lower.contains("401 unauthorized")
            || lower.contains("invalid_token")
        {
            return format!(
                "Your `{cli}` sign-in has expired. Run `{sign_in}` in Terminal, sign in again, \
                 then press Test."
            );
        }
        if lower.contains("rate limit")
            || lower.contains("usage limit")
            || lower.contains("quota")
            || lower.contains("429")
        {
            return format!(
                "The `{cli}` subscription has hit its usage limit. Wait for it to reset, or \
                 switch chat to another brain for now."
            );
        }
        if lower.contains("no such file") || lower.contains("not found") {
            return format!("The `{cli}` CLI is no longer installed on this computer.");
        }
        // `api_error` with nothing more specific is, in practice, always the
        // sign-in having lapsed.
        if lower.contains("api_error") {
            return format!(
                "The `{cli}` CLI could not reach its service — usually a lapsed sign-in. Run \
                 `{sign_in}` in Terminal, then press Test."
            );
        }
    }

    if lower.contains("connection refused")
        || lower.contains("tcp connect error")
        || lower.contains("error sending request")
    {
        return "Could not reach the provider. If this is Ollama, start it (`ollama serve`); \
                otherwise check the network."
            .to_string();
    }
    if lower.contains("invalid api key")
        || lower.contains("invalid_api_key")
        || lower.contains("unauthorized")
        || lower.contains("401")
    {
        return "The API key was rejected. Re-enter it and press Connect again.".to_string();
    }
    if lower.contains("rate limit") || lower.contains("429") {
        return "The provider is rate limiting this account. Wait a moment and press Test again."
            .to_string();
    }

    first_meaningful_line(raw)
}

/// `POST` handler: runs one real, minimal turn through a provider.
///
/// A reachability probe only proves a binary exists on disk; it says nothing
/// about whether the subscription behind it is signed in and serving. This
/// endpoint is the difference between the UI claiming "connected" and the UI
/// being able to show the sentence the model just produced.
pub(super) async fn verify_llm_provider(
    State(state): State<ServerState>,
    Path(name): Path<String>,
    Json(body): Json<VerifyLlmProviderBody>,
) -> Json<LlmProviderVerifyResult> {
    let Ok(s) = control(&state) else {
        return Json(LlmProviderVerifyResult::failed("control state unavailable"));
    };
    let Some(cfg) = state.app_config.as_ref() else {
        return Json(LlmProviderVerifyResult::failed("app config unavailable"));
    };

    let requested_model = body
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .unwrap_or("default");

    let Some(resolved) = resolve_llm_for_preference(cfg.as_ref(), s, &name, requested_model).await
    else {
        return Json(LlmProviderVerifyResult::failed(format!(
            "`{name}` is not connected yet — press Connect first, then test"
        )));
    };

    let api_key = credentials_resolver::resolve_llm_api_key(&resolved.api_key_env);
    let client = build_llm_client(
        cfg.as_ref(),
        &resolved.base_url,
        &api_key,
        &resolved.api_key_env,
    );

    let request = CompletionRequest {
        model: resolved.model.clone(),
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

    match outcome {
        Ok(Ok(response)) => {
            let reply = response.content.unwrap_or_default();
            let reply = reply.trim();
            if reply.is_empty() {
                return Json(LlmProviderVerifyResult {
                    ok: false,
                    provider: Some(resolved.provider),
                    model: Some(response.model),
                    latency_ms: Some(latency_ms),
                    reply: None,
                    error: Some(
                        "the provider answered but returned no text — it is reachable but not \
                         producing output"
                            .into(),
                    ),
                });
            }
            // A one-word answer is the happy path; anything longer is still a
            // pass, just trimmed so the UI stays a single line.
            let snippet: String = reply.chars().take(160).collect();
            Json(LlmProviderVerifyResult {
                ok: true,
                provider: Some(resolved.provider),
                model: Some(response.model),
                latency_ms: Some(latency_ms),
                reply: Some(snippet),
                error: None,
            })
        }
        Ok(Err(err)) => {
            let raw = format!("{err:#}");
            warn!(provider = %resolved.provider, error = %raw, "provider verification failed");
            Json(LlmProviderVerifyResult {
                ok: false,
                provider: Some(resolved.provider),
                model: Some(resolved.model),
                latency_ms: Some(latency_ms),
                reply: None,
                error: Some(humanize_provider_error(&resolved.api_key_env, &raw)),
            })
        }
        Err(_) => Json(LlmProviderVerifyResult {
            ok: false,
            provider: Some(resolved.provider),
            model: Some(resolved.model),
            latency_ms: Some(latency_ms),
            reply: None,
            error: Some(format!(
                "no answer within {}s — the provider is installed but not responding",
                VERIFY_TIMEOUT.as_secs()
            )),
        }),
    }
}

/// Query-string override for the provider-models discovery endpoint.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LlmModelsQuery {
    #[serde(default)]
    pub(super) base_url: Option<String>,
}

/// One model id discovered from a provider's model-listing endpoint.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LlmDiscoveredModel {
    pub(super) id: String,
}

/// Response listing a provider's discovered models.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct LlmProviderModelsResponse {
    pub(super) models: Vec<LlmDiscoveredModel>,
}

/// `GET` handler: lists a provider's available models, probing its API —
/// except CLI-backed subscription providers (Codex/Claude Code CLI), which
/// have no HTTP models endpoint and just offer a `default` sentinel.
pub(super) async fn list_llm_provider_models(
    State(state): State<ServerState>,
    Path(name): Path<String>,
    Query(q): Query<LlmModelsQuery>,
) -> Result<Json<LlmProviderModelsResponse>, StatusCode> {
    state
        .app_config
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let Some((_prov, base, api_key_env)) = resolve_provider_runtime(&state, &name).await else {
        return Err(StatusCode::NOT_FOUND);
    };
    let base_url = probe_base_url(q.base_url.as_deref(), &base, &api_key_env)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    // CLI-backed subscription providers do not expose an HTTP `/models`
    // endpoint. `default` means “use the CLI's own current model” and keeps
    // the first-run setup usable without asking the user to know an internal
    // model id.
    if api_key_env == "CODEX_CLI" || api_key_env == "CLAUDE_CODE_CLI" {
        return Ok(Json(LlmProviderModelsResponse {
            models: vec![LlmDiscoveredModel {
                id: "default".to_string(),
            }],
        }));
    }
    let api_key = credentials_resolver::resolve_llm_api_key(&api_key_env);
    let ids = crate::llm_provider_probe::fetch_model_ids(base_url, &api_key, &api_key_env)
        .await
        .map_err(|e| {
            tracing::warn!(provider = %name, error = %e, "list_llm_provider_models probe failed");
            StatusCode::BAD_GATEWAY
        })?;
    Ok(Json(LlmProviderModelsResponse {
        // Chat-model choices only: an embedding model listed here would be
        // picked as the brain and fail every reply.
        models: ids
            .into_iter()
            .filter(|id| !crate::llm_provider_probe::is_embedding_model(id))
            .map(|id| LlmDiscoveredModel { id })
            .collect(),
    }))
}

#[cfg(test)]
mod probe_url_tests {
    use super::probe_base_url;

    #[test]
    fn a_keyed_provider_is_only_probed_at_its_own_url() {
        let configured = "https://api.openai.com/v1";
        assert_eq!(
            probe_base_url(None, configured, "OPENAI_API_KEY"),
            Ok(configured)
        );
        assert_eq!(
            probe_base_url(Some(configured), configured, "OPENAI_API_KEY"),
            Ok(configured)
        );
        // The exfiltration shape: resolved key, attacker's URL.
        assert!(
            probe_base_url(Some("https://evil.example"), configured, "OPENAI_API_KEY").is_err()
        );
        // Keyless lanes may still be pointed somewhere else.
        assert_eq!(
            probe_base_url(
                Some("http://10.0.0.5:11434"),
                "http://127.0.0.1:11434",
                "OLLAMA_API_KEY"
            ),
            Ok("http://10.0.0.5:11434")
        );
    }
}
