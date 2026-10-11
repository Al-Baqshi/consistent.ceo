//! ② Forward — from the Snapshot's stated goals and current gaps, name the
//! next plausible states.
//!
//! Deliberately deterministic and model-free. Every rule here is a fact about
//! rows Inspect already read, so this stage costs nothing to run, produces the
//! same answer twice for the same snapshot, and is testable without a provider.
//! That is what lets it run in shadow mode over real traffic.
//!
//! ## What this stage cannot see
//!
//! `active_missions` comes from `list_active_for_entity`, which filters on
//! `status = 'active'`. A mission that is `Blocked` or `UserInputNeeded` is
//! therefore **absent from the snapshot entirely**, and reads here as *no
//! mission at all* rather than as a stuck one. [`GapKind::NoActiveMission`] is
//! worded to admit that rather than claiming the entity is idle.

use serde::{Deserialize, Serialize};

use crate::cite::{CiteRef, CiteSource};
use crate::inspect::Snapshot;

/// How many candidate next-states Forward will propose. A bounded list is the
/// point: an operator can read eight, not eighty.
pub const MAX_CANDIDATES: usize = 8;

/// A mission untouched for this long is treated as stale.
pub const STALE_MISSION_DAYS: i64 = 14;

/// A named shortfall in what the entity has told the system about itself, or in
/// the shape of its current work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GapKind {
    /// The entity has no stated goal, so nothing can be judged against it.
    MissingEntityGoal,
    /// No operating rules, so the system has no stated constraints to respect.
    MissingOperatingRules,
    /// No knowledge documents, so nothing the entity knows is citable.
    NoKnowledgeDocs,
    /// No mission is active. May also mean every mission is blocked — see the
    /// module docs.
    NoActiveMission,
    /// A mission exists but carries no goal text.
    MissionWithoutGoal,
    /// A mission has not moved in [`STALE_MISSION_DAYS`].
    StaleMission,
    /// An active mission has no open tasks, so nothing will advance it.
    MissionWithoutOpenTasks,
    /// Work is waiting on a human decision.
    AwaitingApproval,
}

impl GapKind {
    /// Stable token for the wire and the audit trail.
    pub fn as_str(self) -> &'static str {
        match self {
            GapKind::MissingEntityGoal => "missing_entity_goal",
            GapKind::MissingOperatingRules => "missing_operating_rules",
            GapKind::NoKnowledgeDocs => "no_knowledge_docs",
            GapKind::NoActiveMission => "no_active_mission",
            GapKind::MissionWithoutGoal => "mission_without_goal",
            GapKind::StaleMission => "stale_mission",
            GapKind::MissionWithoutOpenTasks => "mission_without_open_tasks",
            GapKind::AwaitingApproval => "awaiting_approval",
        }
    }
}

/// One shortfall, with the row it was observed on where there is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gap {
    pub kind: GapKind,
    /// The row this is about (mission id, task id), when it is about one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cite: Option<CiteRef>,
}

/// A plausible next state, with why it is plausible and what blocks it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateState {
    /// Stable within a snapshot, so two runs over the same world agree.
    pub id: String,
    pub headline: String,
    pub rationale: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<GapKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<CiteRef>,
    /// Higher is more urgent. Ordering only — not a probability.
    pub score: u8,
}

/// What Forward produced.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardView {
    #[serde(default)]
    pub gaps: Vec<Gap>,
    #[serde(default)]
    pub candidates: Vec<CandidateState>,
}

impl ForwardView {
    /// One-line note for the audit trail, so a reviewer need not expand the
    /// payload to see what Forward concluded.
    pub fn summary_note(&self) -> String {
        format!(
            "gaps={} candidates={} top={}",
            self.gaps.len(),
            self.candidates.len(),
            self.candidates
                .first()
                .map(|c| c.id.as_str())
                .unwrap_or("none"),
        )
    }
}

/// Read a string field from a JSON row, treating whitespace as absent.
fn field<'a>(row: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    row.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Whole days between two RFC3339 instants, or `None` if either fails to parse.
fn days_between(earlier: &str, later: &str) -> Option<i64> {
    let a = chrono::DateTime::parse_from_rfc3339(earlier).ok()?;
    let b = chrono::DateTime::parse_from_rfc3339(later).ok()?;
    Some((b - a).num_days())
}

/// Run Forward over a snapshot.
///
/// `now` is injected rather than read from the clock so the same snapshot
/// always yields the same view — which is what makes this testable and what
/// keeps a replayed audit row meaningful.
pub fn forward(snap: &Snapshot, now: &str) -> ForwardView {
    let mut gaps: Vec<Gap> = Vec::new();
    let mut candidates: Vec<CandidateState> = Vec::new();

    // --- What the entity has told us about itself -------------------------
    if let Some(entity) = &snap.entity {
        let entity_id = field(entity, "id").unwrap_or("entity").to_string();

        if field(entity, "goals").is_none() {
            gaps.push(Gap {
                kind: GapKind::MissingEntityGoal,
                subject: Some(entity_id.clone()),
                detail: "The entity has no stated goal, so no work can be judged against one."
                    .to_string(),
                cite: None,
            });
            candidates.push(CandidateState {
                id: "state:entity-goal".to_string(),
                headline: "Agree a goal for this entity".to_string(),
                rationale: "Everything downstream — mission planning, definition of done, \
                            whether a beat was useful — is measured against a goal that does \
                            not exist yet."
                    .to_string(),
                blocked_by: vec![GapKind::MissingEntityGoal],
                evidence: Vec::new(),
                score: 90,
            });
        }

        // `Entity` serialises camelCase.
        if field(entity, "operatingRules").is_none() {
            gaps.push(Gap {
                kind: GapKind::MissingOperatingRules,
                subject: Some(entity_id.clone()),
                detail: "No operating rules, so the system has no stated constraints to respect."
                    .to_string(),
                cite: None,
            });
        }
    }

    if snap.entity_docs.is_empty() {
        gaps.push(Gap {
            kind: GapKind::NoKnowledgeDocs,
            subject: snap.entity_id.clone(),
            detail: "No knowledge documents are indexed, so nothing this entity knows can be \
                     cited back to it."
                .to_string(),
            cite: None,
        });
        candidates.push(CandidateState {
            id: "state:index-knowledge".to_string(),
            headline: "Import something this entity knows".to_string(),
            rationale: "Without indexed documents every answer is model reasoning; with them \
                        answers can resolve to a source."
                .to_string(),
            blocked_by: vec![GapKind::NoKnowledgeDocs],
            evidence: Vec::new(),
            score: 55,
        });
    }

    // --- Missions ---------------------------------------------------------
    if snap.active_missions.is_empty() {
        gaps.push(Gap {
            kind: GapKind::NoActiveMission,
            subject: snap.entity_id.clone(),
            detail: "No mission is active. Note this also reads true when every mission is \
                     blocked or waiting on input, since those are not returned as active."
                .to_string(),
            cite: None,
        });
        candidates.push(CandidateState {
            id: "state:start-mission".to_string(),
            headline: "Start a mission, or unblock the one that stalled".to_string(),
            rationale: "Nothing is currently advancing toward the entity's goal.".to_string(),
            blocked_by: vec![GapKind::NoActiveMission],
            evidence: Vec::new(),
            score: 80,
        });
    }

    for mission in &snap.active_missions {
        let mid = field(mission, "id").unwrap_or("mission").to_string();
        let title = field(mission, "title").unwrap_or("(untitled mission)");
        // `Mission` has no `rename_all`, so its fields are snake_case.
        let cite = CiteRef::new(CiteSource::Mission, mid.clone(), title);

        if field(mission, "goal").is_none() {
            gaps.push(Gap {
                kind: GapKind::MissionWithoutGoal,
                subject: Some(mid.clone()),
                detail: format!("Mission {title:?} carries no goal text."),
                cite: Some(cite.clone()),
            });
        }

        if let Some(updated) = field(mission, "updated_at")
            && let Some(days) = days_between(updated, now)
            && days >= STALE_MISSION_DAYS
        {
            gaps.push(Gap {
                kind: GapKind::StaleMission,
                subject: Some(mid.clone()),
                detail: format!("Mission {title:?} has not moved in {days} days."),
                cite: Some(cite.clone()),
            });
            candidates.push(CandidateState {
                id: format!("state:revive:{mid}"),
                headline: format!("Revive or close {title:?}"),
                rationale: format!(
                    "It has been active but untouched for {days} days; either it is still \
                     wanted and needs a next task, or it should be closed."
                ),
                blocked_by: vec![GapKind::StaleMission],
                evidence: vec![cite.clone()],
                // Staleness grows in urgency, but never outranks a missing goal.
                score: 60u8.saturating_add((days / 7).clamp(0, 25) as u8),
            });
        }

        let has_open_task = snap
            .open_tasks
            .iter()
            .any(|t| field(t, "mission_id") == Some(mid.as_str()));
        if !has_open_task {
            gaps.push(Gap {
                kind: GapKind::MissionWithoutOpenTasks,
                subject: Some(mid.clone()),
                detail: format!("Mission {title:?} is active but has no open task to advance it."),
                cite: Some(cite.clone()),
            });
            candidates.push(CandidateState {
                id: format!("state:next-task:{mid}"),
                headline: format!("Decide the next task for {title:?}"),
                rationale: "An active mission with no open task will not move on its own."
                    .to_string(),
                blocked_by: vec![GapKind::MissionWithoutOpenTasks],
                evidence: vec![cite],
                score: 70,
            });
        }
    }

    // --- Work waiting on a human -----------------------------------------
    let awaiting: Vec<&serde_json::Value> = snap
        .open_tasks
        .iter()
        .filter(|t| {
            t.get("requires_human_approval")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        })
        .collect();
    if !awaiting.is_empty() {
        let evidence: Vec<CiteRef> = awaiting
            .iter()
            .take(3)
            .map(|t| {
                CiteRef::new(
                    CiteSource::Task,
                    field(t, "id").unwrap_or("task").to_string(),
                    field(t, "title").unwrap_or("(untitled task)"),
                )
            })
            .collect();
        gaps.push(Gap {
            kind: GapKind::AwaitingApproval,
            subject: None,
            detail: format!(
                "{} task(s) are waiting on a human decision.",
                awaiting.len()
            ),
            cite: evidence.first().cloned(),
        });
        candidates.push(CandidateState {
            id: "state:clear-approvals".to_string(),
            headline: format!("Clear {} pending approval(s)", awaiting.len()),
            rationale: "Approval-gated work is stopped until a person answers; this is the \
                        cheapest unblock available."
                .to_string(),
            blocked_by: vec![GapKind::AwaitingApproval],
            evidence,
            // The cheapest real unblock in the system, so it outranks planning work.
            score: 95,
        });
    }

    // Deterministic order: urgency first, then id, so equal scores never
    // reorder between runs over the same snapshot.
    candidates.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.id.cmp(&b.id)));
    candidates.truncate(MAX_CANDIDATES);
    gaps.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.subject.cmp(&b.subject))
    });

    ForwardView { gaps, candidates }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-09-12T00:00:00Z";

    fn snap() -> Snapshot {
        Snapshot {
            workspace_id: Some("ws".into()),
            entity_id: Some("ent_1".into()),
            entity: None,
            recent_memories: Vec::new(),
            recent_lessons: Vec::new(),
            active_missions: Vec::new(),
            open_tasks: Vec::new(),
            entity_docs: Vec::new(),
            inspected_at: NOW.to_string(),
        }
    }

    fn kinds(v: &ForwardView) -> Vec<GapKind> {
        v.gaps.iter().map(|g| g.kind).collect()
    }

    #[test]
    fn an_empty_world_asks_for_a_goal_and_a_mission() {
        let v = forward(&snap(), NOW);
        assert!(kinds(&v).contains(&GapKind::NoActiveMission));
        assert!(kinds(&v).contains(&GapKind::NoKnowledgeDocs));
        assert!(!v.candidates.is_empty());
    }

    #[test]
    fn a_stated_goal_is_not_reported_as_missing() {
        let mut s = snap();
        s.entity = Some(serde_json::json!({
            "id": "ent_1",
            "goals": "Ship the installer",
            "operatingRules": "Never push to main",
        }));
        let v = forward(&s, NOW);
        assert!(!kinds(&v).contains(&GapKind::MissingEntityGoal));
        assert!(!kinds(&v).contains(&GapKind::MissingOperatingRules));
    }

    #[test]
    fn whitespace_counts_as_absent() {
        let mut s = snap();
        s.entity = Some(serde_json::json!({ "id": "ent_1", "goals": "   \n " }));
        let v = forward(&s, NOW);
        assert!(kinds(&v).contains(&GapKind::MissingEntityGoal));
    }

    #[test]
    fn a_mission_untouched_for_a_fortnight_is_stale() {
        let mut s = snap();
        s.active_missions = vec![serde_json::json!({
            "id": "mis_1",
            "title": "Launch",
            "goal": "Ship it",
            "updated_at": "2026-08-01T00:00:00Z",
        })];
        let v = forward(&s, NOW);
        assert!(kinds(&v).contains(&GapKind::StaleMission));

        // A mission touched yesterday is not.
        s.active_missions = vec![serde_json::json!({
            "id": "mis_1",
            "title": "Launch",
            "goal": "Ship it",
            "updated_at": "2026-09-11T00:00:00Z",
        })];
        assert!(!kinds(&forward(&s, NOW)).contains(&GapKind::StaleMission));
    }

    #[test]
    fn an_unparseable_timestamp_is_not_reported_as_stale() {
        // Degrading to "no opinion" beats inventing a 50,000-day staleness.
        let mut s = snap();
        s.active_missions = vec![serde_json::json!({
            "id": "mis_1", "title": "Launch", "goal": "Ship it", "updated_at": "not a date",
        })];
        assert!(!kinds(&forward(&s, NOW)).contains(&GapKind::StaleMission));
    }

    #[test]
    fn an_active_mission_with_no_open_task_is_flagged() {
        let mut s = snap();
        s.active_missions = vec![serde_json::json!({
            "id": "mis_1", "title": "Launch", "goal": "Ship it", "updated_at": NOW,
        })];
        assert!(kinds(&forward(&s, NOW)).contains(&GapKind::MissionWithoutOpenTasks));

        s.open_tasks = vec![serde_json::json!({
            "id": "task_1", "title": "Write the brief", "mission_id": "mis_1",
        })];
        assert!(!kinds(&forward(&s, NOW)).contains(&GapKind::MissionWithoutOpenTasks));
    }

    #[test]
    fn pending_approvals_outrank_everything_else() {
        let mut s = snap();
        s.entity = Some(serde_json::json!({
            "id": "ent_1", "goals": "Ship", "operatingRules": "Be careful",
        }));
        s.open_tasks = vec![serde_json::json!({
            "id": "task_1",
            "title": "Send the contract",
            "requires_human_approval": true,
        })];
        let v = forward(&s, NOW);
        assert_eq!(
            v.candidates.first().map(|c| c.id.as_str()),
            Some("state:clear-approvals"),
            "the cheapest unblock leads: {:?}",
            v.candidates.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
        assert!(!v.candidates[0].evidence.is_empty(), "and it cites the task");
    }

    #[test]
    fn the_same_snapshot_always_yields_the_same_view() {
        // Determinism is what makes a replayed audit row meaningful.
        let mut s = snap();
        s.active_missions = vec![
            serde_json::json!({"id": "mis_b", "title": "B", "goal": "g", "updated_at": NOW}),
            serde_json::json!({"id": "mis_a", "title": "A", "goal": "g", "updated_at": NOW}),
        ];
        assert_eq!(forward(&s, NOW), forward(&s, NOW));
    }

    #[test]
    fn the_candidate_list_is_bounded() {
        let mut s = snap();
        s.active_missions = (0..40)
            .map(|i| {
                serde_json::json!({
                    "id": format!("mis_{i:02}"),
                    "title": format!("M{i}"),
                    "goal": "g",
                    "updated_at": "2026-01-01T00:00:00Z",
                })
            })
            .collect();
        assert!(forward(&s, NOW).candidates.len() <= MAX_CANDIDATES);
    }

    #[test]
    fn summary_note_names_the_top_candidate() {
        let v = forward(&snap(), NOW);
        let note = v.summary_note();
        assert!(note.starts_with("gaps="), "{note}");
        assert!(note.contains("top=state:"), "{note}");
    }
}
