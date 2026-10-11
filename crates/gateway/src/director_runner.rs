//! Phase 1 (Build 2) — Director runner.
//!
//! Given a director and the canonical context for a single firing (entity,
//! optional active mission, recent lessons, fired_at), this module assembles
//! the system prompt via [`assemble_system_prompt`], resolves the LLM
//! provider/model per the director's own `model_provider`/`model_name`
//! (falling through to operations defaults when the director leaves them
//! unset), sends a *single* non-streaming chat completion, and returns the
//! assistant text + usage metadata.
//!
//! Provider resolution mirrors `chat_agent::build_provider`'s alias-hop
//! pattern so an alias like `"brain-default"` correctly expands to a real
//! model id.
//!
//! Public entry points:
//! - [`run_director`] — the full pipeline (used by the heartbeat dispatcher).
//! - [`run_director_with_provider`] — same pipeline minus provider resolution;
//!   tests inject a mock `LlmProvider`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use consistent_world_llm::client::{ChatMessage, CompletionRequest, LlmProvider, Role};
use std::sync::Arc;

use crate::capture::{build_llm_client, operations_capture_preferences};
use crate::config::AppConfig;
use crate::credentials_resolver::resolve_llm_api_key;
use crate::entity_heartbeat_defaults::BeatIntent;
use crate::prompt_assembly::{CanonicalInputs, assemble_system_prompt};
use crate::state::{ControlState, Director, DirectorLesson, Entity, EntityDoc, Mission};

#[cfg(test)]
use crate::state::MissionStatus;

/// Result of one director run: the generated text plus usage metadata so the
/// caller can debit the cost ledger.
#[derive(Debug, Clone)]
pub struct DirectorRun {
    pub text: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
}

/// Cap on how many alias hops `resolve_director_provider` will follow before
/// giving up and using whatever model id it has reached.
const MAX_ALIAS_HOPS: usize = 4;

/// Task kind used to route heartbeat director runs. A
/// `[routing.task_kinds.heartbeat]` entry (provider + model) sends every beat
/// to that model — the natural place to keep beats on a local model while chat
/// stays on the cloud brain.
pub const HEARTBEAT_TASK_KIND: &str = "heartbeat";

/// Resolve `(base_url, api_key_env, model_id)` for a director.
///
/// Preference order:
/// 1. Director's own `model_provider` + `model_name` (when both non-empty).
/// 2. `[routing.task_kinds.heartbeat]` route, when configured.
/// 3. Operations director defaults (via [`operations_capture_preferences`]).
///
/// Then the same alias-hop loop as `chat_agent::build_provider` expands
/// `model_id` until it matches itself (a real model id), capped at
/// [`MAX_ALIAS_HOPS`].
pub(crate) async fn resolve_director_provider(
    cfg: &AppConfig,
    control: &ControlState,
    director: &Director,
) -> Option<(String, String, String)> {
    let mut prov = director.model_provider.clone().unwrap_or_default();
    let mut model = director.model_name.clone().unwrap_or_default();

    // A director whose row pins both provider and model is the most specific
    // setting there is; the global heartbeat route only steers the rest.
    let director_pinned = !prov.trim().is_empty() && !model.trim().is_empty();

    if !director_pinned {
        let (default_prov, default_model) = operations_capture_preferences(cfg, control).await;
        if prov.trim().is_empty() {
            prov = default_prov;
        }
        if model.trim().is_empty() {
            model = default_model;
        }
    }

    let task_kind = (!director_pinned).then_some(HEARTBEAT_TASK_KIND);
    let (mut base_url, mut api_key_env, mut model_id) =
        crate::config::resolve_llm_for_task(cfg, task_kind, prov.trim(), model.trim()).0?;

    let mut hops = 0;
    while let Some((next_prov, real)) = cfg.llm_models_map.get(&model_id).cloned() {
        if real == model_id {
            break;
        }
        model_id = real;
        if let Some((b, a)) = cfg.llm_providers_map.get(&next_prov) {
            base_url = b.clone();
            api_key_env = a.clone();
        }
        hops += 1;
        if hops > MAX_ALIAS_HOPS {
            break;
        }
    }

    Some((base_url, api_key_env, model_id))
}

/// What *this particular beat* is for, plus what the previous beats already
/// said. Without it every firing asks the same generic question and the entity
/// gets the same paragraph forever; with it, a reminder beat, a review beat and
/// an ideas beat each ask for something different, and none of them repeat what
/// the last few beats already told the owner.
///
/// Built by [`crate::heartbeat_dispatcher`] from the heartbeat's `condition`
/// column (see [`crate::entity_heartbeat_defaults::parse_beat_condition`]).
#[derive(Debug, Clone)]
pub struct BeatBrief {
    /// Which kind of beat this firing is (setup / reminder / review / ideas).
    pub intent: BeatIntent,
    /// The beat's standing brief — what it should produce every time.
    pub focus: Option<String>,
    /// Summaries of the most recent beats for this entity, newest first.
    pub recent: Vec<String>,
}

/// How many prior beat summaries to replay. Enough for the model to see a
/// direction of travel, few enough to stay cheap.
pub const RECENT_BEATS_IN_PROMPT: usize = 3;

/// Bounded entity-memory slice injected into beat prompts: enough durable
/// facts plus recent notes to keep a beat consistent with what the entity's
/// own agent runs have learned, small enough to stay cheap.
pub const ENTITY_MEMORY_LONG_IN_PROMPT: usize = 10;
pub const ENTITY_MEMORY_SHORT_IN_PROMPT: usize = 5;

/// The intent-specific ask. This is the difference between a heartbeat that
/// pings and one that compounds.
fn intent_instruction(intent: BeatIntent) -> &'static str {
    match intent {
        BeatIntent::Setup => {
            "This entity is not configured enough to act on yet. Ask exactly one question — the one \
             whose answer unlocks the most — and say in a single line what it would let this entity do. \
             Do not invent status, do not list tasks, do not ask more than one question."
        }
        BeatIntent::Reminder => {
            "In one short paragraph (≤120 words), tell the owner what needs their attention right now: \
             what is blocked, what is owed to someone else, and the single highest-leverage next action. \
             Be specific to this entity's actual state — no generic advice."
        }
        BeatIntent::Review => {
            "In ≤150 words, review the period since the last review against this entity's goal: what \
             actually moved, what stalled, and the one correction to make next. Name the evidence you \
             are relying on. If nothing moved, say so plainly rather than padding."
        }
        BeatIntent::Drive => {
            "Report in ≤120 words on the mission you are driving: what the last round of work \
             actually produced, and what the next step is. Judge by evidence, not by claims. If \
             there is nothing to drive right now, say so plainly in one line."
        }
        BeatIntent::Ideas => {
            "Offer two or three options the owner has probably not considered for this entity, each in \
             one line with a concrete first step. Ground them in this entity's real situation and goal. \
             Skip anything already tried or already on the list — say so if you have nothing new."
        }
    }
}

/// User-side prompt for a heartbeat firing. Kept tiny on purpose — the
/// heavy lifting lives in the system prompt assembled from canonical inputs.
///
/// `beat` is `None` for hand-created heartbeats that carry no `beat:` condition;
/// those keep the original generic paragraph ask.
pub(crate) fn heartbeat_user_prompt(
    fired_at: DateTime<Utc>,
    entity_name: &str,
    beat: Option<&BeatBrief>,
) -> String {
    let mut out = format!(
        "A scheduled heartbeat just fired at {} UTC for the entity \"{}\".",
        fired_at.format("%Y-%m-%d %H:%M:%S"),
        entity_name
    );

    match beat {
        Some(brief) => {
            out.push_str("\n\n");
            out.push_str(intent_instruction(brief.intent));
            if let Some(focus) = brief
                .focus
                .as_deref()
                .map(str::trim)
                .filter(|f| !f.is_empty())
            {
                out.push_str("\n\nThis beat's standing brief: ");
                out.push_str(focus);
            }
            if !brief.recent.is_empty() {
                out.push_str(
                    "\n\nRecent beats for this entity, newest first — build on these rather than \
                     restating them. If something you flagged before is still open, say that it is \
                     still open and escalate; if it closed, acknowledge it and move on:",
                );
                for (i, prior) in brief.recent.iter().enumerate() {
                    out.push_str(&format!("\n{}. {}", i + 1, prior.trim()));
                }
            }
        }
        None => {
            out.push_str(
                " In one short paragraph (≤120 words), tell the user what they should know or do right now, \
                 drawing on the canonical entity, the active mission's narrative + pinned notes, and any lessons learned.",
            );
        }
    }

    out.push_str("\n\nNo greeting, no sign-off, no markdown formatting.");
    out
}

/// Build the `CompletionRequest` for a director firing. Separated from the
/// LLM call so tests can assert the request shape without a network round-trip.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_request(
    model_id: String,
    director: &Director,
    entity: &Entity,
    mission: Option<&Mission>,
    lessons: &[DirectorLesson],
    beat: Option<&BeatBrief>,
    entity_memory: &[String],
    fired_at: DateTime<Utc>,
) -> CompletionRequest {
    build_request_with_docs(
        model_id,
        director,
        entity,
        mission,
        lessons,
        beat,
        entity_memory,
        &[],
        fired_at,
    )
}

/// [`build_request`] plus the entity's enabled knowledge docs (README, repo
/// map, conventions — see `entity_knowledge`), which ride the system prompt
/// so a beat can reason about the project, not just its name.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_request_with_docs(
    model_id: String,
    director: &Director,
    entity: &Entity,
    mission: Option<&Mission>,
    lessons: &[DirectorLesson],
    beat: Option<&BeatBrief>,
    entity_memory: &[String],
    docs: &[EntityDoc],
    fired_at: DateTime<Utc>,
) -> CompletionRequest {
    let mut system = assemble_system_prompt(&CanonicalInputs {
        soul: None,
        director: Some(director),
        entity: Some(entity),
        mission,
        lessons,
        docs,
    });
    // Entity memory rides the system prompt like every other canonical
    // section; a blank slice adds nothing, not even the heading.
    let mem: Vec<&str> = entity_memory
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if !mem.is_empty() {
        system.push_str("\n\n## Entity memory\n");
        system.push_str(
            &mem.iter()
                .map(|line| format!("- {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let user = heartbeat_user_prompt(fired_at, &entity.name, beat);

    CompletionRequest {
        model: model_id,
        messages: vec![
            ChatMessage {
                role: Role::System,
                content: system,
                tool_calls: None,
                tool_call_id: None,
            },
            ChatMessage {
                role: Role::User,
                content: user,
                tool_calls: None,
                tool_call_id: None,
            },
        ],
        tools: None,
        max_tokens: Some(400),
        temperature: director.temperature,
        stream: false,
    }
}

/// Send a single completion to the given provider and convert the response
/// into a [`DirectorRun`]. Returns `Err` if the model returns empty content
/// or the provider call itself fails.
pub(crate) async fn run_director_with_provider(
    provider: &dyn LlmProvider,
    request: CompletionRequest,
) -> anyhow::Result<DirectorRun> {
    let response = provider.complete(request).await?;
    let text = response.content.unwrap_or_default().trim().to_string();
    if text.is_empty() {
        return Err(anyhow::anyhow!("director returned empty content"));
    }
    Ok(DirectorRun {
        text,
        model: response.model,
        input_tokens: response.usage.input_tokens,
        output_tokens: response.usage.output_tokens,
        latency_ms: response.latency_ms,
    })
}

/// Run the director against its preferred provider/model. Returns `Err` if
/// the provider cannot be resolved, the credential is missing, or the model
/// returns empty content. The heartbeat dispatcher falls back to the canned
/// summary on any `Err` so the chat surface keeps a steady pulse.
#[allow(clippy::too_many_arguments)]
pub async fn run_director(
    cfg: &AppConfig,
    control: &ControlState,
    director: &Director,
    entity: &Entity,
    mission: Option<&Mission>,
    lessons: &[DirectorLesson],
    beat: Option<&BeatBrief>,
    entity_memory: &[String],
    docs: &[EntityDoc],
    fired_at: DateTime<Utc>,
) -> anyhow::Result<DirectorRun> {
    let (base_url, api_key_env, model_id) = resolve_director_provider(cfg, control, director)
        .await
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not resolve LLM provider for director {}",
                director.id
            )
        })?;
    let api_key = resolve_llm_api_key(&api_key_env);
    let provider = build_llm_client(cfg, &base_url, &api_key, &api_key_env);
    let request = build_request_with_docs(
        model_id,
        director,
        entity,
        mission,
        lessons,
        beat,
        entity_memory,
        docs,
        fired_at,
    );
    run_director_with_provider(provider.as_ref(), request).await
}

/// Decide whether a failed director run earns one retry on the local lane,
/// and against what: `(base_url, api_key_env, model_id)`. `None` when the
/// failed run was already local (retrying the same lane buys nothing) or when
/// no `ollama` provider is registered (a dormant local lane stays dormant).
pub(crate) fn local_retry_lane(
    cfg: &AppConfig,
    failed_base_url: &str,
    failed_api_key_env: &str,
) -> Option<(String, String, String)> {
    if crate::capture::is_ollama_target(failed_base_url, failed_api_key_env) {
        return None;
    }
    crate::config::resolve_local_ollama_lane(cfg)
}

/// Indirection so the heartbeat dispatcher (and other future callers) can swap
/// the LLM-backed runner for a mock in tests without dragging real `AppConfig`
/// / `ControlState` plumbing into every test case.
#[async_trait]
pub trait DirectorRunnerHandle: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        director: &Director,
        entity: &Entity,
        mission: Option<&Mission>,
        lessons: &[DirectorLesson],
        beat: Option<&BeatBrief>,
        entity_memory: &[String],
        fired_at: DateTime<Utc>,
    ) -> anyhow::Result<DirectorRun>;

    /// One follow-up attempt through the registered local (Ollama) lane after
    /// a failed [`run`](DirectorRunnerHandle::run). `None` means there is
    /// nothing worth retrying against — no `ollama` provider registered, or
    /// the failed run was already local. Defaulted so mock runners keep their
    /// old behavior (straight to canned) unless a test opts in.
    #[allow(clippy::too_many_arguments)]
    async fn run_local_retry(
        &self,
        director: &Director,
        entity: &Entity,
        mission: Option<&Mission>,
        lessons: &[DirectorLesson],
        beat: Option<&BeatBrief>,
        entity_memory: &[String],
        fired_at: DateTime<Utc>,
    ) -> Option<anyhow::Result<DirectorRun>> {
        let _ = (
            director,
            entity,
            mission,
            lessons,
            beat,
            entity_memory,
            fired_at,
        );
        None
    }
}

/// Production runner: defers to [`run_director`]. `cfg` + `control` are held by
/// `Arc` because the heartbeat ticker lives for the lifetime of the process.
pub struct LlmDirectorRunner {
    cfg: Arc<AppConfig>,
    control: Arc<ControlState>,
    /// Where the entity's knowledge docs live. `None` keeps beats docless
    /// (tests, or a gateway whose docs schema failed to initialise).
    docs_store: Option<Arc<crate::entity_docs_store::SurrealEntityDocsStore>>,
}

impl LlmDirectorRunner {
    /// Wrap the shared config/control handles in an `Arc` for cheap cloning
    /// across the heartbeat ticker's async tasks.
    pub fn new(cfg: Arc<AppConfig>, control: Arc<ControlState>) -> Arc<Self> {
        Self::new_with_docs(cfg, control, None)
    }

    /// [`Self::new`] with the entity docs store, so every beat carries the
    /// entity's indexed knowledge.
    pub fn new_with_docs(
        cfg: Arc<AppConfig>,
        control: Arc<ControlState>,
        docs_store: Option<Arc<crate::entity_docs_store::SurrealEntityDocsStore>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            control,
            docs_store,
        })
    }

    async fn docs_for(&self, entity_id: &str) -> Vec<EntityDoc> {
        let Some(store) = self.docs_store.as_ref() else {
            return Vec::new();
        };
        store
            .list_enabled_for_entity(entity_id)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(entity_id, error = %e, "heartbeat: entity docs listing failed");
                Vec::new()
            })
    }
}

#[async_trait]
impl DirectorRunnerHandle for LlmDirectorRunner {
    async fn run(
        &self,
        director: &Director,
        entity: &Entity,
        mission: Option<&Mission>,
        lessons: &[DirectorLesson],
        beat: Option<&BeatBrief>,
        entity_memory: &[String],
        fired_at: DateTime<Utc>,
    ) -> anyhow::Result<DirectorRun> {
        let docs = self.docs_for(&entity.id).await;
        run_director(
            &self.cfg,
            &self.control,
            director,
            entity,
            mission,
            lessons,
            beat,
            entity_memory,
            &docs,
            fired_at,
        )
        .await
    }

    async fn run_local_retry(
        &self,
        director: &Director,
        entity: &Entity,
        mission: Option<&Mission>,
        lessons: &[DirectorLesson],
        beat: Option<&BeatBrief>,
        entity_memory: &[String],
        fired_at: DateTime<Utc>,
    ) -> Option<anyhow::Result<DirectorRun>> {
        // Re-resolve the lane the primary run used — deterministic, so this
        // names the same provider that just failed. A primary that never
        // resolved at all still gets the local lane, when one is registered.
        let (base_url, api_key_env, model_id) =
            match resolve_director_provider(&self.cfg, &self.control, director).await {
                Some((b, e, _)) => local_retry_lane(&self.cfg, &b, &e)?,
                None => crate::config::resolve_local_ollama_lane(&self.cfg)?,
            };
        let api_key = resolve_llm_api_key(&api_key_env);
        let provider = build_llm_client(&self.cfg, &base_url, &api_key, &api_key_env);
        let docs = self.docs_for(&entity.id).await;
        let request = build_request_with_docs(
            model_id,
            director,
            entity,
            mission,
            lessons,
            beat,
            entity_memory,
            &docs,
            fired_at,
        );
        Some(run_director_with_provider(provider.as_ref(), request).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use async_trait::async_trait;
    use consistent_world_llm::client::{CompletionResponse, TokenUsage};
    use std::sync::Arc;
    use std::sync::Mutex;

    fn make_entity() -> Entity {
        Entity {
            id: "ent_qsme".to_string(),
            name: "QS Me".to_string(),
            description: Some("Quantity surveying platform".to_string()),
            ..Entity::default()
        }
    }

    fn make_director() -> Director {
        Director {
            id: "dir_ops".to_string(),
            slug: "operations".to_string(),
            name: "Operations".to_string(),
            prompt: "Be the operator.".to_string(),
            model_provider: Some("openai".to_string()),
            model_name: Some("gpt-test".to_string()),
            temperature: Some(0.4),
            ..Director::default()
        }
    }

    fn make_mission() -> Mission {
        Mission {
            id: "mis_1".to_string(),
            entity_id: "ent_qsme".to_string(),
            title: "Ship MVP".to_string(),
            summary: "Wedge".to_string(),
            status: MissionStatus::Active,
            priority: "normal".to_string(),
            goal: "Ship".to_string(),
            owner: None,
            created_by: None,
            source: None,
            created_at: "2026-05-24T00:00:00Z".to_string(),
            updated_at: "2026-05-24T00:00:00Z".to_string(),
            last_reviewed_at: None,
            review_cadence_hours: None,
            narrative: Some("We're closing in on the wedge.".to_string()),
            canonical_notes: vec!["No paywall at launch".to_string()],
            assigned_director_id: None,
            final_report: None,
            loop_config: None,
            loop_state: None,
        }
    }

    struct MockProvider {
        captured: Arc<Mutex<Option<CompletionRequest>>>,
        response: anyhow::Result<CompletionResponse>,
    }

    impl MockProvider {
        fn new(
            response: anyhow::Result<CompletionResponse>,
        ) -> (Self, Arc<Mutex<Option<CompletionRequest>>>) {
            let captured = Arc::new(Mutex::new(None));
            (
                MockProvider {
                    captured: captured.clone(),
                    response,
                },
                captured,
            )
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, request: CompletionRequest) -> anyhow::Result<CompletionResponse> {
            *self.captured.lock().unwrap() = Some(request);
            match &self.response {
                Ok(r) => Ok(r.clone()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
        fn provider_name(&self) -> &'static str {
            "mock"
        }
    }

    fn ok_response(text: &str) -> CompletionResponse {
        CompletionResponse {
            content: Some(text.to_string()),
            tool_calls: vec![],
            usage: TokenUsage {
                input_tokens: 42,
                output_tokens: 17,
            },
            model: "gpt-test".to_string(),
            latency_ms: 123,
        }
    }

    #[test]
    fn heartbeat_user_prompt_shape() {
        let fired = DateTime::parse_from_rfc3339("2026-05-24T13:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let p = heartbeat_user_prompt(fired, "QS Me", None);
        assert!(p.contains("2026-05-24 13:00:00 UTC"));
        assert!(p.contains("\"QS Me\""));
        assert!(p.contains("≤120 words"));
        assert!(p.contains("No greeting"));
    }

    #[test]
    fn each_intent_asks_for_something_different() {
        let fired = Utc::now();
        let ask = |intent| {
            heartbeat_user_prompt(
                fired,
                "QS Me",
                Some(&BeatBrief {
                    intent,
                    focus: None,
                    recent: vec![],
                }),
            )
        };
        let reminder = ask(BeatIntent::Reminder);
        let review = ask(BeatIntent::Review);
        let ideas = ask(BeatIntent::Ideas);
        let setup = ask(BeatIntent::Setup);

        assert!(reminder.contains("needs their attention right now"));
        assert!(review.contains("what stalled"));
        assert!(ideas.contains("two or three options"));
        assert!(setup.contains("exactly one question"));
        assert_ne!(reminder, review);
        assert_ne!(review, ideas);
    }

    #[test]
    fn prior_beats_are_replayed_so_the_next_one_builds_on_them() {
        let p = heartbeat_user_prompt(
            Utc::now(),
            "QS Me",
            Some(&BeatBrief {
                intent: BeatIntent::Reminder,
                focus: Some("Blockers first.".to_string()),
                recent: vec![
                    "Variation #12 is still unsigned.".to_string(),
                    "Tile schedule slipped a week.".to_string(),
                ],
            }),
        );
        assert!(p.contains("standing brief: Blockers first."));
        assert!(p.contains("1. Variation #12 is still unsigned."));
        assert!(p.contains("2. Tile schedule slipped a week."));
        assert!(
            p.contains("build on these rather than restating them"),
            "the model must be told not to repeat itself"
        );
    }

    #[test]
    fn build_request_contains_canonical_block_and_user_ask() {
        let director = make_director();
        let entity = make_entity();
        let mission = make_mission();
        let fired = Utc::now();
        let req = build_request(
            "gpt-test".to_string(),
            &director,
            &entity,
            Some(&mission),
            &[],
            None,
            &[],
            fired,
        );

        assert_eq!(req.model, "gpt-test");
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, Role::System);
        assert_eq!(req.messages[1].role, Role::User);
        assert_eq!(req.temperature, Some(0.4));
        assert_eq!(req.max_tokens, Some(400));
        assert!(!req.stream);

        let sys = &req.messages[0].content;
        // assemble_system_prompt should have emitted director block + entity block + mission block.
        assert!(sys.contains("Be the operator."), "director prompt missing");
        assert!(sys.contains("QS Me"), "entity name missing");
        assert!(
            sys.contains("We're closing in"),
            "mission narrative missing"
        );
        assert!(sys.contains("No paywall"), "pinned note missing");
        assert!(
            !sys.contains("## SOUL"),
            "SOUL must not leak when soul=None"
        );

        let user = &req.messages[1].content;
        assert!(user.contains("heartbeat"));
        assert!(user.contains("\"QS Me\""));
    }

    #[tokio::test]
    async fn run_with_provider_success_returns_text_and_usage() {
        let (mock, captured) = MockProvider::new(Ok(ok_response(
            "Standing up the QS Me wedge: ship the chat loop today.",
        )));
        let req = build_request(
            "gpt-test".to_string(),
            &make_director(),
            &make_entity(),
            Some(&make_mission()),
            &[],
            None,
            &[],
            Utc::now(),
        );
        let out = run_director_with_provider(&mock, req).await.expect("ok");

        assert_eq!(
            out.text,
            "Standing up the QS Me wedge: ship the chat loop today."
        );
        assert_eq!(out.model, "gpt-test");
        assert_eq!(out.input_tokens, 42);
        assert_eq!(out.output_tokens, 17);
        assert_eq!(out.latency_ms, 123);
        assert!(
            captured.lock().unwrap().is_some(),
            "request must be captured"
        );
    }

    #[tokio::test]
    async fn run_with_provider_empty_content_is_err() {
        let mut resp = ok_response("");
        resp.content = Some("   \n  ".to_string());
        let (mock, _) = MockProvider::new(Ok(resp));
        let req = build_request(
            "gpt-test".to_string(),
            &make_director(),
            &make_entity(),
            None,
            &[],
            None,
            &[],
            Utc::now(),
        );
        let err = run_director_with_provider(&mock, req).await.unwrap_err();
        assert!(err.to_string().contains("empty content"));
    }

    #[tokio::test]
    async fn run_with_provider_propagates_provider_error() {
        let (mock, _) = MockProvider::new(Err(anyhow::anyhow!("HTTP 401: bad key")));
        let req = build_request(
            "gpt-test".to_string(),
            &make_director(),
            &make_entity(),
            None,
            &[],
            None,
            &[],
            Utc::now(),
        );
        let err = run_director_with_provider(&mock, req).await.unwrap_err();
        assert!(err.to_string().contains("401"));
    }

    #[tokio::test]
    async fn resolve_uses_director_preference_then_alias_hops() {
        let mut cfg = AppConfig::default();
        cfg.llm_providers_map.insert(
            "openai".to_string(),
            (
                "https://api.openai.com/v1".to_string(),
                "OPENAI_API_KEY".to_string(),
            ),
        );
        // Alias hop: "brain-default" → ("openai", "gpt-4o-mini")
        cfg.llm_models_map.insert(
            "brain-default".to_string(),
            ("openai".to_string(), "gpt-4o-mini".to_string()),
        );
        cfg.llm_models_map.insert(
            "gpt-4o-mini".to_string(),
            ("openai".to_string(), "gpt-4o-mini".to_string()),
        );

        let mut director = make_director();
        director.model_provider = Some("openai".to_string());
        director.model_name = Some("brain-default".to_string());

        let control = ControlState::new("openai", "https://api.openai.com/v1", "brain-default");
        let (base_url, api_key_env, model_id) =
            resolve_director_provider(&cfg, &control, &director)
                .await
                .expect("resolves");

        assert_eq!(base_url, "https://api.openai.com/v1");
        assert_eq!(api_key_env, "OPENAI_API_KEY");
        assert_eq!(
            model_id, "gpt-4o-mini",
            "alias should expand to real model id"
        );
    }

    #[tokio::test]
    async fn resolve_falls_through_to_operations_when_director_blank() {
        let mut cfg = AppConfig {
            llm_provider: "openai".to_string(),
            default_model: "brain-default".to_string(),
            ..AppConfig::default()
        };
        cfg.llm_providers_map.insert(
            "openai".to_string(),
            (
                "https://api.openai.com/v1".to_string(),
                "OPENAI_API_KEY".to_string(),
            ),
        );
        cfg.llm_models_map.insert(
            "brain-default".to_string(),
            ("openai".to_string(), "brain-default".to_string()),
        );

        let mut director = make_director();
        director.model_provider = None;
        director.model_name = None;

        // ControlState::default() yields no agents → operations_capture_preferences
        // falls back to the CONFIG defaults (openai + brain-default here), so
        // we get a clean base_url + api_key_env back without panicking.
        let control = ControlState::new("openai", "https://api.openai.com/v1", "brain-default");
        let resolved = resolve_director_provider(&cfg, &control, &director).await;
        assert!(resolved.is_some(), "fallthrough must resolve");
        let (base_url, api_key_env, _model_id) = resolved.unwrap();
        assert_eq!(base_url, "https://api.openai.com/v1");
        assert_eq!(api_key_env, "OPENAI_API_KEY");
    }

    #[tokio::test]
    async fn resolve_fallthrough_honors_non_openai_config_default() {
        // Cloud shape: default provider is anthropic, no seeded agents. The
        // old hard-coded "openai" fallback sent chat/heartbeats to a provider
        // the deploy never intended (and whose key may be dead).
        let mut cfg = AppConfig {
            llm_provider: "anthropic".to_string(),
            default_model: "operator-default".to_string(),
            ..AppConfig::default()
        };
        cfg.llm_providers_map.insert(
            "anthropic".to_string(),
            (
                "https://api.anthropic.com/v1".to_string(),
                "ANTHROPIC_API_KEY".to_string(),
            ),
        );
        cfg.llm_models_map.insert(
            "operator-default".to_string(),
            ("anthropic".to_string(), "claude-opus-4-8".to_string()),
        );

        let mut director = make_director();
        director.model_provider = None;
        director.model_name = None;

        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        let (base_url, api_key_env, model_id) =
            resolve_director_provider(&cfg, &control, &director)
                .await
                .expect("resolves via config default");
        assert_eq!(base_url, "https://api.anthropic.com/v1");
        assert_eq!(api_key_env, "ANTHROPIC_API_KEY");
        assert_eq!(model_id, "claude-opus-4-8");
    }

    // Config with a [routing.task_kinds.heartbeat] entry pointing beats at the
    // local model through the "local-fast" alias, while the config default
    // stays on a cloud provider.
    fn cfg_with_heartbeat_route() -> AppConfig {
        let mut cfg = AppConfig {
            llm_provider: "openai".to_string(),
            default_model: "gpt-4o-mini".to_string(),
            ..AppConfig::default()
        };
        cfg.llm_providers_map.insert(
            "openai".to_string(),
            (
                "https://api.openai.com/v1".to_string(),
                "OPENAI_API_KEY".to_string(),
            ),
        );
        cfg.llm_providers_map.insert(
            "ollama".to_string(),
            (
                "http://127.0.0.1:11434".to_string(),
                "OLLAMA_API_KEY".to_string(),
            ),
        );
        cfg.llm_models_map.insert(
            "local-fast".to_string(),
            ("ollama".to_string(), "gemma4:e4b".to_string()),
        );
        cfg.task_kind_routing.insert(
            HEARTBEAT_TASK_KIND.to_string(),
            crate::config::TaskKindRoutingEntry {
                provider: Some("ollama".to_string()),
                model: Some("local-fast".to_string()),
                ..Default::default()
            },
        );
        cfg
    }

    #[tokio::test]
    async fn resolve_routes_unpinned_director_through_heartbeat_task_kind() {
        let cfg = cfg_with_heartbeat_route();
        let mut director = make_director();
        director.model_provider = None;
        director.model_name = None;

        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        let (base_url, api_key_env, model_id) =
            resolve_director_provider(&cfg, &control, &director)
                .await
                .expect("heartbeat route resolves");
        assert_eq!(base_url, "http://127.0.0.1:11434");
        assert_eq!(api_key_env, "OLLAMA_API_KEY");
        assert_eq!(model_id, "gemma4:e4b", "alias must expand to the real id");
    }

    #[test]
    fn build_request_injects_entity_memory_as_its_own_section() {
        let memory = vec![
            "Owner prefers Tuesday check-ins.".to_string(),
            "   ".to_string(),
            "Variation #12 is contested.".to_string(),
        ];
        let req = build_request(
            "gpt-test".to_string(),
            &make_director(),
            &make_entity(),
            None,
            &[],
            None,
            &memory,
            Utc::now(),
        );
        let sys = &req.messages[0].content;
        assert!(sys.contains("## Entity memory"), "heading missing: {sys}");
        assert!(sys.contains("- Owner prefers Tuesday check-ins."));
        assert!(sys.contains("- Variation #12 is contested."));
    }

    #[test]
    fn build_request_empty_memory_emits_no_heading() {
        for memory in [vec![], vec!["  ".to_string()]] {
            let req = build_request(
                "gpt-test".to_string(),
                &make_director(),
                &make_entity(),
                None,
                &[],
                None,
                &memory,
                Utc::now(),
            );
            assert!(
                !req.messages[0].content.contains("## Entity memory"),
                "blank memory must not emit the heading"
            );
        }
    }

    #[test]
    fn local_retry_lane_targets_registered_ollama() {
        let cfg = cfg_with_heartbeat_route();
        let (base_url, api_key_env, model) =
            local_retry_lane(&cfg, "https://api.openai.com/v1", "OPENAI_API_KEY")
                .expect("cloud failure with a registered local lane retries");
        assert_eq!(base_url, "http://127.0.0.1:11434");
        assert_eq!(api_key_env, "OLLAMA_API_KEY");
        assert_eq!(model, "gemma4:e4b", "local-fast alias must resolve");
    }

    #[test]
    fn local_retry_lane_skips_when_primary_was_already_ollama() {
        let cfg = cfg_with_heartbeat_route();
        assert!(
            local_retry_lane(&cfg, "http://127.0.0.1:11434", "OLLAMA_API_KEY").is_none(),
            "retrying the lane that just failed buys nothing"
        );
    }

    #[test]
    fn local_retry_lane_skips_when_no_ollama_registered() {
        let cfg = AppConfig {
            llm_provider: "openai".to_string(),
            default_model: "gpt-4o-mini".to_string(),
            ..AppConfig::default()
        };
        assert!(local_retry_lane(&cfg, "https://api.openai.com/v1", "OPENAI_API_KEY").is_none());
    }

    #[tokio::test]
    async fn resolve_pinned_director_beats_heartbeat_task_kind() {
        let mut cfg = cfg_with_heartbeat_route();
        cfg.llm_models_map.insert(
            "gpt-4o".to_string(),
            ("openai".to_string(), "gpt-4o".to_string()),
        );
        let mut director = make_director();
        director.model_provider = Some("openai".to_string());
        director.model_name = Some("gpt-4o".to_string());

        let control = ControlState::new("openai", "https://api.openai.com/v1", "gpt-4o-mini");
        let (base_url, _api_key_env, model_id) =
            resolve_director_provider(&cfg, &control, &director)
                .await
                .expect("pinned director resolves");
        assert_eq!(
            base_url, "https://api.openai.com/v1",
            "a director row that pins provider+model must not be re-routed"
        );
        assert_eq!(model_id, "gpt-4o");
    }
}
