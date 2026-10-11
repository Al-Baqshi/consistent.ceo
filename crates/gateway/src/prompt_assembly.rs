//! Phase 6 — Prompt assembly pipeline.
//!
//! Stitches together the canonical system prompt for an outbound LLM turn so
//! the three pillars (heartbeats, canonical model, director orchestration)
//! actually reach the model in a single coherent block.
//!
//! Documented order (top → bottom):
//!
//! 1. **SOUL** — global persona (Consistent's voice).
//! 2. **Agent operating contract** — local-first ledger/executor rules.
//! 3. **Director block** — director.prompt → rules_global → rules_per_entity[entity_id] → recalled lessons.
//!    (Delegated to [`crate::director_store::assemble_director_block`].)
//! 4. **Entity canonical** — active entity name + description + profile + goals + operating rules.
//! 5. **Mission narrative + pinned notes** — story-so-far + canonical_notes for the active mission.
//!
//! The function is **pure**: caller pre-recalls lessons and looks up the active
//! mission. That keeps unit tests deterministic and easy to write.
//!
//! Any input that is missing or empty after `trim()` is silently skipped — the
//! assembled prompt never contains an empty heading.

use crate::director_store::assemble_director_block;
use crate::state::{Director, DirectorLesson, Entity, EntityDoc, Mission};

/// Ceiling for the whole docs block, in characters. Docs ride every turn for
/// the entity — chat and heartbeats, cloud and local muscle alike — so the
/// block is bounded here regardless of how much the index holds. Roughly
/// 4k tokens: the brief, a README and a repository map fit; a fourth long
/// doc is cut, with a visible marker, rather than silently crowding out the
/// conversation.
pub const MAX_DOCS_BLOCK_CHARS: usize = 16_000;
/// Below this much room left, skip the next doc instead of emitting a stub.
const MIN_DOC_TAIL_CHARS: usize = 400;

/// All canonical inputs that feed the system prompt for one chat turn.
///
/// Every field is optional so the same struct works for a fresh "no entity
/// selected" turn, a heartbeat-driven director turn, and everything in
/// between.
#[derive(Default)]
pub struct CanonicalInputs<'a> {
    pub soul: Option<&'a str>,
    pub director: Option<&'a Director>,
    pub entity: Option<&'a Entity>,
    pub mission: Option<&'a Mission>,
    pub lessons: &'a [DirectorLesson],
    /// Enabled Cursor-style entity docs, eagerly injected after the entity block.
    pub docs: &'a [EntityDoc],
}

/// Assemble the full canonical system prompt.
///
/// Sections are joined with a blank line. Empty / whitespace-only inputs are
/// dropped without emitting their heading, so callers don't have to guard
/// each field.
pub fn assemble_system_prompt(inputs: &CanonicalInputs<'_>) -> String {
    let mut sections: Vec<String> = Vec::new();

    if let Some(soul) = inputs.soul.map(str::trim).filter(|s| !s.is_empty()) {
        sections.push(format!("## SOUL\n{soul}"));
    }

    if should_render_agent_contract(inputs) {
        sections.push(render_agent_operating_contract());
    }

    if let Some(director) = inputs.director {
        let entity_id = inputs.entity.map(|e| e.id.as_str());
        let block = assemble_director_block(director, entity_id, inputs.lessons);
        if !block.trim().is_empty() {
            sections.push(block);
        }
    }

    if let Some(block) = render_entity_block(inputs.entity) {
        sections.push(block);
    }

    if let Some(block) = render_entity_docs_block(inputs.docs) {
        sections.push(block);
    }

    if let Some(block) = render_mission_block(inputs.mission) {
        sections.push(block);
    }

    sections.join("\n\n")
}

// The agent operating contract is only worth emitting once there's something
// for it to govern — a director, entity, mission content, lessons, or docs.
// A completely empty CanonicalInputs stays an empty prompt.
fn should_render_agent_contract(inputs: &CanonicalInputs<'_>) -> bool {
    inputs.director.is_some()
        || inputs.entity.is_some()
        || mission_has_prompt_content(inputs.mission)
        || !inputs.lessons.is_empty()
        || inputs
            .docs
            .iter()
            .any(|d| d.enabled && !d.content.trim().is_empty())
}

// True when the mission carries a narrative or at least one non-empty
// pinned note — i.e. when `render_mission_block` would actually emit a block.
fn mission_has_prompt_content(mission: Option<&Mission>) -> bool {
    let Some(mission) = mission else {
        return false;
    };
    mission
        .narrative
        .as_deref()
        .map(str::trim)
        .is_some_and(|s| !s.is_empty())
        || mission.canonical_notes.iter().any(|n| !n.trim().is_empty())
}

// Fixed rules block reminding the model that Consistent is the ledger and it
// is the executor — joined line-by-line so each rule stays a distinct string.
fn render_agent_operating_contract() -> String {
    [
        "## Agent operating contract",
        "- Treat Consistent as the ledger and the tools/runtime as the executor.",
        "- Before acting, use the active entity, mission, pinned notes, docs, lessons, and recent memory as your source of truth.",
        "- Do not claim you created, changed, saved, scheduled, or sent anything unless a tool/API call actually performed that side effect.",
        "- When a user asks for durable work, prefer creating or updating local artifacts tied to the workspace/entity instead of leaving the result only in chat.",
        "- If context is missing, capture the gap as a note/task/question rather than rushing into irreversible action.",
        "- After a useful outcome or correction, make the result easy to learn from: summarize the durable artifact, decision, or next action clearly.",
    ]
    .join("\n")
}

/// Render the entity canonical block. Returns `None` when there is no entity
/// or every field is empty.
fn render_entity_block(entity: Option<&Entity>) -> Option<String> {
    let entity = entity?;
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("- Name: {}", entity.name.trim()));
    push_field(&mut lines, "Description", entity.description.as_deref());
    push_field(&mut lines, "Profile", entity.profile.as_deref());
    push_field(&mut lines, "Goals", entity.goals.as_deref());
    push_field(
        &mut lines,
        "Active context",
        entity.active_context.as_deref(),
    );
    push_field(
        &mut lines,
        "Operating rules",
        entity.operating_rules.as_deref(),
    );

    if lines.len() == 1 && entity.name.trim().is_empty() {
        return None;
    }

    Some(format!(
        "## Active entity (canonical)\n{}",
        lines.join("\n")
    ))
}

/// Render the entity docs block (Cursor-style reference docs). Only docs that
/// are enabled and carry non-empty content are emitted; returns `None` when
/// nothing survives so the heading is omitted cleanly.
fn render_entity_docs_block(docs: &[EntityDoc]) -> Option<String> {
    let live: Vec<&EntityDoc> = docs
        .iter()
        .filter(|d| d.enabled && !d.content.trim().is_empty())
        .collect();
    if live.is_empty() {
        return None;
    }
    let mut out = String::from("## Entity docs (canonical reference)\n");
    let mut spent = 0usize;
    for d in live {
        let title = d.title.trim();
        let mut section = if title.is_empty() {
            String::from("### (untitled)\n")
        } else {
            format!("### {title}\n")
        };
        if let Some(url) = d.url.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            section.push_str(&format!("Source: {url}\n"));
        }
        let remaining = MAX_DOCS_BLOCK_CHARS.saturating_sub(spent);
        if remaining < MIN_DOC_TAIL_CHARS {
            out.push_str(&format!(
                "(further docs omitted: prompt budget of {MAX_DOCS_BLOCK_CHARS} characters reached)\n"
            ));
            break;
        }
        let body = d.content.trim();
        let body_len = body.chars().count();
        if body_len > remaining {
            let cut: String = body.chars().take(remaining).collect();
            section.push_str(&cut);
            section.push_str("\n…(truncated to fit the prompt budget)");
        } else {
            section.push_str(body);
        }
        spent += section.chars().count();
        out.push_str(&section);
        out.push_str("\n\n");
    }
    Some(out.trim_end().to_string())
}

/// Render the mission narrative + pinned notes block. Returns `None` when the
/// mission carries no narrative and no pinned notes.
fn render_mission_block(mission: Option<&Mission>) -> Option<String> {
    let mission = mission?;
    let narrative = mission
        .narrative
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let pinned: Vec<&str> = mission
        .canonical_notes
        .iter()
        .map(|n| n.trim())
        .filter(|n| !n.is_empty())
        .collect();

    if narrative.is_none() && pinned.is_empty() {
        return None;
    }

    let mut out = String::from("## Active mission\n");
    let title = mission.title.trim();
    if !title.is_empty() {
        out.push_str(&format!("- Title: {title}\n"));
    }
    let goal = mission.goal.trim();
    if !goal.is_empty() {
        out.push_str(&format!("- Goal: {goal}\n"));
    }
    if let Some(n) = narrative {
        out.push_str("\n### Story so far\n");
        out.push_str(n);
        out.push('\n');
    }
    if !pinned.is_empty() {
        out.push_str("\n### Pinned notes (canonical)\n");
        for n in pinned {
            out.push_str("- ");
            out.push_str(n);
            out.push('\n');
        }
    }
    Some(out.trim_end().to_string())
}

// Append a "- Label: value" line only when value is present and non-blank
// after trimming, so optional entity fields don't emit empty bullets.
fn push_field(lines: &mut Vec<String>, label: &str, value: Option<&str>) {
    if let Some(v) = value.map(str::trim).filter(|s| !s.is_empty()) {
        lines.push(format!("- {label}: {v}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        Director, DirectorLesson, Entity, EntityVisibility, Mission, MissionStatus,
    };
    use std::collections::HashMap;

    fn fixed_director() -> Director {
        let mut per_entity = HashMap::new();
        per_entity.insert("ent_qsme".to_string(), "Match QS Me tone.".to_string());
        Director {
            id: "dir_ops".to_string(),
            slug: "operations".to_string(),
            name: "Operations".to_string(),
            kind: "director".to_string(),
            description: "Runs the project".to_string(),
            role: "operations".to_string(),
            scope: "global".to_string(),
            workspace_id: Some("ws_default".to_string()),
            group_id: Some("operations".to_string()),
            prompt: "You are the operations director.".to_string(),
            model_provider: Some("anthropic".to_string()),
            model_name: Some("claude-sonnet-4-6".to_string()),
            temperature: Some(0.4),
            tool_allowlist: vec!["search".to_string()],
            rules_global: "Be terse.".to_string(),
            rules_per_entity: per_entity,
            default_heartbeat: None,
            version: 1,
            is_active: true,
            created_at: "2026-05-24T03:00:00Z".to_string(),
            updated_at: "2026-05-24T03:00:00Z".to_string(),
        }
    }

    fn fixed_entity() -> Entity {
        Entity {
            id: "ent_qsme".to_string(),
            name: "QS Me".to_string(),
            description: Some("Quantity surveying platform".to_string()),
            profile: Some("Construction-tech tone, no jargon dump.".to_string()),
            goals: Some("Ship the takeoff workflow.".to_string()),
            active_context: Some("Currently wiring the BIM importer.".to_string()),
            operating_rules: Some("Never push to main on Fridays.".to_string()),
            status: "active".to_string(),
            visibility: EntityVisibility::Private,
            ..Entity::default()
        }
    }

    fn fixed_mission() -> Mission {
        Mission {
            id: "mis_takeoff".to_string(),
            entity_id: "ent_qsme".to_string(),
            title: "Takeoff workflow v1".to_string(),
            summary: "First-cut takeoff UX".to_string(),
            status: MissionStatus::Active,
            priority: "high".to_string(),
            goal: "Estimator can produce a priced takeoff in <10 min.".to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-05-20T00:00:00Z".to_string(),
            updated_at: "2026-05-24T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: Some(
                "Carlos wants the takeoff workflow shippable before the QS expo.".to_string(),
            ),
            canonical_notes: vec![
                "Imperial units default for AU customers.".to_string(),
                "Always show subtotal per trade.".to_string(),
            ],
            assigned_director_id: Some("dir_ops".to_string()),
            final_report: None,
            loop_config: None,
            loop_state: None,
        }
    }

    fn fixed_lessons() -> Vec<DirectorLesson> {
        vec![
            DirectorLesson {
                id: "les_1".to_string(),
                director_id: "dir_ops".to_string(),
                scope: "global".to_string(),
                lesson: "Confirm units before pricing.".to_string(),
                source: "correction".to_string(),
                severity: "hard".to_string(),
                embedding: None,
                created_at: "2026-05-22T00:00:00Z".to_string(),
                created_by: Some("carlos".to_string()),
            },
            DirectorLesson {
                id: "les_2".to_string(),
                director_id: "dir_ops".to_string(),
                scope: "entity:ent_qsme".to_string(),
                lesson: "Use trade-level breakdowns in summaries.".to_string(),
                source: "observation".to_string(),
                severity: "soft".to_string(),
                embedding: None,
                created_at: "2026-05-23T00:00:00Z".to_string(),
                created_by: None,
            },
        ]
    }

    /// Phase 6 done-when: fixed entity + mission + director + 2 lessons →
    /// assembled prompt contains all sections in the documented order.
    #[test]
    fn full_assembly_contains_all_sections_in_order() {
        let soul = "You are Consistent.";
        let director = fixed_director();
        let entity = fixed_entity();
        let mission = fixed_mission();
        let lessons = fixed_lessons();

        let prompt = assemble_system_prompt(&CanonicalInputs {
            soul: Some(soul),
            director: Some(&director),
            entity: Some(&entity),
            mission: Some(&mission),
            lessons: &lessons,
            docs: &[],
        });

        // Every documented section must be present.
        assert!(prompt.contains("## SOUL"));
        assert!(prompt.contains("You are Consistent."));
        assert!(prompt.contains("## Agent operating contract"));
        assert!(prompt.contains("Consistent as the ledger"));
        assert!(
            prompt
                .contains("Do not claim you created, changed, saved, scheduled, or sent anything")
        );
        assert!(prompt.contains("You are the operations director."));
        assert!(prompt.contains("## Rules (global)\nBe terse."));
        assert!(prompt.contains("## Rules (this entity)\nMatch QS Me tone."));
        assert!(prompt.contains("## Lessons learned"));
        assert!(prompt.contains("•! Confirm units before pricing."));
        assert!(prompt.contains("• Use trade-level breakdowns in summaries."));
        assert!(prompt.contains("## Active entity (canonical)"));
        assert!(prompt.contains("- Name: QS Me"));
        assert!(prompt.contains("- Goals: Ship the takeoff workflow."));
        assert!(prompt.contains("- Operating rules: Never push to main on Fridays."));
        assert!(prompt.contains("## Active mission"));
        assert!(prompt.contains("- Title: Takeoff workflow v1"));
        assert!(prompt.contains("### Story so far"));
        assert!(prompt.contains("Carlos wants the takeoff workflow shippable"));
        assert!(prompt.contains("### Pinned notes (canonical)"));
        assert!(prompt.contains("- Imperial units default for AU customers."));
        assert!(prompt.contains("- Always show subtotal per trade."));

        // Documented order check.
        let i_soul = prompt.find("## SOUL").unwrap();
        let i_contract = prompt.find("## Agent operating contract").unwrap();
        let i_director = prompt.find("You are the operations director.").unwrap();
        let i_rules_global = prompt.find("## Rules (global)").unwrap();
        let i_rules_entity = prompt.find("## Rules (this entity)").unwrap();
        let i_lessons = prompt.find("## Lessons learned").unwrap();
        let i_entity = prompt.find("## Active entity (canonical)").unwrap();
        let i_mission = prompt.find("## Active mission").unwrap();
        assert!(i_soul < i_contract);
        assert!(i_contract < i_director);
        assert!(i_director < i_rules_global);
        assert!(i_rules_global < i_rules_entity);
        assert!(i_rules_entity < i_lessons);
        assert!(i_lessons < i_entity);
        assert!(i_entity < i_mission);
    }

    #[test]
    fn omits_sections_when_inputs_absent() {
        let entity = fixed_entity();
        let prompt = assemble_system_prompt(&CanonicalInputs {
            soul: None,
            director: None,
            entity: Some(&entity),
            mission: None,
            lessons: &[],
            docs: &[],
        });
        assert!(!prompt.contains("## SOUL"));
        assert!(prompt.contains("## Agent operating contract"));
        assert!(!prompt.contains("## Rules (global)"));
        assert!(!prompt.contains("## Lessons learned"));
        assert!(!prompt.contains("## Active mission"));
        assert!(prompt.contains("## Active entity (canonical)"));
        assert!(prompt.contains("- Name: QS Me"));
    }

    #[test]
    fn mission_without_narrative_or_notes_is_omitted() {
        let mut mission = fixed_mission();
        mission.narrative = None;
        mission.canonical_notes.clear();
        let prompt = assemble_system_prompt(&CanonicalInputs {
            mission: Some(&mission),
            ..CanonicalInputs::default()
        });
        assert!(!prompt.contains("## Active mission"));
        assert!(prompt.is_empty());
    }

    #[test]
    fn empty_whitespace_inputs_drop_cleanly() {
        let mut entity = fixed_entity();
        entity.description = Some("   ".to_string());
        entity.profile = None;
        let mut mission = fixed_mission();
        mission.narrative = Some("\n  \n".to_string());
        mission.canonical_notes = vec!["".to_string(), "  ".to_string()];

        let prompt = assemble_system_prompt(&CanonicalInputs {
            soul: Some("   "),
            director: None,
            entity: Some(&entity),
            mission: Some(&mission),
            lessons: &[],
            docs: &[],
        });
        assert!(!prompt.contains("## SOUL"));
        assert!(prompt.contains("## Agent operating contract"));
        assert!(!prompt.contains("Description"));
        assert!(!prompt.contains("Profile"));
        // Mission had only whitespace content → omitted entirely.
        assert!(!prompt.contains("## Active mission"));
        assert!(prompt.contains("## Active entity (canonical)"));
    }

    #[test]
    fn director_alone_assembles_without_entity_or_mission() {
        let director = fixed_director();
        let prompt = assemble_system_prompt(&CanonicalInputs {
            director: Some(&director),
            ..CanonicalInputs::default()
        });
        assert!(prompt.contains("You are the operations director."));
        assert!(prompt.contains("## Agent operating contract"));
        assert!(prompt.contains("## Rules (global)"));
        // Without an entity, entity-scoped rules don't leak in.
        assert!(!prompt.contains("## Rules (this entity)"));
        assert!(!prompt.contains("## Active entity"));
        assert!(!prompt.contains("## Active mission"));
    }
    #[test]
    fn docs_block_is_bounded_by_the_prompt_budget() {
        let big = |title: &str| EntityDoc {
            id: title.to_string(),
            entity_id: "ent_qsme".to_string(),
            title: title.to_string(),
            content: "x".repeat(7_000),
            source: "import".to_string(),
            url: None,
            enabled: true,
            embedding: None,
            created_at: String::new(),
            updated_at: String::new(),
            created_by: None,
        };
        let docs = vec![big("one"), big("two"), big("three"), big("four")];
        let block = render_entity_docs_block(&docs).expect("docs render");
        assert!(
            block.chars().count() <= MAX_DOCS_BLOCK_CHARS + 400,
            "block stays near the budget: {}",
            block.chars().count()
        );
        assert!(block.contains("### one"));
        assert!(block.contains("### two"));
        assert!(block.contains("### three"), "third doc is cut, not dropped");
        assert!(block.contains("truncated to fit the prompt budget"));
        assert!(
            !block.contains("### four"),
            "docs past the budget are omitted"
        );
        assert!(block.contains("further docs omitted"));

        let small = EntityDoc {
            content: "short".to_string(),
            ..big("small")
        };
        let block = render_entity_docs_block(&[small]).unwrap();
        assert!(
            block.ends_with("short"),
            "an unbounded doc is untouched: {block}"
        );
    }
}
