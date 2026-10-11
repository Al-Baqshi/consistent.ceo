//! LLM provider and model registry.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// `GET` handler: lists registered LLM providers.
pub(super) async fn list_llm_providers(
    State(state): State<ServerState>,
) -> Result<Json<Vec<LlmProvider>>, StatusCode> {
    let s = control(&state)?;
    Ok(Json(s.llm_providers.read().await.clone()))
}

/// A JSON `{"error": …}` body with a status, which the settings page shows
/// verbatim instead of a bare status code.
pub(super) fn provider_error(
    status: StatusCode,
    message: impl Into<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(serde_json::json!({ "ok": false, "error": message.into() })),
    )
}

/// The instance-admin gate in this module's error shape.
pub(super) fn require_admin_json(
    auth: &AuthContext,
    state: &ServerState,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    require_instance_admin(auth, state).map_err(|status| {
        provider_error(
            status,
            "only the operator of this instance can change LLM providers",
        )
    })
}

/// Refuse an `api_key_env` the API may not set. See
/// [`credentials_resolver::is_allowed_llm_key_env`] for the rule and why.
fn check_api_key_env(
    api_key_env: Option<&str>,
) -> Result<Option<String>, (StatusCode, Json<serde_json::Value>)> {
    let Some(raw) = api_key_env else {
        return Ok(None);
    };
    let name = raw.trim();
    if credentials_resolver::is_allowed_llm_key_env(name) {
        Ok(Some(name.to_string()))
    } else {
        Err(provider_error(
            StatusCode::BAD_REQUEST,
            format!(
                "`{name}` is not an LLM key variable; use a name ending in _API_KEY \
                 (e.g. OPENAI_API_KEY)"
            ),
        ))
    }
}

/// `POST` handler: registers an LLM provider, or updates the one already
/// registered under that name (case-insensitive).
///
/// It used to append unconditionally, so every "Connect" press on the
/// settings page added another `anthropic` row; the page then matched the
/// first and edited a stale one. Upserting by name makes the call idempotent,
/// and collapses duplicates already on disk onto the first row (re-pointing
/// their models) so lookups by name are unambiguous again.
pub(super) async fn create_llm_provider(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateLlmProviderRequest>,
) -> Result<Json<LlmProvider>, (StatusCode, Json<serde_json::Value>)> {
    require_admin_json(&auth, &state)?;
    let s = control(&state).map_err(|status| provider_error(status, "control state unavailable"))?;
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(provider_error(
            StatusCode::BAD_REQUEST,
            "provider name is required",
        ));
    }
    let api_key_env = check_api_key_env(body.api_key_env.as_deref())?;

    let mut providers = s.llm_providers.write().await;
    let matching: Vec<usize> = providers
        .iter()
        .enumerate()
        .filter(|(_, p)| p.name.trim().eq_ignore_ascii_case(&name))
        .map(|(i, _)| i)
        .collect();
    let Some(&keep) = matching.first() else {
        let p = LlmProvider {
            id: Uuid::new_v4().to_string(),
            name,
            provider_type: body.provider_type,
            base_url: body.base_url,
            api_key_env,
        };
        providers.push(p.clone());
        return Ok(Json(p));
    };

    {
        let existing = &mut providers[keep];
        existing.provider_type = body.provider_type;
        if body.base_url.is_some() {
            existing.base_url = body.base_url;
        }
        if api_key_env.is_some() {
            existing.api_key_env = api_key_env;
        }
    }
    let kept = providers[keep].clone();
    if matching.len() > 1 {
        let dupes: HashSet<String> = matching[1..]
            .iter()
            .map(|&i| providers[i].id.clone())
            .collect();
        providers.retain(|p| !dupes.contains(&p.id));
        drop(providers);
        for m in s.llm_models.write().await.iter_mut() {
            if dupes.contains(&m.provider_id) {
                m.provider_id = kept.id.clone();
            }
        }
        tracing::info!(provider = %kept.name, removed = dupes.len(), "collapsed duplicate LLM provider rows");
    }
    Ok(Json(kept))
}

/// `PATCH` handler: partial-updates an LLM provider.
pub(super) async fn update_llm_provider(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<UpdateLlmProviderRequest>,
) -> Result<Json<LlmProvider>, (StatusCode, Json<serde_json::Value>)> {
    require_admin_json(&auth, &state)?;
    let s = control(&state).map_err(|status| provider_error(status, "control state unavailable"))?;
    let api_key_env = check_api_key_env(body.api_key_env.as_deref())?;
    let mut providers = s.llm_providers.write().await;
    let pos = providers
        .iter()
        .position(|x| x.id == id)
        .ok_or_else(|| provider_error(StatusCode::NOT_FOUND, "unknown provider"))?;
    if let Some(name) = body.name {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(provider_error(
                StatusCode::BAD_REQUEST,
                "provider name is required",
            ));
        }
        if providers
            .iter()
            .any(|p| p.id != id && p.name.trim().eq_ignore_ascii_case(&name))
        {
            return Err(provider_error(
                StatusCode::CONFLICT,
                format!("a provider named `{name}` already exists"),
            ));
        }
        providers[pos].name = name;
    }
    if let Some(provider_type) = body.provider_type {
        providers[pos].provider_type = provider_type;
    }
    if let Some(base_url) = body.base_url {
        providers[pos].base_url = Some(base_url);
    }
    if api_key_env.is_some() {
        providers[pos].api_key_env = api_key_env;
    }
    Ok(Json(providers[pos].clone()))
}

pub(super) async fn list_llm_models(
    State(state): State<ServerState>,
) -> Result<Json<Vec<LlmModel>>, StatusCode> {
    let s = control(&state)?;
    Ok(Json(s.llm_models.read().await.clone()))
}

/// `POST` handler: registers a model under a provider, clearing the previous
/// default and updating the display name when this one is marked default.
pub(super) async fn create_llm_model(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<CreateLlmModelRequest>,
) -> Result<Json<LlmModel>, StatusCode> {
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    let id = Uuid::new_v4().to_string();
    let is_default = body.is_default.unwrap_or(false);
    if is_default {
        let mut models = s.llm_models.write().await;
        for m in models.iter_mut() {
            m.is_default = false;
        }
        drop(models);
        let mut display = s.current_model_display.write().await;
        *display = body.name.clone();
        drop(display);
    }
    let m = LlmModel {
        id: id.clone(),
        provider_id: body.provider_id,
        name: body.name,
        is_default,
    };
    s.llm_models.write().await.push(m.clone());
    Ok(Json(m))
}

/// `PATCH` handler: partial-updates a model, keeping the current-model
/// display name in sync when the default model's name changes.
pub(super) async fn update_llm_model(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<UpdateLlmModelRequest>,
) -> Result<Json<LlmModel>, StatusCode> {
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    let mut models = s.llm_models.write().await;
    let pos = models
        .iter()
        .position(|x| x.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(n) = body.name.clone() {
        models[pos].name = n.clone();
        if models[pos].is_default {
            *s.current_model_display.write().await = n;
        }
    }
    if body.is_default == Some(true) {
        for x in models.iter_mut() {
            x.is_default = x.id == id;
        }
        let name = models[pos].name.clone();
        *s.current_model_display.write().await = name;
    }
    Ok(Json(models[pos].clone()))
}

/// `GET` handler: the current LLM routing mode.
pub(super) async fn get_llm_mode(
    State(state): State<ServerState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = control(&state)?;
    let mode = s.llm_mode.read().await.clone();
    Ok(Json(serde_json::json!({ "mode": mode })))
}

/// Normalizes an LLM-mode string to one of `sleeping | balance | fast`,
/// defaulting to `balance` for anything unrecognized.
pub(super) fn canonical_llm_mode(input: &str) -> String {
    match input.to_lowercase().as_str() {
        "sleep" | "sleeping" | "away" => "sleeping".to_string(),
        "balanced" | "balance" | "normal" => "balance".to_string(),
        "rush" | "fast" => "fast".to_string(),
        _ => "balance".to_string(),
    }
}

/// `POST` handler: sets the LLM routing mode.
pub(super) async fn set_llm_mode(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<SetLlmModeRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    let mode = canonical_llm_mode(&body.mode);
    *s.llm_mode.write().await = mode.clone();
    Ok(Json(serde_json::json!({ "mode": mode })))
}

/// `GET` handler: the resolved active provider/model plus whether its
/// credential is configured, for the settings page.
pub(super) async fn get_llm_profile(
    State(state): State<ServerState>,
) -> Result<Json<LlmProfileDto>, StatusCode> {
    let s = control(&state)?;
    let cfg = state
        .app_config
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let mode = s.llm_mode.read().await.clone();
    let resolved = resolve_operations_llm(cfg.as_ref(), s).await;
    let (
        current_model,
        provider,
        model,
        base_url,
        api_key_env,
        preferred_provider,
        preferred_model,
        secret_configured,
        secret_source,
    ) = if let Some(resolved) = resolved {
        let status = compute_llm_provider_status(
            cfg.as_ref(),
            &resolved.provider,
            &resolved.base_url,
            &resolved.api_key_env,
        );
        (
            Some(format!("{} · {}", resolved.provider, resolved.model)),
            Some(resolved.provider),
            Some(resolved.model),
            Some(resolved.base_url),
            Some(resolved.api_key_env),
            Some(resolved.preferred_provider),
            Some(resolved.preferred_model),
            status.secret_configured,
            Some(status.secret_source),
        )
    } else {
        (
            Some(s.current_model_display.read().await.clone()),
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            None,
        )
    };
    Ok(Json(LlmProfileDto {
        mode,
        current_model,
        provider,
        model,
        base_url,
        api_key_env,
        preferred_provider,
        preferred_model,
        secret_configured,
        secret_source,
    }))
}

/// `POST` handler: pins the active provider/model by setting them as the
/// default capture agent's preference, after confirming they resolve.
pub(super) async fn set_llm_profile(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Json(body): Json<SetLlmProfileRequest>,
) -> Result<Json<LlmProfileDto>, (StatusCode, Json<serde_json::Value>)> {
    require_admin_json(&auth, &state)?;
    let s = control(&state).map_err(|status| {
        (
            status,
            Json(serde_json::json!({ "ok": false, "error": "control state unavailable" })),
        )
    })?;
    let cfg = state.app_config.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "ok": false, "error": "app config unavailable" })),
    ))?;

    let provider = body.provider.trim().to_string();
    let model = body.model.trim().to_string();
    if provider.is_empty() || model.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "ok": false, "error": "provider and model are required" })),
        ));
    }
    if resolve_llm_for_preference(cfg.as_ref(), s, &provider, &model)
        .await
        .is_none()
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "provider/model could not be resolved; connect the provider first"
            })),
        ));
    }

    {
        let mut agents = s.agents.write().await;
        let pos = agents
            .iter()
            .position(|a| a.id == DEFAULT_CAPTURE_AGENT_ID)
            .or_else(|| {
                agents
                    .iter()
                    .position(|a| a.group_id == DEFAULT_CAPTURE_DIRECTOR_ID)
            });
        if let Some(pos) = pos {
            let agent = &mut agents[pos];
            agent.preferred_provider = provider.clone();
            agent.preferred_model = model.clone();
        } else {
            agents.push(Agent {
                id: DEFAULT_CAPTURE_AGENT_ID.to_string(),
                group_id: DEFAULT_CAPTURE_DIRECTOR_ID.to_string(),
                name: "Operations Coordinator".to_string(),
                role: "Default intake agent".to_string(),
                kind: "operator".to_string(),
                preferred_provider: provider.clone(),
                preferred_model: model.clone(),
                mission: String::new(),
                profile: String::new(),
            });
        }
    }
    *s.current_model_display.write().await = format!("{} · {}", provider, model);

    get_llm_profile(State(state)).await.map_err(|status| {
        (
            status,
            Json(serde_json::json!({ "ok": false, "error": "profile update failed" })),
        )
    })
}
