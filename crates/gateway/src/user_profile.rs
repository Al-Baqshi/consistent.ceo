//! User-level profile: who the user is, what they want, and **how they work**.
//!
//! Entities have narrative (`profile`, `goals`, …); until now the *user* had
//! none. This module adds the missing layer: a short first-run conversation
//! captures identity + intent and infers a [`WorkingStyle`], which the client
//! then uses to shape the environment.
//!
//! Two shapes exist on purpose:
//!
//! - **Holistic** — many things moving at once. Full environment: every nav
//!   surface, multiple entities encouraged.
//! - **Linear** — one thing until it's done. Focused environment: a reduced
//!   nav, landing on `/today`, steered toward a single active entity.
//!
//! [`classify_working_style`] is deliberately **pure and offline**. No LLM sits
//! on the critical path of a user's very first interaction: an explicit choice
//! wins outright, and otherwise a deterministic keyword score decides. When the
//! signal is absent or evenly split it returns [`WorkingStyle::Unset`], which
//! renders as the *full* environment — we never hide a surface on a coin flip.
//!
//! Privacy: answers are personal data. Nothing in here logs answer contents,
//! and every read/write is keyed by the requesting user's id.

use serde::{Deserialize, Serialize};

/// How a user prefers to work. The one bit that reshapes the environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkingStyle {
    /// Not yet known (or deliberately skipped). Treated as "show everything".
    #[default]
    Unset,
    /// Many things in motion; wants the expanded environment.
    Holistic,
    /// One thing at a time; wants a focused environment.
    Linear,
}

impl WorkingStyle {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkingStyle::Unset => "unset",
            WorkingStyle::Holistic => "holistic",
            WorkingStyle::Linear => "linear",
        }
    }

    /// Parse a client-supplied style. Unknown strings are rejected rather than
    /// silently coerced, so a typo can't quietly shrink someone's sidebar.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "holistic" => Some(WorkingStyle::Holistic),
            "linear" => Some(WorkingStyle::Linear),
            "unset" | "" => Some(WorkingStyle::Unset),
            _ => None,
        }
    }
}

/// Raw answers to the first-run questions, kept verbatim so the profile can be
/// re-derived later (e.g. if classification improves) without re-asking.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingAnswers {
    /// Q1 — "Who are you, and what do you do?"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Q2 — "What do you want Consistent to do for you right now?"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    /// Q3 — free-text answer to the "many things vs one thing" question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<String>,
    /// Q3 — the explicit option the user tapped, if they tapped one.
    /// `"holistic"` | `"linear"`. This beats every other signal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus_choice: Option<String>,
    /// Q4 (optional) — "How much should it act on its own?" Stored as a stated
    /// preference only; it deliberately does **not** override any entity's
    /// `AutomationMode`, whose no-config default is already `AutoRun`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autonomy: Option<String>,
}

/// The per-user profile stored on the `auth_users` row.
///
/// Every field is `serde(default)` so rows written before this existed load
/// cleanly — the `auth_users` table is SCHEMALESS, so no migration is needed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Who they are / what they do (Q1, lightly trimmed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// What they want from the product right now (Q2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_intent: Option<String>,
    #[serde(default)]
    pub working_style: WorkingStyle,
    /// RFC3339. Unset means first-run hasn't happened — this, not localStorage,
    /// is what gates the first-run flow, so it survives a browser change and
    /// never nags a returning user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarded_at: Option<String>,
    /// Escape hatch: force the full nav even in the linear environment. Stored
    /// server-side so a focused user is never trapped on a new device either.
    #[serde(default)]
    pub show_all_nav: bool,
    #[serde(default)]
    pub answers: OnboardingAnswers,
}

impl UserProfile {
    /// Has the user been through (or explicitly skipped) first-run?
    pub fn is_onboarded(&self) -> bool {
        self.onboarded_at
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
    }
}

/// The exact questions asked at first run. Kept here so the server is the
/// source of truth for the wording and the client can render it verbatim.
///
/// `(key, question, helper)`
pub const ONBOARDING_QUESTIONS: &[(&str, &str, &str)] = &[
    (
        "identity",
        "Who are you, and what do you do?",
        "A sentence is plenty — your role, your work, what you're building.",
    ),
    (
        "intent",
        "What do you want Consistent to do for you right now?",
        "The thing on your mind today, not your five-year plan.",
    ),
    (
        "focus",
        "What matters more: keeping many things moving at once, or going deep on one thing until it's done?",
        "There's no wrong answer — it decides how much of the workspace you see.",
    ),
];

/// Phrases that indicate a linear, one-thing-at-a-time operator.
const LINEAR_MARKERS: &[&str] = &[
    "one thing",
    "one at a time",
    "one project",
    "one entity",
    "one business",
    "step by step",
    "start to finish",
    "deep",
    "deeply",
    "depth",
    "focus",
    "focused",
    "focusing",
    "finish",
    "finished",
    "finishing",
    "complete",
    "completion",
    "done",
    "single",
    "linear",
    "sequential",
    "narrow",
    "simple",
    "just one",
];

/// Phrases that indicate a holistic operator running many things in parallel.
const HOLISTIC_MARKERS: &[&str] = &[
    "many",
    "multiple",
    "several",
    "at once",
    "in parallel",
    "parallel",
    "juggle",
    "juggling",
    "everything",
    "lots",
    "loads",
    "broad",
    "big picture",
    "overview",
    "simultaneously",
    "holistic",
    "moving",
    "in motion",
    "wide",
    "all of it",
    "spinning plates",
    "context switch",
];

/// Lowercase and reduce to space-separated words, then pad with a leading and
/// trailing space. Padding lets a marker be matched as `" marker "`, which
/// gives whole-word matching for free (so "one" never fires on "phone").
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push(' ');
    let mut last_space = true;
    for ch in text.chars() {
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
    if !last_space {
        out.push(' ');
    }
    out
}

// `haystack` must already be `normalize`d (space-padded) for the whole-word match to work.
fn count_markers(haystack: &str, markers: &[&str]) -> usize {
    markers
        .iter()
        .filter(|m| haystack.contains(&format!(" {m} ")))
        .count()
}

/// Score one blob of text. Returns `None` when there is no signal or the signal
/// is evenly split — an honest "I don't know" rather than a guess.
fn score_text(text: &str) -> Option<WorkingStyle> {
    let norm = normalize(text);
    if norm.trim().is_empty() {
        return None;
    }
    let linear = count_markers(&norm, LINEAR_MARKERS);
    let holistic = count_markers(&norm, HOLISTIC_MARKERS);
    match linear.cmp(&holistic) {
        std::cmp::Ordering::Greater => Some(WorkingStyle::Linear),
        std::cmp::Ordering::Less => Some(WorkingStyle::Holistic),
        // Includes 0 vs 0 (no markers at all) and any tie.
        std::cmp::Ordering::Equal => None,
    }
}

/// Infer a working style from the first-run answers.
///
/// Precedence, highest first:
/// 1. An explicit option the user picked (Q3) — wins outright, always.
/// 2. Q3's free text — the question was written to elicit exactly this signal.
/// 3. Q1 + Q2 together — weaker, but "I juggle four businesses" is still a tell.
/// 4. [`WorkingStyle::Unset`] — the full environment, nothing hidden.
///
/// Pure and synchronous by design: it must work offline and instantly, because
/// it runs on the very first screen a user ever sees. An LLM may later *refine*
/// this, but must never be the only path to a result.
pub fn classify_working_style(answers: &OnboardingAnswers) -> WorkingStyle {
    // 1. An explicit pick is a statement, not a hint. Nothing overrides it.
    if let Some(raw) = answers.focus_choice.as_deref()
        && let Some(style) = WorkingStyle::parse(raw)
        && style != WorkingStyle::Unset
    {
        return style;
    }

    // 2. The question that was asked to determine exactly this.
    if let Some(focus) = answers.focus.as_deref()
        && let Some(style) = score_text(focus)
    {
        return style;
    }

    // 3. Weaker corroborating signal from how they described themselves.
    let corroborating = format!(
        "{} {}",
        answers.identity.as_deref().unwrap_or_default(),
        answers.intent.as_deref().unwrap_or_default()
    );
    if let Some(style) = score_text(&corroborating) {
        return style;
    }

    // 4. No idea — and that's fine. Show everything.
    WorkingStyle::Unset
}

/// Trim and cap a free-text answer before it is persisted.
pub fn tidy_answer(raw: &str, max: usize) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(max).collect())
}

/// Max length for any single stored free-text answer.
pub const ANSWER_MAX: usize = 2000;

/// Seed the user's Personal Space entity narrative from their first-run answers
/// so the AI has grounding from minute one.
///
/// Q1 ("who are you") becomes the entity `profile`; Q2 ("what do you want")
/// becomes its `goals` — the same two narrative fields entity creation fills.
/// Existing text is never clobbered: onboarding runs once, but a re-submit
/// must not wipe narrative the user has since edited by hand.
///
/// Best-effort. A failure here is never allowed to fail the onboarding request.
pub async fn seed_personal_space_narrative(
    control: &crate::state::ControlState,
    principal_id: &str,
    workspace_id: &str,
    answers: &OnboardingAnswers,
) {
    let identity = answers
        .identity
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let intent = answers
        .intent
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if identity.is_none() && intent.is_none() {
        return;
    }

    let target = crate::state::personal_space_id(workspace_id, principal_id);
    let mut changed = false;
    {
        let mut entities = control.entities.write().await;
        let Some(entity) = entities.iter_mut().find(|e| {
            e.id == target && e.is_personal && e.personal_user_id.as_deref() == Some(principal_id)
        }) else {
            return;
        };
        let empty = |field: &Option<String>| {
            field
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true)
        };
        if let Some(identity) = identity
            && empty(&entity.profile)
        {
            entity.profile = Some(identity.to_string());
            changed = true;
        }
        if let Some(intent) = intent
            && empty(&entity.goals)
        {
            entity.goals = Some(intent.to_string());
            changed = true;
        }
    }
    if !changed {
        return;
    }
    // Note: no answer text in any log line — this is personal data.
    if let Err(e) = control.sync_entities_to_store().await {
        tracing::warn!(error = %e, "failed to persist personal space narrative seed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answers(focus: &str) -> OnboardingAnswers {
        OnboardingAnswers {
            focus: Some(focus.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn holistic_free_text_is_classified_holistic() {
        assert_eq!(
            classify_working_style(&answers(
                "I need to keep many things moving at once across all my businesses"
            )),
            WorkingStyle::Holistic
        );
        assert_eq!(
            classify_working_style(&answers("juggling several projects in parallel")),
            WorkingStyle::Holistic
        );
    }

    #[test]
    fn linear_free_text_is_classified_linear() {
        assert_eq!(
            classify_working_style(&answers(
                "I want to go deep on one thing until it is finished"
            )),
            WorkingStyle::Linear
        );
        assert_eq!(
            classify_working_style(&answers("just one project, start to finish")),
            WorkingStyle::Linear
        );
    }

    #[test]
    fn ambiguous_input_stays_unset() {
        // No markers at all.
        assert_eq!(
            classify_working_style(&answers("not sure yet, ask me later")),
            WorkingStyle::Unset
        );
        // Evenly split — a tie must not become a guess.
        assert_eq!(
            classify_working_style(&answers("I like to focus but I have many projects")),
            WorkingStyle::Unset
        );
        // Nothing answered at all.
        assert_eq!(
            classify_working_style(&OnboardingAnswers::default()),
            WorkingStyle::Unset
        );
    }

    #[test]
    fn explicit_choice_wins_over_contradicting_text() {
        let a = OnboardingAnswers {
            // Text screams holistic...
            focus: Some("many things at once, everything in parallel".to_string()),
            identity: Some("I juggle multiple companies".to_string()),
            // ...but the user tapped "linear".
            focus_choice: Some("linear".to_string()),
            ..Default::default()
        };
        assert_eq!(classify_working_style(&a), WorkingStyle::Linear);

        let b = OnboardingAnswers {
            focus: Some("one thing, deep, until it is done".to_string()),
            focus_choice: Some("Holistic".to_string()), // case-insensitive
            ..Default::default()
        };
        assert_eq!(classify_working_style(&b), WorkingStyle::Holistic);
    }

    #[test]
    fn unparseable_choice_falls_through_to_text() {
        let a = OnboardingAnswers {
            focus: Some("one thing at a time".to_string()),
            focus_choice: Some("whatever".to_string()),
            ..Default::default()
        };
        assert_eq!(classify_working_style(&a), WorkingStyle::Linear);
    }

    #[test]
    fn identity_corroborates_when_focus_is_silent() {
        let a = OnboardingAnswers {
            identity: Some("I run several businesses and juggle many launches".to_string()),
            intent: Some("keep it all straight".to_string()),
            ..Default::default()
        };
        assert_eq!(classify_working_style(&a), WorkingStyle::Holistic);
    }

    #[test]
    fn markers_match_whole_words_only() {
        // "phone" must not trip the "one" family; "money" must not trip "one".
        let a = answers("my phone and my money");
        assert_eq!(classify_working_style(&a), WorkingStyle::Unset);
    }

    #[test]
    fn style_round_trips_through_strings() {
        for style in [
            WorkingStyle::Unset,
            WorkingStyle::Holistic,
            WorkingStyle::Linear,
        ] {
            assert_eq!(WorkingStyle::parse(style.as_str()), Some(style));
        }
        assert_eq!(WorkingStyle::parse("nonsense"), None);
    }

    #[test]
    fn profile_json_is_camel_case_and_defaults_fill_old_rows() {
        // An old row that predates the profile field.
        let old: UserProfile = serde_json::from_str("{}").expect("empty object loads");
        assert_eq!(old.working_style, WorkingStyle::Unset);
        assert!(!old.is_onboarded());

        let p = UserProfile {
            display_name: Some("Moe".to_string()),
            working_style: WorkingStyle::Linear,
            onboarded_at: Some("2026-08-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_value(&p).expect("serialize");
        assert_eq!(json["displayName"], "Moe");
        assert_eq!(json["workingStyle"], "linear");
        assert!(p.is_onboarded());
        // Absent optional fields are omitted, never emitted as null — the
        // Surreal SDK rejects a serialized `None`.
        assert!(json.get("role").is_none());
    }

    #[test]
    fn tidy_answer_trims_and_caps() {
        assert_eq!(tidy_answer("  hello  ", 100).as_deref(), Some("hello"));
        assert_eq!(tidy_answer("   ", 100), None);
        assert_eq!(tidy_answer("abcdef", 3).as_deref(), Some("abc"));
    }
}
