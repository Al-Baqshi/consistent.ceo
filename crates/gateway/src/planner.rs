//! Thin routing hints for entity automation modes (Manual / Suggest / Auto-run).

use crate::config::GlobalPolicyLayer;
use crate::entity_runtime::effective_automation_mode;
use crate::policy::{CapabilityPolicyDecision, evaluate_capability, merge_global_and_entity};
use crate::state::{AutomationMode, Entity};
use consistent_world_common::capabilities::CapabilityId;

/// Which of the three automation lanes governs an entity's actions: answer
/// only, suggest work for a human to queue, or auto-execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlannerLane {
    AnswerOnly,
    SuggestWork,
    AutoExecute,
}

/// Map an entity's `AutomationMode` to its `PlannerLane`. No entity (or no
/// `entity_runtime` config) defaults to the most permissive lane, `AutoRun`.
pub fn lane_for_entity(entity: Option<&Entity>) -> PlannerLane {
    let mode = entity
        .map(effective_automation_mode)
        .unwrap_or(AutomationMode::AutoRun);
    match mode {
        AutomationMode::Manual => PlannerLane::AnswerOnly,
        AutomationMode::Suggest => PlannerLane::SuggestWork,
        AutomationMode::AutoRun => PlannerLane::AutoExecute,
    }
}

/// High-level outcome for logging, operator feed detail, or UI tooltips.
/// What the planner decided to do with a piece of work, plus a human-readable
/// `detail` explaining why — surfaced in logs, the operator feed, and UI
/// tooltips via [`Self::detail_str`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerDecision {
    /// Manual lane: no side-effecting action, just a response.
    AnswerOnly { detail: String },
    /// Produce draft output only; no side-effecting execution.
    DraftOnly { detail: String },
    /// Queue the work as a task for later execution rather than running it now.
    QueueTask { detail: String },
    /// Side-effecting action blocked pending human sign-off.
    RequireApproval { detail: String },
    /// Run the action now.
    Execute { detail: String },
    /// Hand off to a human / higher tier rather than retry or execute.
    Escalate { detail: String },
    /// Transient failure (e.g. timeout); worth retrying without escalating.
    RetryLater { detail: String },
    /// Needs human input before proceeding.
    DeferToUser { detail: String },
    /// Memory-informed path (e.g. task ordering, preamble injection).
    PreferMemoryContext { detail: String },
    /// Integration auth expired or revoked — reconnect or rotate secrets.
    ReconnectRequired { detail: String },
}

impl PlannerDecision {
    /// The `detail` string carried by whichever variant this is.
    pub fn detail_str(&self) -> &str {
        match self {
            PlannerDecision::AnswerOnly { detail }
            | PlannerDecision::DraftOnly { detail }
            | PlannerDecision::QueueTask { detail }
            | PlannerDecision::RequireApproval { detail }
            | PlannerDecision::Execute { detail }
            | PlannerDecision::Escalate { detail }
            | PlannerDecision::RetryLater { detail }
            | PlannerDecision::DeferToUser { detail }
            | PlannerDecision::PreferMemoryContext { detail }
            | PlannerDecision::ReconnectRequired { detail } => detail.as_str(),
        }
    }
}

/// Classify normalized adapter / connection errors for planner-style logging.
pub fn decision_from_connection_error_message(user_message: &str) -> PlannerDecision {
    let lower = user_message.to_lowercase();
    if lower.contains("reauth")
        || lower.contains("authentication failed")
        || lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("403")
    {
        PlannerDecision::ReconnectRequired {
            detail: user_message.to_string(),
        }
    } else if lower.contains("timeout") || lower.contains("timed out") {
        PlannerDecision::RetryLater {
            detail: user_message.to_string(),
        }
    } else {
        PlannerDecision::Escalate {
            detail: user_message.to_string(),
        }
    }
}

/// Log line when orchestrator picks a task using memory keyword bias.
pub fn heartbeat_memory_bias_decision(score: i32) -> PlannerDecision {
    PlannerDecision::PreferMemoryContext {
        detail: format!(
            "heartbeat task pick used short-memory keyword overlap (bias_score={score})"
        ),
    }
}

/// LLM tool calls are disabled in Manual (answer-only) lane for non-interpretation tasks.
pub fn allow_llm_tool_calls(lane: PlannerLane) -> bool {
    !matches!(lane, PlannerLane::AnswerOnly)
}

/// External / side-effecting entity actions (`send_email`, `publish_post`) need human approval in Manual and Suggest lanes.
pub fn entity_action_requires_approval_lane(
    lane: PlannerLane,
    action: &str,
    policy_already_requires: bool,
) -> bool {
    let external = matches!(action, "send_email" | "publish_post");
    if policy_already_requires {
        return true;
    }
    matches!(
        lane,
        PlannerLane::AnswerOnly | PlannerLane::SuggestWork if external
    )
}

/// Decide how a run should proceed given its lane and task kind.
/// `prompt-interpretation` tasks always route to the structured-brief queue
/// regardless of lane; everything else follows the lane's default decision.
pub fn planner_decision_for_run(
    lane: PlannerLane,
    tools_were_enabled: bool,
    task_kind: Option<&str>,
) -> PlannerDecision {
    if task_kind == Some("prompt-interpretation") {
        return PlannerDecision::QueueTask {
            detail: "interpretation lane: structured brief".to_string(),
        };
    }
    match lane {
        PlannerLane::AnswerOnly => PlannerDecision::AnswerOnly {
            detail: if !tools_were_enabled {
                "automation_mode=manual: tools disabled".to_string()
            } else {
                "automation_mode=manual".to_string()
            },
        },
        PlannerLane::SuggestWork => PlannerDecision::QueueTask {
            detail: "automation_mode=suggest: tools allowed; external actions need approval"
                .to_string(),
        },
        PlannerLane::AutoExecute => PlannerDecision::Execute {
            detail: "automation_mode=auto_run: tools and policy-gated execution".to_string(),
        },
    }
}

/// Decide how a specific entity action should proceed: merges global +
/// entity policy for `capability`, then combines the policy verdict with the
/// lane's own approval rule (see [`entity_action_requires_approval_lane`]).
pub fn planner_decision_for_entity_action(
    lane: PlannerLane,
    action: &str,
    global_policy: &GlobalPolicyLayer,
    entity: &Entity,
    capability: CapabilityId,
) -> PlannerDecision {
    let merged = merge_global_and_entity(global_policy, entity.execution_policies.as_ref());
    let policy_decision = evaluate_capability(&merged, capability);
    let policy_requires = matches!(policy_decision, CapabilityPolicyDecision::RequireApproval);
    let lane_requires = entity_action_requires_approval_lane(lane, action, policy_requires);
    if matches!(policy_decision, CapabilityPolicyDecision::Deny(_)) {
        return PlannerDecision::Escalate {
            detail: "capability denied by merged policy".to_string(),
        };
    }
    if lane_requires {
        PlannerDecision::RequireApproval {
            detail: format!("lane={lane:?} action={action} approval required"),
        }
    } else {
        PlannerDecision::Execute {
            detail: format!("lane={lane:?} action={action} execute under policy"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GlobalPolicyLayer;
    use crate::state::{Entity, EntityRuntimeConfig, ExecutionPolicies};
    use consistent_world_common::capabilities::{CapabilityId, ExecutionLevel};

    fn entity_with_mode(mode: AutomationMode) -> Entity {
        Entity {
            id: "e1".into(),
            name: "E".into(),
            status: "Active".into(),
            dept: "General".into(),
            execution_policies: Some(ExecutionPolicies {
                allowed_capabilities: vec![
                    CapabilityId::MessageExternal,
                    CapabilityId::PublishPublic,
                ],
                allowed_execution_levels: vec![ExecutionLevel::Draft, ExecutionLevel::Coordinate],
                approval_required_capabilities: vec![],
            }),
            entity_runtime: Some(EntityRuntimeConfig {
                automation_mode: mode,
                heartbeat_mode: Default::default(),
                default_chat_session_id: None,
                quiet_hours_local: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn lane_matches_automation_mode() {
        assert!(matches!(
            lane_for_entity(Some(&entity_with_mode(AutomationMode::Manual))),
            PlannerLane::AnswerOnly
        ));
        assert!(matches!(
            lane_for_entity(Some(&entity_with_mode(AutomationMode::Suggest))),
            PlannerLane::SuggestWork
        ));
        assert!(matches!(
            lane_for_entity(Some(&entity_with_mode(AutomationMode::AutoRun))),
            PlannerLane::AutoExecute
        ));
    }

    #[test]
    fn manual_disallows_tools() {
        assert!(!allow_llm_tool_calls(PlannerLane::AnswerOnly));
        assert!(allow_llm_tool_calls(PlannerLane::SuggestWork));
    }

    #[test]
    fn suggest_send_email_requires_approval_even_if_policy_allows() {
        let g = GlobalPolicyLayer::default();
        let e = entity_with_mode(AutomationMode::Suggest);
        let d = planner_decision_for_entity_action(
            PlannerLane::SuggestWork,
            "send_email",
            &g,
            &e,
            CapabilityId::MessageExternal,
        );
        assert!(matches!(d, PlannerDecision::RequireApproval { .. }));
    }

    #[test]
    fn auto_run_send_email_without_policy_approval_executes() {
        let g = GlobalPolicyLayer::default();
        let e = entity_with_mode(AutomationMode::AutoRun);
        let d = planner_decision_for_entity_action(
            PlannerLane::AutoExecute,
            "send_email",
            &g,
            &e,
            CapabilityId::MessageExternal,
        );
        assert!(matches!(d, PlannerDecision::Execute { .. }));
    }

    #[test]
    fn connection_error_maps_to_reconnect() {
        let d = decision_from_connection_error_message(
            "Authentication failed — reconnect or rotate credentials (reauth required).",
        );
        assert!(matches!(d, PlannerDecision::ReconnectRequired { .. }));
    }
}
