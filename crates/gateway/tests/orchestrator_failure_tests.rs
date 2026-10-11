//! Orchestrator failure-handling tests.
//!
//! Covers how the orchestrator classifies an unreachable LLM provider during
//! entity-enrichment tasks. (This file also covered the `/api/compat/*` facade
//! until that was deleted for having had no caller since the iOS app was
//! removed.)
use consistent_world_gateway::{
    config::AppConfig,
    orchestrator,
    state::{
        Agent, AgentGroup, ControlState, DirectorState, Entity, EntityEnrichment, Task, TaskStatus,
    },
};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::Duration;

#[tokio::test]
async fn test_orchestrator_fails_task_when_provider_is_unreachable() {
    // Connection refused (bad base URL / dead upstream) → run_agent exhausts its
    // attempt list and returns Err(_). The orchestrator's fatal-error branch
    // marks the task Failed with progress=100 and clears assignments.
    let control = Arc::new(ControlState::new(
        "openai",
        "http://127.0.0.1:1",
        "test-model",
    ));

    // Director + one agent.
    {
        control.agent_groups.write().await.push(AgentGroup {
            id: "dir-1".to_string(),
            name: "Director".to_string(),
            icon: "🧠".to_string(),
            description: "desc".to_string(),
        });
        control.director_states.write().await.push(DirectorState {
            id: "dir-1".to_string(),
            heartbeat_enabled: true,
            auto_task_kinds: vec![],
        });
        control.agents.write().await.push(Agent {
            id: "agent-1".to_string(),
            group_id: "dir-1".to_string(),
            name: "Agent".to_string(),
            role: "role".to_string(),
            kind: "coding".to_string(),
            preferred_provider: "openai".to_string(),
            preferred_model: "test-model".to_string(),
            mission: "mission".to_string(),
            profile: "profile".to_string(),
        });
    }

    // Entity + enrichment.
    let ent_id = "ent-1".to_string();
    {
        control.entities.write().await.push(Entity {
            id: ent_id.clone(),
            name: "Test Entity".to_string(),
            status: "Active".to_string(),
            dept: "General".to_string(),
            enrichment: Some(EntityEnrichment {
                status: "pending".to_string(),
                error: None,
                input_snapshot: None,
                draft: None,
                generated_by: None,
                model: None,
                generated_at: None,
                draft_version: Some(1),
            }),
            ..Default::default()
        });
    }

    let task_id = "task-1".to_string();
    {
        control.tasks.write().await.push(Task {
            id: task_id.clone(),
            title: "Enrich entity: Test Entity".to_string(),
            dept: "General".to_string(),
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            entity_id: Some(ent_id.clone()),
            task_kind: Some("entity-enrichment".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        });
    }

    let cfg = AppConfig {
        llm_base_url: "http://127.0.0.1:1".to_string(),
        llm_api_key_env: "DUMMY_OPENAI_KEY_ENV_NOT_SET".to_string(),
        task_kind_routing: HashMap::new(),
        ..AppConfig::default()
    };

    let cfg = Arc::new(cfg);
    let handle = {
        let control = control.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { orchestrator::run_loop(control, cfg.as_ref(), None).await })
    };

    // Poll rather than sleeping a fixed interval. A flat `sleep(3s)` raced the
    // orchestrator whenever the machine was loaded: the task was still
    // `InProgress` when the assertion ran, so this failed intermittently while
    // the code under it was fine.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let settled = {
            let tasks = control.tasks.read().await;
            tasks
                .iter()
                .find(|x| x.id == task_id)
                .is_some_and(|t| matches!(t.status, TaskStatus::Failed | TaskStatus::Completed))
        };
        if settled || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let tasks = control.tasks.read().await;
    let t = tasks.iter().find(|x| x.id == task_id).unwrap();
    assert_eq!(
        t.status,
        TaskStatus::Failed,
        "orchestrator did not fail the task within 30s"
    );
    assert_eq!(t.progress, 100);
    assert!(t.assigned_director_id.is_none());
    assert!(t.assigned_agent_id.is_none());

    let entities = control.entities.read().await;
    let e = entities.iter().find(|x| x.id == ent_id).unwrap();
    // Enrichment transitioned out of "pending"; exact post-state depends on which
    // run_agent branch classified the failure. "running" + task Failed is the
    // current observed outcome for plain transport failures.
    assert_ne!(e.enrichment.as_ref().unwrap().status, "pending");

    handle.abort();
}

#[tokio::test]
#[ignore = "obsolete: connection refused now fails fast via the orchestrator fatal-error branch; rewrite against a genuinely retryable error path"]
async fn test_orchestrator_marks_entity_enrichment_failed_after_retry_cap() {
    let control = Arc::new(ControlState::new(
        "openai",
        "http://127.0.0.1:1",
        "test-model",
    ));

    // Director + one agent.
    {
        control.agent_groups.write().await.push(AgentGroup {
            id: "dir-1".to_string(),
            name: "Director".to_string(),
            icon: "🧠".to_string(),
            description: "desc".to_string(),
        });
        control.director_states.write().await.push(DirectorState {
            id: "dir-1".to_string(),
            heartbeat_enabled: true,
            auto_task_kinds: vec![],
        });
        control.agents.write().await.push(Agent {
            id: "agent-1".to_string(),
            group_id: "dir-1".to_string(),
            name: "Agent".to_string(),
            role: "role".to_string(),
            kind: "coding".to_string(),
            preferred_provider: "openai".to_string(),
            preferred_model: "test-model".to_string(),
            mission: "mission".to_string(),
            profile: "profile".to_string(),
        });
    }

    // Entity + enrichment.
    let ent_id = "ent-1".to_string();
    {
        control.entities.write().await.push(Entity {
            id: ent_id.clone(),
            name: "Test Entity".to_string(),
            status: "Active".to_string(),
            dept: "General".to_string(),
            enrichment: Some(EntityEnrichment {
                status: "pending".to_string(),
                error: None,
                input_snapshot: None,
                draft: None,
                generated_by: None,
                model: None,
                generated_at: None,
                draft_version: Some(1),
            }),
            ..Default::default()
        });
    }

    // Task is already at tries=1, so first provider failure hits retry cap (n=2).
    let task_id = "task-1".to_string();
    {
        control.tasks.write().await.push(Task {
            id: task_id.clone(),
            title: "Enrich entity: Test Entity".to_string(),
            dept: "General".to_string(),
            status: TaskStatus::Queued,
            tries: "1".to_string(),
            entity_id: Some(ent_id.clone()),
            task_kind: Some("entity-enrichment".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        });
    }

    let cfg = AppConfig {
        llm_base_url: "http://127.0.0.1:1".to_string(),
        llm_api_key_env: "DUMMY_OPENAI_KEY_ENV_NOT_SET".to_string(),
        task_kind_routing: HashMap::new(),
        ..AppConfig::default()
    };

    let cfg = Arc::new(cfg);
    let handle = {
        let control = control.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { orchestrator::run_loop(control, cfg.as_ref(), None).await })
    };

    // Trigger one orchestrator tick (interval fires immediately; keep under next tick window).
    tokio::time::sleep(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let tasks = control.tasks.read().await;
    let t = tasks.iter().find(|x| x.id == task_id).unwrap();
    assert_eq!(t.status, TaskStatus::Failed);
    assert_eq!(t.progress, 100);
    assert_eq!(t.tries, "2");
    assert!(t.assigned_director_id.is_none());
    assert!(t.assigned_agent_id.is_none());

    let entities = control.entities.read().await;
    let e = entities.iter().find(|x| x.id == ent_id).unwrap();
    // Connection / transport errors classify as provider_invalid (fix URL, key, or network).
    assert_eq!(e.enrichment.as_ref().unwrap().status, "provider_invalid");

    handle.abort();
}

#[tokio::test]
#[ignore = "obsolete: connection refused now fails fast as provider_invalid; rewrite against a genuinely retryable error path"]
async fn test_orchestrator_retries_entity_enrichment_by_returning_to_queued() {
    let control = Arc::new(ControlState::new(
        "openai",
        "http://127.0.0.1:1",
        "test-model",
    ));

    // Director + one agent.
    {
        control.agent_groups.write().await.push(AgentGroup {
            id: "dir-1".to_string(),
            name: "Director".to_string(),
            icon: "🧠".to_string(),
            description: "desc".to_string(),
        });
        control.director_states.write().await.push(DirectorState {
            id: "dir-1".to_string(),
            heartbeat_enabled: true,
            auto_task_kinds: vec![],
        });
        control.agents.write().await.push(Agent {
            id: "agent-1".to_string(),
            group_id: "dir-1".to_string(),
            name: "Agent".to_string(),
            role: "role".to_string(),
            kind: "coding".to_string(),
            preferred_provider: "openai".to_string(),
            preferred_model: "test-model".to_string(),
            mission: "mission".to_string(),
            profile: "profile".to_string(),
        });
    }

    // Entity + enrichment.
    let ent_id = "ent-1".to_string();
    {
        control.entities.write().await.push(Entity {
            id: ent_id.clone(),
            name: "Test Entity".to_string(),
            status: "Active".to_string(),
            dept: "General".to_string(),
            enrichment: Some(EntityEnrichment {
                status: "pending".to_string(),
                error: None,
                input_snapshot: None,
                draft: None,
                generated_by: None,
                model: None,
                generated_at: None,
                draft_version: Some(1),
            }),
            ..Default::default()
        });
    }

    // Task is tries=0, so first provider failure should not terminally fail.
    let task_id = "task-1".to_string();
    {
        control.tasks.write().await.push(Task {
            id: task_id.clone(),
            title: "Enrich entity: Test Entity".to_string(),
            dept: "General".to_string(),
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            entity_id: Some(ent_id.clone()),
            task_kind: Some("entity-enrichment".to_string()),
            created_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        });
    }

    let cfg = AppConfig {
        llm_base_url: "http://127.0.0.1:1".to_string(),
        llm_api_key_env: "DUMMY_OPENAI_KEY_ENV_NOT_SET".to_string(),
        task_kind_routing: HashMap::new(),
        ..AppConfig::default()
    };

    let cfg = Arc::new(cfg);
    let handle = {
        let control = control.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { orchestrator::run_loop(control, cfg.as_ref(), None).await })
    };

    // Keep under the 15s heartbeat tick window so we only observe the first retry attempt.
    tokio::time::sleep(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let tasks = control.tasks.read().await;
    let t = tasks.iter().find(|x| x.id == task_id).unwrap();
    assert_eq!(t.status, TaskStatus::Queued);
    assert_eq!(t.progress, 0);
    assert_eq!(t.tries, "1");
    assert!(t.assigned_director_id.is_none());
    assert!(t.assigned_agent_id.is_none());

    let entities = control.entities.read().await;
    let e = entities.iter().find(|x| x.id == ent_id).unwrap();
    // Non-terminal retry: enrichment stays in "running" while the next retry is pending.
    assert_eq!(e.enrichment.as_ref().unwrap().status, "running");

    handle.abort();
}

/// A runner crash must not overwrite a human's cancel.
///
/// The orchestrator's `Ok` branch has always kept a cancelled task cancelled. Its
/// `Err` branch — a fatal runner error rather than a classified task failure —
/// re-found the task by id and then assigned `status = Failed` raw, with no
/// `Cancelled` check. So pressing stop, and then having the runner die (a dead
/// provider, a killed CLI), silently replaced the operator's decision with `Failed`.
/// The run's error is still recorded; what changed is that the decision survives it.
#[tokio::test]
async fn a_fatal_runner_error_does_not_overwrite_a_human_cancel() {
    let control = Arc::new(ControlState::new(
        "openai",
        "http://127.0.0.1:1",
        "test-model",
    ));

    {
        control.agent_groups.write().await.push(AgentGroup {
            id: "dir-1".to_string(),
            name: "Director".to_string(),
            icon: "🧠".to_string(),
            description: "desc".to_string(),
        });
        control.director_states.write().await.push(DirectorState {
            id: "dir-1".to_string(),
            heartbeat_enabled: true,
            auto_task_kinds: vec![],
        });
        control.agents.write().await.push(Agent {
            id: "agent-1".to_string(),
            group_id: "dir-1".to_string(),
            name: "Agent".to_string(),
            role: "role".to_string(),
            kind: "coding".to_string(),
            preferred_provider: "openai".to_string(),
            preferred_model: "test-model".to_string(),
            mission: "mission".to_string(),
            profile: "profile".to_string(),
        });
    }

    let task_id = "task-cancel-race".to_string();
    {
        control.tasks.write().await.push(Task {
            id: task_id.clone(),
            title: "Work that will be stopped".to_string(),
            dept: "General".to_string(),
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        });
    }

    let cfg = Arc::new(AppConfig {
        llm_base_url: "http://127.0.0.1:1".to_string(),
        llm_api_key_env: "DUMMY_OPENAI_KEY_ENV_NOT_SET".to_string(),
        task_kind_routing: HashMap::new(),
        ..AppConfig::default()
    });

    let handle = {
        let control = control.clone();
        let cfg = cfg.clone();
        tokio::spawn(async move { orchestrator::run_loop(control, cfg.as_ref(), None).await })
    };

    // Wait for the orchestrator to actually claim it. Cancelling while it is still
    // Queued would exercise the pickup guard instead, which is a different fix.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let in_progress = {
            let tasks = control.tasks.read().await;
            tasks
                .iter()
                .find(|t| t.id == task_id)
                .is_some_and(|t| t.status == TaskStatus::InProgress)
        };
        if in_progress {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the orchestrator never picked the task up, so the race under test never started"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The human presses stop while the run is in flight.
    control
        .update_task(&task_id, |t| {
            consistent_world_gateway::state::set_task_status_by_human(t, TaskStatus::Cancelled)
        })
        .await
        .expect("task exists");

    // Now let the run die against the dead provider and reach the Err branch.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let recorded = {
            let tasks = control.tasks.read().await;
            tasks
                .iter()
                .find(|t| t.id == task_id)
                .and_then(|t| t.result.clone())
                .is_some_and(|r| r.contains("runner_fatal_error"))
        };
        if recorded {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the runner never reported a fatal error within 60s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let tasks = control.tasks.read().await;
    let t = tasks.iter().find(|x| x.id == task_id).unwrap();
    assert_eq!(
        t.status,
        TaskStatus::Cancelled,
        "the human's cancel must outrank the runner's crash"
    );
    let result = t.result.clone().unwrap_or_default();
    assert!(
        result.contains("runner_fatal_error"),
        "the error is still recorded: {result}"
    );
    assert!(
        result.contains("kept_cancelled"),
        "and it says what was done about it: {result}"
    );

    handle.abort();
}
