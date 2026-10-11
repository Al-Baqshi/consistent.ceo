//! Perform an outward action (send email, publish a post, …) on behalf of an
//! entity, through whichever connection it has for that channel.
//!
//! Entry point is [`perform_entity_action`], called from the HTTP endpoint in
//! `control_api`. It resolves the entity's connection, runs the capability
//! through policy (deny / require-approval / allow), records the attempt, and
//! — only when policy allows it and an adapter actually exists for the
//! action — dispatches to `entity_adapters`.

use crate::config::GlobalPolicyLayer;
use crate::planner::{
    entity_action_requires_approval_lane, lane_for_entity, planner_decision_for_entity_action,
};
use crate::policy::{CapabilityPolicyDecision, evaluate_capability, merge_global_and_entity};
use crate::state::{
    ActionEvent, ActionOutcome, ApprovalRequest, ApprovalStatus, ControlState, EntityConnection,
    EntityConnectionEvent, EntityConnectionEventKind,
};
use chrono::Utc;
use consistent_world_common::capabilities::{CapabilityId, ExecutionLevel};
use consistent_world_common::types::ScopeType;
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

/// Everything needed to attempt one action on one entity's connection.
#[derive(Debug, Clone)]
pub struct PerformEntityActionInput {
    pub entity_id: String,
    pub action: String,
    pub channel: String,
    pub payload: serde_json::Value,
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
}

/// Outcome of a [`perform_entity_action`] call — never panics or errors, the
/// caller inspects `ok`/`status`/`message` instead.
#[derive(Debug, Clone)]
pub struct PerformEntityActionResult {
    pub ok: bool,
    pub status: String,
    pub message: String,
    pub connection_id: Option<String>,
    pub requires_approval: bool,
}

// Map an action name to the capability that gates it. Unrecognised actions
// fall back to the most conservative capability rather than skipping the
// policy check entirely.
fn action_to_capability(action: &str) -> CapabilityId {
    match action {
        "publish_post" => CapabilityId::PublishPublic,
        "send_email" => CapabilityId::MessageExternal,
        _ => CapabilityId::MessageInternal,
    }
}

/// Find the entity's connected integration for `input.channel` and, if one
/// exists, hand off to [`run_with_connection`] for the policy + dispatch work.
pub async fn perform_entity_action(
    control: &Arc<ControlState>,
    global_policy: &GlobalPolicyLayer,
    input: PerformEntityActionInput,
) -> PerformEntityActionResult {
    let connection = {
        let connections = control.entity_connections.read().await;
        connections
            .iter()
            .find(|c| {
                c.entity_id == input.entity_id
                    && c.provider.eq_ignore_ascii_case(input.channel.as_str())
                    && (c.status == "connected" || c.status == "limited")
            })
            .cloned()
    };
    let Some(connection) = connection else {
        return PerformEntityActionResult {
            ok: false,
            status: "connection_not_found".to_string(),
            message: format!(
                "No connected '{}' integration for this entity",
                input.channel
            ),
            connection_id: None,
            requires_approval: false,
        };
    };
    run_with_connection(control, global_policy, &connection, input).await
}

// The actual gate: evaluate policy for this capability, log the attempt,
// queue an approval if required, and dispatch to an adapter only when policy
// allows it and one exists — otherwise report the truth instead of a fake
// success (see the `not_implemented` branch below).
async fn run_with_connection(
    control: &Arc<ControlState>,
    global_policy: &GlobalPolicyLayer,
    connection: &EntityConnection,
    input: PerformEntityActionInput,
) -> PerformEntityActionResult {
    let capability = action_to_capability(&input.action);
    let entities = control.entities.read().await;
    let Some(entity) = entities.iter().find(|e| e.id == input.entity_id) else {
        return PerformEntityActionResult {
            ok: false,
            status: "entity_not_found".to_string(),
            message: "Entity not found".to_string(),
            connection_id: Some(connection.id.clone()),
            requires_approval: false,
        };
    };

    let lane = lane_for_entity(Some(entity));
    let planner_note =
        planner_decision_for_entity_action(lane, &input.action, global_policy, entity, capability);
    debug!(detail = %planner_note.detail_str(), "perform_entity_action: planner");

    let merged = merge_global_and_entity(global_policy, entity.execution_policies.as_ref());
    let mut requires_approval = false;
    match evaluate_capability(&merged, capability) {
        CapabilityPolicyDecision::Deny(reason) => {
            control
                .append_entity_connection_event(EntityConnectionEvent {
                    id: Uuid::new_v4().to_string(),
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    entity_id: input.entity_id.clone(),
                    connection_id: connection.id.clone(),
                    kind: EntityConnectionEventKind::ActionBlockedByPolicy,
                    detail: Some(format!("action={} blocked: {}", input.action, reason)),
                })
                .await;
            return PerformEntityActionResult {
                ok: false,
                status: "blocked_by_policy".to_string(),
                message: "Blocked by execution policy".to_string(),
                connection_id: Some(connection.id.clone()),
                requires_approval: false,
            };
        }
        CapabilityPolicyDecision::RequireApproval => {
            requires_approval = true;
        }
        CapabilityPolicyDecision::Allow => {}
    }
    requires_approval =
        entity_action_requires_approval_lane(lane, &input.action, requires_approval);
    drop(entities);

    // `verify_ownership` (domain check) doesn't need a stored secret — it
    // proves control of a domain over HTTP, not an authenticated API call.
    if connection.secret_ref.is_none() && input.action != "verify_ownership" {
        return PerformEntityActionResult {
            ok: false,
            status: "missing_secret".to_string(),
            message: "Connection is missing secret_ref; reconnect required".to_string(),
            connection_id: Some(connection.id.clone()),
            requires_approval: false,
        };
    }

    let details = format!(
        "action={} provider={} channel={} payload_keys={}",
        input.action,
        connection.provider,
        input.channel,
        input
            .payload
            .as_object()
            .map(|m| m.keys().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or_default()
    );
    control
        .append_action_event(ActionEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            scope_type: ScopeType::Entity,
            scope_id: input.entity_id.clone(),
            entity_id: Some(input.entity_id.clone()),
            task_id: input.task_id.clone(),
            agent_id: input.agent_id.clone(),
            capability,
            execution_level: if requires_approval {
                ExecutionLevel::Coordinate
            } else {
                ExecutionLevel::Draft
            },
            target: Some(format!("entity_connection:{}", connection.id)),
            outcome: if requires_approval {
                ActionOutcome::ApprovalRequired
            } else {
                ActionOutcome::Allowed
            },
            reason: Some(details.clone()),
        })
        .await;
    control
        .append_entity_connection_event(EntityConnectionEvent {
            id: Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            entity_id: input.entity_id.clone(),
            connection_id: connection.id.clone(),
            kind: EntityConnectionEventKind::ActionAttempted,
            detail: Some(details),
        })
        .await;

    if requires_approval {
        let now = Utc::now().to_rfc3339();
        let approval = ApprovalRequest {
            id: Uuid::new_v4().to_string(),
            created_at: now.clone(),
            updated_at: now,
            entity_id: input.entity_id.clone(),
            connection_id: Some(connection.id.clone()),
            task_id: input.task_id.clone(),
            action: input.action.clone(),
            status: ApprovalStatus::Pending,
            detail: Some(
                serde_json::to_string(&input.payload).unwrap_or_else(|_| "(payload)".to_string()),
            ),
            created_by: input.agent_id.clone(),
        };
        control.enqueue_approval_request(approval).await;
    }

    if !requires_approval && action_has_adapter(&input.action, &connection.kind) {
        let sec = crate::credentials_resolver::read_entity_connection_secret(&connection.id);
        let sub = input
            .payload
            .get("subject")
            .and_then(|v| v.as_str())
            .unwrap_or("(no subject)");
        let body_txt = input
            .payload
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if let Err(e) = crate::entity_adapters::send_email_via_adapter(
            sec.as_deref(),
            &connection.config,
            sub,
            body_txt,
        )
        .await
        {
            return PerformEntityActionResult {
                ok: false,
                status: "send_failed".to_string(),
                message: crate::entity_adapters::user_facing_adapter_error(&e.to_string()),
                connection_id: Some(connection.id.clone()),
                requires_approval: false,
            };
        }
    }

    if requires_approval {
        return PerformEntityActionResult {
            ok: false,
            status: "approval_required".to_string(),
            message: "Action requires approval by policy".to_string(),
            connection_id: Some(connection.id.clone()),
            requires_approval,
        };
    }

    // Anything that reaches here without an adapter did *nothing*. Saying
    // "executed" anyway is the worst possible answer: the operator files the
    // work as done, the post never goes out, and the system's reports stop
    // meaning anything. Say plainly that nothing happened.
    if !action_has_adapter(&input.action, &connection.kind) {
        return PerformEntityActionResult {
            ok: false,
            status: "not_implemented".to_string(),
            message: format!(
                "No adapter is wired for '{}' on a '{}' connection yet, so nothing was sent. \
                 The request was recorded but not performed.",
                input.action, connection.provider
            ),
            connection_id: Some(connection.id.clone()),
            requires_approval: false,
        };
    }

    PerformEntityActionResult {
        ok: true,
        status: "executed".to_string(),
        message: "Action executed".to_string(),
        connection_id: Some(connection.id.clone()),
        requires_approval: false,
    }
}

/// Does an adapter actually exist behind this action?
///
/// One entry today — sending mail from an email account. Every other action is
/// a stub, and this function is the single place that admits it.
pub fn action_has_adapter(action: &str, connection_kind: &str) -> bool {
    matches!((action, connection_kind), ("send_email", "email_account"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Entity;

    fn control() -> Arc<ControlState> {
        Arc::new(ControlState::new(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
        ))
    }

    fn connection(kind: &str, provider: &str) -> EntityConnection {
        EntityConnection {
            id: "conn-1".to_string(),
            entity_id: "ent-1".to_string(),
            kind: kind.to_string(),
            provider: provider.to_string(),
            label: "Test".to_string(),
            status: "connected".to_string(),
            capability_state: serde_json::json!({}),
            config: serde_json::json!({}),
            secret_ref: Some("secret-1".to_string()),
            external_account_id: None,
            external_handle: None,
            scopes: Vec::new(),
            last_checked_at: None,
            last_error: None,
            created_by: None,
            created_at: "2026-08-01T00:00:00Z".to_string(),
            updated_at: "2026-08-01T00:00:00Z".to_string(),
        }
    }

    async fn seed(control: &Arc<ControlState>, conn: EntityConnection) {
        control.entities.write().await.push(Entity {
            id: "ent-1".to_string(),
            name: "Acme".to_string(),
            ..Entity::default()
        });
        control.entity_connections.write().await.push(conn);
    }

    fn input(action: &str, channel: &str) -> PerformEntityActionInput {
        PerformEntityActionInput {
            entity_id: "ent-1".to_string(),
            action: action.to_string(),
            channel: channel.to_string(),
            payload: serde_json::json!({}),
            task_id: None,
            agent_id: None,
        }
    }

    #[test]
    fn only_email_on_an_email_account_has_an_adapter_today() {
        assert!(action_has_adapter("send_email", "email_account"));
        assert!(!action_has_adapter("publish_post", "social_account"));
        assert!(!action_has_adapter("send_email", "social_account"));
        assert!(!action_has_adapter("verify_ownership", "domain"));
    }

    #[tokio::test]
    async fn an_action_with_no_adapter_says_so_instead_of_claiming_success() {
        // The old code reported "executed" for publish_post while doing
        // nothing at all — the operator's report said the post went out.
        let control = control();
        seed(&control, connection("social_account", "twitter")).await;
        let result = perform_entity_action(
            &control,
            &GlobalPolicyLayer::default(),
            input("publish_post", "twitter"),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.status, "not_implemented");
        assert!(result.message.contains("publish_post"));
        assert!(result.message.contains("nothing was sent"));
        assert!(!result.requires_approval);
    }

    #[tokio::test]
    async fn a_missing_integration_still_reports_the_missing_connection() {
        let control = control();
        control.entities.write().await.push(Entity {
            id: "ent-1".to_string(),
            name: "Acme".to_string(),
            ..Entity::default()
        });
        let result = perform_entity_action(
            &control,
            &GlobalPolicyLayer::default(),
            input("publish_post", "twitter"),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.status, "connection_not_found");
    }

    #[tokio::test]
    async fn the_approval_gate_still_comes_first_and_is_unchanged() {
        let control = control();
        seed(&control, connection("social_account", "twitter")).await;
        let policy = GlobalPolicyLayer {
            allowed_capabilities: Vec::new(),
            allowed_execution_levels: Vec::new(),
            approval_required_capabilities: vec![CapabilityId::PublishPublic],
        };
        let result =
            perform_entity_action(&control, &policy, input("publish_post", "twitter")).await;
        assert!(result.requires_approval);
        assert_eq!(result.status, "approval_required");
        assert!(!result.ok);
        // ...and the approval actually reached the queue.
        assert_eq!(control.approvals.read().await.len(), 1);
    }

    #[tokio::test]
    async fn a_send_email_attempt_reaches_the_adapter_rather_than_reporting_executed_blindly() {
        // No real credentials here, so the adapter fails — the point is that
        // this path *runs* an adapter instead of returning a fake success.
        let control = control();
        seed(&control, connection("email_account", "gmail")).await;
        let result = perform_entity_action(
            &control,
            &GlobalPolicyLayer::default(),
            input("send_email", "gmail"),
        )
        .await;
        assert_ne!(result.status, "not_implemented");
        assert!(matches!(result.status.as_str(), "send_failed" | "executed"));
    }
}
