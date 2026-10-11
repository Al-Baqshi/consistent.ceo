//! Intake classification policy: classify input as note/task/reminder/mission and route correctly.
//! Operations is the default intake steward.

use crate::state::{Mission, MissionStatus, Task, TaskStatus};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Classification result from intake
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeClassification {
    /// Simple note (no action required, just stored)
    Note,
    /// Task with action required
    Task,
    /// Reminder with time component
    Reminder,
    /// Complex multi-step work
    Mission,
    /// Follow-up to existing work
    FollowUp {
        /// ID of task/mission to follow up on
        parent_id: String,
        /// Type: task or mission
        parent_type: String,
    },
    /// Update to existing work
    Update {
        /// ID of task/mission to update
        target_id: String,
        /// Type: task or mission
        target_type: String,
    },
    /// Ambiguous - needs clarification
    Clarify {
        /// Why it's ambiguous
        reason: String,
        /// Suggested clarifying question
        question: String,
    },
}

/// Intake request
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntakeRequest {
    /// Input text from user
    pub text: String,
    /// Optional entity context
    pub entity_id: Option<String>,
    /// Optional explicit intent hint (e.g. "note", "task")
    pub intent_hint: Option<String>,
    /// Workspace ID
    pub workspace_id: String,
}

/// Intake decision with routing info
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntakeDecision {
    /// Classification result
    pub classification: IntakeClassification,
    /// Confidence score (0-1)
    pub confidence: f32,
    /// Suggested director for routing
    pub suggested_director: Option<String>,
    /// Reasoning for the decision
    pub reasoning: String,
    /// Duplicate warnings if any
    pub duplicate_warnings: Vec<DuplicateWarning>,
}

/// Duplicate warning
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DuplicateWarning {
    /// ID of similar existing item
    pub existing_id: String,
    /// Type: task, mission, reminder
    pub item_type: String,
    /// Title of existing item
    pub existing_title: String,
    /// Similarity score (0-1)
    pub similarity: f32,
    /// Why it's considered a duplicate
    pub reason: String,
}

/// Check for duplicate tasks based on title similarity and entity
pub fn check_duplicate_tasks(
    new_title: &str,
    entity_id: Option<&str>,
    existing_tasks: &[Task],
    similarity_threshold: f32,
) -> Vec<DuplicateWarning> {
    let mut duplicates = Vec::new();
    let new_title_normalized = normalize_text(new_title);

    for task in existing_tasks {
        // Skip completed tasks
        if task.status == TaskStatus::Completed {
            continue;
        }

        // Check entity match
        if let Some(eid) = entity_id
            && task.entity_id.as_deref() != Some(eid)
        {
            continue;
        }

        let existing_title_normalized = normalize_text(&task.title);
        let similarity = text_similarity(&new_title_normalized, &existing_title_normalized);

        if similarity >= similarity_threshold {
            duplicates.push(DuplicateWarning {
                existing_id: task.id.clone(),
                item_type: "task".to_string(),
                existing_title: task.title.clone(),
                similarity,
                reason: format!("Title similarity: {:.0}%", similarity * 100.0),
            });
        }
    }

    duplicates
}

/// Check for duplicate missions
pub fn check_duplicate_missions(
    new_title: &str,
    entity_id: Option<&str>,
    existing_missions: &[Mission],
    similarity_threshold: f32,
) -> Vec<DuplicateWarning> {
    let mut duplicates = Vec::new();
    let new_title_normalized = normalize_text(new_title);

    for mission in existing_missions {
        // Skip completed missions
        if mission.status == MissionStatus::Completed {
            continue;
        }

        // Check entity match
        if mission.entity_id != entity_id.unwrap_or("") {
            continue;
        }

        let existing_title_normalized = normalize_text(&mission.title);
        let similarity = text_similarity(&new_title_normalized, &existing_title_normalized);

        if similarity >= similarity_threshold {
            duplicates.push(DuplicateWarning {
                existing_id: mission.id.clone(),
                item_type: "mission".to_string(),
                existing_title: mission.title.clone(),
                similarity,
                reason: format!("Title similarity: {:.0}%", similarity * 100.0),
            });
        }
    }

    duplicates
}

/// Simple text normalization for comparison
fn normalize_text(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
}

/// Simple text similarity using word overlap (Jaccard-like)
fn text_similarity(a: &str, b: &str) -> f32 {
    let words_a: HashSet<&str> = a.split_whitespace().collect();
    let words_b: HashSet<&str> = b.split_whitespace().collect();

    if words_a.is_empty() && words_b.is_empty() {
        return 1.0;
    }
    if words_a.is_empty() || words_b.is_empty() {
        return 0.0;
    }

    let intersection = words_a.intersection(&words_b).count();
    let union = words_a.union(&words_b).count();

    if union == 0 {
        0.0
    } else {
        intersection as f32 / union as f32
    }
}

/// Keywords that suggest a note (no action)
const NOTE_KEYWORDS: &[&str] = &[
    "note",
    "remember",
    "fyi",
    "for your information",
    "just saying",
    "btw",
    "by the way",
    "thought",
    "idea",
    "observation",
    "reminder to self",
];

/// Keywords that suggest a task (action required)
const TASK_KEYWORDS: &[&str] = &[
    "do",
    "complete",
    "finish",
    "create",
    "make",
    "build",
    "write",
    "send",
    "call",
    "email",
    "meet",
    "schedule",
    "prepare",
    "review",
    "update",
    "fix",
    "implement",
    "deploy",
    "action",
    "task",
    "todo",
    "to-do",
];

/// Keywords that suggest a reminder (time-based)
const REMINDER_KEYWORDS: &[&str] = &[
    "remind",
    "reminder",
    "don't forget",
    "remember to",
    "later",
    "tomorrow",
    "next week",
    "in an hour",
    "at 5pm",
    "on monday",
    "schedule",
    "alarm",
    "notify",
];

/// Keywords that suggest a mission (complex, multi-step)
const MISSION_KEYWORDS: &[&str] = &[
    "project",
    "initiative",
    "goal",
    "objective",
    "launch",
    "release",
    "campaign",
    "quarterly",
    "annual",
    "strategic",
    "plan",
    "roadmap",
    "milestone",
];

/// Classify intake text
pub fn classify_intake(request: &IntakeRequest) -> IntakeClassification {
    let text_lower = request.text.to_lowercase();

    // Check for explicit intent hint
    if let Some(hint) = &request.intent_hint {
        match hint.to_lowercase().as_str() {
            "note" => return IntakeClassification::Note,
            "task" => return IntakeClassification::Task,
            "reminder" | "followup" | "follow-up" => return IntakeClassification::Reminder,
            "mission" => return IntakeClassification::Mission,
            _ => {}
        }
    }

    // Check for follow-up patterns
    if text_lower.contains("follow up")
        || text_lower.contains("follow-up")
        || text_lower.contains("check on")
    {
        // Would need context to resolve parent_id - return Clarify for now
        return IntakeClassification::Clarify {
            reason: "Follow-up detected but no parent task/mission specified".to_string(),
            question: "Which task or mission should this follow up on?".to_string(),
        };
    }

    // Check for update patterns
    if (text_lower.contains("update")
        || text_lower.contains("change")
        || text_lower.contains("modify"))
        && (text_lower.contains("task") || text_lower.contains("mission"))
    {
        return IntakeClassification::Clarify {
            reason: "Update detected but no target task/mission specified".to_string(),
            question: "Which task or mission should be updated?".to_string(),
        };
    }

    // Score for each classification
    let mut note_score = 0;
    let mut task_score = 0;
    let mut reminder_score = 0;
    let mut mission_score = 0;

    for keyword in NOTE_KEYWORDS {
        if text_lower.contains(keyword) {
            note_score += 1;
        }
    }

    for keyword in TASK_KEYWORDS {
        if text_lower.contains(keyword) {
            task_score += 1;
        }
    }

    for keyword in REMINDER_KEYWORDS {
        if text_lower.contains(keyword) {
            reminder_score += 1;
        }
    }

    for keyword in MISSION_KEYWORDS {
        if text_lower.contains(keyword) {
            mission_score += 1;
        }
    }

    // Time expressions suggest reminder
    if has_time_expression(&text_lower) {
        reminder_score += 2;
    }

    // Determine classification
    let max_score = note_score
        .max(task_score)
        .max(reminder_score)
        .max(mission_score);

    // Tie-break order below, when two categories share the top score:
    // reminder > mission > task > note. A time expression is the strongest
    // signal, so reminder wins first.
    if max_score == 0 {
        // Default to task if unclear but has action verb
        if has_action_verb(&text_lower) {
            IntakeClassification::Task
        } else {
            IntakeClassification::Note
        }
    } else if reminder_score == max_score && reminder_score > 0 {
        IntakeClassification::Reminder
    } else if mission_score == max_score && mission_score > 0 {
        IntakeClassification::Mission
    } else if task_score == max_score && task_score > 0 {
        IntakeClassification::Task
    } else {
        IntakeClassification::Note
    }
}

/// Check if text has time expressions
fn has_time_expression(text: &str) -> bool {
    let time_patterns = [
        "tomorrow",
        "today",
        "yesterday",
        "next week",
        "last week",
        "next month",
        "in an hour",
        "in 2 hours",
        "at 5",
        "at 10",
        "pm",
        "am",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
        "q1",
        "q2",
        "q3",
        "q4",
        "2025",
        "2026",
        "2027",
    ];
    time_patterns.iter().any(|p| text.contains(p))
}

/// Check if text has action verbs
fn has_action_verb(text: &str) -> bool {
    let action_verbs = [
        "do",
        "make",
        "create",
        "build",
        "write",
        "send",
        "call",
        "email",
        "meet",
        "prepare",
        "review",
        "update",
        "fix",
        "implement",
        "deploy",
        "complete",
        "finish",
        "start",
        "begin",
        "launch",
        "release",
    ];
    action_verbs.iter().any(|v| text.contains(v))
}

/// Make intake decision with duplicate detection
pub fn make_intake_decision(
    request: &IntakeRequest,
    existing_tasks: &[Task],
    existing_missions: &[Mission],
) -> IntakeDecision {
    let classification = classify_intake(request);

    let (reasoning, suggested_director) = match &classification {
        IntakeClassification::Note => (
            "Classified as note - informational only, no action required".to_string(),
            Some("operations".to_string()),
        ),
        IntakeClassification::Task => (
            "Classified as task - action required, will be routed to appropriate director"
                .to_string(),
            Some("operations".to_string()),
        ),
        IntakeClassification::Reminder => (
            "Classified as reminder - time-based, will be scheduled".to_string(),
            Some("operations".to_string()),
        ),
        IntakeClassification::Mission => (
            "Classified as mission - complex multi-step work requiring planning".to_string(),
            Some("operations".to_string()),
        ),
        IntakeClassification::FollowUp {
            parent_id,
            parent_type,
        } => (
            format!("Follow-up to {} {}", parent_type, parent_id),
            Some("operations".to_string()),
        ),
        IntakeClassification::Update {
            target_id,
            target_type,
        } => (
            format!("Update to {} {}", target_type, target_id),
            Some("operations".to_string()),
        ),
        IntakeClassification::Clarify { reason, .. } => {
            (format!("Needs clarification: {}", reason), None)
        }
    };

    // Extract title for duplicate detection
    let title_for_check = extract_title_from_text(&request.text);

    let mut duplicate_warnings = Vec::new();

    if let IntakeClassification::Task | IntakeClassification::Reminder = &classification {
        let task_duplicates = check_duplicate_tasks(
            &title_for_check,
            request.entity_id.as_deref(),
            existing_tasks,
            0.6, // 60% similarity threshold
        );
        duplicate_warnings.extend(task_duplicates);
    }

    if let IntakeClassification::Mission = &classification {
        let mission_duplicates = check_duplicate_missions(
            &title_for_check,
            request.entity_id.as_deref(),
            existing_missions,
            0.6, // 60% similarity threshold
        );
        duplicate_warnings.extend(mission_duplicates);
    }

    // Calculate confidence
    let confidence = if duplicate_warnings.is_empty() {
        0.9
    } else {
        0.7 // Lower confidence if duplicates detected
    };

    IntakeDecision {
        classification,
        confidence,
        suggested_director,
        reasoning,
        duplicate_warnings,
    }
}

/// Extract a title-like phrase from text (first sentence or up to first punctuation)
fn extract_title_from_text(text: &str) -> String {
    text.split('.')
        .next()
        .unwrap_or(text)
        .split('!')
        .next()
        .unwrap_or(text)
        .split('?')
        .next()
        .unwrap_or(text)
        .trim()
        .chars()
        .take(100)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_note() {
        let req = IntakeRequest {
            text: "FYI: the meeting notes are in the shared folder".to_string(),
            entity_id: None,
            intent_hint: None,
            workspace_id: "default".to_string(),
        };
        let result = classify_intake(&req);
        assert!(matches!(result, IntakeClassification::Note));
    }

    #[test]
    fn classifies_task() {
        let req = IntakeRequest {
            text: "Create a landing page for the new product".to_string(),
            entity_id: None,
            intent_hint: None,
            workspace_id: "default".to_string(),
        };
        let result = classify_intake(&req);
        assert!(matches!(result, IntakeClassification::Task));
    }

    #[test]
    fn classifies_reminder() {
        let req = IntakeRequest {
            text: "Remind me to call John tomorrow at 5pm".to_string(),
            entity_id: None,
            intent_hint: None,
            workspace_id: "default".to_string(),
        };
        let result = classify_intake(&req);
        assert!(matches!(result, IntakeClassification::Reminder));
    }

    #[test]
    fn classifies_mission() {
        let req = IntakeRequest {
            text: "Q4 strategic roadmap planning initiative".to_string(),
            entity_id: None,
            intent_hint: None,
            workspace_id: "default".to_string(),
        };
        let result = classify_intake(&req);
        assert!(matches!(result, IntakeClassification::Mission));
    }

    #[test]
    fn detects_duplicate_tasks() {
        let existing = vec![Task {
            id: "task-1".to_string(),
            title: "Create landing page".to_string(),
            dept: "General".to_string(),
            roles: vec![],
            status: TaskStatus::Queued,
            tries: "0".to_string(),
            progress: 0,
            entity_id: Some("entity-1".to_string()),
            mission_id: None,
            priority: None,
            requires_human_approval: false,
            assigned_director_id: None,
            assigned_agent_id: None,
            connection_id: None,
            external_source: None,
            external_id: None,
            result: None,
            task_kind: None,
            metadata: None,
            outcome: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            due_at: None,
            completed_at: None,
            status_changed_at: None,
            source_task_id: None,
        }];

        let duplicates =
            check_duplicate_tasks("Create a landing page", Some("entity-1"), &existing, 0.6);

        assert_eq!(duplicates.len(), 1);
        assert_eq!(duplicates[0].existing_id, "task-1");
        assert!(duplicates[0].similarity >= 0.6);
    }

    #[test]
    fn text_similarity_works() {
        let sim = text_similarity("create landing page", "create a landing page");
        assert!(sim > 0.7);

        let sim_different = text_similarity("create landing page", "fix the database");
        assert!(sim_different < 0.3);
    }
}
