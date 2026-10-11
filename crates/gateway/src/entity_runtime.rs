//! Resolve `EntityRuntimeContext` and effective automation/heartbeat for orchestration.

use consistent_world_common::types::ScopeType;

use crate::state::{
    AutomationMode, Entity, EntityHeartbeatMode, EntityRuntimeContext, ExecutionContext,
    ResolvedScope,
};

/// Legacy entities (no `entity_runtime`) behave like full auto-run + execute heartbeat.
pub fn effective_automation_mode(entity: &Entity) -> AutomationMode {
    entity
        .entity_runtime
        .as_ref()
        .map(|r| r.automation_mode)
        .unwrap_or(AutomationMode::AutoRun)
}

// Same legacy-entity fallback as `effective_automation_mode`, for the
// heartbeat mode instead of the automation mode.
pub fn effective_heartbeat_mode(entity: &Entity) -> EntityHeartbeatMode {
    entity
        .entity_runtime
        .as_ref()
        .map(|r| r.heartbeat_mode)
        .unwrap_or(EntityHeartbeatMode::Execute)
}

/// Build execution context from entity (same rules as task-bound context without a task row).
pub fn build_execution_context_for_entity(entity: &Entity) -> ExecutionContext {
    let allowed_resource_roots: Vec<String> =
        entity.resources.iter().map(|r| r.root.clone()).collect();
    // Fallback chain: the operator's declared workspace root wins, then the
    // first allowed resource root, then the entity's own `path`.
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

    ExecutionContext {
        scope: ResolvedScope {
            scope_type: ScopeType::Entity,
            scope_id: entity.id.clone(),
            resolution_reason: Some("entity-runtime-context".to_string()),
        },
        entity_id: Some(entity.id.clone()),
        allowed_resource_roots,
        allowed_capabilities,
        allowed_execution_levels,
        approval_required_capabilities,
        effective_workspace,
        memory_scope: entity.memory_scope.clone(),
        preferred_model_tier: entity.preferred_model_tier.clone(),
    }
}

/// Full runtime context for an entity: automation/heartbeat mode, its bound
/// chat session (if any), and the derived [`ExecutionContext`].
pub fn resolve_entity_runtime_context(entity: &Entity) -> EntityRuntimeContext {
    let (automation_mode, heartbeat_mode, default_chat_session_id, quiet_hours_local) =
        match &entity.entity_runtime {
            Some(r) => (
                r.automation_mode,
                r.heartbeat_mode,
                r.default_chat_session_id.clone(),
                r.quiet_hours_local.clone(),
            ),
            None => (
                AutomationMode::AutoRun,
                EntityHeartbeatMode::Execute,
                None,
                None,
            ),
        };
    EntityRuntimeContext {
        entity_id: entity.id.clone(),
        automation_mode,
        heartbeat_mode,
        default_chat_session_id,
        quiet_hours_local,
        execution: build_execution_context_for_entity(entity),
    }
}

/// Skip automatic interpretation follow-up when entity is in manual mode.
pub fn should_suppress_interpretation_followup(entity: Option<&Entity>) -> bool {
    match entity {
        None => false,
        Some(e) => matches!(effective_automation_mode(e), AutomationMode::Manual),
    }
}

/// Heartbeat loop should not pick tasks for this entity.
pub fn entity_heartbeat_skips_task_pickup(entity: Option<&Entity>) -> bool {
    match entity {
        None => false,
        Some(e) => matches!(effective_heartbeat_mode(e), EntityHeartbeatMode::Off),
    }
}

/// If `session_id` matches an entity's `default_chat_session_id`, return that entity id (entity-native chat).
pub fn entity_id_for_bound_chat_session(entities: &[Entity], session_id: &str) -> Option<String> {
    if session_id.is_empty() {
        return None;
    }
    for e in entities {
        let Some(rt) = &e.entity_runtime else {
            continue;
        };
        let Some(ref bound) = rt.default_chat_session_id else {
            continue;
        };
        if bound == session_id {
            return Some(e.id.clone());
        }
    }
    None
}
