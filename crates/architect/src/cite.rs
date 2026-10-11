//! The evidence vocabulary — one definition, shared by everything that labels
//! a claim.
//!
//! `docs/VISION.md` has published a four-state table since before the code had
//! a type for it, and `STATUS.md` recorded the gap ("No `EvidenceState` type
//! exists in code"). This module closes it, using VISION's names rather than
//! inventing a second set: a claim labelled one thing in the docs, another on
//! the wire and a third in the UI is exactly the drift `STATUS.md` exists to
//! catch.
//!
//! Two producers sit at different depths and both emit these states:
//!
//!   - the gateway's chat path, which knows only provenance — did a tool
//!     produce this during the turn, or did the model reason it out? That
//!     distinguishes [`EvidenceState::Observed`] from
//!     [`EvidenceState::Inferred`] and costs nothing.
//!   - the Architect's Verify stage, which resolves a claim's tokens against
//!     the Inspect snapshot and can therefore promote it to
//!     [`EvidenceState::Verified`] or demote it to
//!     [`EvidenceState::Unverified`].
//!
//! Keeping them on one enum is what makes the second an upgrade of the first
//! rather than a competing vocabulary.

use serde::{Deserialize, Serialize};

/// Where a cite points. These are the row families Inspect actually collects,
/// plus the two that exist only within a turn (`Tool`, `UserMessage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiteSource {
    Entity,
    Memory,
    Lesson,
    Mission,
    Task,
    Doc,
    /// A tool result produced during this turn.
    Tool,
    /// The operator's own message — quoting them back is not a claim about the
    /// world, and must not be treated as unresolved.
    UserMessage,
}

/// A resolved pointer from a claim to the row that backs it.
///
/// `excerpt` is carried rather than looked up later because the snapshot is a
/// point-in-time read: by the time anyone inspects the audit trail, the row may
/// have changed or been deleted, and a cite that cannot be rendered is not
/// evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiteRef {
    pub source: CiteSource,
    pub id: String,
    /// Which field of the row matched, when the row has more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Bounded snippet of the matching text, for rendering without a re-read.
    pub excerpt: String,
}

impl CiteRef {
    /// Build a cite, truncating `excerpt` to a bounded length.
    ///
    /// Cites travel into `audit_trail.payload`; an unbounded excerpt would let
    /// one verbose row dominate a whole turn's trace.
    pub fn new(source: CiteSource, id: impl Into<String>, excerpt: &str) -> Self {
        CiteRef {
            source,
            id: id.into(),
            field: None,
            excerpt: excerpt.chars().take(EXCERPT_CHARS).collect(),
        }
    }

    /// Same, naming the field that matched.
    pub fn with_field(
        source: CiteSource,
        id: impl Into<String>,
        field: impl Into<String>,
        excerpt: &str,
    ) -> Self {
        CiteRef {
            source,
            id: id.into(),
            field: Some(field.into()),
            excerpt: excerpt.chars().take(EXCERPT_CHARS).collect(),
        }
    }
}

/// Upper bound on a cite excerpt, in characters.
pub const EXCERPT_CHARS: usize = 240;

/// The evidence states published in `docs/VISION.md`, plus one.
///
/// The first four are VISION's table verbatim, in descending order of
/// groundedness. [`EvidenceState::NotApplicable`] is the one addition, and it
/// earns its place: without it a turn that makes no factual claim at all
/// ("Happy to help — shall I start there?") has zero resolved claims, which is
/// indistinguishable from a turn whose every claim failed to resolve. Verify
/// would then branch to ASK on pleasantries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceState {
    /// Resolved against workspace truth — entities, missions, memory, lessons,
    /// docs. May trigger side-effects under policy.
    Verified,
    /// Backed by a tool result or ingested source in this turn. May trigger
    /// side-effects under policy.
    Observed,
    /// Model reasoning, explicitly labelled. Plans, drafts and approval
    /// requests only — never a side-effect.
    Inferred,
    /// An unresolved claim. Never permits a side-effect.
    Unverified,
    /// Nothing here is a factual claim about the world.
    NotApplicable,
}

impl EvidenceState {
    /// The wire/UI token for this state. Snake_case, matching every other kind
    /// that crosses to desktop and mobile.
    pub fn as_str(self) -> &'static str {
        match self {
            EvidenceState::Verified => "verified",
            EvidenceState::Observed => "observed",
            EvidenceState::Inferred => "inferred",
            EvidenceState::Unverified => "unverified",
            EvidenceState::NotApplicable => "not_applicable",
        }
    }

    /// Whether VISION's table lets a claim in this state authorise a
    /// side-effect (still subject to the policy gate — this is the evidence
    /// precondition, not the decision).
    pub fn may_trigger_side_effects(self) -> bool {
        matches!(self, EvidenceState::Verified | EvidenceState::Observed)
    }

    /// The weaker of two states, used when one claim carries mixed support:
    /// a claim is only as grounded as its least-grounded part.
    pub fn weakest(self, other: EvidenceState) -> EvidenceState {
        // `Ord` follows declaration order, which is strongest-first, so the
        // weaker state is the larger one.
        if self >= other { self } else { other }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_tokens_match_the_vision_table() {
        // These strings cross to desktop and mobile; changing one silently is
        // how a chip renders blank on a phone.
        assert_eq!(EvidenceState::Verified.as_str(), "verified");
        assert_eq!(EvidenceState::Observed.as_str(), "observed");
        assert_eq!(EvidenceState::Inferred.as_str(), "inferred");
        assert_eq!(EvidenceState::Unverified.as_str(), "unverified");
        assert_eq!(EvidenceState::NotApplicable.as_str(), "not_applicable");
    }

    #[test]
    fn serde_round_trips_through_the_wire_token() {
        for s in [
            EvidenceState::Verified,
            EvidenceState::Observed,
            EvidenceState::Inferred,
            EvidenceState::Unverified,
            EvidenceState::NotApplicable,
        ] {
            let json = serde_json::to_string(&s).expect("serialise");
            assert_eq!(json, format!("\"{}\"", s.as_str()));
            let back: EvidenceState = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(back, s);
        }
    }

    #[test]
    fn only_verified_and_observed_may_act() {
        // VISION's table, as an assertion rather than a paragraph.
        assert!(EvidenceState::Verified.may_trigger_side_effects());
        assert!(EvidenceState::Observed.may_trigger_side_effects());
        assert!(!EvidenceState::Inferred.may_trigger_side_effects());
        assert!(!EvidenceState::Unverified.may_trigger_side_effects());
        assert!(!EvidenceState::NotApplicable.may_trigger_side_effects());
    }

    #[test]
    fn a_claim_is_only_as_grounded_as_its_weakest_part() {
        assert_eq!(
            EvidenceState::Verified.weakest(EvidenceState::Inferred),
            EvidenceState::Inferred
        );
        assert_eq!(
            EvidenceState::Inferred.weakest(EvidenceState::Verified),
            EvidenceState::Inferred
        );
        assert_eq!(
            EvidenceState::Observed.weakest(EvidenceState::Unverified),
            EvidenceState::Unverified
        );
        assert_eq!(
            EvidenceState::Verified.weakest(EvidenceState::Verified),
            EvidenceState::Verified
        );
    }

    #[test]
    fn cite_excerpts_are_bounded() {
        let long = "x".repeat(EXCERPT_CHARS * 3);
        let c = CiteRef::new(CiteSource::Memory, "mem_1", &long);
        assert_eq!(c.excerpt.chars().count(), EXCERPT_CHARS);
        assert!(c.field.is_none());

        let f = CiteRef::with_field(CiteSource::Entity, "ent_1", "goals", "ship by Friday");
        assert_eq!(f.field.as_deref(), Some("goals"));
        assert_eq!(f.excerpt, "ship by Friday");
    }

    #[test]
    fn an_absent_field_is_omitted_from_the_wire() {
        let c = CiteRef::new(CiteSource::Task, "task_1", "write the brief");
        let v = serde_json::to_value(&c).expect("serialise");
        assert!(
            v.get("field").is_none(),
            "an absent field must not ship as null: {v}"
        );
        assert_eq!(v["source"], "task");
    }
}
