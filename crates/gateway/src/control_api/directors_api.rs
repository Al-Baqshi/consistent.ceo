//! Directors: heartbeat state merged with agent groups.
//!
//! A child module of `control_api`: `use super::*` reaches the parent's
//! private helpers, which is what keeps this a pure move.

use super::*;

/// Director view for UI: group info + heartbeat state.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorView {
    pub id: String,
    pub name: String,
    pub icon: String,
    pub description: String,
    pub heartbeat_enabled: bool,
    pub auto_task_kinds: Vec<String>,
    // ── The "muscle": which model this director thinks with. ──
    //
    // These come from the Surreal `directors` row, which is what
    // `director_runner::resolve_director_provider` actually reads at run time.
    // The fields above come from the in-memory agent-group view. Two different
    // notions of "director" existed and only the shallow one was ever exposed,
    // which is why assigning muscles was invisible from the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// True when a Surreal director row backs this group — i.e. the muscle is
    /// editable. False means the group exists but has no director row yet.
    pub configurable: bool,
}

/// Body for updating a director's "muscle" (model provider/name/temperature)
/// and/or its system prompt.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateDirectorRequest {
    /// Provider name from `[llm.providers.*]` — e.g. `claude-code`, `ollama`,
    /// `google`. Empty string clears it back to the global default.
    pub model_provider: Option<String>,
    /// Model id or `[llm.models.*]` alias.
    pub model_name: Option<String>,
    pub temperature: Option<f32>,
    /// Replace the director's system prompt / persona.
    pub prompt: Option<String>,
}

/// A provider the operator can assign as a muscle, with enough context to
/// choose sensibly.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MuscleOption {
    pub provider: String,
    /// Whether this provider needs an API key to work.
    pub needs_key: bool,
    /// Roughly what it costs to run.
    pub cost: String,
    pub note: String,
}

/// What muscles this install can actually offer right now.
pub(super) async fn list_muscles(
    State(state): State<ServerState>,
) -> Result<Json<Vec<MuscleOption>>, StatusCode> {
    let cfg = state
        .app_config
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let mut out = Vec::new();
    for (provider, (base_url, api_key_env)) in cfg.llm_providers_map.iter() {
        let (needs_key, cost, note) = match provider.as_str() {
            "claude-code" | "codex" => (
                false,
                "included in your subscription".to_string(),
                "Uses the locally authenticated CLI subscription; no API key is stored."
                    .to_string(),
            ),
            "ollama" => (
                false,
                "free (local)".to_string(),
                format!("Runs on this machine via {base_url}. Good for bulk and overnight work."),
            ),
            _ => (
                true,
                "metered API".to_string(),
                format!("Needs {api_key_env}. Fast; good for intake and classification."),
            ),
        };
        out.push(MuscleOption {
            provider: provider.clone(),
            needs_key,
            cost,
            note,
        });
    }
    out.sort_by(|a, b| a.provider.cmp(&b.provider));
    Ok(Json(out))
}

/// Assign a director's muscle (and optionally its prompt).
///
/// Writes the Surreal `directors` row — the one the runner reads — resolving the
/// caller's id against row id, slug, then group id, the same three ways
/// `heartbeat_dispatcher::resolve_director` does. Without that, an id that works
/// for heartbeats would 404 here.
/// A model id that actually resolves to `provider`.
///
/// Prefers an existing `[llm.models.*]` alias owned by that provider so the
/// operator's own naming is respected; falls back to the provider name itself,
/// which `resolve_provider_model` then resolves through `[llm.providers.*]`.
pub(super) fn default_model_for_provider(cfg: &AppConfig, provider: &str) -> Option<String> {
    if let Some((alias, _)) = cfg
        .llm_models_map
        .iter()
        .find(|(_, (prov, _))| prov.eq_ignore_ascii_case(provider))
    {
        return Some(alias.clone());
    }
    cfg.llm_providers_map
        .contains_key(provider)
        .then(|| provider.to_string())
}

/// `PATCH` handler: assigns a director's muscle (provider/model/temperature)
/// and/or prompt, resolving the path id against row id, slug, then group id.
pub(super) async fn update_director(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDirectorRequest>,
) -> Result<Json<crate::state::Director>, StatusCode> {
    // Directors are shared by every workspace on the instance: their muscle
    // and prompt shape everyone's runs.
    require_instance_admin(&auth, &state)?;
    let store = state
        .director_store
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let mut director = match store.get(&id).await {
        Ok(Some(d)) => d,
        _ => match store.get_by_slug(&id).await {
            Ok(Some(d)) => d,
            _ => store
                .list_all()
                .await
                .ok()
                .and_then(|all| {
                    all.into_iter()
                        .find(|d| d.is_active && d.group_id.as_deref() == Some(id.as_str()))
                })
                .ok_or(StatusCode::NOT_FOUND)?,
        },
    };

    if let Some(p) = body.model_provider {
        let p = p.trim().to_string();
        // Empty string is a deliberate "unset" so the director falls back to the
        // global default, rather than being stuck on a provider forever.
        director.model_provider = (!p.is_empty()).then_some(p.clone());
        // Setting a provider alone is not enough. `config::resolve_provider_model`
        // looks the *model* up in `[llm.models.*]` first and returns that alias's
        // provider — so a director left on the default alias (which points at
        // ollama) would silently keep using ollama no matter which muscle was
        // picked. Pin a provider-native model at the same time unless the caller
        // named one explicitly.
        if body.model_name.is_none() {
            director.model_name = if p.is_empty() {
                // "Default" clears the provider, and the model with it: the
                // name left behind was the old provider's own alias, which
                // `resolve_provider_model` would keep resolving back to that
                // provider — so choosing "default" silently changed nothing.
                None
            } else {
                state
                    .app_config
                    .as_ref()
                    .and_then(|cfg| default_model_for_provider(cfg, &p))
            };
        }
    }
    if let Some(m) = body.model_name {
        let m = m.trim().to_string();
        director.model_name = (!m.is_empty()).then_some(m);
    }
    if let Some(temp) = body.temperature {
        director.temperature = Some(temp.clamp(0.0, 2.0));
    }
    if let Some(prompt) = body.prompt {
        director.prompt = prompt;
    }
    director.version = director.version.saturating_add(1);
    director.updated_at = Utc::now().to_rfc3339();

    store.update(&director).await.map_err(|e| {
        tracing::warn!(error = %e, director = %director.id, "director update failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(director))
}

/// `GET` handler: lists directors (default workspace only), joining each
/// agent group with its heartbeat state and Surreal director row.
pub(super) async fn list_directors(
    State(state): State<ServerState>,
    Extension(workspace): Extension<WorkspaceCtx>,
) -> Result<Json<Vec<DirectorView>>, StatusCode> {
    let s = control(&state)?;
    let workspace_id = workspace.id;
    if workspace_id != DEFAULT_WORKSPACE_ID {
        return Ok(Json(Vec::new()));
    }
    let groups = s.agent_groups.read().await;
    let states = s.director_states.read().await;
    // Join the Surreal director rows so each group shows the muscle it will
    // actually think with. Best-effort: without the store the view degrades to
    // the old group-only shape rather than failing the page.
    let rows: Vec<crate::state::Director> = match state.director_store.as_ref() {
        Some(store) => store.list_all().await.unwrap_or_default(),
        None => Vec::new(),
    };
    let views: Vec<DirectorView> = groups
        .iter()
        .map(|g| {
            let ds = states.iter().find(|x| x.id == g.id);
            // Same three-way resolution the runner uses: row id, slug, group id.
            let row = rows.iter().find(|d| {
                d.is_active
                    && (d.id == g.id
                        || d.slug == g.id
                        || d.group_id.as_deref() == Some(g.id.as_str()))
            });
            DirectorView {
                id: g.id.clone(),
                name: g.name.clone(),
                icon: g.icon.clone(),
                description: g.description.clone(),
                heartbeat_enabled: ds.map(|d| d.heartbeat_enabled).unwrap_or(false),
                auto_task_kinds: ds.map(|d| d.auto_task_kinds.clone()).unwrap_or_default(),
                model_provider: row.and_then(|d| d.model_provider.clone()),
                model_name: row.and_then(|d| d.model_name.clone()),
                temperature: row.and_then(|d| d.temperature),
                configurable: row.is_some(),
            }
        })
        .collect();
    Ok(Json(views))
}

/// Lesson row exposed to clients — omits the raw embedding vector (6KB of
/// floats per lesson) and surfaces `has_embedding` instead.
#[derive(Debug, Serialize)]
pub struct DirectorLessonView {
    pub id: String,
    pub director_id: String,
    pub scope: String,
    pub lesson: String,
    pub source: String,
    pub severity: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub has_embedding: bool,
}

/// Who is asking for lessons, and what they can see.
pub(super) struct LessonViewer {
    pub(super) workspace_id: String,
    pub(super) principal_id: String,
    pub(super) admin: bool,
    /// Entities visible to the viewer.
    pub(super) entity_ids: HashSet<String>,
    /// Missions whose entity is visible to the viewer.
    pub(super) mission_ids: HashSet<String>,
}

impl LessonViewer {
    async fn resolve(state: &ServerState, auth: &AuthContext) -> Self {
        let (entity_ids, mission_ids) = match control(state) {
            Ok(s) => {
                let entity_ids = {
                    let entities = s.entities.read().await;
                    entity_ids_for_workspace(
                        &entities,
                        &auth.workspace_id,
                        Some(auth.principal_id.as_str()),
                    )
                };
                let mission_ids = s
                    .missions
                    .read()
                    .await
                    .iter()
                    .filter(|m| entity_ids.contains(&m.entity_id))
                    .map(|m| m.id.clone())
                    .collect();
                (entity_ids, mission_ids)
            }
            Err(_) => (HashSet::new(), HashSet::new()),
        };
        Self {
            workspace_id: auth.workspace_id.clone(),
            principal_id: auth.principal_id.clone(),
            admin: crate::server::is_instance_admin(
                auth,
                crate::server::gateway_is_self_hosted(state),
            ),
            entity_ids,
            mission_ids,
        }
    }
}

/// Whether a lesson's `scope` puts it in front of `viewer`.
///
/// Lessons are recalled into prompts, so one tenant's lesson listed — or
/// deletable — by another is both a leak and a way to edit someone else's
/// agent. Scopes:
///
/// - `workspace:<id>` — that workspace only.
/// - `entity:<id>` / `mission:<id>` — whoever can see that entity / mission.
/// - `global` (legacy: chat wrote it for every entity-less lesson, from every
///   tenant) — its author, the instance admin, and, for author-less rows, the
///   default workspace.
/// - anything else — the instance admin only.
pub(super) fn lesson_visible(lesson: &crate::state::DirectorLesson, viewer: &LessonViewer) -> bool {
    let scope = lesson.scope.as_str();
    if scope == "global" {
        return viewer.admin
            || match lesson.created_by.as_deref() {
                Some(author) => author == viewer.principal_id,
                None => viewer.workspace_id == DEFAULT_WORKSPACE_ID,
            };
    }
    if let Some(ws) = scope.strip_prefix("workspace:") {
        return ws == viewer.workspace_id;
    }
    if let Some(eid) = scope.strip_prefix("entity:") {
        return viewer.entity_ids.contains(eid);
    }
    if let Some(mid) = scope.strip_prefix("mission:") {
        return viewer.mission_ids.contains(mid);
    }
    viewer.admin
}

/// `GET` handler: a director's lessons visible to the caller, newest first.
pub(super) async fn list_director_lessons(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<Vec<DirectorLessonView>>, StatusCode> {
    let store = state
        .director_lessons_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let mut lessons = store.list_for_director(&id).await.map_err(|e| {
        tracing::warn!(director_id = %id, error = %e, "list director lessons failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let viewer = LessonViewer::resolve(&state, &auth).await;
    lessons.retain(|l| lesson_visible(l, &viewer));
    lessons.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let views = lessons
        .into_iter()
        .map(|l| DirectorLessonView {
            id: l.id,
            director_id: l.director_id,
            scope: l.scope,
            lesson: l.lesson,
            source: l.source,
            severity: l.severity,
            created_at: l.created_at,
            created_by: l.created_by,
            has_embedding: l.embedding.is_some(),
        })
        .collect();
    Ok(Json(views))
}

/// Handler: deletes a director lesson, scoped to the path director and to
/// lessons the caller can see.
pub(super) async fn delete_director_lesson(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path((id, lesson_id)): Path<(String, String)>,
) -> Result<StatusCode, StatusCode> {
    let store = state
        .director_lessons_store
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let lessons = store
        .list_for_director(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let viewer = LessonViewer::resolve(&state, &auth).await;
    // Only lessons belonging to the path director are deletable — mirrors the
    // store's no-cross-director-leakage guarantee — and only ones the caller
    // could list; someone else's lesson is a 404, not a 403.
    if !lessons
        .iter()
        .any(|l| l.id == lesson_id && lesson_visible(l, &viewer))
    {
        return Err(StatusCode::NOT_FOUND);
    }
    store.delete(&lesson_id).await.map_err(|e| {
        tracing::warn!(lesson_id = %lesson_id, error = %e, "delete director lesson failed");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(StatusCode::NO_CONTENT)
}

/// Body for toggling a director's heartbeat.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetHeartbeatRequest {
    pub enabled: bool,
}

/// Handler: enables or disables a director's heartbeat, creating its
/// [`DirectorState`] row if it doesn't exist yet.
pub(super) async fn set_director_heartbeat(
    State(state): State<ServerState>,
    Extension(auth): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<SetHeartbeatRequest>,
) -> Result<Json<DirectorView>, StatusCode> {
    // A director's heartbeat runs for the whole instance.
    require_instance_admin(&auth, &state)?;
    let s = control(&state)?;
    // Take an owned copy of the group while holding the read lock, then drop the lock
    let group = {
        let groups = s.agent_groups.read().await;
        groups
            .iter()
            .find(|g| g.id == id)
            .cloned()
            .ok_or(StatusCode::NOT_FOUND)?
    };

    let mut states = s.director_states.write().await;
    let view = if let Some(ds) = states.iter_mut().find(|x| x.id == id) {
        ds.heartbeat_enabled = body.enabled;
        DirectorView {
            id: ds.id.clone(),
            name: group.name.clone(),
            icon: group.icon.clone(),
            description: group.description.clone(),
            heartbeat_enabled: ds.heartbeat_enabled,
            auto_task_kinds: ds.auto_task_kinds.clone(),
            // Not resolved on this path — toggling the heartbeat doesn't touch
            // the muscle, and clients refetch the list after mutating.
            model_provider: None,
            model_name: None,
            temperature: None,
            configurable: false,
        }
    } else {
        states.push(DirectorState {
            id: id.clone(),
            heartbeat_enabled: body.enabled,
            auto_task_kinds: vec![],
        });
        DirectorView {
            id: id.clone(),
            name: group.name.clone(),
            icon: group.icon.clone(),
            description: group.description.clone(),
            heartbeat_enabled: body.enabled,
            auto_task_kinds: vec![],
            model_provider: None,
            model_name: None,
            temperature: None,
            configurable: false,
        }
    };
    Ok(Json(view))
}

#[cfg(test)]
mod lesson_scope_tests {
    use super::*;

    fn lesson(scope: &str, by: Option<&str>) -> crate::state::DirectorLesson {
        crate::state::DirectorLesson {
            id: "les".into(),
            director_id: "operations".into(),
            scope: scope.into(),
            lesson: "x".into(),
            source: "observation".into(),
            severity: "soft".into(),
            embedding: None,
            created_at: String::new(),
            created_by: by.map(str::to_string),
        }
    }

    fn viewer(ws: &str, who: &str, admin: bool) -> LessonViewer {
        LessonViewer {
            workspace_id: ws.into(),
            principal_id: who.into(),
            admin,
            entity_ids: ["ent-a".to_string()].into_iter().collect(),
            mission_ids: ["mis-a".to_string()].into_iter().collect(),
        }
    }

    #[test]
    fn a_tenant_sees_only_its_own_lessons() {
        let alice = viewer("ws-a", "alice", false);
        assert!(lesson_visible(&lesson("workspace:ws-a", None), &alice));
        assert!(!lesson_visible(&lesson("workspace:ws-b", None), &alice));
        assert!(lesson_visible(&lesson("entity:ent-a", None), &alice));
        assert!(!lesson_visible(&lesson("entity:ent-b", None), &alice));
        assert!(lesson_visible(&lesson("mission:mis-a", None), &alice));
        assert!(!lesson_visible(&lesson("mission:mis-b", None), &alice));
        // Legacy `global`: the author keeps theirs; nobody else's leaks.
        assert!(lesson_visible(&lesson("global", Some("alice")), &alice));
        assert!(!lesson_visible(&lesson("global", Some("bob")), &alice));
        assert!(!lesson_visible(&lesson("global", None), &alice));
        assert!(!lesson_visible(&lesson("something-else", None), &alice));
    }

    #[test]
    fn the_operator_still_sees_instance_lessons() {
        let op = viewer("default", "bearer", true);
        assert!(lesson_visible(&lesson("global", None), &op));
        assert!(lesson_visible(&lesson("global", Some("bob")), &op));
        // …but not another workspace's own lessons.
        assert!(!lesson_visible(&lesson("workspace:ws-b", None), &op));
        let default_user = viewer("default", "someone", false);
        assert!(lesson_visible(&lesson("global", None), &default_user));
    }
}
