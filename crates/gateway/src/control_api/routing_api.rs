//! Task-kind and model-tier routing rules.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Effective routing (config defaults merged with any live override) for one task kind.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct TaskKindRecommendation {
    pub(super) task_kind: String,
    pub(super) director_id: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) model: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) generation_tier: Option<String>,
    pub(super) review_tier: Option<String>,
    pub(super) fallback_tiers: Option<Vec<String>>,
}

/// Partial-update body for a task kind's routing override.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateTaskKindRoutingOverrideRequest {
    pub(super) director_id: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) model: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) generation_tier: Option<String>,
    pub(super) review_tier: Option<String>,
    pub(super) fallback_tiers: Option<Vec<String>>,
}

/// Effective provider/model/mode routing for one model tier.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ModelTierRouteRecommendation {
    pub(super) tier: String,
    pub(super) provider: Option<String>,
    pub(super) model: Option<String>,
    pub(super) mode: Option<String>,
}

/// Partial-update body for a model tier's routing override.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UpdateModelTierRouteRequest {
    pub(super) provider: Option<String>,
    pub(super) model: Option<String>,
    pub(super) mode: Option<String>,
}

/// `GET` handler: lists every known task kind's effective routing (config
/// defaults with live overrides layered on top).
pub(super) async fn list_task_kind_recommendations(
    State(state): State<ServerState>,
) -> Result<Json<Vec<TaskKindRecommendation>>, StatusCode> {
    let s = control(&state)?;
    let cfg = state
        .app_config
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let overrides = s.routing_task_kind_overrides.read().await.clone();
    let mut keys: Vec<String> = cfg.task_kind_routing.keys().cloned().collect();
    for k in overrides.keys() {
        if !keys.iter().any(|x| x == k) {
            keys.push(k.clone());
        }
    }
    let mut items: Vec<TaskKindRecommendation> = keys
        .into_iter()
        .map(|task_kind| {
            let base = cfg.task_kind_routing.get(task_kind.as_str()).cloned();
            let ov = overrides.get(task_kind.as_str()).cloned();
            let merged = match (base, ov) {
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
                    b
                }
                (Some(b), None) => b,
                (None, Some(o)) => o,
                (None, None) => crate::config::TaskKindRoutingEntry::default(),
            };
            TaskKindRecommendation {
                task_kind,
                director_id: merged.director_id,
                provider: merged.provider,
                model: merged.model,
                mode: merged.mode,
                generation_tier: merged.generation_tier,
                review_tier: merged.review_tier,
                fallback_tiers: merged.fallback_tiers,
            }
        })
        .collect();
    items.sort_by(|a, b| a.task_kind.cmp(&b.task_kind));
    Ok(Json(items))
}

/// `PATCH` handler: partial-updates a task kind's routing override.
pub(super) async fn update_task_kind_routing_override(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(task_kind): Path<String>,
    Json(body): Json<UpdateTaskKindRoutingOverrideRequest>,
) -> Result<Json<TaskKindRecommendation>, StatusCode> {
    // Routing overrides apply to every tenant's work on this instance.
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    let mut overrides = s.routing_task_kind_overrides.write().await;
    let entry = overrides.entry(task_kind.clone()).or_default();
    if body.director_id.is_some() {
        entry.director_id = body.director_id;
    }
    if body.provider.is_some() {
        entry.provider = body.provider;
    }
    if body.model.is_some() {
        entry.model = body.model;
    }
    if body.mode.is_some() {
        entry.mode = body.mode;
    }
    if body.generation_tier.is_some() {
        entry.generation_tier = body.generation_tier;
    }
    if body.review_tier.is_some() {
        entry.review_tier = body.review_tier;
    }
    if body.fallback_tiers.is_some() {
        entry.fallback_tiers = body.fallback_tiers;
    }
    let out = TaskKindRecommendation {
        task_kind,
        director_id: entry.director_id.clone(),
        provider: entry.provider.clone(),
        model: entry.model.clone(),
        mode: entry.mode.clone(),
        generation_tier: entry.generation_tier.clone(),
        review_tier: entry.review_tier.clone(),
        fallback_tiers: entry.fallback_tiers.clone(),
    };
    Ok(Json(out))
}

/// `GET` handler: lists every known model tier's effective routing.
pub(super) async fn list_model_tier_routes(
    State(state): State<ServerState>,
) -> Result<Json<Vec<ModelTierRouteRecommendation>>, StatusCode> {
    let s = control(&state)?;
    let cfg = state
        .app_config
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let overrides = s.routing_model_tier_overrides.read().await.clone();
    let mut keys: Vec<String> = cfg.model_tier_routes.keys().cloned().collect();
    for k in overrides.keys() {
        if !keys.iter().any(|x| x == k) {
            keys.push(k.clone());
        }
    }
    let mut out: Vec<ModelTierRouteRecommendation> = keys
        .into_iter()
        .map(|tier| {
            let mut route = cfg
                .model_tier_routes
                .get(tier.as_str())
                .cloned()
                .unwrap_or_default();
            if let Some(ov) = overrides.get(tier.as_str()) {
                if ov.provider.is_some() {
                    route.provider = ov.provider.clone();
                }
                if ov.model.is_some() {
                    route.model = ov.model.clone();
                }
                if ov.mode.is_some() {
                    route.mode = ov.mode.clone();
                }
            }
            ModelTierRouteRecommendation {
                tier,
                provider: route.provider,
                model: route.model,
                mode: route.mode,
            }
        })
        .collect();
    out.sort_by(|a, b| a.tier.cmp(&b.tier));
    Ok(Json(out))
}

/// `PATCH` handler: partial-updates a model tier's routing override.
pub(super) async fn update_model_tier_route(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(tier): Path<String>,
    Json(body): Json<UpdateModelTierRouteRequest>,
) -> Result<Json<ModelTierRouteRecommendation>, StatusCode> {
    // Routing overrides apply to every tenant's work on this instance.
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    let mut overrides = s.routing_model_tier_overrides.write().await;
    let entry = overrides.entry(tier.clone()).or_default();
    if body.provider.is_some() {
        entry.provider = body.provider;
    }
    if body.model.is_some() {
        entry.model = body.model;
    }
    if body.mode.is_some() {
        entry.mode = body.mode;
    }
    Ok(Json(ModelTierRouteRecommendation {
        tier,
        provider: entry.provider.clone(),
        model: entry.model.clone(),
        mode: entry.mode.clone(),
    }))
}
