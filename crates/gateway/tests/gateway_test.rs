//! Integration tests for the gateway crate's axum router: entities, tasks,
//! missions, raw captures, agent runs, the live canvas, and related HTTP
//! surfaces. Each test builds an in-memory Surreal-backed `ServerState` and
//! drives it through `server::build_router` with `oneshot`, exactly as a
//! real request would.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use chrono::Utc;
use consistent_world_common::types::{ChannelKind, ScopeType, Session};
use consistent_world_gateway::{
    config, entity_store, heartbeat_store, planner, server,
    state::{
        AgentGroup, ControlState, Entity, ExternalAgentRun, MissionStatus, OperatorFeedEvent,
        OperatorFeedKind, RawCapture, RawCaptureStatus, ServerState, Task, TaskOutcome, TaskStatus,
    },
    task_store,
};
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};
use http_body_util::BodyExt;
use std::io::Write;
use std::sync::Arc;
use tempfile::{NamedTempFile, TempDir};
use tower::util::ServiceExt;

/// Config enabling the `<secret>.<workspace>.<principal>` bearer form, which
/// tests use to act as several users against one router.
///
/// That form fabricates a principal with no database lookup, so it is off
/// unless a deployment opts in — a test that impersonates users has to opt in
/// exactly as a real deployment would.
fn static_principals_config() -> Arc<config::AppConfig> {
    Arc::new(config::AppConfig {
        gateway_static_principals: true,
        ..Default::default()
    })
}

#[tokio::test]
async fn test_health_endpoint() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let app = server::build_router(ServerState::new(
        "test-version".into(),
        Some("secret".into()),
        memory,
    ));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["version"], "test-version");
    assert_eq!(json["memory_backend"], "surreal");
}

// Deleted: legacy sqlite-backend tests. The gateway is Surreal-only now; see
// `startup::create_memory_and_schedule` and the `test_health_endpoint`
// in-memory Surreal test above.

#[tokio::test]
async fn test_config_loading() {
    let mut temp = NamedTempFile::new().unwrap();
    writeln!(
        temp,
        "[gateway]\nport = 19000\nbind = \"0.0.0.0\"\n\n[agents.defaults]\nmodel = \"dashscope/qwen3.5-plus\"\nmax_concurrent = 2\n\n[agents.defaults.subagents]\nmax_concurrent = 5\n\n[memory]\nsurreal_url = \"ws://127.0.0.1:8000\"\nsurreal_ns = \"consistent-world\"\nsurreal_db = \"main\"\nvector_search = false\n\n[plugins]\nwasm_sandbox = false\n\n[credentials]\nbackend = \"vault\"\n\n[telemetry]\nlog_level = \"debug\"\n"
    )
    .unwrap();

    let cfg = config::load(temp.path().to_str().unwrap()).unwrap();
    assert_eq!(cfg.gateway_port, 19000);
    assert_eq!(cfg.gateway_bind, "0.0.0.0");
    assert_eq!(cfg.default_model, "dashscope/qwen3.5-plus");
    assert_eq!(cfg.max_concurrent_agents, 2);
    assert_eq!(cfg.max_concurrent_subagents, 5);
    assert!(!cfg.vector_search_enabled);
    assert!(!cfg.wasm_sandbox_enabled);
    assert_eq!(cfg.credentials_backend, "vault");
    assert_eq!(cfg.telemetry_log_level, "debug");
    // Secrets are not read from TOML; resolved at runtime from credentials store
    assert_eq!(cfg.gateway_auth_token, None);

    assert_eq!(cfg.surreal_url, "ws://127.0.0.1:8000");
    assert_eq!(cfg.surreal_ns, "consistent-world");
    assert_eq!(cfg.surreal_db, "main");
}

#[tokio::test]
async fn operator_feed_entity_id_query_filters() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    control
        .entities
        .write()
        .await
        .push(consistent_world_gateway::state::Entity {
            id: "e1".into(),
            name: "Entity One".into(),
            owner: Some("default".into()),
            status: "Active".into(),
            dept: "General".into(),
            ..Default::default()
        });
    control
        .entities
        .write()
        .await
        .push(consistent_world_gateway::state::Entity {
            id: "e2".into(),
            name: "Entity Two".into(),
            owner: Some("default".into()),
            status: "Active".into(),
            dept: "General".into(),
            ..Default::default()
        });
    control
        .push_operator_feed_surface(
            OperatorFeedKind::ConnectionVerified,
            "Alpha",
            None,
            Some("e1".into()),
            Some("c1".into()),
        )
        .await;
    control
        .push_operator_feed_surface(
            OperatorFeedKind::ConnectionVerified,
            "Beta",
            None,
            Some("e2".into()),
            None,
        )
        .await;

    let state =
        ServerState::new("t".into(), Some("secret".into()), memory).with_control_state(control);
    let app = server::build_router(state);

    let res = app
        .oneshot(
            Request::builder()
                .uri("/api/operator-feed?entity_id=e1&limit=50")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let paginated: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let items = paginated["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], "Alpha");
    assert_eq!(items[0]["entityId"], "e1");
}

#[tokio::test]
async fn planner_suggest_lane_requires_approval_for_external_actions() {
    use consistent_world_common::capabilities::{CapabilityId, ExecutionLevel};
    use consistent_world_gateway::state::{
        AutomationMode, Entity, EntityRuntimeConfig, ExecutionPolicies,
    };

    let entity = Entity {
        id: "e1".into(),
        name: "E".into(),
        status: "Active".into(),
        dept: "General".into(),
        execution_policies: Some(ExecutionPolicies {
            allowed_capabilities: vec![CapabilityId::MessageExternal],
            allowed_execution_levels: vec![ExecutionLevel::Draft, ExecutionLevel::Coordinate],
            approval_required_capabilities: vec![],
        }),
        entity_runtime: Some(EntityRuntimeConfig {
            automation_mode: AutomationMode::Suggest,
            heartbeat_mode: Default::default(),
            default_chat_session_id: None,
            quiet_hours_local: None,
        }),
        ..Default::default()
    };
    let lane = planner::lane_for_entity(Some(&entity));
    assert!(planner::entity_action_requires_approval_lane(
        lane,
        "send_email",
        false
    ));
    assert!(!planner::entity_action_requires_approval_lane(
        lane,
        "verify_ownership",
        false
    ));
}

#[tokio::test]
async fn test_auth_middleware() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let session = Session {
        id: "session-1".into(),
        agent_id: "agent-a".into(),
        channel: ChannelKind::Web,
        peer_id: "peer-123".into(),
        scope_type: ScopeType::Principal,
        scope_id: "webhook:peer-123".into(),
        channel_binding_metadata: None,
        resolution_reason: Some("test".into()),
        created_at: Utc::now(),
        last_active: Utc::now(),
    };
    memory.upsert_session(&session).await.unwrap();

    let app = server::build_router(ServerState::new(
        "test-version".into(),
        Some("secret".into()),
        memory,
    ));

    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let authorized = app
        .oneshot(
            Request::builder()
                .uri("/api/sessions")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::OK);
    let body = authorized.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["items"].as_array().unwrap().len(), 1);
    assert_eq!(json["items"][0]["id"], "session-1");
}

#[tokio::test]
async fn test_ensure_config_file_creates_default() {
    let temp_dir = TempDir::new().unwrap();
    let config_path = temp_dir.path().join("consistent_world.toml");
    let path_str = config_path.to_str().unwrap();

    assert!(!config_path.exists());
    let expanded = config::ensure_config_file(path_str).unwrap();
    assert_eq!(expanded, path_str);
    assert!(config_path.exists());

    let cfg = config::load(&expanded).unwrap();
    assert_eq!(cfg.gateway_port, 18800);
    assert_eq!(cfg.gateway_bind, "127.0.0.1");
    // Keyless-first default, same as the desktop shell: the operator lane
    // rides the installed `claude` CLI (subscription, no API key) and falls
    // back to local Ollama — see `default_toml_content`.
    assert_eq!(cfg.llm_provider, "claude-code");
    assert_eq!(cfg.default_model, "operator-default");
    let raw = std::fs::read_to_string(&config_path).unwrap();
    assert!(raw.contains("[gateway]"));
    // Both semantic tiers must be present in the freshly-written default; this
    // is what `run_agent` resolves against to route `prompt-interpretation`
    // through the fast interpreter lane and everything else through operator.
    assert!(
        raw.contains("[routing.model_tiers.operator]"),
        "default config must define the operator tier"
    );
    assert!(
        raw.contains("[routing.model_tiers.interpreter]"),
        "default config must define the interpreter tier"
    );
    assert!(cfg.model_tier_routes.contains_key("operator"));
    assert!(cfg.model_tier_routes.contains_key("interpreter"));
    // The interpreter tier must resolve keyless out of the box: the alias
    // rides the `claude` CLI (subscription) with the tier's own provider as
    // the local last resort. Gemini survives as interpreter-fallback for
    // operators who add a key.
    let interp = cfg
        .model_tier_routes
        .get("interpreter")
        .expect("interpreter tier present");
    assert_eq!(interp.provider.as_deref(), Some("ollama"));
    let interp_alias = interp.model.as_deref().unwrap_or("");
    let interp_model = cfg
        .llm_models_map
        .get(interp_alias)
        .expect("interpreter alias resolves through llm.models");
    assert_eq!(interp_model.0, "claude-code");
    assert_eq!(interp_model.1, "sonnet");
    let interp_fallback = cfg
        .llm_models_map
        .get("interpreter-fallback")
        .expect("interpreter-fallback alias present");
    assert_eq!(interp_fallback.0, "google");
    assert!(interp_fallback.1.contains("gemini"));
}

#[tokio::test]
async fn mission_timeline_actions_emit_expected_flow_events() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let create_entity_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/entities")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Phase 2 Entity"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_entity_res.status(), StatusCode::OK);
    let create_entity_body = create_entity_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let created_entity: serde_json::Value = serde_json::from_slice(&create_entity_body).unwrap();
    let entity_id = created_entity["id"].as_str().unwrap().to_string();

    let create_mission_payload = serde_json::json!({
        "entity_id": entity_id,
        "title": "Phase 2 Mission",
        "status": "active",
        "priority": "high",
        "goal": "verify mission flow"
    });
    let create_mission_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/missions")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(create_mission_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_mission_res.status(), StatusCode::OK);
    let create_mission_body = create_mission_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let created_mission: serde_json::Value = serde_json::from_slice(&create_mission_body).unwrap();
    let mission_id = created_mission["id"].as_str().unwrap().to_string();

    // Simulate heartbeat review output: a stalled recommendation signal before manual actions.
    let mission = control
        .missions
        .read()
        .await
        .iter()
        .find(|m| m.id == mission_id)
        .cloned()
        .expect("mission should exist in control state");
    control
        .push_mission_feed(
            OperatorFeedKind::MissionStalled,
            &mission,
            Some("reason=no_next_executable_task\nbranch=user_input".to_string()),
        )
        .await;

    let review_now_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/missions/{mission_id}/actions"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"action":"review_now"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(review_now_res.status(), StatusCode::OK);

    let mark_user_input_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/missions/{mission_id}/actions"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"action":"mark_user_input_needed","note":"manual clarification required"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(mark_user_input_res.status(), StatusCode::OK);

    let accept_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/missions/{mission_id}/actions"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"action":"accept_proposed_next_step"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(accept_res.status(), StatusCode::OK);

    let mission_after = control
        .missions
        .read()
        .await
        .iter()
        .find(|m| m.id == mission_id)
        .cloned()
        .expect("mission should still exist");
    assert_eq!(mission_after.status, MissionStatus::UserInputNeeded);

    let task_count = control
        .tasks
        .read()
        .await
        .iter()
        .filter(|t| t.mission_id.as_deref() == Some(mission_id.as_str()))
        .count();
    assert!(task_count >= 1);

    let feed = control.operator_feed.read().await.clone();
    let mission_feed: Vec<_> = feed
        .iter()
        .filter(|e| e.mission_id.as_deref() == Some(mission_id.as_str()))
        .cloned()
        .collect();
    assert!(
        mission_feed
            .iter()
            .any(|e| e.kind == OperatorFeedKind::MissionStalled)
    );
    assert!(
        mission_feed
            .iter()
            .any(|e| e.kind == OperatorFeedKind::MissionReviewDue)
    );
    assert!(
        mission_feed
            .iter()
            .any(|e| e.kind == OperatorFeedKind::MissionUserInputNeeded)
    );
    assert!(
        mission_feed
            .iter()
            .any(|e| e.kind == OperatorFeedKind::MissionRecommendationAccepted)
    );
    assert!(
        mission_feed
            .iter()
            .any(|e| e.kind == OperatorFeedKind::TaskCreated)
    );
}

#[tokio::test]
async fn create_mission_emits_machine_readable_autorun_block_when_no_runner_exists() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let create_entity_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/entities")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Unstaffed Entity"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_entity_res.status(), StatusCode::OK);
    let create_entity_body = create_entity_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let created_entity: serde_json::Value = serde_json::from_slice(&create_entity_body).unwrap();
    let entity_id = created_entity["id"].as_str().unwrap().to_string();

    let create_mission_payload = serde_json::json!({
        "entity_id": entity_id,
        "title": "Autorun blocked mission",
        "status": "active",
        "priority": "high",
        "goal": "verify machine-readable autorun blocker"
    });
    let create_mission_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/missions")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(create_mission_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_mission_res.status(), StatusCode::OK);
    let create_mission_body = create_mission_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let created_mission: serde_json::Value = serde_json::from_slice(&create_mission_body).unwrap();
    let mission_id = created_mission["id"].as_str().unwrap().to_string();

    let feed = control.operator_feed.read().await.clone();
    let stalled = feed
        .iter()
        .find(|e| {
            e.mission_id.as_deref() == Some(mission_id.as_str())
                && e.kind == OperatorFeedKind::MissionStalled
        })
        .expect("mission stalled event should exist");
    let detail = stalled.detail.as_deref().unwrap_or("");
    assert!(detail.contains("reason=autorun_blocked"));
    assert!(detail.contains("action=mission_create"));
    assert!(detail.contains("blockers="));
}

#[tokio::test]
async fn create_command_rejects_unknown_capture_intent() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state =
        ServerState::new("t".into(), Some("secret".into()), memory).with_control_state(control);
    let app = server::build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/commands")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                      "director_id":"any",
                      "prompt":"hello",
                      "capture_intent":"unknown-intent"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn workspace_isolation_blocks_cross_workspace_command_runtime_and_requeue_access() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    control.agent_groups.write().await.push(AgentGroup {
        id: "dir-1".to_string(),
        name: "General".to_string(),
        icon: "🧭".to_string(),
        description: "General director".to_string(),
    });
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone())
        .with_app_config(static_principals_config());
    let app = server::build_router(state);

    let create_entity_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/entities")
                .header("Authorization", "Bearer secret.ws-a.user")
                .header("x-workspace-id", "ws-a")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Workspace A Entity"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_entity_res.status(), StatusCode::OK);
    let entity_body = create_entity_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let entity: serde_json::Value = serde_json::from_slice(&entity_body).unwrap();
    let entity_id = entity["id"].as_str().unwrap().to_string();

    let runtime_forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/entities/{entity_id}/runtime-context"))
                .header("Authorization", "Bearer secret.ws-b.user")
                .header("x-workspace-id", "ws-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(runtime_forbidden.status(), StatusCode::NOT_FOUND);

    let memory_preview_forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/entities/{entity_id}/memory-preview"))
                .header("Authorization", "Bearer secret.ws-b.user")
                .header("x-workspace-id", "ws-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(memory_preview_forbidden.status(), StatusCode::NOT_FOUND);

    let create_command_payload = serde_json::json!({
        "director_id": "dir-1",
        "entity_id": entity_id,
        "prompt": "Do the thing"
    });
    let command_forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/commands")
                .header("Authorization", "Bearer secret.ws-b.user")
                .header("x-workspace-id", "ws-b")
                .header("content-type", "application/json")
                .body(Body::from(create_command_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(command_forbidden.status(), StatusCode::FORBIDDEN);

    let create_task_payload = serde_json::json!({
        "title": "Workspace A task",
        "entity_id": entity["id"].as_str().unwrap()
    });
    let create_task_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks")
                .header("Authorization", "Bearer secret.ws-a.user")
                .header("x-workspace-id", "ws-a")
                .header("content-type", "application/json")
                .body(Body::from(create_task_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_task_res.status(), StatusCode::OK);
    let task_body = create_task_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let task: serde_json::Value = serde_json::from_slice(&task_body).unwrap();
    let task_id = task["id"].as_str().unwrap();

    let requeue_forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/tasks/{task_id}/requeue"))
                .header("Authorization", "Bearer secret.ws-b.user")
                .header("x-workspace-id", "ws-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(requeue_forbidden.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn create_task_canonicalizes_follow_up_kind_to_reminder() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state =
        ServerState::new("t".into(), Some("secret".into()), memory).with_control_state(control);
    let app = server::build_router(state);

    let create_entity_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/entities")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Canonical Kind Entity"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_entity_res.status(), StatusCode::OK);
    let create_entity_body = create_entity_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let created_entity: serde_json::Value = serde_json::from_slice(&create_entity_body).unwrap();
    let entity_id = created_entity["id"].as_str().unwrap();

    let create_task_payload = serde_json::json!({
        "title": "Follow up with legal",
        "dept": "General",
        "entity_id": entity_id,
        "task_kind": "follow-up",
        "content": "Need to revisit legal terms next week"
    });
    let create_task_res = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(create_task_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_task_res.status(), StatusCode::OK);
    let create_task_body = create_task_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let created_task: serde_json::Value = serde_json::from_slice(&create_task_body).unwrap();
    assert_eq!(created_task["task_kind"], "reminder");
}

#[tokio::test]
async fn openclaw_order_ingest_creates_idempotent_entity_task() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let create_entity_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/entities")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"OpenClaw Entity"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_entity_res.status(), StatusCode::OK);
    let create_entity_body = create_entity_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let created_entity: serde_json::Value = serde_json::from_slice(&create_entity_body).unwrap();
    let entity_id = created_entity["id"].as_str().unwrap();

    let create_connection_payload = serde_json::json!({
        "kind": "api_service",
        "provider": "openclaw",
        "label": "OpenClaw Orders",
        "status": "connected",
        "scopes": ["orders:ingest", "tasks:create"]
    });
    let create_connection_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/entities/{entity_id}/connections"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(create_connection_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_connection_res.status(), StatusCode::OK);
    let create_connection_body = create_connection_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let created_connection: serde_json::Value =
        serde_json::from_slice(&create_connection_body).unwrap();
    let connection_id = created_connection["id"].as_str().unwrap();

    let order_payload = serde_json::json!({
        "orderId": "order-123",
        "title": "Prepare onboarding harness",
        "summary": "Turn this OpenClaw order into a Consistent task",
        "payload": {
            "sourceConversation": "main",
            "priority": "now"
        }
    });
    let ingest_uri =
        format!("/api/entities/{entity_id}/connections/{connection_id}/openclaw/orders");
    let first_ingest_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&ingest_uri)
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(order_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_ingest_res.status(), StatusCode::OK);
    let first_body = first_ingest_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    let first_task_id = first_json["task"]["id"].as_str().unwrap();
    assert_eq!(first_json["externalSource"], "openclaw");
    assert_eq!(first_json["externalId"], "order-123");
    assert_eq!(first_json["task"]["connection_id"], connection_id);
    assert_eq!(first_json["task"]["external_source"], "openclaw");
    assert_eq!(first_json["task"]["external_id"], "order-123");

    let duplicate_ingest_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&ingest_uri)
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(order_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicate_ingest_res.status(), StatusCode::OK);
    let duplicate_body = duplicate_ingest_res
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let duplicate_json: serde_json::Value = serde_json::from_slice(&duplicate_body).unwrap();
    assert_eq!(
        duplicate_json["task"]["id"].as_str().unwrap(),
        first_task_id
    );

    let tasks = control.tasks.read().await;
    let matching: Vec<_> = tasks
        .iter()
        .filter(|task| {
            task.entity_id.as_deref() == Some(entity_id)
                && task.external_source.as_deref() == Some("openclaw")
                && task.external_id.as_deref() == Some("order-123")
        })
        .collect();
    assert_eq!(matching.len(), 1);
}

/// Regression test for the SurrealDB Thing-vs-string entity hydration mismatch.
///
/// Before the fix, `SurrealEntityStore::list_all` deserialised rows directly
/// into `Vec<Entity>`, which expects `id: String` while Surreal returns a
/// `Thing` for the row id. The select would fail with `expected string, found
/// object`, breaking entity hydration on cold start. This test exercises the
/// full sync_all → list_all round-trip against an in-memory Surreal so the
/// regression cannot come back silently.
#[tokio::test]
async fn entity_store_round_trip_through_surreal() {
    let memory = SurrealMemoryStore::connect_memory().await.unwrap();
    let store = entity_store::SurrealEntityStore::new(memory.db());
    store.init_schema().await.unwrap();

    let entity = Entity {
        id: "ent-roundtrip".into(),
        name: "Roundtrip Entity".into(),
        description: Some("hydration regression coverage".into()),
        status: "Active".into(),
        dept: "General".into(),
        path: Some("/tmp/roundtrip".into()),
        workspace_id: Some("ws-test".into()),
        ..Default::default()
    };

    store.sync_all(std::slice::from_ref(&entity)).await.unwrap();
    let listed = store.list_all().await.unwrap();
    assert_eq!(listed.len(), 1, "exactly one entity should be persisted");
    assert_eq!(listed[0].id, "ent-roundtrip");
    assert_eq!(listed[0].name, "Roundtrip Entity");
    assert_eq!(listed[0].workspace_id.as_deref(), Some("ws-test"));

    let by_ws = store.list_by_workspace("ws-test").await.unwrap();
    assert_eq!(by_ws.len(), 1);
    assert_eq!(by_ws[0].id, "ent-roundtrip");

    // A sync never infers a delete from a missing id (its input may be a stale
    // clone or a fallback snapshot); only an explicit delete removes the row.
    store.sync_all(&[]).await.unwrap();
    assert_eq!(store.list_all().await.unwrap().len(), 1);
    store.delete("ent-roundtrip").await.unwrap();
    let after_delete = store.list_all().await.unwrap();
    assert!(
        after_delete.is_empty(),
        "an explicit delete must remove the row; got {after_delete:?}"
    );
}

// ---------------------------------------------------------------------------
// Raw capture lifecycle: create, list, promote to task/memory, and dismiss.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn raw_capture_promote_and_dismiss_transitions() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    // Create an entity to scope one capture to.
    let entity_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/entities")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Capture Entity"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(entity_res.status(), StatusCode::OK);
    let entity_body = entity_res.into_body().collect().await.unwrap().to_bytes();
    let entity_json: serde_json::Value = serde_json::from_slice(&entity_body).unwrap();
    let entity_id = entity_json["id"].as_str().unwrap().to_string();

    // Create an entity-scoped raw capture.
    let create_payload = serde_json::json!({
        "raw_text": "Investigate the onboarding drop-off",
        "entity_id": entity_id,
        "source": "chat"
    });
    let create_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/captures")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(create_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_res.status(), StatusCode::OK);
    let create_body = create_res.into_body().collect().await.unwrap().to_bytes();
    let capture_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
    assert_eq!(capture_json["status"], "raw");
    let capture_id = capture_json["id"].as_str().unwrap().to_string();

    // It shows up in the raw list.
    let list_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/captures?status=raw")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);
    let list_body = list_res.into_body().collect().await.unwrap().to_bytes();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list_json["total"].as_u64().unwrap(), 1);

    // Promote it to a task.
    let promote_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/captures/{capture_id}/promote-task"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(promote_res.status(), StatusCode::OK);
    let promote_body = promote_res.into_body().collect().await.unwrap().to_bytes();
    let promote_json: serde_json::Value = serde_json::from_slice(&promote_body).unwrap();
    let task_id = promote_json["task"]["id"].as_str().unwrap().to_string();
    assert_eq!(promote_json["capture"]["status"], "promoted_to_task");
    assert_eq!(promote_json["capture"]["promoted_task_id"], task_id);
    assert_eq!(promote_json["task"]["entity_id"], entity_id);

    // State reflects the transition and the task now exists.
    {
        let captures = control.raw_captures.read().await;
        let cap = captures.iter().find(|c| c.id == capture_id).unwrap();
        assert_eq!(cap.status, RawCaptureStatus::PromotedToTask);
        assert_eq!(cap.promoted_task_id.as_deref(), Some(task_id.as_str()));
    }
    assert!(control.tasks.read().await.iter().any(|t| t.id == task_id));

    // Promotion is idempotent — a second call returns the same task.
    let promote_again = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/captures/{capture_id}/promote-task"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(promote_again.status(), StatusCode::OK);
    let again_body = promote_again
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let again_json: serde_json::Value = serde_json::from_slice(&again_body).unwrap();
    assert_eq!(again_json["task"]["id"].as_str().unwrap(), task_id);

    // A second, inbox-level capture can be dismissed.
    let inbox_payload = serde_json::json!({ "raw_text": "Ping the ops channel" });
    let inbox_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/captures")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(inbox_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(inbox_res.status(), StatusCode::OK);
    let inbox_body = inbox_res.into_body().collect().await.unwrap().to_bytes();
    let inbox_json: serde_json::Value = serde_json::from_slice(&inbox_body).unwrap();
    let inbox_id = inbox_json["id"].as_str().unwrap().to_string();

    let dismiss_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/captures/{inbox_id}/dismiss"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(dismiss_res.status(), StatusCode::OK);
    let dismiss_body = dismiss_res.into_body().collect().await.unwrap().to_bytes();
    let dismiss_json: serde_json::Value = serde_json::from_slice(&dismiss_body).unwrap();
    assert_eq!(dismiss_json["status"], "dismissed");

    // Neither the promoted nor dismissed capture remains in the raw list.
    let final_list = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/captures?status=raw")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let final_body = final_list.into_body().collect().await.unwrap().to_bytes();
    let final_json: serde_json::Value = serde_json::from_slice(&final_body).unwrap();
    assert_eq!(final_json["total"].as_u64().unwrap(), 0);
}

/// Serialises the tests that redirect the vault root.
///
/// `CONSISTENT_WORLD_VAULT_ROOT` is process-global and `local_vault::vault_root()`
/// reads it fresh on every call, but `cargo test` runs these on parallel threads:
/// one test would overwrite another's root mid-flight, and whichever `TempDir`
/// dropped first left the variable pointing at a deleted directory. That raced
/// rarely enough to look like an unrelated flake under full-workspace load.
static VAULT_ROOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Point the local vault at a throwaway dir so review actions do not touch the
/// developer's real `~/.consistent-world/vault`.
///
/// Holds [`VAULT_ROOT_LOCK`] for the caller's lifetime, so only one test owns the
/// variable at a time. Returns both guards: dropping them releases the lock and
/// removes the directory, in that order.
fn scoped_vault_root() -> (TempDir, std::sync::MutexGuard<'static, ()>) {
    // A poisoned lock just means some other vault test panicked; the root is
    // about to be overwritten anyway, so recover rather than cascade failures.
    let guard = VAULT_ROOT_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: test-only env var, and the lock above guarantees no other test is
    // reading or writing it concurrently.
    unsafe {
        std::env::set_var("CONSISTENT_WORLD_VAULT_ROOT", dir.path());
    }
    (dir, guard)
}

/// Creates an entity via the API and returns its id.
async fn create_review_entity(app: &axum::Router, name: &str) -> String {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/entities")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"name":"{name}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    json["id"].as_str().unwrap().to_string()
}

/// Creates a raw capture scoped to `entity_id` via the API.
async fn create_entity_capture(app: &axum::Router, entity_id: &str, text: &str) -> String {
    let payload = serde_json::json!({
        "raw_text": text,
        "entity_id": entity_id,
        "source": "chat",
    });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/captures")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    json["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn raw_capture_promote_memory_writes_note() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Memory Entity").await;
    let capture_id =
        create_entity_capture(&app, &entity_id, "The pricing page converts best at $29").await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/captures/{capture_id}/promote-memory"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"kind":"canonical"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["capture"]["status"], "promoted_to_memory");

    // The durable note exists on disk with the captured content.
    let note_path = json["note_path"].as_str().unwrap();
    let note = tokio::fs::read_to_string(note_path).await.unwrap();
    assert!(note.contains("type: canonical-note"));
    assert!(note.contains("The pricing page converts best at $29"));

    // Canonical control state reflects the transition.
    let captures = control.raw_captures.read().await;
    let cap = captures.iter().find(|c| c.id == capture_id).unwrap();
    assert_eq!(cap.status, RawCaptureStatus::PromotedToMemory);
}

#[tokio::test]
async fn raw_capture_update_narrative_marks_status() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Narrative Entity").await;
    let capture_id =
        create_entity_capture(&app, &entity_id, "Shipped the capture review loop").await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/captures/{capture_id}/update-narrative"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["capture"]["status"], "narrative_updated");

    let note_path = json["note_path"].as_str().unwrap();
    let note = tokio::fs::read_to_string(note_path).await.unwrap();
    assert!(note.contains("type: entity-narrative-log"));
    assert!(note.contains("Shipped the capture review loop"));

    let captures = control.raw_captures.read().await;
    let cap = captures.iter().find(|c| c.id == capture_id).unwrap();
    assert_eq!(cap.status, RawCaptureStatus::NarrativeUpdated);
}

#[tokio::test]
async fn raw_capture_review_respects_entity_access() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    // An entity owned by a different workspace must not be reachable from the
    // `default` workspace token.
    let now = Utc::now().to_rfc3339();
    control.entities.write().await.push(Entity {
        id: "ent-foreign".to_string(),
        name: "Foreign Entity".to_string(),
        owner: Some("tenant-b".to_string()),
        ..Entity::default()
    });
    control.raw_captures.write().await.push(RawCapture {
        id: "cap-foreign".to_string(),
        workspace_id: "default".to_string(),
        entity_id: Some("ent-foreign".to_string()),
        source: "chat".to_string(),
        source_ref: None,
        session_id: None,
        provider: None,
        raw_text: "cross-workspace leak attempt".to_string(),
        summary: None,
        status: RawCaptureStatus::Raw,
        promoted_task_id: None,
        metadata: serde_json::Value::Null,
        created_at: now.clone(),
        updated_at: now,
    });

    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    for route in ["promote-memory", "update-narrative"] {
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/captures/cap-foreign/{route}"))
                    .header("Authorization", "Bearer secret")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN, "route {route}");
    }

    // A narrative update with no entity anywhere is a bad request.
    let inbox_payload = serde_json::json!({ "raw_text": "no entity here" });
    let inbox_res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/captures")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(inbox_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let inbox_body = inbox_res.into_body().collect().await.unwrap().to_bytes();
    let inbox_json: serde_json::Value = serde_json::from_slice(&inbox_body).unwrap();
    let inbox_id = inbox_json["id"].as_str().unwrap();

    let narr = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/captures/{inbox_id}/update-narrative"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(narr.status(), StatusCode::BAD_REQUEST);
}

/// Creates an unscoped (inbox-level) raw capture via the API.
async fn create_inbox_capture(app: &axum::Router, text: &str) -> String {
    let payload = serde_json::json!({ "raw_text": text });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/captures")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    json["id"].as_str().unwrap().to_string()
}

/// POSTs to a capture's `/route` endpoint and returns the parsed response body.
async fn post_route(
    app: &axum::Router,
    capture_id: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/captures/{capture_id}/route"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn raw_capture_route_explicit_entity_id() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Route Entity").await;
    let capture_id = create_entity_capture(&app, &entity_id, "already scoped work").await;

    let json = post_route(&app, &capture_id, serde_json::json!({})).await;
    assert_eq!(
        json["suggestion"]["entityId"].as_str(),
        Some(entity_id.as_str())
    );
    assert_eq!(json["suggestion"]["confidence"].as_f64(), Some(1.0));
    assert_eq!(json["applied"], false);
}

#[tokio::test]
async fn raw_capture_route_by_github_url() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    // Entity whose setup.repoPath points at a GitHub repo.
    let mut entity = Entity {
        id: "ent-gh".to_string(),
        name: "GitHub Entity".to_string(),
        ..Entity::default()
    };
    entity.setup.repo_path = Some("https://github.com/acme/widgets".to_string());
    control.entities.write().await.push(entity);

    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let capture_id = create_inbox_capture(
        &app,
        "please review https://github.com/acme/widgets/pull/12",
    )
    .await;

    // Suggestion matches; with auto_apply it attaches.
    let json = post_route(&app, &capture_id, serde_json::json!({ "auto_apply": true })).await;
    assert_eq!(json["suggestion"]["entityId"].as_str(), Some("ent-gh"));
    assert!(json["suggestion"]["confidence"].as_f64().unwrap() >= 0.8);
    assert_eq!(json["applied"], true);
    assert_eq!(json["capture"]["entity_id"].as_str(), Some("ent-gh"));

    // Applied entity_id is reflected in canonical state, status still raw.
    let captures = control.raw_captures.read().await;
    let cap = captures.iter().find(|c| c.id == capture_id).unwrap();
    assert_eq!(cap.entity_id.as_deref(), Some("ent-gh"));
    assert_eq!(cap.status, RawCaptureStatus::Raw);
}

#[tokio::test]
async fn raw_capture_route_unresolved_no_suggestion() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let capture_id = create_inbox_capture(&app, "a plain thought with no links at all").await;
    let json = post_route(&app, &capture_id, serde_json::json!({ "auto_apply": true })).await;
    assert!(json["suggestion"]["entityId"].is_null());
    assert_eq!(json["suggestion"]["confidence"].as_f64(), Some(0.0));
    assert_eq!(json["applied"], false);
}

#[tokio::test]
async fn raw_capture_attach_preserves_status() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Attach Entity").await;
    let capture_id = create_inbox_capture(&app, "route me manually").await;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/captures/{capture_id}/attach"))
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "entity_id": entity_id }).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["entity_id"].as_str(), Some(entity_id.as_str()));
    // Attach must not promote or change the review status.
    assert_eq!(json["status"], "raw");

    let captures = control.raw_captures.read().await;
    let cap = captures.iter().find(|c| c.id == capture_id).unwrap();
    assert_eq!(cap.entity_id.as_deref(), Some(entity_id.as_str()));
    assert_eq!(cap.status, RawCaptureStatus::Raw);
}

// ---------------------------------------------------------------------------
// External captures: idempotent ingest and repo-based auto-routing.
// ---------------------------------------------------------------------------
/// POSTs to `/api/external-captures` and returns `(status, body)`.
async fn post_external_capture(
    app: &axum::Router,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/external-captures")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn external_capture_create_and_idempotency() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let body = serde_json::json!({
        "raw_text": "Refactored the auth middleware.",
        "source": "claude_code",
        "external_id": "cc-run-1",
        "provider": "claude",
    });
    let (status, json) = post_external_capture(&app, body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["existing"], false);
    assert_eq!(json["applied"], false);
    assert_eq!(json["capture"]["status"], "raw");
    assert_eq!(json["capture"]["source"], "claude_code");
    let capture_id = json["capture"]["id"].as_str().unwrap().to_string();

    // Same source + external_id returns the existing capture, no duplicate.
    let (status2, json2) = post_external_capture(&app, body).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(json2["existing"], true);
    assert_eq!(json2["capture"]["id"].as_str().unwrap(), capture_id);
    assert_eq!(control.raw_captures.read().await.len(), 1);
}

#[tokio::test]
async fn external_capture_auto_route_by_repo_url() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let mut entity = Entity {
        id: "ent-gh".to_string(),
        name: "GitHub Entity".to_string(),
        ..Entity::default()
    };
    entity.setup.repo_path = Some("https://github.com/acme/widgets".to_string());
    control.entities.write().await.push(entity);

    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let body = serde_json::json!({
        "raw_text": "did some refactoring",
        "source": "claude_code",
        "repo_url": "https://github.com/acme/widgets",
        "auto_route": true,
    });
    let (status, json) = post_external_capture(&app, body).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["applied"], true);
    assert_eq!(json["capture"]["entity_id"].as_str(), Some("ent-gh"));
    assert!(json["suggestion"]["confidence"].as_f64().unwrap() >= 0.8);
    // Auto-route attaches but never promotes.
    assert_eq!(json["capture"]["status"], "raw");
}

#[tokio::test]
async fn external_capture_foreign_entity_is_forbidden() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    control.entities.write().await.push(Entity {
        id: "ent-foreign".to_string(),
        name: "Foreign".to_string(),
        owner: Some("tenant-b".to_string()),
        ..Entity::default()
    });
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let body = serde_json::json!({
        "raw_text": "cross workspace attempt",
        "source": "shell",
        "entity_id": "ent-foreign",
    });
    let (status, _json) = post_external_capture(&app, body).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(control.raw_captures.read().await.len(), 0);
}

#[tokio::test]
async fn external_capture_batch_partial_failure() {
    let _vault = scoped_vault_root();
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let body = serde_json::json!({
        "items": [
            { "raw_text": "log line A", "source": "shell", "external_id": "a1" },
            { "raw_text": "", "source": "shell" },
            { "raw_text": "log line A again", "source": "shell", "external_id": "a1" },
        ]
    });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/external-captures/batch")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // One created, one failed (empty text), one idempotent existing.
    assert_eq!(json["created"], 1);
    assert_eq!(json["failed"], 1);
    assert_eq!(json["existing"], 1);
    assert_eq!(json["results"][1]["ok"], false);
    assert_eq!(json["results"][1]["error_status"], 400);
    // Only the one distinct capture was stored.
    assert_eq!(control.raw_captures.read().await.len(), 1);
}

// ---------------------------------------------------------------------------
// Mission Definition of Done and the Mission Control activity timeline.
// ---------------------------------------------------------------------------
/// Creates a mission for `entity_id` via the API and returns its id.
async fn create_mission_via_api(app: &axum::Router, entity_id: &str) -> String {
    let payload = serde_json::json!({
        "entity_id": entity_id,
        "title": "DoD Mission",
        "status": "active",
        "priority": "medium",
        "goal": "finish the work",
    });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/missions")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    json["id"].as_str().unwrap().to_string()
}

/// Fetches a mission's Definition of Done summary.
async fn get_dod(app: &axum::Router, mission_id: &str) -> serde_json::Value {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/missions/{mission_id}/dod"))
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn mission_dod_reflects_task_state() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "DoD Entity").await;
    let mission_id = create_mission_via_api(&app, &entity_id).await;

    // Mission creation fans out starter tasks; clear them for a deterministic
    // baseline so the assertions below exercise DoD directly.
    control
        .tasks
        .write()
        .await
        .retain(|t| t.mission_id.as_deref() != Some(mission_id.as_str()));

    // No tasks → not started.
    let dod = get_dod(&app, &mission_id).await;
    assert_eq!(dod["status"], "not_started");
    assert_eq!(dod["totalTasks"], 0);

    // Two completed, verified tasks → the mission's work is done.
    let mk = |id: &str, status: TaskStatus, outcome: TaskOutcome| Task {
        id: id.to_string(),
        title: "t".to_string(),
        entity_id: Some(entity_id.clone()),
        mission_id: Some(mission_id.clone()),
        status,
        outcome: Some(outcome),
        ..Task::default()
    };
    {
        let mut tasks = control.tasks.write().await;
        tasks.push(mk("t-a", TaskStatus::Completed, TaskOutcome::Completed));
        tasks.push(mk("t-b", TaskStatus::Completed, TaskOutcome::Completed));
    }
    let dod = get_dod(&app, &mission_id).await;
    assert_eq!(dod["status"], "met");
    assert_eq!(dod["completed"], 2);

    // An open task pulls it back to in-progress.
    control
        .tasks
        .write()
        .await
        .push(mk("t-c", TaskStatus::InProgress, TaskOutcome::Completed));
    let dod = get_dod(&app, &mission_id).await;
    assert_eq!(dod["status"], "in_progress");
    assert_eq!(dod["open"], 1);

    // A flagged output moves a done mission to needs_review.
    {
        let mut tasks = control.tasks.write().await;
        tasks.retain(|t| t.id != "t-c");
        tasks.push(mk("t-d", TaskStatus::Completed, TaskOutcome::NeedsReview));
    }
    let dod = get_dod(&app, &mission_id).await;
    assert_eq!(dod["status"], "needs_review");
    assert_eq!(dod["needsReview"], 1);

    // Unknown mission is not found (workspace/entity access is respected).
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/missions/does-not-exist/dod")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// Fetches the Mission Control activity timeline, optionally filtered by `query`.
async fn get_activity(app: &axum::Router, query: &str) -> Vec<serde_json::Value> {
    let uri = if query.is_empty() {
        "/api/activity".to_string()
    } else {
        format!("/api/activity?{query}")
    };
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    json.as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn activity_timeline_merges_orders_filters_and_scopes() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Timeline Entity").await;

    // Three visible items with controlled timestamps (feed 10:00, run 11:00,
    // capture 12:00) plus one foreign-workspace capture that must be excluded.
    control
        .append_operator_feed_event(OperatorFeedEvent {
            id: "f1".to_string(),
            timestamp: "2026-07-04T10:00:00+00:00".to_string(),
            kind: OperatorFeedKind::TaskCompleted,
            task_id: Some("t1".to_string()),
            mission_id: None,
            entity_id: Some(entity_id.clone()),
            connection_id: None,
            title: "Task done".to_string(),
            detail: None,
            approval_id: None,
        })
        .await;
    {
        let mut runs = control.external_agent_runs.write().await;
        runs.push(ExternalAgentRun {
            id: "r1".to_string(),
            workspace_id: "default".to_string(),
            entity_id: entity_id.clone(),
            provider: "claude".to_string(),
            prompt: "do it".to_string(),
            cwd: "/tmp".to_string(),
            status: "completed".to_string(),
            task_id: None,
            output: String::new(),
            error: None,
            started_at: "2026-07-04T11:00:00+00:00".to_string(),
            completed_at: None,
            metadata: serde_json::Value::Null,
            review_policy: None,
            review_status: None,
            review_verdict: None,
            review_run_id: None,
            reviewed_run_id: None,
        });
    }
    {
        let mut caps = control.raw_captures.write().await;
        caps.push(RawCapture {
            id: "cap-visible".to_string(),
            workspace_id: "default".to_string(),
            entity_id: Some(entity_id.clone()),
            source: "chat".to_string(),
            source_ref: None,
            session_id: None,
            provider: None,
            raw_text: "captured thought".to_string(),
            summary: None,
            status: RawCaptureStatus::Raw,
            promoted_task_id: None,
            metadata: serde_json::Value::Null,
            created_at: "2026-07-04T12:00:00+00:00".to_string(),
            updated_at: "2026-07-04T12:00:00+00:00".to_string(),
        });
        // Foreign workspace — must never appear for the default-workspace caller.
        caps.push(RawCapture {
            id: "cap-foreign".to_string(),
            workspace_id: "tenant-b".to_string(),
            entity_id: None,
            source: "chat".to_string(),
            source_ref: None,
            session_id: None,
            provider: None,
            raw_text: "secret".to_string(),
            summary: None,
            status: RawCaptureStatus::Raw,
            promoted_task_id: None,
            metadata: serde_json::Value::Null,
            created_at: "2026-07-04T13:00:00+00:00".to_string(),
            updated_at: "2026-07-04T13:00:00+00:00".to_string(),
        });
    }

    // Merged, newest first, foreign capture excluded.
    let items = get_activity(&app, "").await;
    assert_eq!(items.len(), 3, "foreign-workspace capture must be excluded");
    assert_eq!(items[0]["source"], "capture");
    assert_eq!(items[0]["refId"], "cap-visible");
    assert_eq!(items[1]["source"], "agent_run");
    assert_eq!(items[2]["source"], "feed");
    assert!(!items.iter().any(|i| i["refId"] == "cap-foreign"));

    // Filter by source.
    let feed_only = get_activity(&app, "source=feed").await;
    assert_eq!(feed_only.len(), 1);
    assert_eq!(feed_only[0]["kind"], "task_completed");

    // Filter by kind.
    let by_kind = get_activity(&app, "kind=agent_run").await;
    assert_eq!(by_kind.len(), 1);
    assert_eq!(by_kind[0]["source"], "agent_run");

    // Filter by entity_id (all three share the entity).
    let by_entity = get_activity(&app, &format!("entity_id={entity_id}")).await;
    assert_eq!(by_entity.len(), 3);

    // Limit.
    let limited = get_activity(&app, "limit=2").await;
    assert_eq!(limited.len(), 2);
}

// ---------------------------------------------------------------------------
// Agent-run review and the mission goal loop.
// ---------------------------------------------------------------------------
/// Fetches a single external agent run for an entity.
async fn get_agent_run(app: &axum::Router, entity_id: &str, run_id: &str) -> serde_json::Value {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/entities/{entity_id}/agent-runs/{run_id}"))
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn agent_review_completed_run_gets_review_requested_and_attached() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Review Entity").await;

    // A completed primary run with auto policy and a linked task.
    let now = Utc::now().to_rfc3339();
    control.tasks.write().await.push(Task {
        id: "task-run".to_string(),
        title: "build the thing".to_string(),
        status: TaskStatus::Completed,
        entity_id: Some(entity_id.clone()),
        ..Task::default()
    });
    control
        .external_agent_runs
        .write()
        .await
        .push(ExternalAgentRun {
            id: "run-primary".to_string(),
            workspace_id: "default".to_string(),
            entity_id: entity_id.clone(),
            provider: "claude".to_string(),
            prompt: "build the thing".to_string(),
            cwd: "/tmp".to_string(),
            status: "completed".to_string(),
            task_id: Some("task-run".to_string()),
            output: "I built the thing".to_string(),
            error: None,
            started_at: now.clone(),
            completed_at: Some(now.clone()),
            metadata: serde_json::Value::Null,
            review_policy: Some("auto".to_string()),
            review_status: None,
            review_verdict: None,
            review_run_id: None,
            reviewed_run_id: None,
        });

    // Request the review (the deterministic core the run-completion path calls).
    let reviewer =
        consistent_world_gateway::agent_review::start_review(&control, "run-primary", "claude")
            .await
            .expect("reviewer run created");

    // The original run now reports a review in progress, surfaced via the API.
    let orig = get_agent_run(&app, &entity_id, "run-primary").await;
    assert_eq!(orig["reviewStatus"], "review_running");
    assert_eq!(orig["reviewRunId"].as_str(), Some(reviewer.id.as_str()));

    // The reviewer flags issues; the result is attached.
    consistent_world_gateway::agent_review::finish_review(
        &control,
        &reviewer.id,
        "VERDICT: NEEDS_REVIEW the output is incomplete",
        "completed",
    )
    .await;

    let orig = get_agent_run(&app, &entity_id, "run-primary").await;
    assert_eq!(orig["reviewStatus"], "needs_review");
    assert_eq!(orig["reviewVerdict"], "needs_review");

    let reviewer_run = get_agent_run(&app, &entity_id, &reviewer.id).await;
    assert_eq!(reviewer_run["status"], "completed");
    assert_eq!(reviewer_run["reviewedRunId"], "run-primary");

    // The linked task is flagged for review, and the reviewer output was captured.
    {
        let tasks = control.tasks.read().await;
        let t = tasks.iter().find(|t| t.id == "task-run").unwrap();
        assert_eq!(t.outcome, Some(TaskOutcome::NeedsReview));
    }
    {
        let caps = control.raw_captures.read().await;
        assert!(caps.iter().any(|c| c.source == "agent_review"));
    }
}

#[tokio::test]
async fn goal_loop_progresses_mission_and_surfaces_in_timeline() {
    use std::collections::HashMap;
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Loop Entity").await;
    let mission_id = create_mission_via_api(&app, &entity_id).await;
    // Clear the auto-created starter tasks for a deterministic single-task run.
    control
        .tasks
        .write()
        .await
        .retain(|t| t.mission_id.as_deref() != Some(mission_id.as_str()));
    control.tasks.write().await.push(Task {
        id: "loop-task".to_string(),
        title: "do the work".to_string(),
        status: TaskStatus::Queued,
        entity_id: Some(entity_id.clone()),
        mission_id: Some(mission_id.clone()),
        ..Task::default()
    });

    let mut last: HashMap<String, String> = HashMap::new();

    // Tick 1: a ready task → execute; mission stays active.
    let d1 = consistent_world_gateway::mission_goal_loop::tick(&control, &mut last).await;
    assert_eq!(
        d1[0].decision,
        consistent_world_gateway::mission_goal_loop::GoalDecision::Execute
    );

    // The task completes cleanly.
    {
        let mut tasks = control.tasks.write().await;
        let t = tasks.iter_mut().find(|t| t.id == "loop-task").unwrap();
        t.status = TaskStatus::Completed;
        t.outcome = Some(TaskOutcome::Completed);
    }

    // Tick 2: DoD met → the mission completes.
    let d2 = consistent_world_gateway::mission_goal_loop::tick(&control, &mut last).await;
    assert_eq!(
        d2[0].decision,
        consistent_world_gateway::mission_goal_loop::GoalDecision::Complete
    );
    {
        let missions = control.missions.read().await;
        let m = missions.iter().find(|m| m.id == mission_id).unwrap();
        assert_eq!(m.status, MissionStatus::Completed);
    }

    // The loop's decisions are recorded in the Mission Control activity timeline.
    let activity = get_activity(&app, &format!("mission_id={mission_id}")).await;
    let kinds: Vec<&str> = activity.iter().filter_map(|i| i["kind"].as_str()).collect();
    assert!(kinds.contains(&"goal_loop_decision"));
    assert!(kinds.contains(&"mission_completed"));
}

// ---------------------------------------------------------------------------
// Task/mission cancellation, mission reports, and shell verifications.
// ---------------------------------------------------------------------------
/// POSTs an empty JSON body to `uri` and returns `(status, body)`.
async fn post_json(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn cancel_and_report_endpoints() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Cancel Entity").await;
    let mission_id = create_mission_via_api(&app, &entity_id).await;

    // Cancel one starter task directly.
    let tid = {
        let tasks = control.tasks.read().await;
        tasks
            .iter()
            .find(|t| t.mission_id.as_deref() == Some(mission_id.as_str()))
            .unwrap()
            .id
            .clone()
    };
    let (status, task) = post_json(&app, &format!("/api/tasks/{tid}/cancel")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["status"], "cancelled");
    // Cancelling again is a conflict.
    let (again, _) = post_json(&app, &format!("/api/tasks/{tid}/cancel")).await;
    assert_eq!(again, StatusCode::CONFLICT);

    // Cancel the whole mission — it and its remaining tasks stop.
    let (status, mission) = post_json(&app, &format!("/api/missions/{mission_id}/cancel")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(mission["status"], "cancelled");
    {
        let tasks = control.tasks.read().await;
        assert!(
            tasks
                .iter()
                .filter(|t| t.mission_id.as_deref() == Some(mission_id.as_str()))
                .all(|t| t.status == TaskStatus::Cancelled)
        );
    }
    // A cancelled mission is ignored by the goal loop.
    {
        let mut last = std::collections::HashMap::new();
        let decisions =
            consistent_world_gateway::mission_goal_loop::tick(&control, &mut last).await;
        assert!(decisions.is_empty());
    }

    // Final report is available on demand.
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/missions/{mission_id}/report"))
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let report: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        report["report"]
            .as_str()
            .unwrap()
            .contains("Mission Report")
    );
    assert!(
        report["report"]
            .as_str()
            .unwrap()
            .contains("Cancelled tasks")
    );
}

/// POSTs to `/api/verifications` and returns `(status, body)`.
async fn post_verification(
    app: &axum::Router,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/verifications")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn shell_verification_pass_and_fail_transitions() {
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    // Shell verifications only run on a gateway that is the user's own
    // machine (local files wired), inside the entity's own folder.
    let work = TempDir::new().unwrap();
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone())
        .with_local_files(Arc::new(
            consistent_world_gateway::local_files::LocalFiles::new(work.path(), work.path()),
        ));
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Verify Entity").await;
    {
        let mut entities = control.entities.write().await;
        let entity = entities.iter_mut().find(|e| e.id == entity_id).unwrap();
        entity.local_path = Some(work.path().to_string_lossy().to_string());
    }
    control.tasks.write().await.push(Task {
        id: "task-v".to_string(),
        title: "ship it".to_string(),
        status: TaskStatus::Completed,
        entity_id: Some(entity_id.clone()),
        ..Task::default()
    });

    // A passing command: status passed, exit 0, no follow-up, task untouched.
    let (status, pass) = post_verification(
        &app,
        serde_json::json!({
            "kind": "shell",
            "command": "printf ok",
            "entity_id": entity_id,
            "task_id": "task-v",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pass["status"], "passed");
    assert_eq!(pass["exitCode"], 0);
    assert!(pass["followUpTaskId"].is_null());
    {
        let tasks = control.tasks.read().await;
        let t = tasks.iter().find(|t| t.id == "task-v").unwrap();
        assert_eq!(t.outcome, None);
    }

    // A failing command: status failed, task flagged NeedsReview, a follow-up
    // "next action" task created.
    let (status, fail) = post_verification(
        &app,
        serde_json::json!({
            "kind": "shell",
            "command": "echo bad 1>&2; exit 1",
            "entity_id": entity_id,
            "task_id": "task-v",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fail["status"], "failed");
    assert_eq!(fail["exitCode"], 1);
    let follow_up = fail["followUpTaskId"].as_str().expect("follow-up created");
    {
        let tasks = control.tasks.read().await;
        let t = tasks.iter().find(|t| t.id == "task-v").unwrap();
        assert_eq!(t.outcome, Some(TaskOutcome::NeedsReview));
        assert!(tasks.iter().any(|t| t.id == follow_up));
    }

    // Verifications surface in the Mission Control activity timeline.
    let activity = get_activity(&app, "source=verification").await;
    assert_eq!(activity.len(), 2);
    assert!(activity.iter().any(|i| i["status"] == "failed"));
    assert!(activity.iter().all(|i| i["source"] == "verification"));

    // Listing is filterable by status.
    let list_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/verifications?status=failed")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_res.status(), StatusCode::OK);
    let bytes = list_res.into_body().collect().await.unwrap().to_bytes();
    let listed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);

    // Missing command for a shell verification is a bad request.
    let (bad, _) = post_verification(&app, serde_json::json!({ "kind": "shell" })).await;
    assert_eq!(bad, StatusCode::BAD_REQUEST);

    // A shell verification runs in the entity's own folder by default…
    let (status, cwd_check) = post_verification(
        &app,
        serde_json::json!({
            "kind": "shell",
            "command": "pwd",
            "entity_id": entity_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let root = work.path().canonicalize().unwrap();
    assert_eq!(
        cwd_check["logs"].as_str().unwrap().trim(),
        root.to_string_lossy()
    );

    // …and a request `cwd` may narrow inside it, never step outside it. (It
    // used to win outright, which made `cwd: "/"` a shell anywhere on disk.)
    let (status, _) = post_verification(
        &app,
        serde_json::json!({
            "kind": "shell",
            "command": "pwd",
            "entity_id": entity_id,
            "cwd": "/",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An entity-less shell run has no folder to be jailed in.
    let (status, _) = post_verification(
        &app,
        serde_json::json!({ "kind": "shell", "command": "pwd" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The linked task must be the caller's: an unknown id is refused before
    // anything runs.
    let (status, _) = post_verification(
        &app,
        serde_json::json!({
            "kind": "shell",
            "command": "exit 1",
            "entity_id": entity_id,
            "task_id": "someone-elses-task",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn shell_verification_is_refused_off_the_users_own_machine() {
    // No local files: a network-bound (hosted) gateway. `sh -c` there is remote
    // code execution for anyone who can sign up, so it is refused outright.
    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone());
    let app = server::build_router(state);
    let entity_id = create_review_entity(&app, "Hosted Entity").await;

    let (status, _) = post_verification(
        &app,
        serde_json::json!({
            "kind": "shell",
            "command": "touch /tmp/pwned",
            "entity_id": entity_id,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(control.verifications.read().await.is_empty(), "nothing ran");
}

#[tokio::test]
async fn orphaned_external_agent_runs_are_marked_stale() {
    let control = ControlState::new("ollama", "http://localhost:11434", "llama3.1");
    let now = Utc::now().to_rfc3339();
    let run_id = "run-stale-test".to_string();
    control
        .external_agent_runs
        .write()
        .await
        .push(ExternalAgentRun {
            id: run_id.clone(),
            workspace_id: "default".to_string(),
            entity_id: "entity-1".to_string(),
            provider: "claude".to_string(),
            prompt: "Do the work".to_string(),
            cwd: "/tmp".to_string(),
            status: "running".to_string(),
            task_id: Some("task-1".to_string()),
            output: String::new(),
            error: None,
            started_at: now.clone(),
            completed_at: None,
            metadata: serde_json::json!({}),
            review_policy: None,
            review_status: None,
            review_verdict: None,
            review_run_id: None,
            reviewed_run_id: None,
        });
    control.tasks.write().await.push(Task {
        id: "task-1".to_string(),
        title: "External run".to_string(),
        dept: "Engineering".to_string(),
        roles: vec!["External agent".to_string()],
        status: TaskStatus::InProgress,
        tries: "0".to_string(),
        progress: 5,
        entity_id: Some("entity-1".to_string()),
        mission_id: None,
        priority: Some("medium".to_string()),
        requires_human_approval: false,
        assigned_director_id: None,
        assigned_agent_id: None,
        connection_id: None,
        external_source: Some("claude".to_string()),
        external_id: Some(run_id),
        result: None,
        task_kind: Some("external-agent-run".to_string()),
        metadata: None,
        outcome: None,
        created_at: now.clone(),
        due_at: None,
        completed_at: None,
        status_changed_at: None,
        source_task_id: None,
    });

    control.mark_orphaned_external_agent_runs_stale().await;

    let runs = control.external_agent_runs.read().await;
    let run = runs.first().unwrap();
    assert_eq!(run.status, "stale");
    assert!(
        run.error
            .as_deref()
            .unwrap_or("")
            .contains("Gateway restarted")
    );
    assert!(run.completed_at.is_some());
    drop(runs);

    let tasks = control.tasks.read().await;
    let task = tasks.first().unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert_eq!(task.progress, 0);
    assert!(task.result.as_deref().unwrap_or("").contains("stale"));
}

// ---------------------------------------------------------------------------
// Live canvas (spatial brain map)
// ---------------------------------------------------------------------------

/// Builds a router wired with a canvas store and one seeded in-progress task.
async fn canvas_test_app() -> axum::Router {
    let sm = SurrealMemoryStore::connect_memory().await.unwrap();
    let db = sm.db();
    let memory: Arc<dyn MemoryStore> = Arc::new(sm);
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    control.tasks.write().await.push(Task {
        id: "task-live-1".into(),
        title: "Ship the canvas".into(),
        dept: "General".into(),
        status: TaskStatus::InProgress,
        ..Default::default()
    });
    let canvas = consistent_world_gateway::canvas_store::CanvasStore::new(db);
    canvas.init_schema().await.unwrap();
    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control)
        .with_canvas_store(canvas)
        .with_app_config(static_principals_config());
    server::build_router(state)
}

/// Sends an authenticated canvas request and returns `(status, parsed body)`.
async fn canvas_json(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", format!("Bearer {token}"));
    let body = match body {
        Some(v) => {
            builder = builder.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn canvas_personal_space_crud_and_cross_user_isolation() {
    let app = canvas_test_app().await;
    let alice = "secret.ws1.alice";
    let bob = "secret.ws1.bob";

    // Alice drops a note on her default (Personal Space) canvas.
    let (status, created) = canvas_json(
        &app,
        "POST",
        "/api/canvas/nodes",
        alice,
        Some(serde_json::json!({"kind": "note", "label": "big idea", "x": 10.0, "y": 20.0})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create node: {created}");
    let node_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["createdBy"], "alice");

    // Alice sees it; the canvas resolves to her personal space.
    let (status, canvas) = canvas_json(&app, "GET", "/api/canvas", alice, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(canvas["nodes"].as_array().unwrap().len(), 1);
    let space_id = canvas["entityId"].as_str().unwrap();
    assert!(space_id.starts_with("personal_"), "space: {space_id}");

    // Bob's default canvas is HIS personal space: empty, different id.
    let (status, bob_canvas) = canvas_json(&app, "GET", "/api/canvas", bob, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(bob_canvas["nodes"].as_array().unwrap().is_empty());
    assert_ne!(bob_canvas["entityId"], canvas["entityId"]);

    // Bob cannot read Alice's space by id, nor touch her node.
    let (status, _) = canvas_json(
        &app,
        "GET",
        &format!("/api/canvas?entity_id={space_id}"),
        bob,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = canvas_json(
        &app,
        "PATCH",
        &format!("/api/canvas/nodes/{node_id}"),
        bob,
        Some(serde_json::json!({"x": 0.0})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Alice drags the node; the move persists.
    let (status, moved) = canvas_json(
        &app,
        "PATCH",
        &format!("/api/canvas/nodes/{node_id}"),
        alice,
        Some(serde_json::json!({"x": 111.5, "y": -40.0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(moved["x"], 111.5);

    // Delete works and empties the canvas.
    let (status, _) = canvas_json(
        &app,
        "DELETE",
        &format!("/api/canvas/nodes/{node_id}"),
        alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, canvas) = canvas_json(&app, "GET", "/api/canvas", alice, None).await;
    assert!(canvas["nodes"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn canvas_live_task_node_hydrates_and_edges_cascade() {
    let app = canvas_test_app().await;
    let alice = "secret.default.alice";

    // A live task node mirrors the real task's title and status.
    let (status, task_node) = canvas_json(
        &app,
        "POST",
        "/api/canvas/nodes",
        alice,
        Some(serde_json::json!({"kind": "task", "refId": "task-live-1", "x": 0.0, "y": 0.0})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create live node: {task_node}");
    assert_eq!(task_node["live"]["title"], "Ship the canvas");
    assert_eq!(task_node["live"]["exists"], true);

    // Live kinds require a refId.
    let (status, _) = canvas_json(
        &app,
        "POST",
        "/api/canvas/nodes",
        alice,
        Some(serde_json::json!({"kind": "task", "x": 0.0, "y": 0.0})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Second node + edge between them.
    let (_, note) = canvas_json(
        &app,
        "POST",
        "/api/canvas/nodes",
        alice,
        Some(serde_json::json!({"kind": "text", "content": "do it today", "x": 5.0, "y": 5.0})),
    )
    .await;
    let (status, edge) = canvas_json(
        &app,
        "POST",
        "/api/canvas/edges",
        alice,
        Some(serde_json::json!({
            "fromNode": task_node["id"],
            "toNode": note["id"],
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create edge: {edge}");

    let (_, canvas) = canvas_json(&app, "GET", "/api/canvas", alice, None).await;
    assert_eq!(canvas["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(canvas["edges"].as_array().unwrap().len(), 1);

    // Deleting a node takes its edges with it.
    let (status, _) = canvas_json(
        &app,
        "DELETE",
        &format!("/api/canvas/nodes/{}", task_node["id"].as_str().unwrap()),
        alice,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (_, canvas) = canvas_json(&app, "GET", "/api/canvas", alice, None).await;
    assert_eq!(canvas["nodes"].as_array().unwrap().len(), 1);
    assert!(canvas["edges"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn director_lessons_api_lists_and_deletes_with_isolation() {
    use consistent_world_gateway::director_lessons_store::SurrealDirectorLessonsStore;
    use consistent_world_gateway::state::DirectorLesson;

    let memory: Arc<dyn MemoryStore> =
        Arc::new(SurrealMemoryStore::connect_memory().await.unwrap());
    let db = surrealdb::engine::any::connect("memory").await.unwrap();
    db.use_ns("test").use_db("test").await.unwrap();
    let lessons = SurrealDirectorLessonsStore::new(Arc::new(db));
    lessons.init_schema().await.unwrap();

    let mk = |id: &str, director: &str, lesson: &str, at: &str| DirectorLesson {
        id: id.to_string(),
        director_id: director.to_string(),
        scope: "global".to_string(),
        lesson: lesson.to_string(),
        source: "correction".to_string(),
        severity: "soft".to_string(),
        embedding: None,
        created_at: at.to_string(),
        created_by: None,
    };
    lessons
        .append_lesson(&mk("les_a", "operations", "older", "2026-07-01T00:00:00Z"))
        .await
        .unwrap();
    lessons
        .append_lesson(&mk("les_b", "operations", "newer", "2026-07-02T00:00:00Z"))
        .await
        .unwrap();
    lessons
        .append_lesson(&mk(
            "les_x",
            "fullstack",
            "other director",
            "2026-07-03T00:00:00Z",
        ))
        .await
        .unwrap();

    let app = server::build_router(
        ServerState::new("test-version".into(), Some("secret".into()), memory)
            .with_director_lessons_store(lessons.clone()),
    );

    let get = |uri: String| {
        Request::builder()
            .uri(uri)
            .header("Authorization", "Bearer secret")
            .body(Body::empty())
            .unwrap()
    };

    // List: newest first, only this director, embedding omitted.
    let response = app
        .clone()
        .oneshot(get("/api/directors/operations/lessons".to_string()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let rows = json.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["id"], "les_b");
    assert_eq!(rows[1]["id"], "les_a");
    assert!(rows[0].get("embedding").is_none(), "raw vector not exposed");
    assert_eq!(rows[0]["has_embedding"], false);

    // Delete guarded by director ownership: fullstack's lesson 404s under operations.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/directors/operations/lessons/les_x")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    // Legitimate delete returns 204 and the row is gone.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/directors/operations/lessons/les_a")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let response = app
        .clone()
        .oneshot(get("/api/directors/operations/lessons".to_string()))
        .await
        .unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.as_array().unwrap().len(), 1);
    assert_eq!(json[0]["id"], "les_b");
}

/// Shipped mobile builds register at `POST /api/devices/register`, a route the
/// gateway never had — every install 405'd, so no push target was ever created.
/// The alias points at the same handler as `POST /api/devices`, with the same
/// body and the same `installId` idempotency key.
#[tokio::test]
async fn device_register_alias_hits_the_same_handler() {
    let surreal = SurrealMemoryStore::connect_memory().await.unwrap();
    let devices = consistent_world_gateway::device_store::SurrealDeviceStore::new(surreal.db());
    devices.init_schema().await.unwrap();
    let memory: Arc<dyn MemoryStore> = Arc::new(surreal);

    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));
    control.attach_device_store(devices);
    let state =
        ServerState::new("t".into(), Some("secret".into()), memory).with_control_state(control);
    let app = server::build_router(state);

    let register = |uri: &'static str, body: &'static str| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("Authorization", "Bearer secret")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };

    // Byte-for-byte the payload `mobile/src/lib/companion.ts` sends.
    let response = app
        .clone()
        .oneshot(register(
            "/api/devices/register",
            r#"{"platform":"ios","installId":"install-abc","label":"ios"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "the alias must route to register_device, not 405"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["platform"], "ios");
    assert_eq!(json["installId"], "install-abc");
    assert!(json.get("pushToken").is_none(), "poll-only device");
    let device_id = json["id"].as_str().unwrap().to_string();

    // Canonical route, same install id: updates that row rather than adding one.
    let response = app
        .clone()
        .oneshot(register(
            "/api/devices",
            r#"{"platform":"ios","installId":"install-abc","label":"iPhone","pushToken":"ExponentPushToken[xyz]"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["id"], device_id, "same install id keeps the same row");
    assert_eq!(json["pushToken"], "ExponentPushToken[xyz]");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/devices")
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json.as_array().unwrap().len(),
        1,
        "alias + canonical route must not create two devices"
    );
}

#[tokio::test]
async fn delete_entity_cascades_to_heartbeats_tasks_and_entity_rows() {
    use consistent_world_gateway::heartbeat_store::HeartbeatStore;

    let surreal = SurrealMemoryStore::connect_memory().await.unwrap();
    let db = surreal.db();
    let memory: Arc<dyn MemoryStore> = Arc::new(surreal);
    let control = Arc::new(ControlState::new(
        "openai",
        "https://api.openai.com/v1",
        "gpt-4o-mini",
    ));

    let entity_persister = entity_store::SurrealEntityStore::new(db.clone());
    entity_persister.init_schema().await.unwrap();
    control.attach_entity_store(entity_persister.clone());

    let heartbeat_persister = heartbeat_store::SurrealHeartbeatStore::new(db.clone());
    heartbeat_persister.init_schema().await.unwrap();
    control.attach_heartbeat_store(heartbeat_persister.clone());

    let task_persister = task_store::SurrealTaskStore::new(db.clone());
    control.attach_task_store(task_persister.clone());

    let state = ServerState::new("t".into(), Some("secret".into()), memory)
        .with_control_state(control.clone())
        .with_heartbeat_store(heartbeat_persister.clone());
    let app = server::build_router(state);

    let entity_id = create_review_entity(&app, "Doomed Entity").await;

    // Birth provisions default heartbeats targeting the entity.
    let beats = heartbeat_persister.list_all().await.unwrap();
    assert!(
        beats.iter().any(
            |b| b.target_kind == "entity" && b.target_id.as_deref() == Some(entity_id.as_str())
        ),
        "expected default heartbeats for the new entity"
    );

    // A task referencing the entity, written through to its own Surreal row.
    let create_task_payload = serde_json::json!({
        "title": "Task that must not survive",
        "dept": "General",
        "entity_id": entity_id,
    });
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/tasks")
                .header("Authorization", "Bearer secret")
                .header("content-type", "application/json")
                .body(Body::from(create_task_payload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        task_persister
            .list_all()
            .await
            .unwrap()
            .iter()
            .any(|t| t.entity_id.as_deref() == Some(entity_id.as_str())),
        "expected a write-through task row before the delete"
    );

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/entities/{entity_id}"))
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NO_CONTENT);

    // Orphaned heartbeats would keep firing forever; task rows would
    // merge-hydrate the task back on the next boot; a surviving entity row
    // would resurrect the entity itself.
    let beats = heartbeat_persister.list_all().await.unwrap();
    assert!(
        beats
            .iter()
            .all(|b| b.target_id.as_deref() != Some(entity_id.as_str())),
        "heartbeats targeting the deleted entity must be gone"
    );
    assert!(
        task_persister
            .list_all()
            .await
            .unwrap()
            .iter()
            .all(|t| t.entity_id.as_deref() != Some(entity_id.as_str())),
        "task rows referencing the deleted entity must be gone"
    );
    assert!(
        entity_persister
            .list_all()
            .await
            .unwrap()
            .iter()
            .all(|e| e.id != entity_id),
        "the entity's own Surreal row must be pruned immediately"
    );

    let res = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/entities/{entity_id}"))
                .header("Authorization", "Bearer secret")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}
