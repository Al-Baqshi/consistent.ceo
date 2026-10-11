//! Default heartbeat plan for every entity.
//!
//! Pillar 1 shipped the heartbeat *mechanism* (cadence rows + ticker +
//! dispatcher) but left the rows to be created by hand, so a freshly created
//! entity had no pulse at all until someone POSTed to `/api/heartbeats`. This
//! module closes that gap: **every entity gets a default operating rhythm at
//! creation time**, derived from what the entity actually is and from whatever
//! the owner told us during onboarding.
//!
//! The default plan is three beats, matching how an operator actually runs a
//! thing rather than one undifferentiated "ping":
//!
//! | intent     | job                                                        |
//! |------------|------------------------------------------------------------|
//! | `Reminder` | the daily pulse — what needs attention *right now*          |
//! | `Review`   | the weekly process check — is the goal actually moving      |
//! | `Ideas`    | the horizon scan — options the owner hasn't considered yet  |
//!
//! Plus a fourth that replaces the daily pulse while an entity is still
//! half-configured:
//!
//! | `Setup`    | ask the single most valuable missing question, then stop    |
//!
//! ### Deriving the plan
//! [`classify`] reads the entity's own narrative (name, type, department,
//! profile, goals, stack fields, content pipeline, domains) and picks an
//! [`EntityArchetype`]. The archetype decides **which director owns each beat**
//! and **when it fires** — a legal entity does not want a 08:00 daily standup,
//! a content entity does not want its ideas beat routed to an engineer. The
//! entity's [`crate::state::EntityReadiness`] decides whether the daily slot is
//! a pulse or a setup question.
//!
//! ### Where the per-beat brief lives
//! [`crate::state::Heartbeat`] has no dedicated "what is this beat for" field,
//! and adding one means touching the persistence projection + its completeness
//! guard. Instead the brief rides in the existing free-text `condition` column
//! under a parseable prefix:
//!
//! ```text
//! beat:reminder:daily-pulse | Blockers first, then the one change that moves the goal.
//! ```
//!
//! [`parse_beat_condition`] reads it back on the dispatch side, so the beat
//! knows its own job and the model gets an intent-shaped instruction instead of
//! the same generic paragraph three times a week.
//!
//! ### Provisioning contract
//! - Defaults are a **floor, not a source of truth** (same contract as
//!   [`crate::director_seed`]): if an entity already has *any* heartbeat row,
//!   provisioning is a no-op and never touches the user's edits.
//! - An entity whose runtime config sets `heartbeat_mode = Off` is skipped
//!   entirely — that flag is the explicit opt-out.
//! - Ids are deterministic (`hb_{entity}_{slug}`) so a re-run upserts in place
//!   rather than piling up duplicates.

use anyhow::Result;
use chrono::Utc;

use crate::entity_runtime::effective_heartbeat_mode;
use crate::heartbeat_store::HeartbeatStore;
use crate::state::{Entity, EntityHeartbeatMode, Heartbeat};

/// What a given beat is *for*. Drives the instruction the director receives
/// when the beat fires (see [`crate::director_runner::heartbeat_user_prompt`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeatIntent {
    /// Entity is still half-configured: ask the highest-value missing question.
    Setup,
    /// Daily pulse — what needs attention right now.
    Reminder,
    /// Periodic process check — is the goal actually moving.
    Review,
    /// Horizon scan — options the owner hasn't considered yet.
    Ideas,
    /// Push a loop-enabled mission forward: read the last round's results and
    /// queue the next iteration's executable work. The only beat intent that
    /// causes work to happen rather than prose to be written.
    Drive,
}

impl BeatIntent {
    pub fn tag(self) -> &'static str {
        match self {
            BeatIntent::Setup => "setup",
            BeatIntent::Reminder => "reminder",
            BeatIntent::Review => "review",
            BeatIntent::Ideas => "ideas",
            BeatIntent::Drive => "drive",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag.trim().to_ascii_lowercase().as_str() {
            "setup" => Some(BeatIntent::Setup),
            "reminder" => Some(BeatIntent::Reminder),
            "review" => Some(BeatIntent::Review),
            "ideas" => Some(BeatIntent::Ideas),
            "drive" => Some(BeatIntent::Drive),
            _ => None,
        }
    }
}

/// Coarse entity shape used to route beats to the right director and pick a
/// cadence that fits the work. Deliberately small — this is a routing hint,
/// not a taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityArchetype {
    /// Software / product being built.
    Product,
    /// Media, publishing, audience.
    Content,
    /// Pipeline, outbound, marketing.
    GrowthSales,
    /// Client or account delivery work.
    Client,
    /// Legal, compliance, contracts.
    Legal,
    /// A user's private Personal Space.
    Personal,
    /// Everything else — a business/initiative run on general operations.
    Venture,
}

impl EntityArchetype {
    pub fn label(self) -> &'static str {
        match self {
            EntityArchetype::Product => "product",
            EntityArchetype::Content => "content",
            EntityArchetype::GrowthSales => "growth",
            EntityArchetype::Client => "client",
            EntityArchetype::Legal => "legal",
            EntityArchetype::Personal => "personal",
            EntityArchetype::Venture => "venture",
        }
    }
}

/// One planned beat, before it becomes a persisted [`Heartbeat`].
#[derive(Debug, Clone)]
pub struct DefaultBeat {
    /// Stable suffix for the deterministic row id. Never reuse across intents.
    pub slug: &'static str,
    pub name: String,
    /// Director reference. Group ids (`operations`, `content-creative`, …) are
    /// accepted — the dispatcher resolves id → slug → first active director of
    /// the group.
    pub director_id: &'static str,
    /// 6-field cron (`sec min hour dom mon dow`), UTC — matches what
    /// [`crate::heartbeat_store::next_fire_at`] feeds to the `cron` crate.
    pub cron: &'static str,
    pub intent: BeatIntent,
    /// Standing brief: what this beat should produce every time it fires.
    pub focus: String,
}

impl DefaultBeat {
    /// Encode intent + slug + brief into the `condition` column.
    pub fn condition(&self) -> String {
        format!(
            "beat:{}:{} | {}",
            self.intent.tag(),
            self.slug,
            self.focus.trim()
        )
    }
}

/// The brief recovered from a heartbeat's `condition` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeatCondition {
    pub intent: BeatIntent,
    pub slug: String,
    pub focus: Option<String>,
}

/// Parse a `condition` written by [`DefaultBeat::condition`]. Returns `None`
/// for hand-written conditions that don't use the `beat:` prefix, so
/// user-created heartbeats keep the old generic behaviour.
pub fn parse_beat_condition(condition: Option<&str>) -> Option<BeatCondition> {
    let raw = condition?.trim();
    let rest = raw.strip_prefix("beat:")?;
    let (head, focus) = match rest.split_once('|') {
        Some((h, f)) => (
            h.trim(),
            Some(f.trim().to_string()).filter(|s| !s.is_empty()),
        ),
        None => (rest.trim(), None),
    };
    let (intent_tag, slug) = head.split_once(':')?;
    Some(BeatCondition {
        intent: BeatIntent::from_tag(intent_tag)?,
        slug: slug.trim().to_string(),
        focus,
    })
}

/// Deterministic row id so re-provisioning upserts in place.
pub fn beat_id(entity_id: &str, slug: &str) -> String {
    let sanitized: String = entity_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("hb_{sanitized}_{slug}")
}

/// Lowercased haystack of everything the owner told us about this entity.
fn narrative_haystack(entity: &Entity) -> String {
    let mut parts: Vec<&str> = vec![&entity.name, &entity.dept, &entity.status];
    parts.extend(
        [
            entity.description.as_deref(),
            entity.department.as_deref(),
            entity.entity_type.as_deref(),
            entity.profile.as_deref(),
            entity.goals.as_deref(),
            entity.active_context.as_deref(),
            entity.stage.as_deref(),
            entity.operating_rules.as_deref(),
            entity.content_pipeline.as_deref(),
            entity.setup.what_it_is.as_deref(),
            entity.setup.primary_goal.as_deref(),
            entity.setup.success_criteria.as_deref(),
        ]
        .into_iter()
        .flatten(),
    );
    let mut hay = parts.join(" ").to_ascii_lowercase();
    for extra in entity.domains.iter().chain(entity.markets.iter()) {
        hay.push(' ');
        hay.push_str(&extra.to_ascii_lowercase());
    }
    hay
}

fn hits(haystack: &str, needles: &[&str]) -> usize {
    needles.iter().filter(|n| haystack.contains(**n)).count()
}

/// Pick the archetype that decides director routing + cadence.
///
/// Order matters: explicit structural signals (personal flag, a declared
/// stack) beat keyword scoring, because they're facts rather than vocabulary.
pub fn classify(entity: &Entity) -> EntityArchetype {
    if entity.is_personal || entity.entity_type.as_deref() == Some("personal") {
        return EntityArchetype::Personal;
    }

    let hay = narrative_haystack(entity);

    // A declared stack or repo is the strongest "this is a product being
    // built" signal there is — no keyword can outrank it.
    let has_stack = [
        entity.setup.backend_stack.as_deref(),
        entity.setup.frontend_stack.as_deref(),
        entity.setup.database_stack.as_deref(),
        entity.setup.repo_path.as_deref(),
    ]
    .iter()
    .any(|v| v.map(str::trim).is_some_and(|s| !s.is_empty()));
    if has_stack {
        return EntityArchetype::Product;
    }

    let scores = [
        (
            EntityArchetype::Product,
            hits(
                &hay,
                &[
                    "product", "app", "platform", "software", "saas", "engineer", "build", "ship",
                    "api", "codebase", "repo", "feature", "backend", "frontend", "mvp",
                ],
            ),
        ),
        (
            EntityArchetype::Content,
            hits(
                &hay,
                &[
                    "content",
                    "newsletter",
                    "blog",
                    "podcast",
                    "video",
                    "audience",
                    "editorial",
                    "publish",
                    "media",
                    "brand",
                    "social",
                    "youtube",
                    "writing",
                ],
            ),
        ),
        (
            EntityArchetype::GrowthSales,
            hits(
                &hay,
                &[
                    "sales",
                    "pipeline",
                    "lead",
                    "outbound",
                    "revenue",
                    "marketing",
                    "growth",
                    "campaign",
                    "conversion",
                    "crm",
                    "deal",
                    "prospect",
                ],
            ),
        ),
        (
            EntityArchetype::Client,
            hits(
                &hay,
                &[
                    "client",
                    "account",
                    "customer",
                    "delivery",
                    "retainer",
                    "agency",
                    "project",
                    "contract work",
                    "consult",
                    "stakeholder",
                ],
            ),
        ),
        (
            EntityArchetype::Legal,
            hits(
                &hay,
                &[
                    "legal",
                    "compliance",
                    "contract",
                    "regulat",
                    "licence",
                    "license",
                    "policy",
                    "gdpr",
                    "counsel",
                    "litigation",
                ],
            ),
        ),
    ];

    let best = scores.iter().max_by_key(|(_, n)| *n);
    match best {
        Some((archetype, n)) if *n > 0 => *archetype,
        _ => EntityArchetype::Venture,
    }
}

/// True while the entity is too thin for a meaningful daily pulse — the daily
/// slot becomes a [`BeatIntent::Setup`] question instead.
fn needs_setup(entity: &Entity) -> bool {
    let has_goal = entity
        .goals
        .as_deref()
        .or(entity.setup.primary_goal.as_deref())
        .map(str::trim)
        .is_some_and(|s| !s.is_empty());
    entity.readiness.percent < 40 || !has_goal
}

/// Director + cron for each beat, by archetype.
///
/// Times are UTC. Legal deliberately has no daily beat — a compliance entity
/// that pings every morning trains the owner to ignore it.
fn slots(
    archetype: EntityArchetype,
) -> (
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
    &'static str,
) {
    // (pulse_director, pulse_cron, review_director, review_cron, ideas_director, ideas_cron)
    match archetype {
        EntityArchetype::Product => (
            "full-stack-development",
            "0 0 8 * * *",
            "strategy-partner",
            "0 30 8 * * MON",
            "strategy-partner",
            "0 0 15 * * FRI",
        ),
        EntityArchetype::Content => (
            "content-creative",
            "0 0 8 * * *",
            "content-creative",
            "0 30 8 * * MON",
            "content-creative",
            "0 0 15 * * FRI",
        ),
        EntityArchetype::GrowthSales => (
            "sales-marketing",
            "0 0 8 * * *",
            "sales-marketing",
            "0 30 8 * * MON",
            "strategy-partner",
            "0 0 15 * * FRI",
        ),
        EntityArchetype::Client => (
            "operations",
            "0 0 8 * * *",
            "operations",
            "0 30 8 * * MON",
            "strategy-partner",
            "0 0 15 * * FRI",
        ),
        EntityArchetype::Legal => (
            "law",
            "0 0 9 * * TUE",
            "law",
            "0 30 8 * * MON",
            "strategy-partner",
            "0 0 15 * * FRI",
        ),
        EntityArchetype::Personal => (
            "operations",
            "0 30 7 * * *",
            "operations",
            "0 0 17 * * SUN",
            "strategy-partner",
            "0 0 15 * * FRI",
        ),
        EntityArchetype::Venture => (
            "operations",
            "0 0 8 * * *",
            "strategy-partner",
            "0 30 8 * * MON",
            "strategy-partner",
            "0 0 15 * * FRI",
        ),
    }
}

fn pulse_focus(archetype: EntityArchetype) -> &'static str {
    match archetype {
        EntityArchetype::Product => {
            "Blockers first, then the single highest-leverage change that moves the goal today."
        }
        EntityArchetype::Content => {
            "What is due to go out, what is half-finished, and the one piece worth finishing today."
        }
        EntityArchetype::GrowthSales => {
            "Who is waiting on a reply, what has gone cold, and the one follow-up worth sending today."
        }
        EntityArchetype::Client => {
            "Open commitments to this account, anything slipping, and the next thing owed to them."
        }
        EntityArchetype::Legal => {
            "Obligations coming due, unsigned or unreviewed documents, and exposure that is growing."
        }
        EntityArchetype::Personal => {
            "Loose threads from yesterday and the one thing worth protecting time for today."
        }
        EntityArchetype::Venture => {
            "What is stalled, what is owed to someone else, and the one action that moves the goal today."
        }
    }
}

fn review_focus(archetype: EntityArchetype) -> &'static str {
    match archetype {
        EntityArchetype::Product => {
            "Did the build actually move the goal this week? Name what shipped, what slipped, and the correction."
        }
        EntityArchetype::Legal => {
            "Review the week's obligations and risk posture. Name what closed, what is still open, and the correction."
        }
        EntityArchetype::Personal => {
            "Look back across the week: what got done, what kept being deferred, and what to drop or reschedule."
        }
        _ => {
            "Did this week move the goal? Name what progressed, what stalled, and the one correction for next week."
        }
    }
}

fn ideas_focus(archetype: EntityArchetype) -> &'static str {
    match archetype {
        EntityArchetype::Content => {
            "Two or three angles or formats worth trying next, given what already landed."
        }
        EntityArchetype::Legal => {
            "Two or three ways to reduce standing risk or cut process cost, given the current posture."
        }
        _ => {
            "Two or three options the owner has not considered yet, each with the first concrete step."
        }
    }
}

/// Build the default plan for an entity. Pure — no store, no clock.
pub fn default_beat_plan(entity: &Entity) -> Vec<DefaultBeat> {
    let archetype = classify(entity);
    let (pulse_dir, pulse_cron, review_dir, review_cron, ideas_dir, ideas_cron) = slots(archetype);
    let name = entity.name.trim();
    let mut plan = Vec::with_capacity(3);

    if needs_setup(entity) {
        // Half-configured entity: the daily slot asks for the missing piece
        // instead of pretending to run a standup over an empty record. The
        // ideas beat is withheld until there is a goal to have ideas about.
        let question = entity
            .readiness
            .next_question
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .unwrap_or("What is the main goal for this entity?");
        plan.push(DefaultBeat {
            slug: "setup-brief",
            name: format!("{name} — setup"),
            director_id: pulse_dir,
            cron: "0 0 9 * * *",
            intent: BeatIntent::Setup,
            focus: format!(
                "Ask exactly one question and stop: {question} \
                 Say in one line why the answer changes what this entity can do."
            ),
        });
    } else {
        plan.push(DefaultBeat {
            slug: "daily-pulse",
            name: format!("{name} — daily pulse"),
            director_id: pulse_dir,
            cron: pulse_cron,
            intent: BeatIntent::Reminder,
            focus: pulse_focus(archetype).to_string(),
        });
        plan.push(DefaultBeat {
            slug: "ideas-scan",
            name: format!("{name} — ideas"),
            director_id: ideas_dir,
            cron: ideas_cron,
            intent: BeatIntent::Ideas,
            focus: ideas_focus(archetype).to_string(),
        });
    }

    // The weekly review runs regardless of readiness — an entity that has been
    // sitting half-configured for a week is itself the thing worth reviewing.
    plan.push(DefaultBeat {
        slug: "weekly-review",
        name: format!("{name} — weekly review"),
        director_id: review_dir,
        cron: review_cron,
        intent: BeatIntent::Review,
        focus: review_focus(archetype).to_string(),
    });

    plan
}

/// Materialise the plan as persistable rows. `next_fire_at` is left unset so
/// the ticker treats each beat as due on the next pass and then settles onto
/// its cron slot — the entity gets a first pulse immediately after creation
/// rather than waiting up to a day for the first cron boundary. The remaining
/// beats are pre-scheduled onto their first real slot so creation doesn't fire
/// the whole plan at once.
pub fn default_heartbeats_for_entity(
    entity: &Entity,
    workspace_id: Option<&str>,
) -> Vec<Heartbeat> {
    let started = Utc::now();
    let now = started.to_rfc3339();
    let workspace_id = workspace_id
        .map(str::to_string)
        .or_else(|| entity.workspace_id.clone());
    default_beat_plan(entity)
        .into_iter()
        .enumerate()
        .map(|(index, beat)| {
            let mut hb = Heartbeat {
                id: beat_id(&entity.id, beat.slug),
                workspace_id: workspace_id.clone(),
                name: beat.name.clone(),
                enabled: true,
                target_kind: "entity".to_string(),
                target_id: Some(entity.id.clone()),
                director_id: beat.director_id.to_string(),
                cron: Some(beat.cron.to_string()),
                cadence_secs: None,
                condition: Some(beat.condition()),
                last_fired_at: None,
                // Unset = due on the next tick. Only the lead beat wants that.
                next_fire_at: None,
                lease_until: None,
                lease_token: None,
                created_at: now.clone(),
                updated_at: now.clone(),
            };
            if index > 0 {
                // The weekly review and the ideas scan wait for their real slot.
                // Firing the whole plan at once would greet a brand-new entity
                // with three bubbles and three model calls in the same minute.
                hb.next_fire_at =
                    crate::heartbeat_store::next_fire_at(&hb, started).map(|dt| dt.to_rfc3339());
            }
            hb
        })
        .collect()
}

/// True when this entity should never be auto-provisioned.
fn opted_out(entity: &Entity) -> bool {
    matches!(effective_heartbeat_mode(entity), EntityHeartbeatMode::Off)
}

fn has_existing_beat(existing: &[Heartbeat], entity_id: &str) -> bool {
    existing
        .iter()
        .any(|h| h.target_kind == "entity" && h.target_id.as_deref() == Some(entity_id))
}

/// Give one entity its default rhythm. No-op when the entity opted out or
/// already has any heartbeat of its own. Returns the number of rows written.
pub async fn provision_default_heartbeats(
    store: &dyn HeartbeatStore,
    entity: &Entity,
    workspace_id: Option<&str>,
) -> Result<usize> {
    if opted_out(entity) {
        return Ok(0);
    }
    let existing = store.list_all().await?;
    if has_existing_beat(&existing, &entity.id) {
        return Ok(0);
    }
    write_plan(store, entity, workspace_id).await
}

// Unconditional write of the whole plan, one upsert per beat. Callers
// (`provision_default_heartbeats`, `backfill_default_heartbeats`) are the
// ones responsible for checking the opt-out / already-has-a-beat gate first.
async fn write_plan(
    store: &dyn HeartbeatStore,
    entity: &Entity,
    workspace_id: Option<&str>,
) -> Result<usize> {
    let mut written = 0usize;
    for hb in default_heartbeats_for_entity(entity, workspace_id) {
        match store.upsert(&hb).await {
            Ok(()) => written += 1,
            Err(e) => tracing::warn!(
                entity_id = %entity.id,
                heartbeat_id = %hb.id,
                error = %e,
                "default heartbeat provisioning: upsert failed"
            ),
        }
    }
    if written > 0 {
        tracing::info!(
            entity_id = %entity.id,
            entity_name = %entity.name,
            archetype = classify(entity).label(),
            beats = written,
            "provisioned default heartbeats"
        );
    }
    Ok(written)
}

/// Startup floor: give every entity that has no heartbeat at all its defaults.
///
/// Covers entities that predate this module and ones created through paths that
/// don't reach a provisioning call (seeded entities, personal spaces created
/// before their store handle was attached). One `list_all` for the whole sweep.
pub async fn backfill_default_heartbeats(
    store: &dyn HeartbeatStore,
    entities: &[Entity],
) -> Result<usize> {
    let existing = store.list_all().await?;
    let mut total = 0usize;
    for entity in entities {
        if opted_out(entity) || has_existing_beat(&existing, &entity.id) {
            continue;
        }
        total += write_plan(store, entity, entity.workspace_id.as_deref()).await?;
    }
    if total > 0 {
        tracing::info!(beats = total, "backfilled default heartbeats");
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heartbeat_store::SurrealHeartbeatStore;
    use crate::state::{EntityReadiness, EntityRuntimeConfig, EntitySetup};
    use chrono::DateTime;
    use cron::Schedule;
    use std::str::FromStr;
    use std::sync::Arc;

    fn configured(name: &str) -> Entity {
        Entity {
            id: "ent-123".to_string(),
            name: name.to_string(),
            workspace_id: Some("ws_1".to_string()),
            goals: Some("Reach 100 paying users.".to_string()),
            readiness: EntityReadiness {
                percent: 75,
                label: "execution ready".to_string(),
                next_question: None,
                missing_fields: vec![],
            },
            ..Entity::default()
        }
    }

    #[test]
    fn every_generated_cron_parses() {
        // The whole plan is worthless if `next_fire_at` can't schedule it —
        // `heartbeat_store::next_fire_at` silently returns None on a bad cron.
        for archetype in [
            EntityArchetype::Product,
            EntityArchetype::Content,
            EntityArchetype::GrowthSales,
            EntityArchetype::Client,
            EntityArchetype::Legal,
            EntityArchetype::Personal,
            EntityArchetype::Venture,
        ] {
            let (_, pulse, _, review, _, ideas) = slots(archetype);
            for expr in [pulse, review, ideas, "0 0 9 * * *"] {
                let schedule = Schedule::from_str(expr)
                    .unwrap_or_else(|e| panic!("{archetype:?} cron {expr:?} rejected: {e}"));
                assert!(
                    schedule.upcoming(Utc).next().is_some(),
                    "{archetype:?} cron {expr:?} never fires"
                );
            }
        }
    }

    #[test]
    fn creation_fires_only_the_lead_beat_immediately() {
        let rows = default_heartbeats_for_entity(&configured("Acme"), Some("ws_1"));
        assert_eq!(rows.len(), 3);
        assert!(
            rows[0].next_fire_at.is_none(),
            "the daily pulse greets the owner on the next tick"
        );
        for hb in &rows[1..] {
            let scheduled = hb
                .next_fire_at
                .as_deref()
                .unwrap_or_else(|| panic!("{} should be pre-scheduled", hb.name));
            let at = DateTime::parse_from_rfc3339(scheduled).expect("rfc3339");
            assert!(
                at > Utc::now(),
                "{} must wait for its slot, not fire at creation",
                hb.name
            );
        }
    }

    #[test]
    fn configured_entity_gets_reminder_review_and_ideas() {
        let plan = default_beat_plan(&configured("Acme"));
        let intents: Vec<_> = plan.iter().map(|b| b.intent).collect();
        assert_eq!(
            intents,
            vec![BeatIntent::Reminder, BeatIntent::Ideas, BeatIntent::Review]
        );
        assert!(plan.iter().all(|b| b.name.starts_with("Acme")));
    }

    #[test]
    fn thin_entity_gets_a_setup_question_instead_of_a_pulse() {
        let mut entity = configured("Vague Thing");
        entity.goals = None;
        entity.readiness = EntityReadiness {
            percent: 10,
            label: "name only".to_string(),
            next_question: Some("What is the main goal for this entity?".to_string()),
            missing_fields: vec!["primary_goal".to_string()],
        };

        let plan = default_beat_plan(&entity);
        let intents: Vec<_> = plan.iter().map(|b| b.intent).collect();
        assert_eq!(
            intents,
            vec![BeatIntent::Setup, BeatIntent::Review],
            "no ideas beat until there is a goal to have ideas about"
        );
        assert!(
            plan[0].focus.contains("What is the main goal"),
            "setup beat carries the readiness question: {}",
            plan[0].focus
        );
    }

    #[test]
    fn declared_stack_beats_keyword_scoring() {
        let mut entity = configured("Client Newsletter");
        // Keywords point at content/client, but a real stack means it's a build.
        entity.setup = EntitySetup {
            backend_stack: Some("Rust + Axum".to_string()),
            ..EntitySetup::default()
        };
        assert_eq!(classify(&entity), EntityArchetype::Product);
    }

    #[test]
    fn classification_routes_beats_to_matching_directors() {
        let cases = [
            ("Weekly newsletter and podcast", EntityArchetype::Content),
            ("Outbound sales pipeline", EntityArchetype::GrowthSales),
            (
                "Contract compliance and regulatory review",
                EntityArchetype::Legal,
            ),
            ("Something entirely unremarkable", EntityArchetype::Venture),
        ];
        for (description, expected) in cases {
            let mut entity = configured("Thing");
            entity.description = Some(description.to_string());
            assert_eq!(classify(&entity), expected, "description: {description}");
        }

        let mut personal = configured("Personal");
        personal.is_personal = true;
        assert_eq!(classify(&personal), EntityArchetype::Personal);

        let content = {
            let mut e = configured("Studio");
            e.description = Some("Weekly newsletter and podcast".to_string());
            e
        };
        assert!(
            default_beat_plan(&content)
                .iter()
                .all(|b| b.director_id == "content-creative"),
            "a content entity should not be routed to engineers"
        );
    }

    #[test]
    fn condition_round_trips_intent_slug_and_focus() {
        let plan = default_beat_plan(&configured("Acme"));
        for beat in &plan {
            let parsed = parse_beat_condition(Some(&beat.condition()))
                .unwrap_or_else(|| panic!("condition unparseable: {}", beat.condition()));
            assert_eq!(parsed.intent, beat.intent);
            assert_eq!(parsed.slug, beat.slug);
            assert_eq!(parsed.focus.as_deref(), Some(beat.focus.trim()));
        }
    }

    #[test]
    fn hand_written_conditions_are_left_alone() {
        assert!(parse_beat_condition(None).is_none());
        assert!(parse_beat_condition(Some("worker:consolidate")).is_none());
        assert!(parse_beat_condition(Some("beat:nonsense:x")).is_none());
    }

    #[test]
    fn ids_are_deterministic_and_record_id_safe() {
        let a = beat_id("2f1c-9d0e:weird", "daily-pulse");
        let b = beat_id("2f1c-9d0e:weird", "daily-pulse");
        assert_eq!(a, b);
        // A colon would be read as the Surreal table/key separator; the entity
        // id is sanitized to `_`, while the slug's dash is safe.
        assert!(!a.contains(':'), "colon breaks the Surreal record id: {a}");
        assert_eq!(a, "hb_2f1c_9d0e_weird_daily-pulse");
    }

    async fn store() -> Arc<SurrealHeartbeatStore> {
        use surrealdb::engine::any::connect;
        let db = connect("memory").await.expect("connect");
        db.use_ns("test").use_db("test").await.expect("use");
        let store = SurrealHeartbeatStore::new(Arc::new(db));
        store.init_schema().await.expect("init");
        store
    }

    #[tokio::test]
    async fn provisioning_is_idempotent_and_respects_user_edits() {
        let store = store().await;
        let entity = configured("Acme");

        let first = provision_default_heartbeats(store.as_ref(), &entity, Some("ws_1"))
            .await
            .expect("provision");
        assert_eq!(first, 3);

        let rows = store.list_all().await.unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|h| h.enabled));
        assert!(
            rows.iter()
                .all(|h| h.target_id.as_deref() == Some("ent-123"))
        );
        assert!(
            rows.iter()
                .all(|h| h.workspace_id.as_deref() == Some("ws_1"))
        );
        // Simulate a user turning one beat off.
        let mut edited = rows[0].clone();
        edited.enabled = false;
        store.upsert(&edited).await.unwrap();

        let second = provision_default_heartbeats(store.as_ref(), &entity, Some("ws_1"))
            .await
            .expect("provision again");
        assert_eq!(second, 0, "existing beats mean hands off");
        let after = store.get(&edited.id).await.unwrap().unwrap();
        assert!(!after.enabled, "the user's disable survived");
    }

    #[tokio::test]
    async fn heartbeat_mode_off_is_the_opt_out() {
        let store = store().await;
        let mut entity = configured("Quiet");
        entity.entity_runtime = Some(EntityRuntimeConfig {
            automation_mode: Default::default(),
            heartbeat_mode: EntityHeartbeatMode::Off,
            default_chat_session_id: None,
            quiet_hours_local: None,
        });

        let n = provision_default_heartbeats(store.as_ref(), &entity, Some("ws_1"))
            .await
            .expect("provision");
        assert_eq!(n, 0);
        assert!(store.list_all().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn backfill_only_touches_entities_with_no_pulse() {
        let store = store().await;
        let mut covered = configured("Covered");
        covered.id = "ent-covered".to_string();
        let mut bare = configured("Bare");
        bare.id = "ent-bare".to_string();

        provision_default_heartbeats(store.as_ref(), &covered, Some("ws_1"))
            .await
            .unwrap();

        let n = backfill_default_heartbeats(store.as_ref(), &[covered.clone(), bare.clone()])
            .await
            .expect("backfill");
        assert_eq!(n, 3, "only the bare entity is provisioned");

        let rows = store.list_all().await.unwrap();
        assert_eq!(rows.len(), 6);
    }
}

/// The beat that drives a loop-enabled mission forward.
///
/// Separate from [`default_beat_plan`] on purpose: the default rhythm is the
/// entity's own pulse and belongs to every entity, while this beat exists only
/// while a mission is asking to be driven. It fires hourly — often enough that
/// an iteration lands the same working day, rare enough that a stuck loop
/// cannot spend a budget overnight.
pub fn drive_beat(entity: &Entity, workspace_id: Option<&str>) -> Heartbeat {
    let now = Utc::now().to_rfc3339();
    let beat = DefaultBeat {
        slug: "mission-drive",
        name: "Drive active mission".to_string(),
        director_id: "operations",
        // Top of every hour.
        cron: "0 0 * * * *",
        intent: BeatIntent::Drive,
        focus: "Push the active mission one iteration toward its definition of done.".to_string(),
    };
    Heartbeat {
        id: beat_id(&entity.id, beat.slug),
        workspace_id: workspace_id
            .map(str::to_string)
            .or_else(|| entity.workspace_id.clone()),
        name: beat.name.clone(),
        enabled: true,
        target_kind: "entity".to_string(),
        target_id: Some(entity.id.clone()),
        director_id: beat.director_id.to_string(),
        cron: Some(beat.cron.to_string()),
        cadence_secs: None,
        condition: Some(beat.condition()),
        last_fired_at: None,
        // Unset: the first iteration should start now, not on the hour.
        next_fire_at: None,
        lease_until: None,
        lease_token: None,
        created_at: now.clone(),
        updated_at: now,
    }
}

/// Ensure the entity has its drive beat. Idempotent: the row id is
/// deterministic, so re-provisioning upserts in place rather than stacking
/// beats per mission.
pub async fn provision_drive_beat(
    store: &dyn HeartbeatStore,
    entity: &Entity,
    workspace_id: Option<&str>,
) -> Result<()> {
    if opted_out(entity) {
        return Ok(());
    }
    let hb = drive_beat(entity, workspace_id);
    store.upsert(&hb).await
}

#[cfg(test)]
mod drive_beat_tests {
    use super::*;

    fn entity() -> Entity {
        Entity {
            id: "ent_1".to_string(),
            name: "Acme".to_string(),
            ..Entity::default()
        }
    }

    #[test]
    fn the_drive_beat_round_trips_its_intent() {
        let hb = drive_beat(&entity(), Some("ws"));
        let parsed = parse_beat_condition(hb.condition.as_deref()).expect("condition parses");
        assert_eq!(parsed.intent, BeatIntent::Drive);
        assert_eq!(parsed.slug, "mission-drive");
    }

    #[test]
    fn the_drive_beat_id_is_stable_so_reprovisioning_upserts() {
        let a = drive_beat(&entity(), None);
        let b = drive_beat(&entity(), None);
        assert_eq!(a.id, b.id);
        // And it never collides with a default-plan beat.
        let defaults = default_heartbeats_for_entity(&entity(), None);
        assert!(defaults.iter().all(|d| d.id != a.id));
    }

    #[test]
    fn an_entity_with_heartbeats_off_gets_no_drive_beat() {
        let mut e = entity();
        e.entity_runtime = Some(crate::state::EntityRuntimeConfig {
            automation_mode: crate::state::AutomationMode::AutoRun,
            heartbeat_mode: EntityHeartbeatMode::Off,
            default_chat_session_id: None,
            quiet_hours_local: None,
        });
        assert!(opted_out(&e));
    }
}
