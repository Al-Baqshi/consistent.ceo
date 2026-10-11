//! Phase 9 (Build 1) glued the heartbeat ticker to the canonical layer with a
//! canned, deterministic summary so the chat surface had a steady pulse before
//! the LLM path was real. Build 2 / Phase 2 replaces that canned text with an
//! actual director run via [`crate::director_runner::DirectorRunnerHandle`]:
//!
//! 1. Resolve the director, entity name, and the active mission for context.
//! 2. Recall lessons for that director — semantic top-k against the run's
//!    entity/mission context when an embedding provider is wired, recency
//!    top-k otherwise.
//! 3. Run the director through its preferred provider+model.
//! 4. **Success** → use the model text + debit the cost ledger (mirroring the
//!    chat_agent path so heartbeats aren't free tokens).
//! 5. **Failure of any kind** (no credentials, provider error, empty content)
//!    → log + fall back to [`build_canned_summary`] so the chat surface keeps a
//!    steady pulse.
//!
//! Everything downstream of `summary: String` is shape-stable, so this is a
//! pure string-source swap; the API + UI + storage all stay as they were.

use chrono::Utc;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use uuid::Uuid;

use consistent_world_memory::embedding::EmbeddingProvider;

use crate::billing::CreditLedger;
use crate::director_lessons_store::SurrealDirectorLessonsStore;
use crate::director_runner::{
    BeatBrief, DirectorRunnerHandle, ENTITY_MEMORY_LONG_IN_PROMPT, ENTITY_MEMORY_SHORT_IN_PROMPT,
    RECENT_BEATS_IN_PROMPT,
};
use crate::director_store::SurrealDirectorStore;
use crate::entity_heartbeat_defaults::parse_beat_condition;
use crate::heartbeat_results_store::{
    HeartbeatResult, HeartbeatResultsStore, build_canned_summary,
};
use crate::heartbeat_store::HeartbeatDispatcher;
use crate::local_vault::{self, ConsolidationSummary};
use crate::mission_store::SurrealMissionStore;
use crate::state::{Entity, Heartbeat, Mission};

const LESSONS_RECALL_TOP_K: usize = 6;

/// Runs a fired heartbeat through its director (LLM), falling back to a canned
/// summary on any failure so the chat surface always gets a beat.
pub struct ChatHeartbeatDispatcher {
    results: Arc<dyn HeartbeatResultsStore>,
    directors: Arc<SurrealDirectorStore>,
    missions: Arc<SurrealMissionStore>,
    lessons: Arc<SurrealDirectorLessonsStore>,
    entities: Arc<crate::state::ControlState>,
    runner: Arc<dyn DirectorRunnerHandle>,
    credit_ledger: Option<Arc<CreditLedger>>,
    embedder: Option<Arc<dyn EmbeddingProvider>>,
    /// Needed only by Drive beats, which call the mission replanner (and so
    /// need the resolved LLM config). Absent in tests and on any deployment
    /// that never runs a loop-enabled mission.
    server_state: Option<crate::state::ServerState>,
}

impl ChatHeartbeatDispatcher {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        results: Arc<dyn HeartbeatResultsStore>,
        directors: Arc<SurrealDirectorStore>,
        missions: Arc<SurrealMissionStore>,
        lessons: Arc<SurrealDirectorLessonsStore>,
        entities: Arc<crate::state::ControlState>,
        runner: Arc<dyn DirectorRunnerHandle>,
        credit_ledger: Option<Arc<CreditLedger>>,
        embedder: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            results,
            directors,
            missions,
            lessons,
            entities,
            runner,
            credit_ledger,
            embedder,
            server_state: None,
        })
    }

    /// Attach the server state Drive beats need to reach the replanner.
    /// Builder-style so every existing construction site stays valid.
    pub fn with_server_state(self: Arc<Self>, state: crate::state::ServerState) -> Arc<Self> {
        Arc::new(Self {
            results: self.results.clone(),
            directors: self.directors.clone(),
            missions: self.missions.clone(),
            lessons: self.lessons.clone(),
            entities: self.entities.clone(),
            runner: self.runner.clone(),
            credit_ledger: self.credit_ledger.clone(),
            embedder: self.embedder.clone(),
            server_state: Some(state),
        })
    }
}

/// Resolve a heartbeat's director reference: exact id, then slug, then the
/// first active director of the group with that id.
async fn resolve_director(
    directors: &SurrealDirectorStore,
    reference: &str,
) -> Option<crate::state::Director> {
    if let Ok(Some(d)) = directors.get(reference).await {
        return Some(d);
    }
    if let Ok(Some(d)) = directors.get_by_slug(reference).await {
        return Some(d);
    }
    if let Ok(all) = directors.list_all().await {
        return all
            .into_iter()
            .find(|d| d.is_active && d.group_id.as_deref() == Some(reference));
    }
    None
}

/// Which lesson scopes a heartbeat may recall.
///
/// Accepts the legacy `global` scope and `workspace:<id>` alike (chat writes
/// entity-less lessons as `workspace:<id>` going forward, `global` before), plus
/// `entity:<id>` / `mission:<id>` for this beat's own target. Everything else —
/// another tenant's workspace, entity or mission — is filtered out.
#[derive(Debug, Clone)]
struct LessonRecallScope {
    workspace_id: String,
    entity_id: Option<String>,
    mission_id: Option<String>,
}

impl LessonRecallScope {
    fn admits(&self, scope: &str) -> bool {
        if scope == "global" {
            return true;
        }
        if let Some(ws) = scope.strip_prefix("workspace:") {
            return ws == self.workspace_id;
        }
        if let Some(eid) = scope.strip_prefix("entity:") {
            return self.entity_id.as_deref() == Some(eid);
        }
        if let Some(mid) = scope.strip_prefix("mission:") {
            return self.mission_id.as_deref() == Some(mid);
        }
        false
    }
}

impl HeartbeatDispatcher for ChatHeartbeatDispatcher {
    fn dispatch(&self, heartbeat: Heartbeat) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let results = self.results.clone();
        let directors = self.directors.clone();
        let missions = self.missions.clone();
        let lessons = self.lessons.clone();
        let entities = self.entities.clone();
        let runner = self.runner.clone();
        let credit_ledger = self.credit_ledger.clone();
        let embedder = self.embedder.clone();
        let server_state = self.server_state.clone();
        Box::pin(async move {
            let fired_at = Utc::now();

            // Director lookup: needed for both the LLM path (full Director
            // struct) and the canned fallback (just the name). Heartbeats may
            // reference a director by row id, by slug, or by group id (the id
            // the /api/directors surface exposes) — resolve all three.
            let director = resolve_director(directors.as_ref(), &heartbeat.director_id).await;
            let director_name = director
                .as_ref()
                .map(|d| d.name.clone())
                .unwrap_or_else(|| heartbeat.director_id.clone());

            // Entity context: resolve via ControlState (in-memory cache) so we
            // get the full Entity for the runner. Fall back to a synthesised
            // placeholder so a missing entity still produces a bubble.
            let (entity_for_run, entity_name, pinned_notes, active_mission): (
                Entity,
                String,
                Vec<String>,
                Option<Mission>,
            ) = match heartbeat
                .target_id
                .as_deref()
                .filter(|_| heartbeat.target_kind == "entity")
            {
                Some(eid) => {
                    let entity = {
                        let map = entities.entities.read().await;
                        map.iter().find(|e| e.id == eid).cloned()
                    };
                    let mission = missions
                        .list_active_for_entity(eid)
                        .await
                        .ok()
                        .and_then(|list| list.into_iter().next());
                    let pinned = mission
                        .as_ref()
                        .map(|m| m.canonical_notes.clone())
                        .unwrap_or_default();
                    let resolved = entity.unwrap_or_else(|| Entity {
                        id: eid.to_string(),
                        name: eid.to_string(),
                        ..Entity::default()
                    });
                    let name = resolved.name.clone();
                    (resolved, name, pinned, mission)
                }
                None => {
                    let e = Entity {
                        id: "global".to_string(),
                        name: "global".to_string(),
                        ..Entity::default()
                    };
                    (e, "global".to_string(), Vec::new(), None)
                }
            };

            let entity_id_for_record: Option<String> = if heartbeat.target_kind == "entity" {
                heartbeat.target_id.clone()
            } else {
                None
            };

            // Recall lessons for this director — semantic when an embedder is
            // available (query = the run's entity/mission context), recency
            // otherwise. Failures are soft — an empty slice means the
            // assembled prompt simply omits the lessons block.
            let query_text = {
                let mut parts = vec![entity_name.clone()];
                if let Some(m) = active_mission.as_ref() {
                    parts.push(m.title.clone());
                }
                parts.extend(pinned_notes.iter().cloned());
                parts.join("\n")
            };
            let query_embedding = crate::lessons::embed_query(embedder.as_ref(), &query_text).await;
            // Lessons are per-director but written from every tenant, so recall
            // is scoped to what this beat is about: instance-global lessons,
            // this beat's workspace, its entity, its active mission. Over-fetch
            // so the filter still leaves up to top-k.
            let beat_workspace = heartbeat
                .workspace_id
                .clone()
                .filter(|w| !w.trim().is_empty())
                .or_else(|| {
                    entity_id_for_record
                        .as_ref()
                        .and_then(|_| entity_for_run.owner.clone())
                        .filter(|w| !w.trim().is_empty())
                })
                .unwrap_or_else(|| "default".to_string());
            let recall_scope = LessonRecallScope {
                workspace_id: beat_workspace,
                entity_id: entity_id_for_record.clone(),
                mission_id: active_mission.as_ref().map(|m| m.id.clone()),
            };
            let recalled = match lessons
                .recall_lessons(
                    &heartbeat.director_id,
                    query_embedding.as_deref(),
                    LESSONS_RECALL_TOP_K * 4,
                )
                .await
            {
                Ok(found) => {
                    let mut found: Vec<crate::state::DirectorLesson> = found
                        .into_iter()
                        .filter(|l| recall_scope.admits(&l.scope))
                        .collect();
                    found.truncate(LESSONS_RECALL_TOP_K);
                    found
                }
                Err(e) => {
                    tracing::warn!(
                        heartbeat_id = %heartbeat.id,
                        director_id = %heartbeat.director_id,
                        error = %e,
                        "heartbeat dispatcher: lesson recall failed; continuing without lessons"
                    );
                    Vec::new()
                }
            };

            // Beat brief: what *this* beat is for, plus the last few beats for
            // this entity so the run compounds instead of restating itself.
            // Absent for hand-created heartbeats, which keep the generic ask.
            let beat_brief: Option<BeatBrief> =
                match parse_beat_condition(heartbeat.condition.as_deref()) {
                    Some(cond) => {
                        let recent = match entity_id_for_record.as_deref() {
                        Some(eid) => results
                            .list_for_entity(eid, RECENT_BEATS_IN_PROMPT)
                            .await
                            .unwrap_or_else(|e| {
                                tracing::warn!(
                                    heartbeat_id = %heartbeat.id,
                                    error = %e,
                                    "heartbeat: prior-beat recall failed; running without history"
                                );
                                Vec::new()
                            })
                            .into_iter()
                            .map(|r| r.summary)
                            .collect(),
                        None => Vec::new(),
                    };
                        Some(BeatBrief {
                            intent: cond.intent,
                            focus: cond.focus,
                            recent,
                        })
                    }
                    None => None,
                };

            // Entity memory: the same bounded slice the agent-run path injects
            // (durable facts first, then the freshest short-term notes), so a
            // beat sees what the entity's own runs have learned. Soft-fails to
            // empty — the assembled prompt then simply omits the section.
            let entity_memory: Vec<String> =
                match (server_state.as_ref(), entity_id_for_record.as_deref()) {
                    (Some(state), Some(eid)) => {
                        let mut lines = crate::entity_memory::recall_entity_long(
                            state.memory.clone(),
                            eid,
                            ENTITY_MEMORY_LONG_IN_PROMPT,
                        )
                        .await;
                        for line in crate::entity_memory::recall_entity_short(
                            state.memory.clone(),
                            eid,
                            ENTITY_MEMORY_SHORT_IN_PROMPT,
                        )
                        .await
                        {
                            // A short line already promoted to long-term would
                            // otherwise appear twice in the prompt.
                            if !lines.contains(&line) {
                                lines.push(line);
                            }
                        }
                        lines
                    }
                    _ => Vec::new(),
                };

            // Zero-balance workspaces get the canned (no-LLM) bubble instead of
            // silently running inference the owner can no longer bill for.
            let credit_ok = match (credit_ledger.as_ref(), heartbeat.workspace_id.as_deref()) {
                (Some(ledger), Some(workspace)) => !ledger.credit_exhausted(workspace).await,
                _ => true,
            };

            // ── The beat that acts ──────────────────────────────────────────
            //
            // A Drive beat on a loop-enabled mission is the whole "heartbeat
            // does the actions" mechanism: it reads what the last iteration
            // produced and queues the next iteration's executable tasks, which
            // the task executor picks up on its own tick. The beat itself still
            // only writes prose — the *work* is done by the executor, inside
            // its directory jail, under the same approval rules as ever.
            //
            // Everything here is skipped when the workspace is out of credit:
            // an iteration that cannot be replanned should pause, not stall
            // halfway.
            let drive_summary: Option<String> = match (
                beat_brief.as_ref().map(|b| b.intent),
                active_mission.as_ref(),
                server_state.as_ref(),
                credit_ok,
            ) {
                (
                    Some(crate::entity_heartbeat_defaults::BeatIntent::Drive),
                    Some(mission),
                    Some(state),
                    true,
                ) if mission.loop_config.is_some() => {
                    // The loop only drives entities the operator put in
                    // auto-run with an executing heartbeat: the same two
                    // switches that gate every other unattended action.
                    let allowed = matches!(
                        crate::entity_runtime::effective_automation_mode(&entity_for_run),
                        crate::state::AutomationMode::AutoRun
                    ) && matches!(
                        crate::entity_runtime::effective_heartbeat_mode(&entity_for_run),
                        crate::state::EntityHeartbeatMode::Execute
                    );
                    if !allowed {
                        Some(format!(
                            "Holding: \"{}\" is ready to drive, but {} is not set to run work \
                             automatically. Turn on auto-run for this entity to let it proceed.",
                            mission.title.trim(),
                            entity_name
                        ))
                    } else {
                        use crate::mission_replanner::AdvanceOutcome;
                        match crate::mission_replanner::advance_mission_loop(
                            state, &entities, mission,
                        )
                        .await
                        {
                            AdvanceOutcome::Queued(n) => Some(format!(
                                "Working on \"{}\": queued {n} next step{} from the last round's \
                                 results. They run on their own from here.",
                                mission.title.trim(),
                                if n == 1 { "" } else { "s" }
                            )),
                            AdvanceOutcome::Completed => Some(format!(
                                "\"{}\" is done — the definition of done is met.",
                                mission.title.trim()
                            )),
                            AdvanceOutcome::Stopped(reason) => Some(format!(
                                "Stopped work on \"{}\": {reason}. It needs your call before it \
                                 goes further.",
                                mission.title.trim()
                            )),
                            AdvanceOutcome::Held => None,
                            AdvanceOutcome::Paused => Some(format!(
                                "Could not plan the next step for \"{}\" this time — the model was \
                                 unavailable. Trying again on the next beat.",
                                mission.title.trim()
                            )),
                        }
                    }
                }
                _ => None,
            };
            // Run the director (or fall back to the canned summary). Director
            // lookup failure is treated the same as a runner failure — we still
            // produce a bubble.
            // The bool is "this is the canned fallback, not a real director
            // run" — canned bubbles still get recorded and shown in chat, but
            // they are not worth interrupting the operator for, so only real
            // runs reach the feed (and from there, notifications).
            let (summary, summary_is_canned) = if let Some(text) = drive_summary {
                // The replanner already did the thinking (and the LLM call);
                // narrating it again would cost a second round trip to say
                // less.
                (text, false)
            } else if !credit_ok {
                tracing::info!(
                    heartbeat_id = %heartbeat.id,
                    workspace_id = ?heartbeat.workspace_id,
                    "heartbeat: workspace credit exhausted; using canned summary"
                );
                (
                    build_canned_summary(&director_name, &entity_name, &pinned_notes, fired_at),
                    true,
                )
            } else {
                match director.as_ref() {
                    Some(d) => {
                        // Primary run, then at most one retry through the
                        // local lane — a cloud hiccup should degrade to the
                        // local muscle before it degrades to canned text.
                        let mut outcome = runner
                            .run(
                                d,
                                &entity_for_run,
                                active_mission.as_ref(),
                                &recalled,
                                beat_brief.as_ref(),
                                &entity_memory,
                                fired_at,
                            )
                            .await;
                        if let Err(e) = &outcome {
                            tracing::warn!(
                                heartbeat_id = %heartbeat.id,
                                director_id = %heartbeat.director_id,
                                error = %e,
                                "heartbeat: director run failed; trying the local lane once"
                            );
                            match runner
                                .run_local_retry(
                                    d,
                                    &entity_for_run,
                                    active_mission.as_ref(),
                                    &recalled,
                                    beat_brief.as_ref(),
                                    &entity_memory,
                                    fired_at,
                                )
                                .await
                            {
                                Some(Ok(run)) => {
                                    tracing::info!(
                                        heartbeat_id = %heartbeat.id,
                                        director_id = %heartbeat.director_id,
                                        model = %run.model,
                                        "heartbeat: local retry succeeded"
                                    );
                                    outcome = Ok(run);
                                }
                                Some(Err(retry_err)) => {
                                    tracing::warn!(
                                        heartbeat_id = %heartbeat.id,
                                        director_id = %heartbeat.director_id,
                                        error = %retry_err,
                                        "heartbeat: local retry failed; falling back to canned summary"
                                    );
                                }
                                None => {
                                    tracing::info!(
                                        heartbeat_id = %heartbeat.id,
                                        director_id = %heartbeat.director_id,
                                        "heartbeat: no local lane to retry on; falling back to canned summary"
                                    );
                                }
                            }
                        }
                        match outcome {
                            Ok(run) => {
                                tracing::info!(
                                    heartbeat_id = %heartbeat.id,
                                    director_id = %heartbeat.director_id,
                                    model = %run.model,
                                    input_tokens = run.input_tokens,
                                    output_tokens = run.output_tokens,
                                    latency_ms = run.latency_ms,
                                    "heartbeat: director run succeeded"
                                );
                                if let (Some(ledger), Some(workspace)) =
                                    (credit_ledger.as_ref(), heartbeat.workspace_id.as_deref())
                                {
                                    debit_director_run_cost(
                                        ledger.as_ref(),
                                        workspace,
                                        &run.model,
                                        run.input_tokens,
                                        run.output_tokens,
                                    )
                                    .await;
                                }
                                (run.text, false)
                            }
                            Err(_) => (
                                build_canned_summary(
                                    &director_name,
                                    &entity_name,
                                    &pinned_notes,
                                    fired_at,
                                ),
                                true,
                            ),
                        }
                    }
                    None => {
                        tracing::warn!(
                            heartbeat_id = %heartbeat.id,
                            director_id = %heartbeat.director_id,
                            "heartbeat: director not found; falling back to canned summary"
                        );
                        (
                            build_canned_summary(
                                &director_name,
                                &entity_name,
                                &pinned_notes,
                                fired_at,
                            ),
                            true,
                        )
                    }
                }
            };

            let result = HeartbeatResult {
                id: format!("hbr_{}", Uuid::new_v4()),
                heartbeat_id: heartbeat.id.clone(),
                workspace_id: heartbeat.workspace_id.clone(),
                entity_id: entity_id_for_record,
                director_id: heartbeat.director_id.clone(),
                fired_at: fired_at.to_rfc3339(),
                summary,
                created_at: fired_at.to_rfc3339(),
            };

            if let Err(e) = results.record(&result).await {
                tracing::warn!(
                    heartbeat_id = %heartbeat.id,
                    error = %e,
                    "heartbeat dispatcher: failed to persist result"
                );
            } else {
                tracing::info!(
                    heartbeat_id = %heartbeat.id,
                    director_id = %heartbeat.director_id,
                    "heartbeat fired: result recorded"
                );
            }

            // Mirror real beats onto the operator feed. Until now a beat only
            // existed in `heartbeat_results`, which is polled by the web chat
            // for the *active entity only* — so a beat for any other entity was
            // effectively invisible, and mobile never saw beats at all. Putting
            // it on the feed makes one write reach the activity timeline, the
            // web shell, mobile, and desktop notifications.
            if !summary_is_canned {
                entities
                    .push_operator_feed_surface(
                        crate::state::OperatorFeedKind::HeartbeatResult,
                        format!("{director_name} · {entity_name}"),
                        Some(result.summary.clone()),
                        result.entity_id.clone(),
                        None,
                    )
                    .await;
            }

            // Consolidation beats additionally mirror their summary into the
            // local vault, separate from the chat/operator-feed surfaces above.
            if is_consolidation_heartbeat(&heartbeat) {
                let workspace_id = heartbeat.workspace_id.as_deref().unwrap_or("default");
                let entity_ref = if result.entity_id.is_some() {
                    Some(&entity_for_run)
                } else {
                    None
                };
                local_vault::mirror_consolidation_summary(ConsolidationSummary {
                    workspace_id,
                    heartbeat_id: &heartbeat.id,
                    director_id: &heartbeat.director_id,
                    fired_at: &result.fired_at,
                    summary: &result.summary,
                    entity: entity_ref,
                })
                .await;
            }
        })
    }
}

// Loose name/condition text match — no dedicated heartbeat "kind" field exists yet.
fn is_consolidation_heartbeat(heartbeat: &Heartbeat) -> bool {
    let name = heartbeat.name.to_ascii_lowercase();
    let condition = heartbeat
        .condition
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    name.contains("consolidate")
        || name.contains("consolidation")
        || condition.contains("consolidate")
        || condition.contains("worker:consolidate")
}

// Skip the ledger write entirely for zero-cost (e.g. local/free) models.
async fn debit_director_run_cost(
    ledger: &CreditLedger,
    workspace_id: &str,
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
) {
    let calc = consistent_world_llm::cost::CostCalculator::new();
    let cost = calc.calculate(model, input_tokens, output_tokens);
    if cost > 0.0 {
        let _ = ledger.debit_for_cost(workspace_id, cost).await;
    }
}

#[cfg(test)]
mod lesson_scope_tests {
    use super::LessonRecallScope;

    #[test]
    fn a_beat_recalls_only_its_own_tenants_lessons() {
        let scope = LessonRecallScope {
            workspace_id: "ws-a".into(),
            entity_id: Some("ent-a".into()),
            mission_id: Some("mis-a".into()),
        };
        // Both the legacy and the new shape of an entity-less lesson.
        assert!(scope.admits("global"));
        assert!(scope.admits("workspace:ws-a"));
        assert!(scope.admits("entity:ent-a"));
        assert!(scope.admits("mission:mis-a"));
        assert!(!scope.admits("workspace:ws-b"));
        assert!(!scope.admits("entity:ent-b"));
        assert!(!scope.admits("mission:mis-b"));
        assert!(!scope.admits("workspace:"));
        assert!(!scope.admits("unknown"));

        let untargeted = LessonRecallScope {
            workspace_id: "default".into(),
            entity_id: None,
            mission_id: None,
        };
        assert!(!untargeted.admits("entity:ent-a"));
        assert!(untargeted.admits("workspace:default"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::director_runner::DirectorRun;
    use crate::heartbeat_results_store::HeartbeatResult;
    use crate::state::{Director, DirectorLesson, Entity, Heartbeat, Mission};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use surrealdb::engine::any::connect;

    struct StubRunner {
        outcome: Mutex<Option<anyhow::Result<DirectorRun>>>,
        /// Handed back once by `run_local_retry`; `None` = no local lane
        /// (the trait default), mirroring production's decision result.
        retry_outcome: Mutex<Option<anyhow::Result<DirectorRun>>>,
        /// What the dispatcher handed us, so tests can assert the beat brief.
        seen_beat: Mutex<Option<Option<BeatBrief>>>,
        /// Entity-memory slice the dispatcher handed to `run`.
        seen_memory: Mutex<Option<Vec<String>>>,
    }

    impl StubRunner {
        fn ok(text: &str) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(Some(Ok(ok_run(text)))),
                retry_outcome: Mutex::new(None),
                seen_beat: Mutex::new(None),
                seen_memory: Mutex::new(None),
            })
        }

        fn err(msg: &'static str) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(Some(Err(anyhow::anyhow!(msg)))),
                retry_outcome: Mutex::new(None),
                seen_beat: Mutex::new(None),
                seen_memory: Mutex::new(None),
            })
        }

        fn err_with_local_retry(
            msg: &'static str,
            retry: anyhow::Result<DirectorRun>,
        ) -> Arc<Self> {
            Arc::new(Self {
                outcome: Mutex::new(Some(Err(anyhow::anyhow!(msg)))),
                retry_outcome: Mutex::new(Some(retry)),
                seen_beat: Mutex::new(None),
                seen_memory: Mutex::new(None),
            })
        }
    }

    fn ok_run(text: &str) -> DirectorRun {
        DirectorRun {
            text: text.to_string(),
            model: "gpt-test".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            latency_ms: 50,
        }
    }

    #[async_trait]
    impl DirectorRunnerHandle for StubRunner {
        async fn run(
            &self,
            _director: &Director,
            _entity: &Entity,
            _mission: Option<&Mission>,
            _lessons: &[DirectorLesson],
            beat: Option<&BeatBrief>,
            entity_memory: &[String],
            _fired_at: chrono::DateTime<chrono::Utc>,
        ) -> anyhow::Result<DirectorRun> {
            *self.seen_beat.lock().unwrap() = Some(beat.cloned());
            *self.seen_memory.lock().unwrap() = Some(entity_memory.to_vec());
            self.outcome
                .lock()
                .unwrap()
                .take()
                .expect("StubRunner outcome already consumed")
        }

        async fn run_local_retry(
            &self,
            _director: &Director,
            _entity: &Entity,
            _mission: Option<&Mission>,
            _lessons: &[DirectorLesson],
            _beat: Option<&BeatBrief>,
            _entity_memory: &[String],
            _fired_at: chrono::DateTime<chrono::Utc>,
        ) -> Option<anyhow::Result<DirectorRun>> {
            self.retry_outcome.lock().unwrap().take()
        }
    }

    async fn fixture() -> (
        Arc<crate::heartbeat_results_store::SurrealHeartbeatResultsStore>,
        Arc<SurrealDirectorStore>,
        Arc<SurrealMissionStore>,
        Arc<SurrealDirectorLessonsStore>,
        Arc<crate::state::ControlState>,
        Heartbeat,
    ) {
        let db = connect("memory").await.expect("surreal mem");
        db.use_ns("test").use_db("test").await.expect("use ns/db");
        let db = Arc::new(db);

        let results = crate::heartbeat_results_store::SurrealHeartbeatResultsStore::new(db.clone());
        results.init_schema().await.expect("results schema");

        let directors = SurrealDirectorStore::new(db.clone());
        directors.init_schema().await.expect("director schema");
        let d = Director {
            id: "dir_ops".to_string(),
            slug: "operations".to_string(),
            name: "Operations".to_string(),
            prompt: "Be the operator.".to_string(),
            is_active: true,
            created_at: "2026-05-24T00:00:00Z".to_string(),
            updated_at: "2026-05-24T00:00:00Z".to_string(),
            ..Director::default()
        };
        directors.upsert(&d).await.expect("upsert director");

        let missions = SurrealMissionStore::new(db.clone());
        missions.init_schema().await.expect("mission schema");

        let lessons = SurrealDirectorLessonsStore::new(db.clone());
        lessons.init_schema().await.expect("lessons schema");

        let control = Arc::new(crate::state::ControlState::new(
            "openai",
            "https://api.openai.com",
            "gpt-4o-mini",
        ));
        {
            let mut map = control.entities.write().await;
            map.push(Entity {
                id: "ent_qsme".to_string(),
                name: "QS Me".to_string(),
                ..Entity::default()
            });
        }

        let heartbeat = Heartbeat {
            id: "hb_test".to_string(),
            workspace_id: Some("ws_test".to_string()),
            name: "QS Me daily".to_string(),
            enabled: true,
            target_kind: "entity".to_string(),
            target_id: Some("ent_qsme".to_string()),
            director_id: "dir_ops".to_string(),
            cron: None,
            cadence_secs: Some(60),
            condition: None,
            last_fired_at: None,
            next_fire_at: None,
            lease_until: None,
            lease_token: None,
            created_at: "2026-05-24T00:00:00Z".to_string(),
            updated_at: "2026-05-24T00:00:00Z".to_string(),
        };

        (results, directors, missions, lessons, control, heartbeat)
    }

    #[tokio::test]
    async fn dispatch_success_path_uses_runner_text() {
        let (results, directors, missions, lessons, control, heartbeat) = fixture().await;
        let runner = StubRunner::ok("Today: ship the chat loop. No paywall yet.");

        let dispatcher = ChatHeartbeatDispatcher::new(
            results.clone(),
            directors,
            missions,
            lessons,
            control,
            runner,
            None,
            None,
        );

        dispatcher.dispatch(heartbeat.clone()).await;

        let rows = results
            .list_for_entity("ent_qsme", 10)
            .await
            .expect("list results");
        assert_eq!(rows.len(), 1, "exactly one result recorded");
        let row: &HeartbeatResult = &rows[0];
        assert_eq!(row.summary, "Today: ship the chat loop. No paywall yet.");
        assert_eq!(row.director_id, "dir_ops");
        assert_eq!(row.entity_id.as_deref(), Some("ent_qsme"));
    }

    #[tokio::test]
    async fn default_beat_reaches_the_runner_with_intent_and_prior_beats() {
        let (results, directors, missions, lessons, control, mut heartbeat) = fixture().await;
        heartbeat.condition =
            Some("beat:review:weekly-review | Did this week move the goal?".to_string());

        // A prior beat for the same entity — the brief must replay it.
        results
            .record(&HeartbeatResult {
                id: "hbr_prior".to_string(),
                heartbeat_id: "hb_test".to_string(),
                workspace_id: Some("ws_test".to_string()),
                entity_id: Some("ent_qsme".to_string()),
                director_id: "dir_ops".to_string(),
                fired_at: "2026-05-23T09:00:00Z".to_string(),
                summary: "Variation #12 still unsigned.".to_string(),
                created_at: "2026-05-23T09:00:00Z".to_string(),
            })
            .await
            .expect("record prior");

        let runner = StubRunner::ok("Nothing moved on the wedge this week.");
        let dispatcher = ChatHeartbeatDispatcher::new(
            results.clone(),
            directors,
            missions,
            lessons,
            control,
            runner.clone(),
            None,
            None,
        );
        dispatcher.dispatch(heartbeat).await;

        let seen = runner.seen_beat.lock().unwrap().clone();
        let brief = seen
            .expect("runner was called")
            .expect("beat brief built from the condition");
        assert_eq!(
            brief.intent,
            crate::entity_heartbeat_defaults::BeatIntent::Review
        );
        assert_eq!(brief.focus.as_deref(), Some("Did this week move the goal?"));
        assert_eq!(
            brief.recent,
            vec!["Variation #12 still unsigned.".to_string()]
        );
    }

    #[tokio::test]
    async fn hand_created_heartbeat_carries_no_beat_brief() {
        let (results, directors, missions, lessons, control, mut heartbeat) = fixture().await;
        heartbeat.condition = Some("whenever the deploy finishes".to_string());

        let runner = StubRunner::ok("All good.");
        let dispatcher = ChatHeartbeatDispatcher::new(
            results,
            directors,
            missions,
            lessons,
            control,
            runner.clone(),
            None,
            None,
        );
        dispatcher.dispatch(heartbeat).await;

        let seen = runner.seen_beat.lock().unwrap().clone();
        assert!(
            seen.expect("runner was called").is_none(),
            "an unrecognised condition must not be coerced into a beat intent"
        );
    }

    #[tokio::test]
    async fn dispatch_fallback_path_uses_canned_summary() {
        let (results, directors, missions, lessons, control, heartbeat) = fixture().await;
        let runner = StubRunner::err("HTTP 401: bad key");

        let dispatcher = ChatHeartbeatDispatcher::new(
            results.clone(),
            directors,
            missions,
            lessons,
            control,
            runner,
            None,
            None,
        );

        dispatcher.dispatch(heartbeat.clone()).await;

        let rows = results
            .list_for_entity("ent_qsme", 10)
            .await
            .expect("list results");
        assert_eq!(rows.len(), 1, "exactly one result recorded");
        let summary = &rows[0].summary;
        // build_canned_summary's signature includes the director name + entity name
        // and an opening phrase referencing the heartbeat firing.
        assert!(
            summary.contains("Operations"),
            "canned summary should reference director name: {summary}"
        );
        assert!(
            summary.contains("QS Me"),
            "canned summary should reference entity name: {summary}"
        );
    }

    #[tokio::test]
    async fn failed_primary_run_retries_on_local_lane_and_records_its_summary() {
        let (results, directors, missions, lessons, control, heartbeat) = fixture().await;
        let runner = StubRunner::err_with_local_retry(
            "HTTP 502: cloud down",
            Ok(ok_run(
                "Local muscle: variation #12 still needs a signature.",
            )),
        );

        let dispatcher = ChatHeartbeatDispatcher::new(
            results.clone(),
            directors,
            missions,
            lessons,
            control,
            runner,
            None,
            None,
        );
        dispatcher.dispatch(heartbeat).await;

        let rows = results
            .list_for_entity("ent_qsme", 10)
            .await
            .expect("list results");
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].summary, "Local muscle: variation #12 still needs a signature.",
            "a successful local retry must be recorded like a normal run"
        );
    }

    #[tokio::test]
    async fn failed_local_retry_still_falls_back_to_canned() {
        let (results, directors, missions, lessons, control, heartbeat) = fixture().await;
        let runner = StubRunner::err_with_local_retry(
            "HTTP 502: cloud down",
            Err(anyhow::anyhow!("connection refused (:11434)")),
        );

        let dispatcher = ChatHeartbeatDispatcher::new(
            results.clone(),
            directors,
            missions,
            lessons,
            control,
            runner,
            None,
            None,
        );
        dispatcher.dispatch(heartbeat).await;

        let rows = results
            .list_for_entity("ent_qsme", 10)
            .await
            .expect("list results");
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].summary.contains("Operations") && rows[0].summary.contains("QS Me"),
            "double failure must still produce the canned bubble: {}",
            rows[0].summary
        );
    }

    /// Serves canned entity-memory lines by namespace suffix. A stub instead
    /// of `SurrealMemoryStore::connect_memory()` because the real store's
    /// `list_recent_memories` query fails to parse on surrealdb 2.6 (`ORDER BY`
    /// on an unselected field) — the dispatcher plumbing under test here is
    /// independent of that bug.
    struct StubMemory;

    #[async_trait]
    impl consistent_world_memory::store::MemoryStore for StubMemory {
        async fn upsert_session(
            &self,
            _session: &consistent_world_common::types::Session,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn list_sessions(
            &self,
        ) -> anyhow::Result<Vec<consistent_world_common::types::Session>> {
            Ok(vec![])
        }
        async fn save_message(
            &self,
            _id: &str,
            _session_id: &str,
            _role: &str,
            _content: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn get_session_history(
            &self,
            _session_id: &str,
            _limit: usize,
        ) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        async fn get_session_messages(
            &self,
            _session_id: &str,
            _limit: usize,
        ) -> anyhow::Result<Vec<(String, String)>> {
            Ok(vec![])
        }
        async fn save_memory(
            &self,
            _agent_id: &str,
            _content: &str,
            _embedding: Option<&[f32]>,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn delete_session(&self, _session_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        async fn search_memories_semantic(
            &self,
            _agent_id: &str,
            _query_embedding: &[f32],
            _top_k: usize,
        ) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        async fn list_recent_memories(
            &self,
            agent_id: &str,
            _limit: usize,
        ) -> anyhow::Result<Vec<String>> {
            // "Ship weekly" lives in both namespaces, as a promoted line
            // would — the dispatcher must inject it only once.
            if agent_id.ends_with(":long") {
                Ok(vec!["Ship weekly".to_string()])
            } else if agent_id.ends_with(":short") {
                Ok(vec![
                    "Investor call moved to Friday".to_string(),
                    "Ship weekly".to_string(),
                ])
            } else {
                Ok(vec![])
            }
        }
        async fn delete_memories_for_agent(&self, _agent_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn entity_memory_reaches_the_runner_when_server_state_is_attached() {
        let (results, directors, missions, lessons, control, heartbeat) = fixture().await;
        let mem: Arc<dyn consistent_world_memory::store::MemoryStore> = Arc::new(StubMemory);

        let runner = StubRunner::ok("On it.");
        let dispatcher = ChatHeartbeatDispatcher::new(
            results,
            directors,
            missions,
            lessons,
            control,
            runner.clone(),
            None,
            None,
        )
        .with_server_state(crate::state::ServerState::new(
            "test".to_string(),
            None,
            mem,
        ));
        dispatcher.dispatch(heartbeat).await;

        let seen = runner
            .seen_memory
            .lock()
            .unwrap()
            .clone()
            .expect("runner was called");
        assert!(seen.iter().any(|l| l == "Ship weekly"), "memory: {seen:?}");
        assert!(
            seen.iter().any(|l| l == "Investor call moved to Friday"),
            "memory: {seen:?}"
        );
        assert_eq!(
            seen.iter().filter(|l| *l == "Ship weekly").count(),
            1,
            "a promoted line must not appear twice"
        );
    }

    #[tokio::test]
    async fn no_server_state_means_no_entity_memory() {
        let (results, directors, missions, lessons, control, heartbeat) = fixture().await;
        let runner = StubRunner::ok("Plain beat.");
        let dispatcher = ChatHeartbeatDispatcher::new(
            results,
            directors,
            missions,
            lessons,
            control,
            runner.clone(),
            None,
            None,
        );
        dispatcher.dispatch(heartbeat).await;

        let seen = runner
            .seen_memory
            .lock()
            .unwrap()
            .clone()
            .expect("runner was called");
        assert!(
            seen.is_empty(),
            "no state handle, no memory lines: {seen:?}"
        );
    }
}
