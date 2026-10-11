//! ⑤ Verify — the claim → cite resolver.
//!
//! Every factual claim in a draft must resolve to a row the system actually
//! holds (entity, memory, lesson, mission, task, document) or to a tool result
//! produced during this turn. A claim that resolves to nothing is
//! [`EvidenceState::Unverified`], and a turn carrying one branches to
//! [`TurnVerdict::Ask`] rather than answering.
//!
//! ## Why this is deterministic
//!
//! There is no model anywhere in this path, and the crate has no LLM
//! dependency — a property provable from `Cargo.toml` rather than from a
//! control-flow argument. That buys three things:
//!
//!   - it can run in shadow mode over real traffic at zero token cost, by
//!     resolving the answer the gateway *already* produced;
//!   - the same draft and snapshot always give the same verdict, so a replayed
//!     audit row still means something;
//!   - it is testable without a provider, a key or a network.
//!
//! ## What it deliberately does not do
//!
//! It does not understand the draft. It tests whether the *load-bearing tokens*
//! of a sentence — numbers, quoted strings, and proper nouns — appear in the
//! world the system read before answering. That is a weaker claim than
//! semantic entailment and a much stronger one than the keyword matching in
//! `gateway::verification`, which only looks for refusal phrases in the output
//! and never consults the world at all.
//!
//! The failure modes are known and chosen. A draft that invents a plausible
//! sentence using only words already in the snapshot passes (false negative).
//! A draft that correctly paraphrases a fact using a synonym absent from the
//! snapshot is flagged (false positive). Both are acceptable for a stage whose
//! output is a *question to the operator*, not a refusal: the cost of a false
//! positive is one clarifying question.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::ObservedFact;
use crate::cite::{CiteRef, CiteSource, EvidenceState};
use crate::inspect::Snapshot;

/// Tuning for claim extraction and resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyPolicy {
    /// Tokens shorter than this are never load-bearing. Keeps "is", "to", "AI"
    /// out of the hard-token set.
    pub min_token_len: usize,
    /// Words that look like proper nouns (capitalised mid-sentence) but carry
    /// no factual weight.
    pub stoplist: BTreeSet<String>,
    /// Upper bound on claims examined per draft, so a very long answer cannot
    /// make one turn's audit row unbounded.
    pub max_claims: usize,
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        VerifyPolicy {
            min_token_len: 4,
            stoplist: DEFAULT_STOPLIST
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            max_claims: 40,
        }
    }
}

/// Capitalised words that start sentences or name the system itself, and so
/// must not be mistaken for citable proper nouns.
const DEFAULT_STOPLIST: &[&str] = &[
    "this", "that", "these", "those", "there", "then", "they", "them", "their", "here", "what",
    "when", "where", "which", "while", "with", "would", "could", "should", "your", "yours", "you",
    "i've", "i'll", "consistent", "claude", "okay", "sure", "yes", "no", "and", "but", "for",
    "the", "from", "into", "about", "also", "just", "only", "once", "some", "any", "all",
];

/// One sentence of a draft, with the tokens that must resolve for it to count
/// as grounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// Position in the draft, so a verdict can be pointed back at a sentence.
    pub index: usize,
    pub text: String,
    /// Tokens that carry the factual weight. Empty means the sentence asserts
    /// nothing about the world.
    pub hard_tokens: Vec<String>,
}

/// What resolution concluded about one claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimVerdict {
    pub claim: Claim,
    pub state: EvidenceState,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cites: Vec<CiteRef>,
    /// Hard tokens that matched nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<String>,
}

/// Where the turn as a whole lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnVerdict {
    /// Every claim resolved, or there were none to resolve.
    Pass,
    /// At least one claim resolved to nothing — ask rather than assert.
    Ask,
    /// Verification itself could not run. Distinct from `Ask`: this says
    /// nothing about the draft, only that the index is unusable.
    Fail,
}

/// The whole result of verifying one draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyReport {
    pub verdict: TurnVerdict,
    pub claims: Vec<ClaimVerdict>,
    /// Suggested clarifying questions, one per unverified claim.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<String>,
    /// Claims that asserted something and were therefore tested.
    pub checked: usize,
    /// Of those, how many resolved to nothing.
    pub unverified: usize,
}

impl VerifyReport {
    /// One-line note for the audit trail.
    pub fn summary_note(&self) -> String {
        format!(
            "verdict={:?} checked={} unverified={}",
            self.verdict, self.checked, self.unverified
        )
    }
}

// ---------------------------------------------------------------------------
// Folding and tokenising
// ---------------------------------------------------------------------------

/// Lowercase, and reduce every non-alphanumeric run to a single space, so
/// "Q3-2026," and "q3 2026" fold together.
fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for ch in s.chars() {
        if ch.is_alphanumeric() {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim_end().to_string()
}

/// Every number-like token in a string, folded. Numbers are the tokens most
/// worth checking: a hallucinated figure is the costliest kind of wrong.
fn numbers_in(folded: &str) -> BTreeSet<String> {
    folded
        .split(' ')
        .filter(|t| t.chars().any(|c| c.is_ascii_digit()))
        .map(|t| t.to_string())
        .collect()
}

/// Does `haystack` contain `needle` as a whole space-delimited token run?
///
/// Substring matching would let "45" resolve against "1450" — the kind of
/// false confidence this stage exists to prevent.
fn contains_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    haystack
        .match_indices(needle)
        .any(|(at, _)| {
            let before_ok = at == 0 || haystack.as_bytes()[at - 1] == b' ';
            let end = at + needle.len();
            let after_ok = end == haystack.len() || haystack.as_bytes()[end] == b' ';
            before_ok && after_ok
        })
}

// ---------------------------------------------------------------------------
// The index
// ---------------------------------------------------------------------------

struct IndexedRow {
    cite: CiteRef,
    folded: String,
    numbers: BTreeSet<String>,
}

/// Everything the system knew before it answered, folded for lookup.
pub struct CiteIndex {
    rows: Vec<IndexedRow>,
}

/// Pull the citable text out of a JSON row: every string leaf, plus the id.
fn row_text(v: &serde_json::Value) -> String {
    fn walk(v: &serde_json::Value, out: &mut String) {
        match v {
            serde_json::Value::String(s) => {
                out.push(' ');
                out.push_str(s);
            }
            serde_json::Value::Number(n) => {
                out.push(' ');
                out.push_str(&n.to_string());
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            serde_json::Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = String::new();
    walk(v, &mut out);
    out
}

fn row_id(v: &serde_json::Value, fallback: &str) -> String {
    v.get("id")
        .and_then(|x| x.as_str())
        .unwrap_or(fallback)
        .to_string()
}

impl CiteIndex {
    /// Build the index from the snapshot Inspect produced plus whatever tools
    /// returned during this turn.
    ///
    /// The two are kept distinguishable on purpose: a claim backed by a
    /// snapshot row is [`EvidenceState::Verified`], one backed only by a tool
    /// result this turn is [`EvidenceState::Observed`]. That is VISION's
    /// distinction, not a new one.
    pub fn from_snapshot(snap: &Snapshot, observed: &[ObservedFact]) -> Self {
        let mut rows: Vec<IndexedRow> = Vec::new();

        let mut push = |cite: CiteRef, text: String| {
            let folded = fold(&text);
            if folded.is_empty() {
                return;
            }
            let numbers = numbers_in(&folded);
            rows.push(IndexedRow {
                cite,
                folded,
                numbers,
            });
        };

        if let Some(entity) = &snap.entity {
            let text = row_text(entity);
            push(
                CiteRef::new(CiteSource::Entity, row_id(entity, "entity"), &text),
                text,
            );
        }
        for (family, source, prefix) in [
            (&snap.recent_memories, CiteSource::Memory, "memory"),
            (&snap.recent_lessons, CiteSource::Lesson, "lesson"),
            (&snap.active_missions, CiteSource::Mission, "mission"),
            (&snap.open_tasks, CiteSource::Task, "task"),
            (&snap.entity_docs, CiteSource::Doc, "doc"),
        ] {
            for row in family.iter() {
                let text = row_text(row);
                push(CiteRef::new(source, row_id(row, prefix), &text), text);
            }
        }
        for fact in observed {
            let id = fact
                .call_id
                .clone()
                .unwrap_or_else(|| fact.tool.clone());
            push(
                CiteRef::with_field(CiteSource::Tool, id, &fact.tool, &fact.output),
                fact.output.clone(),
            );
        }

        CiteIndex { rows }
    }

    /// Rows in the index.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the index holds nothing.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Find a row backing `token`, preferring the most grounded source.
    ///
    /// `CiteSource` is ordered with workspace truth before within-turn results,
    /// so a fact present in both the entity and a tool result cites the entity.
    pub fn resolve(&self, token: &str) -> Option<&CiteRef> {
        let needle = fold(token);
        if needle.is_empty() {
            return None;
        }
        self.rows
            .iter()
            .filter(|r| r.numbers.contains(&needle) || contains_token(&r.folded, &needle))
            .min_by_key(|r| r.cite.source)
            .map(|r| &r.cite)
    }
}

// ---------------------------------------------------------------------------
// Claim extraction
// ---------------------------------------------------------------------------

/// Split a draft into sentences, and pick out each one's load-bearing tokens.
///
/// A token is *hard* when it is one of:
///   - a token containing an ASCII digit (a figure, a date, a version);
///   - a token appearing inside double quotes (the author flagged it as exact);
///   - a capitalised token that is not the first word of its sentence and is
///     not in the stoplist (a proper noun).
///
/// Everything else is ordinary prose, which this stage does not attempt to
/// check — see the module docs on what that trade buys.
pub fn extract_claims(draft: &str, policy: &VerifyPolicy) -> Vec<Claim> {
    let quoted = quoted_spans(draft);
    let mut claims = Vec::new();

    for (index, sentence) in split_sentences(draft).into_iter().enumerate() {
        if claims.len() >= policy.max_claims {
            break;
        }
        let trimmed = sentence.trim();
        if trimmed.is_empty() {
            continue;
        }

        let mut hard: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (word_pos, raw) in trimmed.split_whitespace().enumerate() {
            let word = raw.trim_matches(|c: char| !c.is_alphanumeric() && c != '-');
            if word.chars().count() < policy.min_token_len {
                // Numbers are load-bearing even when short ("12 tasks").
                if !word.chars().any(|c| c.is_ascii_digit()) {
                    continue;
                }
            }
            let folded = fold(word);
            if folded.is_empty() || policy.stoplist.contains(&folded) {
                continue;
            }

            let has_digit = word.chars().any(|c| c.is_ascii_digit());
            let is_quoted = quoted.contains(&folded);
            let is_proper = word_pos > 0
                && word
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_uppercase());

            if (has_digit || is_quoted || is_proper) && seen.insert(folded.clone()) {
                hard.push(folded);
            }
        }

        claims.push(Claim {
            index,
            text: trimmed.to_string(),
            hard_tokens: hard,
        });
    }
    claims
}

/// Folded tokens that appeared inside double quotes anywhere in the draft.
fn quoted_spans(draft: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut inside = false;
    let mut buf = String::new();
    for ch in draft.chars() {
        // Straight and typographic quotes alike.
        if ch == '"' || ch == '\u{201C}' || ch == '\u{201D}' {
            if inside {
                for tok in fold(&buf).split(' ') {
                    if !tok.is_empty() {
                        out.insert(tok.to_string());
                    }
                }
                buf.clear();
            }
            inside = !inside;
            continue;
        }
        if inside {
            buf.push(ch);
        }
    }
    out
}

/// Split on sentence-ending punctuation followed by whitespace, and on line
/// breaks so list items count as their own claims.
///
/// Deliberately simple: a decimal like "3.5" keeps its sentence because the
/// character after the period is not whitespace.
fn split_sentences(draft: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = draft.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\n' {
            if !cur.trim().is_empty() {
                out.push(std::mem::take(&mut cur));
            } else {
                cur.clear();
            }
            continue;
        }
        cur.push(ch);
        if matches!(ch, '.' | '!' | '?')
            && chars
                .peek()
                .is_some_and(|n| n.is_whitespace())
            && !cur.trim().is_empty()
        {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Resolve every claim against the index.
///
/// `snapshot_rows` is how many rows Inspect actually found. It is what
/// separates "this entity genuinely has no history yet" (index empty, snapshot
/// empty — a normal first turn, which must still [`TurnVerdict::Pass`]) from
/// "the index failed to build" (index empty, snapshot non-empty — which is a
/// bug in this stage and must not be reported as the draft's fault).
pub fn verify(
    claims: Vec<Claim>,
    index: &CiteIndex,
    snapshot_rows: usize,
    _policy: &VerifyPolicy,
) -> VerifyReport {
    if index.is_empty() && snapshot_rows > 0 {
        return VerifyReport {
            verdict: TurnVerdict::Fail,
            claims: Vec::new(),
            questions: Vec::new(),
            checked: 0,
            unverified: 0,
        };
    }

    let mut verdicts = Vec::with_capacity(claims.len());
    let mut checked = 0usize;
    let mut unverified = 0usize;
    let mut questions = Vec::new();

    for claim in claims {
        if claim.hard_tokens.is_empty() {
            // Asserts nothing about the world — a greeting, an offer, a question.
            verdicts.push(ClaimVerdict {
                claim,
                state: EvidenceState::NotApplicable,
                cites: Vec::new(),
                unresolved: Vec::new(),
            });
            continue;
        }

        checked += 1;
        let mut cites: BTreeMap<(CiteSource, String), CiteRef> = BTreeMap::new();
        let mut unresolved = Vec::new();
        for token in &claim.hard_tokens {
            match index.resolve(token) {
                Some(c) => {
                    cites.insert((c.source, c.id.clone()), c.clone());
                }
                None => unresolved.push(token.clone()),
            }
        }

        let state = if !unresolved.is_empty() {
            unverified += 1;
            questions.push(question_for(&claim, &unresolved));
            EvidenceState::Unverified
        } else if cites.keys().all(|(s, _)| *s == CiteSource::Tool) {
            // Backed only by what a tool returned during this turn.
            EvidenceState::Observed
        } else {
            EvidenceState::Verified
        };

        verdicts.push(ClaimVerdict {
            claim,
            state,
            cites: cites.into_values().collect(),
            unresolved,
        });
    }

    let verdict = if unverified > 0 {
        TurnVerdict::Ask
    } else {
        TurnVerdict::Pass
    };

    VerifyReport {
        verdict,
        claims: verdicts,
        questions,
        checked,
        unverified,
    }
}

/// Turn an unresolved claim into the question the system should ask instead of
/// asserting it.
fn question_for(claim: &Claim, unresolved: &[String]) -> String {
    let subject = unresolved.join(", ");
    let excerpt: String = claim.text.chars().take(120).collect();
    format!("I can't back {subject:?} with anything on file — is that right? (from: {excerpt:?})")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_with(entity: serde_json::Value, docs: Vec<serde_json::Value>) -> Snapshot {
        Snapshot {
            workspace_id: Some("ws".into()),
            entity_id: Some("ent_1".into()),
            entity: Some(entity),
            recent_memories: Vec::new(),
            recent_lessons: Vec::new(),
            active_missions: Vec::new(),
            open_tasks: Vec::new(),
            entity_docs: docs,
            inspected_at: "2026-09-12T00:00:00Z".into(),
        }
    }

    fn empty_snap() -> Snapshot {
        Snapshot {
            workspace_id: None,
            entity_id: None,
            entity: None,
            recent_memories: Vec::new(),
            recent_lessons: Vec::new(),
            active_missions: Vec::new(),
            open_tasks: Vec::new(),
            entity_docs: Vec::new(),
            inspected_at: "2026-09-12T00:00:00Z".into(),
        }
    }

    fn run(draft: &str, snap: &Snapshot, observed: &[ObservedFact]) -> VerifyReport {
        let policy = VerifyPolicy::default();
        let index = CiteIndex::from_snapshot(snap, observed);
        let rows = snapshot_row_count(snap);
        verify(extract_claims(draft, &policy), &index, rows, &policy)
    }

    fn snapshot_row_count(s: &Snapshot) -> usize {
        usize::from(s.entity.is_some())
            + s.recent_memories.len()
            + s.recent_lessons.len()
            + s.active_missions.len()
            + s.open_tasks.len()
            + s.entity_docs.len()
    }

    #[test]
    fn a_pleasantry_asserts_nothing_and_passes() {
        // Without NotApplicable this would be an unresolved claim and Verify
        // would branch to ASK on every greeting.
        let r = run("Happy to help — shall I start there?", &empty_snap(), &[]);
        assert_eq!(r.verdict, TurnVerdict::Pass);
        assert_eq!(r.checked, 0);
        assert!(
            r.claims
                .iter()
                .all(|c| c.state == EvidenceState::NotApplicable)
        );
    }

    #[test]
    fn a_figure_that_is_in_the_world_verifies() {
        let snap = snap_with(
            serde_json::json!({ "id": "ent_1", "name": "BitRegalo", "goals": "Ship 3 features" }),
            vec![],
        );
        let r = run("BitRegalo plans to ship 3 features.", &snap, &[]);
        assert_eq!(r.verdict, TurnVerdict::Pass, "{:?}", r.claims);
        assert_eq!(r.unverified, 0);
        assert!(r.claims.iter().any(|c| c.state == EvidenceState::Verified));
    }

    #[test]
    fn an_invented_figure_is_unverified_and_becomes_a_question() {
        let snap = snap_with(
            serde_json::json!({ "id": "ent_1", "name": "BitRegalo", "goals": "Ship 3 features" }),
            vec![],
        );
        let r = run("BitRegalo shipped 47 features last quarter.", &snap, &[]);
        assert_eq!(r.verdict, TurnVerdict::Ask);
        assert_eq!(r.unverified, 1);
        assert_eq!(r.questions.len(), 1);
        assert!(r.questions[0].contains("47"), "{}", r.questions[0]);
    }

    #[test]
    fn a_number_must_match_as_a_whole_token_not_a_substring() {
        // "1450" must not vouch for "45" — that is exactly the false
        // confidence this stage exists to prevent.
        let snap = snap_with(
            serde_json::json!({ "id": "ent_1", "name": "Acme", "goals": "Reach 1450 users" }),
            vec![],
        );
        let r = run("Acme has 45 users.", &snap, &[]);
        assert_eq!(r.verdict, TurnVerdict::Ask, "{:?}", r.claims);
    }

    #[test]
    fn a_tool_result_this_turn_is_observed_not_verified() {
        let observed = [ObservedFact {
            tool: "list_entities".into(),
            call_id: Some("call_1".into()),
            output: "Skirting World, 12 open tasks".into(),
        }];
        let r = run("Skirting World has 12 open tasks.", &empty_snap(), &observed);
        assert_eq!(r.verdict, TurnVerdict::Pass);
        let states: Vec<_> = r
            .claims
            .iter()
            .filter(|c| c.state != EvidenceState::NotApplicable)
            .map(|c| c.state)
            .collect();
        assert_eq!(states, vec![EvidenceState::Observed], "{:?}", r.claims);
    }

    #[test]
    fn workspace_truth_outranks_a_tool_result_for_the_same_fact() {
        let snap = snap_with(
            serde_json::json!({ "id": "ent_1", "name": "Skirting", "goals": "Reach 12 shops" }),
            vec![],
        );
        let observed = [ObservedFact {
            tool: "list_entities".into(),
            call_id: None,
            output: "Skirting 12".into(),
        }];
        let r = run("Skirting is at 12 shops.", &snap, &observed);
        let cited: Vec<CiteSource> = r
            .claims
            .iter()
            .flat_map(|c| c.cites.iter().map(|x| x.source))
            .collect();
        assert!(
            cited.contains(&CiteSource::Entity),
            "should prefer workspace truth: {cited:?}"
        );
    }

    #[test]
    fn a_quoted_string_is_load_bearing_even_without_digits_or_capitals() {
        let snap = snap_with(serde_json::json!({ "id": "ent_1", "name": "Acme" }), vec![]);
        let r = run("The policy says \"quarterly rebate\" applies.", &snap, &[]);
        assert_eq!(r.verdict, TurnVerdict::Ask, "{:?}", r.claims);
        assert!(r.claims.iter().any(|c| !c.unresolved.is_empty()));
    }

    #[test]
    fn a_document_body_can_back_a_claim() {
        let snap = snap_with(
            serde_json::json!({ "id": "ent_1", "name": "Acme" }),
            vec![serde_json::json!({
                "id": "doc_1",
                "title": "Rebate policy",
                "content": "A quarterly rebate applies to orders over 500 units.",
            })],
        );
        let r = run("A \"quarterly rebate\" applies over 500 units.", &snap, &[]);
        assert_eq!(r.verdict, TurnVerdict::Pass, "{:?}", r.claims);
        assert!(
            r.claims
                .iter()
                .flat_map(|c| c.cites.iter())
                .any(|c| c.source == CiteSource::Doc)
        );
    }

    #[test]
    fn an_empty_world_with_an_empty_index_still_passes() {
        // A genuine first turn must not be reported as a verification failure.
        let r = run("Happy to help.", &empty_snap(), &[]);
        assert_eq!(r.verdict, TurnVerdict::Pass);
    }

    #[test]
    fn an_index_that_failed_to_build_reports_fail_not_ask() {
        // Index empty while the snapshot had rows: a bug in this stage, and it
        // must not be blamed on the draft.
        let policy = VerifyPolicy::default();
        let index = CiteIndex { rows: Vec::new() };
        let claims = extract_claims("Acme shipped 47 features.", &policy);
        let r = verify(claims, &index, 9, &policy);
        assert_eq!(r.verdict, TurnVerdict::Fail);
        assert_eq!(r.checked, 0, "Fail says nothing about the claims");
    }

    #[test]
    fn the_same_draft_and_snapshot_always_give_the_same_verdict() {
        let snap = snap_with(
            serde_json::json!({ "id": "ent_1", "name": "Acme", "goals": "Ship 3" }),
            vec![],
        );
        let a = run("Acme will ship 3 things, then 9 more.", &snap, &[]);
        let b = run("Acme will ship 3 things, then 9 more.", &snap, &[]);
        assert_eq!(a, b);
    }

    #[test]
    fn a_decimal_does_not_split_a_sentence() {
        let claims = extract_claims("Version 3.5 ships Friday.", &VerifyPolicy::default());
        let real: Vec<_> = claims.iter().filter(|c| !c.text.is_empty()).collect();
        assert_eq!(real.len(), 1, "{:?}", claims);
    }

    #[test]
    fn list_items_are_separate_claims() {
        let claims = extract_claims(
            "Here is the plan\n- Ship 3 features\n- Hire 2 people",
            &VerifyPolicy::default(),
        );
        assert!(claims.len() >= 3, "{claims:?}");
    }

    #[test]
    fn claim_extraction_is_bounded() {
        let long = "Acme shipped 7 things. ".repeat(500);
        let policy = VerifyPolicy::default();
        assert!(extract_claims(&long, &policy).len() <= policy.max_claims);
    }

    #[test]
    fn folding_makes_punctuation_and_case_irrelevant() {
        assert_eq!(fold("Q3-2026,"), "q3 2026");
        assert_eq!(fold("  Hello,   World!  "), "hello world");
        assert!(contains_token("ship 3 features", "3"));
        assert!(!contains_token("ship 1450 users", "45"));
    }
}
