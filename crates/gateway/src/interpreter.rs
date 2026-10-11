//! Prompt interpreter: natural language → structured task brief (`prompt-interpretation` task kind).

use crate::state::{Entity, Task};
use serde::{Deserialize, Serialize};

/// Structured output for [`super::run_agent`] when `task_kind == "prompt-interpretation"`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptInterpretationBrief {
    /// Inferred entity id or name; use `"unknown"` if unclear.
    #[serde(default)]
    pub entity: String,
    /// Suggested downstream task kind (e.g. `content-draft`, `entity-enrichment`).
    #[serde(default)]
    pub task_kind: String,
    /// What the user wants accomplished, in one short sentence.
    #[serde(default)]
    pub goal: String,
    /// Hard limits, tone, format, deadlines, must-nots (free text).
    #[serde(default)]
    pub constraints: String,
    /// Which worker role or agent flavor fits best (short label).
    #[serde(default)]
    pub suggested_worker: String,
    /// One of: `low`, `normal`, `high`, or a short phrase.
    #[serde(default)]
    pub urgency: String,
}

/// System prompt for the Prompt Interpreter LLM call: instructs it to emit a
/// single JSON brief, no prose.
pub fn build_interpretation_system_prompt() -> String {
    r#"You are the Prompt Interpreter. You turn a messy user request into a single machine-readable brief for downstream routing.

Rules:
- Infer entity from the text (id, name, or product). If none, use "unknown".
- task_kind must be a short kebab-case label (e.g. entity-enrichment, content-draft, code-task, extract-task, follow-up-planning, entity-review, general).
- goal: one clear sentence.
- constraints: bullet-style text in a single string if needed; empty string if none.
- suggested_worker: short label (e.g. "content", "code", "research", "ops").
- urgency: one of low, normal, high (default normal if unclear).

Return exactly one JSON object with these keys (all strings): entity, task_kind, goal, constraints, suggested_worker, urgency.
No markdown fences, no commentary outside the JSON."#
        .to_string()
}

/// Build the user-turn prompt: the task title, department/role hints, and any
/// linked entity context.
pub fn build_interpretation_user_prompt(task: &Task, entity: Option<&Entity>) -> String {
    let mut parts = vec![format!("User prompt (task title):\n{}", task.title)];
    if !task.dept.is_empty() && task.dept != "General" {
        parts.push(format!("Department hint: {}", task.dept));
    }
    if !task.roles.is_empty() {
        parts.push(format!("Roles hint: {}", task.roles.join(", ")));
    }
    if let Some(e) = entity {
        parts.push(format!("Linked entity: id={} name={}", e.id, e.name));
        if let Some(ref d) = e.description {
            parts.push(format!("Entity description: {}", d));
        }
    }
    parts.push("Return JSON only.".to_string());
    parts.join("\n\n")
}

/// Parse LLM output into a brief (strips optional ```json fences).
pub fn parse_prompt_interpretation_brief(text: &str) -> Result<PromptInterpretationBrief, String> {
    let json_str = text.trim();
    let json_str = json_str
        .strip_prefix("```json")
        .or_else(|| json_str.strip_prefix("```"))
        .and_then(|s| s.strip_suffix("```").map(|s| s.trim()))
        .unwrap_or(json_str);
    serde_json::from_str::<PromptInterpretationBrief>(json_str)
        .map_err(|e| format!("invalid interpretation JSON: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_json() {
        let j = r#"{"entity":"acme","task_kind":"content-draft","goal":"Draft a landing page","constraints":"","suggested_worker":"content","urgency":"normal"}"#;
        let b = parse_prompt_interpretation_brief(j).unwrap();
        assert_eq!(b.entity, "acme");
        assert_eq!(b.task_kind, "content-draft");
        assert_eq!(b.goal, "Draft a landing page");
    }

    #[test]
    fn strips_fence() {
        let j = "```json\n{\"entity\":\"x\",\"task_kind\":\"general\",\"goal\":\"g\",\"constraints\":\"\",\"suggested_worker\":\"\",\"urgency\":\"low\"}\n```";
        let b = parse_prompt_interpretation_brief(j).unwrap();
        assert_eq!(b.entity, "x");
        assert_eq!(b.urgency, "low");
    }
}
