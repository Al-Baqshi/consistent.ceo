//! Director expectation schemas: what each director needs to execute work.
//! Operations-first: defines minimum readiness for task routing/execution.

use crate::state::Entity;
use serde::{Deserialize, Serialize};

/// Director identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectorId {
    Operations,
    FullStackDevelopment,
    ContentCreative,
    // Catch-all: any director id string not matching the variants above
    // deserializes to this instead of failing.
    #[serde(other)]
    Custom,
}

impl std::fmt::Display for DirectorId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DirectorId::Operations => write!(f, "operations"),
            DirectorId::FullStackDevelopment => write!(f, "full_stack_development"),
            DirectorId::ContentCreative => write!(f, "content_creative"),
            DirectorId::Custom => write!(f, "custom"),
        }
    }
}

/// Field requirement level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldRequirement {
    /// Required for any execution
    Required,
    /// Recommended but can infer or use defaults
    Recommended,
    /// Optional / nice to have
    Optional,
    /// Can be inferred from other fields
    Inferable,
}

/// A field that a director needs
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorField {
    /// Field path (e.g. "setup.what_it_is", "setup.primary_goal")
    pub field_path: String,
    /// Requirement level
    pub requirement: FieldRequirement,
    /// Human-readable description
    pub description: String,
    /// Why this field matters for this director
    pub rationale: String,
    /// Default value if not provided (for Recommended/Optional)
    pub default_value: Option<String>,
}

/// Director schema: what fields are needed to execute
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorSchema {
    /// Director identifier
    pub director_id: DirectorId,
    /// Display name
    pub name: String,
    /// Description of what this director does
    pub description: String,
    /// Required/recommended fields
    pub fields: Vec<DirectorField>,
    /// Minimum readiness percentage to execute
    pub minimum_readiness_percent: u8,
}

/// Readiness check result for a director
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DirectorReadinessCheck {
    /// Director being checked
    pub director_id: DirectorId,
    /// Whether entity is ready for this director
    pub is_ready: bool,
    /// Current readiness percentage
    pub readiness_percent: u8,
    /// Minimum required percentage
    pub minimum_percent: u8,
    /// Missing required fields
    pub missing_required: Vec<String>,
    /// Missing recommended fields
    pub missing_recommended: Vec<String>,
    /// Can execute with warnings (missing recommended but not required)
    pub can_execute_with_warnings: bool,
}

/// Get the Operations director schema
pub fn operations_director_schema() -> DirectorSchema {
    DirectorSchema {
        director_id: DirectorId::Operations,
        name: "Operations".to_string(),
        description: "Intake, routing, reminders, follow-ups, and queue management".to_string(),
        fields: vec![
            DirectorField {
                field_path: "setup.what_it_is".to_string(),
                requirement: FieldRequirement::Required,
                description: "What this entity is about".to_string(),
                rationale:
                    "Operations needs to understand the entity context to route work correctly"
                        .to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.primary_goal".to_string(),
                requirement: FieldRequirement::Required,
                description: "Main goal for this entity".to_string(),
                rationale: "Operations prioritizes work based on entity goals".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "stage".to_string(),
                requirement: FieldRequirement::Required,
                description: "Current stage (idea, prototype, beta, launched, etc.)".to_string(),
                rationale: "Stage determines what kind of operational work is appropriate"
                    .to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.success_criteria".to_string(),
                requirement: FieldRequirement::Recommended,
                description: "What success looks like".to_string(),
                rationale: "Helps Operations measure progress and prioritize follow-ups"
                    .to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.primary_domain".to_string(),
                requirement: FieldRequirement::Recommended,
                description: "Primary domain or website".to_string(),
                rationale: "Useful for external communications and monitoring".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.primary_email".to_string(),
                requirement: FieldRequirement::Recommended,
                description: "Primary contact email".to_string(),
                rationale: "Needed for email-based operations and notifications".to_string(),
                default_value: None,
            },
        ],
        minimum_readiness_percent: 45,
    }
}

/// Get the Full Stack Development director schema
pub fn fullstack_director_schema() -> DirectorSchema {
    DirectorSchema {
        director_id: DirectorId::FullStackDevelopment,
        name: "Full Stack Development".to_string(),
        description: "Code, architecture, deployments, and technical implementation".to_string(),
        fields: vec![
            DirectorField {
                field_path: "setup.what_it_is".to_string(),
                requirement: FieldRequirement::Required,
                description: "What this entity is about".to_string(),
                rationale: "Developers need context for architectural decisions".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.primary_goal".to_string(),
                requirement: FieldRequirement::Required,
                description: "Main technical goal".to_string(),
                rationale: "Guides implementation priorities".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.repo_path".to_string(),
                requirement: FieldRequirement::Required,
                description: "Codebase location (local path or Git URL)".to_string(),
                rationale: "Development requires access to the codebase".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.backend_stack".to_string(),
                requirement: FieldRequirement::Recommended,
                description: "Backend technology stack".to_string(),
                rationale: "Informs implementation approach and tooling".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.frontend_stack".to_string(),
                requirement: FieldRequirement::Recommended,
                description: "Frontend technology stack".to_string(),
                rationale: "Informs UI implementation approach".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.database_stack".to_string(),
                requirement: FieldRequirement::Recommended,
                description: "Database technology".to_string(),
                rationale: "Informs data layer decisions".to_string(),
                default_value: None,
            },
        ],
        minimum_readiness_percent: 55,
    }
}

/// Get the Content & Creative director schema
pub fn content_director_schema() -> DirectorSchema {
    DirectorSchema {
        director_id: DirectorId::ContentCreative,
        name: "Content & Creative".to_string(),
        description: "Drafts, copy, design, tone, and creative output".to_string(),
        fields: vec![
            DirectorField {
                field_path: "setup.what_it_is".to_string(),
                requirement: FieldRequirement::Required,
                description: "What this entity is about".to_string(),
                rationale: "Content must align with entity identity".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.primary_goal".to_string(),
                requirement: FieldRequirement::Required,
                description: "Main goal for this entity".to_string(),
                rationale: "Content supports entity goals".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "profile".to_string(),
                requirement: FieldRequirement::Recommended,
                description: "Entity profile/description".to_string(),
                rationale: "Informs tone, voice, and messaging".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "setup.primary_domain".to_string(),
                requirement: FieldRequirement::Recommended,
                description: "Primary domain or website".to_string(),
                rationale: "Content often published to domain".to_string(),
                default_value: None,
            },
            DirectorField {
                field_path: "goals".to_string(),
                requirement: FieldRequirement::Optional,
                description: "Detailed goals".to_string(),
                rationale: "Provides additional context for content strategy".to_string(),
                default_value: None,
            },
        ],
        minimum_readiness_percent: 40,
    }
}

/// Check if an entity is ready for a specific director
pub fn check_director_readiness(
    entity: &Entity,
    director_schema: &DirectorSchema,
) -> DirectorReadinessCheck {
    let mut missing_required = Vec::new();
    let mut missing_recommended = Vec::new();
    let mut readiness_score = 0u8;
    let mut total_weight = 0u8;

    for field in &director_schema.fields {
        // Required fields count for more of the readiness score than
        // recommended/optional/inferable ones, so missing a required field
        // drags the percentage down harder.
        let weight = match field.requirement {
            FieldRequirement::Required => 3,
            FieldRequirement::Recommended => 2,
            FieldRequirement::Optional => 1,
            FieldRequirement::Inferable => 1,
        };
        total_weight += weight;

        let has_value = field_has_value(entity, &field.field_path);

        if has_value {
            readiness_score += weight;
        } else {
            match field.requirement {
                FieldRequirement::Required => {
                    missing_required.push(field.field_path.clone());
                }
                FieldRequirement::Recommended => {
                    missing_recommended.push(field.field_path.clone());
                }
                _ => {}
            }
        }
    }

    let readiness_percent = if total_weight > 0 {
        ((readiness_score as f32 / total_weight as f32) * 100.0) as u8
    } else {
        0
    };

    let is_ready = missing_required.is_empty()
        && readiness_percent >= director_schema.minimum_readiness_percent;
    let can_execute_with_warnings = missing_required.is_empty() && !missing_recommended.is_empty();

    DirectorReadinessCheck {
        director_id: director_schema.director_id,
        is_ready,
        readiness_percent,
        minimum_percent: director_schema.minimum_readiness_percent,
        missing_required,
        missing_recommended,
        can_execute_with_warnings,
    }
}

/// Check if a specific field has a value in the entity
fn field_has_value(entity: &Entity, field_path: &str) -> bool {
    match field_path {
        "setup.what_it_is" => entity
            .setup
            .what_it_is
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "setup.primary_goal" => entity
            .setup
            .primary_goal
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "setup.success_criteria" => entity
            .setup
            .success_criteria
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "setup.primary_domain" => entity
            .setup
            .primary_domain
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "setup.primary_email" => entity
            .setup
            .primary_email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "setup.repo_path" => entity
            .setup
            .repo_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "setup.backend_stack" => entity
            .setup
            .backend_stack
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "setup.frontend_stack" => entity
            .setup
            .frontend_stack
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "setup.database_stack" => entity
            .setup
            .database_stack
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "stage" => entity
            .stage
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "profile" => entity
            .profile
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        "goals" => entity
            .goals
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_some(),
        _ => false,
    }
}

/// Get all director schemas
pub fn all_director_schemas() -> Vec<DirectorSchema> {
    vec![
        operations_director_schema(),
        fullstack_director_schema(),
        content_director_schema(),
    ]
}

/// Check entity readiness across all directors
pub fn check_all_directors(entity: &Entity) -> Vec<DirectorReadinessCheck> {
    all_director_schemas()
        .iter()
        .map(|schema| check_director_readiness(entity, schema))
        .collect()
}

/// Get the best director for an entity based on readiness
pub fn best_director_for_entity(entity: &Entity) -> Option<DirectorId> {
    let checks = check_all_directors(entity);
    checks
        .iter()
        .filter(|c| c.is_ready || c.can_execute_with_warnings)
        .max_by_key(|c| c.readiness_percent)
        .map(|c| c.director_id)
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
    fn operations_requires_three_fields() {
        let entity = minimal_entity();
        let schema = operations_director_schema();
        let check = check_director_readiness(&entity, &schema);

        assert!(!check.is_ready);
        assert!(
            check
                .missing_required
                .contains(&"setup.what_it_is".to_string())
        );
        assert!(
            check
                .missing_required
                .contains(&"setup.primary_goal".to_string())
        );
        assert!(check.missing_required.contains(&"stage".to_string()));
    }

    #[test]
    fn operations_ready_with_minimal_fields() {
        let mut entity = minimal_entity();
        entity.setup.what_it_is = Some("A test entity".to_string());
        entity.setup.primary_goal = Some("Test goal".to_string());
        entity.stage = Some("prototype".to_string());

        let schema = operations_director_schema();
        let check = check_director_readiness(&entity, &schema);

        assert!(check.is_ready);
        assert!(check.missing_required.is_empty());
        assert!(!check.missing_recommended.is_empty());
    }

    #[test]
    fn fullstack_needs_repo() {
        let mut entity = minimal_entity();
        entity.setup.what_it_is = Some("A test".to_string());
        entity.setup.primary_goal = Some("Goal".to_string());

        let schema = fullstack_director_schema();
        let check = check_director_readiness(&entity, &schema);

        assert!(!check.is_ready);
        assert!(
            check
                .missing_required
                .contains(&"setup.repo_path".to_string())
        );
    }

    #[test]
    fn best_director_selects_operations() {
        let mut entity = minimal_entity();
        entity.setup.what_it_is = Some("A test".to_string());
        entity.setup.primary_goal = Some("Goal".to_string());
        entity.stage = Some("prototype".to_string());

        let best = best_director_for_entity(&entity);
        assert_eq!(best, Some(DirectorId::Operations));
    }
}
