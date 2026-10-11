//! Phase 5: Queue vs Execute routing rules based on entity readiness.
//! Operations is the default queue steward.
//!
//! [`make_routing_decision`] is the entry point: given an intake classification
//! and (optionally) the target entity, it decides whether to execute
//! immediately through a director, queue the item for later, or send it to a
//! human for review.

use crate::director_schemas::{check_director_readiness, operations_director_schema};
use crate::intake_policy::{IntakeClassification, IntakeDecision};
use crate::state::{Entity, Task};
use serde::{Deserialize, Serialize};

/// Routing decision: queue or execute
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingDecision {
    /// Entity is ready, execute through director
    Execute {
        /// Director to execute through
        director_id: String,
        /// Task kind for routing
        task_kind: String,
        /// Confidence in this decision
        confidence: f32,
    },
    /// Entity not ready, queue for later
    Queue {
        /// Why it's being queued
        reason: String,
        /// What's missing
        missing_fields: Vec<String>,
        /// Current readiness percentage
        current_readiness: u8,
        /// Required readiness percentage
        required_readiness: u8,
        /// Suggested next step (e.g., onboarding question)
        suggested_action: String,
    },
    /// Needs human review before routing
    Review {
        /// Why it needs review
        reason: String,
        /// Ambiguities or conflicts
        ambiguities: Vec<String>,
    },
}

/// Routing request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingRequest {
    /// Entity ID (optional for global tasks)
    pub entity_id: Option<String>,
    /// Intake decision from classification
    pub intake_decision: IntakeDecision,
    /// Task kind (e.g., "note", "task", "reminder")
    pub task_kind: String,
    /// Workspace ID
    pub workspace_id: String,
}

/// Routing result with decision and metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingResult {
    /// The routing decision
    pub decision: RoutingDecision,
    /// Entity readiness (if entity specified)
    pub entity_readiness: Option<EntityReadinessSummary>,
    /// Director readiness checks (if entity specified)
    pub director_checks: Option<Vec<DirectorReadinessSummary>>,
    /// Reasoning for the decision
    pub reasoning: String,
}

/// Summary of entity readiness
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityReadinessSummary {
    pub entity_id: String,
    pub overall_percent: u8,
    pub label: String,
}

/// Summary of director readiness
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorReadinessSummary {
    pub director_id: String,
    pub is_ready: bool,
    pub readiness_percent: u8,
    pub minimum_percent: u8,
    pub missing_required: Vec<String>,
}

/// Make routing decision based on entity readiness and intake classification
pub fn make_routing_decision(request: &RoutingRequest, entity: Option<&Entity>) -> RoutingResult {
    // If no entity, default to queue with Operations
    let Some(ent) = entity else {
        return RoutingResult {
            decision: RoutingDecision::Queue {
                reason: "No entity specified for this task".to_string(),
                missing_fields: vec!["entity_id".to_string()],
                current_readiness: 0,
                required_readiness: 45,
                suggested_action: "Create or select an entity for this work".to_string(),
            },
            entity_readiness: None,
            director_checks: None,
            reasoning: "Task has no entity context - queuing for manual routing".to_string(),
        };
    };

    // Check Operations director readiness (default queue steward)
    let ops_schema = operations_director_schema();
    let ops_check = check_director_readiness(ent, &ops_schema);

    let entity_readiness_summary = EntityReadinessSummary {
        entity_id: ent.id.clone(),
        overall_percent: ent.readiness.percent,
        label: ent.readiness.label.clone(),
    };

    // Check all directors
    let director_checks = crate::director_schemas::check_all_directors(ent)
        .iter()
        .map(|c| DirectorReadinessSummary {
            director_id: c.director_id.to_string(),
            is_ready: c.is_ready,
            readiness_percent: c.readiness_percent,
            minimum_percent: c.minimum_percent,
            missing_required: c.missing_required.clone(),
        })
        .collect::<Vec<_>>();

    // Determine routing based on intake classification and readiness
    let (decision, reasoning) = match &request.intake_decision.classification {
        IntakeClassification::Note => {
            // Notes can always be queued (low barrier)
            (
                RoutingDecision::Queue {
                    reason: "Note captured - no immediate action required".to_string(),
                    missing_fields: vec![],
                    current_readiness: ent.readiness.percent,
                    required_readiness: 0,
                    suggested_action: "Note stored in entity memory".to_string(),
                },
                "Notes are always accepted - stored for future reference".to_string(),
            )
        }
        IntakeClassification::Task | IntakeClassification::Reminder => {
            if ops_check.is_ready {
                (
                    RoutingDecision::Execute {
                        director_id: "operations".to_string(),
                        task_kind: request.task_kind.clone(),
                        confidence: request.intake_decision.confidence,
                    },
                    format!(
                        "Entity readiness ({:.0}%) meets Operations threshold ({:.0}%) - executing",
                        ops_check.readiness_percent, ops_schema.minimum_readiness_percent
                    ),
                )
            } else if ops_check.can_execute_with_warnings {
                // Has required fields but missing recommended - can execute with warnings
                (
                    RoutingDecision::Execute {
                        director_id: "operations".to_string(),
                        task_kind: request.task_kind.clone(),
                        confidence: request.intake_decision.confidence * 0.8, // Lower confidence
                    },
                    format!(
                        "Entity has required fields but missing recommended fields - executing with warnings: {:?}",
                        ops_check.missing_recommended
                    ),
                )
            } else {
                // Not ready - queue with explanation
                (
                    RoutingDecision::Queue {
                        reason: "Entity not ready for execution".to_string(),
                        missing_fields: ops_check.missing_required.clone(),
                        current_readiness: ops_check.readiness_percent,
                        required_readiness: ops_schema.minimum_readiness_percent,
                        suggested_action: format!(
                            "Complete onboarding: answer questions about {}",
                            ops_check.missing_required.join(", ")
                        ),
                    },
                    format!(
                        "Entity readiness ({:.0}%) below Operations threshold ({:.0}%) - queuing",
                        ops_check.readiness_percent, ops_schema.minimum_readiness_percent
                    ),
                )
            }
        }
        IntakeClassification::Mission => {
            // Missions need higher readiness - check both director readiness and overall entity readiness
            let effective_readiness = ops_check.readiness_percent.min(ent.readiness.percent);
            if effective_readiness >= 60 {
                (
                    RoutingDecision::Execute {
                        director_id: "operations".to_string(),
                        task_kind: "mission".to_string(),
                        confidence: request.intake_decision.confidence,
                    },
                    "Mission complexity justified by entity maturity - executing".to_string(),
                )
            } else {
                (
                    RoutingDecision::Queue {
                        reason: "Entity not mature enough for mission-level work".to_string(),
                        missing_fields: ops_check.missing_required.clone(),
                        current_readiness: ops_check.readiness_percent,
                        required_readiness: 60,
                        suggested_action: "Start with simpler tasks to build entity context"
                            .to_string(),
                    },
                    "Missions require 60%+ readiness - entity not yet mature enough".to_string(),
                )
            }
        }
        IntakeClassification::FollowUp {
            parent_id,
            parent_type,
        } => {
            // Follow-ups inherit parent context - check if parent exists
            (
                RoutingDecision::Execute {
                    director_id: "operations".to_string(),
                    task_kind: "follow-up".to_string(),
                    confidence: 0.85,
                },
                format!(
                    "Follow-up to {} {} - executing in same context",
                    parent_type, parent_id
                ),
            )
        }
        IntakeClassification::Update {
            target_id,
            target_type,
        } => {
            // Updates target existing work
            (
                RoutingDecision::Execute {
                    director_id: "operations".to_string(),
                    task_kind: "update".to_string(),
                    confidence: 0.85,
                },
                format!("Update to {} {} - executing", target_type, target_id),
            )
        }
        IntakeClassification::Clarify { reason, .. } => {
            // Needs clarification - queue for human review
            (
                RoutingDecision::Review {
                    reason: reason.clone(),
                    ambiguities: vec![reason.clone()],
                },
                "Intake ambiguous - needs human clarification".to_string(),
            )
        }
    };

    RoutingResult {
        decision,
        entity_readiness: Some(entity_readiness_summary),
        director_checks: Some(director_checks),
        reasoning,
    }
}

/// Check if a task should be auto-executed or queued based on entity readiness
pub fn should_auto_execute_task(task: &Task, entity: Option<&Entity>) -> bool {
    let Some(ent) = entity else {
        return false; // No entity = queue
    };

    // Check if task requires human approval
    if task.requires_human_approval {
        return false;
    }

    // Check entity readiness for Operations
    let ops_schema = operations_director_schema();
    let ops_check = check_director_readiness(ent, &ops_schema);

    // Auto-execute if ready
    ops_check.is_ready || ops_check.can_execute_with_warnings
}

/// Get queue explanation for a task (why it's not executing)
pub fn get_queue_explanation(task: &Task, entity: Option<&Entity>) -> String {
    let Some(ent) = entity else {
        return "No entity context - waiting for manual routing".to_string();
    };

    if task.requires_human_approval {
        return "Task marked for human approval - waiting for review".to_string();
    }

    let ops_schema = operations_director_schema();
    let ops_check = check_director_readiness(ent, &ops_schema);

    if !ops_check.is_ready && !ops_check.can_execute_with_warnings {
        return format!(
            "Entity readiness ({:.0}%) below threshold ({:.0}%). Missing: {}",
            ops_check.readiness_percent,
            ops_schema.minimum_readiness_percent,
            ops_check.missing_required.join(", ")
        );
    }

    "In queue - will execute when director available".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Entity;

    fn minimal_entity() -> Entity {
        Entity {
            id: "test-1".to_string(),
            name: "Test".to_string(),
            status: "Active".to_string(),
            dept: "General".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn queues_when_no_entity() {
        let req = RoutingRequest {
            entity_id: None,
            intake_decision: IntakeDecision {
                classification: IntakeClassification::Task,
                confidence: 0.9,
                suggested_director: Some("operations".to_string()),
                reasoning: "Test".to_string(),
                duplicate_warnings: vec![],
            },
            task_kind: "task".to_string(),
            workspace_id: "default".to_string(),
        };

        let result = make_routing_decision(&req, None);
        assert!(matches!(result.decision, RoutingDecision::Queue { .. }));
    }

    #[test]
    fn executes_when_entity_ready() {
        let mut entity = minimal_entity();
        entity.setup.what_it_is = Some("A test entity".to_string());
        entity.setup.primary_goal = Some("Test goal".to_string());
        entity.stage = Some("prototype".to_string());
        entity.readiness.percent = 55;
        entity.readiness.label = "basic setup".to_string();

        let req = RoutingRequest {
            entity_id: Some(entity.id.clone()),
            intake_decision: IntakeDecision {
                classification: IntakeClassification::Task,
                confidence: 0.9,
                suggested_director: Some("operations".to_string()),
                reasoning: "Test".to_string(),
                duplicate_warnings: vec![],
            },
            task_kind: "task".to_string(),
            workspace_id: "default".to_string(),
        };

        let result = make_routing_decision(&req, Some(&entity));
        assert!(matches!(result.decision, RoutingDecision::Execute { .. }));
    }

    #[test]
    fn queues_when_entity_not_ready() {
        let entity = minimal_entity(); // No setup fields

        let req = RoutingRequest {
            entity_id: Some(entity.id.clone()),
            intake_decision: IntakeDecision {
                classification: IntakeClassification::Task,
                confidence: 0.9,
                suggested_director: Some("operations".to_string()),
                reasoning: "Test".to_string(),
                duplicate_warnings: vec![],
            },
            task_kind: "task".to_string(),
            workspace_id: "default".to_string(),
        };

        let result = make_routing_decision(&req, Some(&entity));
        assert!(matches!(result.decision, RoutingDecision::Queue { .. }));
    }

    #[test]
    fn notes_always_queue() {
        let mut entity = minimal_entity();
        entity.setup.what_it_is = Some("Test".to_string());
        entity.setup.primary_goal = Some("Goal".to_string());
        entity.stage = Some("prototype".to_string());

        let req = RoutingRequest {
            entity_id: Some(entity.id.clone()),
            intake_decision: IntakeDecision {
                classification: IntakeClassification::Note,
                confidence: 0.9,
                suggested_director: Some("operations".to_string()),
                reasoning: "Test".to_string(),
                duplicate_warnings: vec![],
            },
            task_kind: "note".to_string(),
            workspace_id: "default".to_string(),
        };

        let result = make_routing_decision(&req, Some(&entity));
        assert!(matches!(result.decision, RoutingDecision::Queue { .. }));
    }

    #[test]
    fn missions_need_higher_readiness() {
        let mut entity = minimal_entity();
        entity.setup.what_it_is = Some("Test".to_string());
        entity.setup.primary_goal = Some("Goal".to_string());
        entity.stage = Some("prototype".to_string());
        // Entity has required fields but low overall readiness
        entity.readiness.percent = 50; // Below 60% mission threshold
        entity.readiness.label = "basic setup".to_string();

        let req = RoutingRequest {
            entity_id: Some(entity.id.clone()),
            intake_decision: IntakeDecision {
                classification: IntakeClassification::Mission,
                confidence: 0.9,
                suggested_director: Some("operations".to_string()),
                reasoning: "Test".to_string(),
                duplicate_warnings: vec![],
            },
            task_kind: "mission".to_string(),
            workspace_id: "default".to_string(),
        };

        let result = make_routing_decision(&req, Some(&entity));
        // Should queue because effective_readiness (min of director and entity) < 60%
        assert!(matches!(result.decision, RoutingDecision::Queue { .. }));
    }
}
