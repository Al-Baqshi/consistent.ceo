//! Bulletproof Architect — the Inspect → Forward → Director → Generate → Verify
//! pipeline that every chat turn (sync) and every scheduled/heartbeat task
//! (async) must traverse before producing user-visible output.
//!
//! Runs in shadow mode: Inspect reads the world, Forward names the gaps in it,
//! and Verify resolves the draft the gateway already produced against what
//! Inspect found. Director and Generate are deliberately not built — see the
//! note on the module list below — and record themselves as `Skipped`.
//!
//! Nothing here can spend a token: the crate has no LLM dependency, which makes
//! that provable from `Cargo.toml` rather than from an argument about control
//! flow.
//!
//! See `docs/diagrams/consistent-world-future-state.v2.mmd` for the contract this
//! crate is implementing.

pub mod audit;
pub mod cite;
pub mod forward;
pub mod inspect;
pub mod verify;

// There is deliberately no `director` or `generate` module. Both were six-line
// placeholders, and the decision recorded in `docs/ROADMAP.md` (P2.3) is not to
// build them: shadow mode verifies the answer the gateway already produced, so
// it needs neither a router nor a drafter, and the gateway already owns both.
// `StepName::Director` and `StepName::Generate` remain — the pipeline still
// reports five stages and records those two as `Skipped` with a reason — but a
// file that exists only to hold a placeholder costs more than it returns.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub use audit::{AuditEvent, AuditWriter, SurrealAuditStore};
pub use cite::{CiteRef, CiteSource, EvidenceState};
pub use forward::{CandidateState, ForwardView, Gap, GapKind, forward};
pub use verify::{
    Claim, ClaimVerdict, CiteIndex, TurnVerdict, VerifyPolicy, VerifyReport, extract_claims, verify,
};
pub use inspect::{
    DocsReader, EntityReader, LessonsReader, MemoryReader, MissionsReader, Snapshot,
    SnapshotSources, inspect,
};

/// Input to a single Architect run. Same shape for sync (chat) and async
/// (heartbeat / scheduler) triggers — the harness fills it in from whichever
/// trigger fired.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnInput {
    pub turn_id: String,
    pub workspace_id: Option<String>,
    pub entity_id: Option<String>,
    /// `chat`, `heartbeat`, `scheduler`. Stored on every audit row so we can
    /// later separate sync vs async load.
    pub trigger: String,
    /// Free-form user message (chat trigger) or task summary (async trigger).
    pub user_message: Option<String>,
    /// The chat session this turn belongs to. `turn_id` identifies the turn;
    /// this groups turns. They used to be the same string, which made the
    /// `audit_turn_idx` index group unrelated turns together.
    #[serde(default)]
    pub session_id: Option<String>,
    /// The answer the gateway already produced. Verify resolves *this* rather
    /// than drafting its own, which is what makes a shadow run cost nothing.
    #[serde(default)]
    pub draft: Option<String>,
    /// What tools returned during this turn.
    #[serde(default)]
    pub observed: Vec<ObservedFact>,
    #[serde(default)]
    pub task_kind: Option<String>,
}

/// A tool result produced during the turn under inspection.
///
/// Distinguished from snapshot rows because VISION's table distinguishes them:
/// a claim backed by workspace truth is `Verified`, one backed only by a tool
/// result this turn is `Observed`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ObservedFact {
    pub tool: String,
    #[serde(default)]
    pub call_id: Option<String>,
    pub output: String,
}

impl TurnInput {
    /// Build the input for a chat turn.
    ///
    /// A constructor rather than a struct literal so that adding a field later
    /// does not break every call site — the lesson from `CaptureChatResponse`,
    /// which derives no `Default` and has to be edited in two places.
    pub fn chat(
        turn_id: impl Into<String>,
        workspace_id: Option<String>,
        entity_id: Option<String>,
        session_id: Option<String>,
        user_message: Option<String>,
    ) -> Self {
        TurnInput {
            turn_id: turn_id.into(),
            workspace_id,
            entity_id,
            trigger: "chat".to_string(),
            user_message,
            session_id,
            draft: None,
            observed: Vec::new(),
            task_kind: Some("chat".to_string()),
        }
    }

    /// Attach the answer the gateway produced, and what tools returned.
    pub fn with_outcome(mut self, draft: Option<String>, observed: Vec<ObservedFact>) -> Self {
        self.draft = draft;
        self.observed = observed;
        self
    }
}

/// What an Architect run produced. The trace is the source of truth for the
/// audit log and is also returned to the caller (the gateway can use it to
/// compare against the legacy `chat_agent` path during shadow rollout).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnTrace {
    pub turn_id: String,
    pub trigger: String,
    pub snapshot: Option<Snapshot>,
    pub steps: Vec<StepRecord>,
    /// What Forward saw. `#[serde(default)]` throughout so rows written by the
    /// Phase 1 shape still deserialize.
    #[serde(default)]
    pub forward: Option<forward::ForwardView>,
    #[serde(default)]
    pub verdict: Option<verify::TurnVerdict>,
    #[serde(default)]
    pub claims: Vec<verify::ClaimVerdict>,
}

/// Which pipeline step a [`StepRecord`] / [`AuditEvent`] refers to.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StepName {
    Inspect,
    Forward,
    Director,
    Generate,
    Verify,
}

/// Outcome of one pipeline step. `Unimplemented` marks the Phase 1 stub
/// steps; `Skipped`/`Error` are reserved for later phases.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Ok,
    Unimplemented,
    Skipped,
    Error,
}

/// One step's timing and outcome, appended to a [`TurnTrace`] as the
/// pipeline runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub step: StepName,
    pub status: StepStatus,
    pub note: Option<String>,
    pub started_at: String,
    pub finished_at: String,
}

/// Top-level handle the gateway holds in `ServerState`. Cheap to clone (all
/// fields are `Arc`).
#[derive(Clone)]
pub struct Architect {
    sources: SnapshotSources,
    audit: Arc<dyn AuditWriter>,
    policy: verify::VerifyPolicy,
}

impl Architect {
    pub fn new(sources: SnapshotSources, audit: Arc<dyn AuditWriter>) -> Self {
        Self {
            sources,
            audit,
            policy: verify::VerifyPolicy::default(),
        }
    }

    /// Override claim-extraction tuning. Builder-style so `new` keeps its
    /// two-argument shape at the one existing call site.
    pub fn with_policy(mut self, policy: verify::VerifyPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Run the pipeline. Phase 1: only Inspect does real work; the other four
    /// steps record themselves as `Unimplemented`. Every step writes one
    /// [`AuditEvent`] so the trail is uniform from day one.
    pub async fn run(&self, input: TurnInput) -> TurnTrace {
        let mut trace = TurnTrace {
            turn_id: input.turn_id.clone(),
            trigger: input.trigger.clone(),
            snapshot: None,
            steps: Vec::with_capacity(5),
            forward: None,
            verdict: None,
            claims: Vec::new(),
        };

        let started = now_iso();
        let snap = inspect(
            &self.sources,
            input.workspace_id.as_deref(),
            input.entity_id.as_deref(),
        )
        .await;
        let finished = now_iso();
        match snap {
            // Inspect succeeded: audit it and carry the snapshot into the trace.
            Ok(snapshot) => {
                self.audit
                    .append(AuditEvent {
                        turn_id: input.turn_id.clone(),
                        trigger: input.trigger.clone(),
                        workspace_id: input.workspace_id.clone(),
                        entity_id: input.entity_id.clone(),
                        step: "inspect".into(),
                        status: "ok".into(),
                        payload: serde_json::to_value(&snapshot).unwrap_or(serde_json::json!({})),
                        occurred_at: finished.clone(),
                    })
                    .await;
                trace.steps.push(StepRecord {
                    step: StepName::Inspect,
                    status: StepStatus::Ok,
                    note: Some(snapshot.summary_note()),
                    started_at: started,
                    finished_at: finished,
                });
                trace.snapshot = Some(snapshot);
            }
            // Inspect failed: log + audit the error, but don't fail the turn
            // — Phase 1 is shadow mode.
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(error = %msg, turn_id = %input.turn_id, "architect inspect failed");
                self.audit
                    .append(AuditEvent {
                        turn_id: input.turn_id.clone(),
                        trigger: input.trigger.clone(),
                        workspace_id: input.workspace_id.clone(),
                        entity_id: input.entity_id.clone(),
                        step: "inspect".into(),
                        status: "error".into(),
                        payload: serde_json::json!({ "error": msg }),
                        occurred_at: finished.clone(),
                    })
                    .await;
                trace.steps.push(StepRecord {
                    step: StepName::Inspect,
                    status: StepStatus::Error,
                    note: Some(msg),
                    started_at: started,
                    finished_at: finished,
                });
            }
        }

        // ② Forward — deterministic, over the snapshot Inspect just read.
        let fwd_at = now_iso();
        match &trace.snapshot {
            Some(snapshot) => {
                let view = forward::forward(snapshot, &fwd_at);
                self.emit(
                    &input,
                    "forward",
                    "ok",
                    serde_json::to_value(&view).unwrap_or(serde_json::json!({})),
                    &fwd_at,
                )
                .await;
                trace.steps.push(StepRecord {
                    step: StepName::Forward,
                    status: StepStatus::Ok,
                    note: Some(view.summary_note()),
                    started_at: fwd_at.clone(),
                    finished_at: now_iso(),
                });
                trace.forward = Some(view);
            }
            // Inspect failed. Skipped, not Error — this stage did not fail,
            // it had nothing to run on.
            None => {
                self.skip(&mut trace, &input, StepName::Forward, "forward", "no snapshot")
                    .await;
            }
        }

        // ③ Director and ④ Generate are deliberately not wired here.
        //
        // Shadow mode verifies the answer the gateway *already* produced, so it
        // needs neither a router nor a drafter — and keeping this crate free of
        // `consistent-world-llm` is what makes "a shadow run cannot spend a
        // token" provable from the manifest rather than from an argument about
        // control flow. Recorded as `Skipped` with a reason, not
        // `Unimplemented`: the distinction is that nobody is waiting on these.
        self.skip(
            &mut trace,
            &input,
            StepName::Director,
            "director",
            "shadow mode: the gateway already routed this turn",
        )
        .await;
        self.skip(
            &mut trace,
            &input,
            StepName::Generate,
            "generate",
            "shadow mode: verifying the gateway's own draft",
        )
        .await;

        // ⑤ Verify — the claim → cite resolver.
        let ver_at = now_iso();
        match (&trace.snapshot, input.draft.as_deref()) {
            (Some(snapshot), Some(draft)) if !draft.trim().is_empty() => {
                let index = verify::CiteIndex::from_snapshot(snapshot, &input.observed);
                let claims = verify::extract_claims(draft, &self.policy);
                let report = verify::verify(claims, &index, snapshot_rows(snapshot), &self.policy);
                self.emit(
                    &input,
                    "verify",
                    match report.verdict {
                        verify::TurnVerdict::Pass => "pass",
                        verify::TurnVerdict::Ask => "ask",
                        verify::TurnVerdict::Fail => "fail",
                    },
                    serde_json::to_value(&report).unwrap_or(serde_json::json!({})),
                    &ver_at,
                )
                .await;
                trace.steps.push(StepRecord {
                    step: StepName::Verify,
                    status: StepStatus::Ok,
                    note: Some(report.summary_note()),
                    started_at: ver_at.clone(),
                    finished_at: now_iso(),
                });
                trace.verdict = Some(report.verdict);
                trace.claims = report.claims;
            }
            _ => {
                self.skip(
                    &mut trace,
                    &input,
                    StepName::Verify,
                    "verify",
                    "no draft to verify",
                )
                .await;
            }
        }

        trace
    }

    /// Write one audit row.
    async fn emit(
        &self,
        input: &TurnInput,
        step: &str,
        status: &str,
        payload: serde_json::Value,
        at: &str,
    ) {
        self.audit
            .append(AuditEvent {
                turn_id: input.turn_id.clone(),
                trigger: input.trigger.clone(),
                workspace_id: input.workspace_id.clone(),
                entity_id: input.entity_id.clone(),
                step: step.to_string(),
                status: status.to_string(),
                payload,
                occurred_at: at.to_string(),
            })
            .await;
    }

    /// Record a stage that had nothing to do, keeping the trail uniform: every
    /// stage writes exactly one row on every run, whatever happened.
    async fn skip(
        &self,
        trace: &mut TurnTrace,
        input: &TurnInput,
        step: StepName,
        key: &str,
        why: &str,
    ) {
        let at = now_iso();
        self.emit(input, key, "skipped", serde_json::json!({ "reason": why }), &at)
            .await;
        trace.steps.push(StepRecord {
            step,
            status: StepStatus::Skipped,
            note: Some(why.to_string()),
            started_at: at.clone(),
            finished_at: at,
        });
    }
}

/// How many rows Inspect actually found. Verify uses it to tell "this entity
/// has no history yet" (a normal first turn) from "the index failed to build"
/// (a bug in Verify, which must not be blamed on the draft).
fn snapshot_rows(s: &Snapshot) -> usize {
    usize::from(s.entity.is_some())
        + s.recent_memories.len()
        + s.recent_lessons.len()
        + s.active_missions.len()
        + s.open_tasks.len()
        + s.entity_docs.len()
}

/// Shared RFC3339 timestamp used for step timing and audit rows.
pub(crate) fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod pipeline_tests {
    use super::*;
    use crate::inspect::EntityReader;
    use async_trait::async_trait;

    struct OneEntity;
    #[async_trait]
    impl EntityReader for OneEntity {
        async fn read_entity(
            &self,
            _ws: Option<&str>,
            entity_id: &str,
        ) -> anyhow::Result<Option<serde_json::Value>> {
            Ok(Some(serde_json::json!({
                "id": entity_id,
                "name": "BitRegalo",
                "goals": "Ship 3 features this quarter",
            })))
        }
    }

    fn architect() -> Architect {
        Architect::new(
            SnapshotSources {
                entities: Some(Arc::new(OneEntity)),
                ..Default::default()
            },
            Arc::new(audit::NoopAuditWriter),
        )
    }

    fn input(draft: &str) -> TurnInput {
        TurnInput::chat(
            "turn_1",
            Some("ws".into()),
            Some("ent_1".into()),
            Some("sess_1".into()),
            Some("how are we doing?".into()),
        )
        .with_outcome(Some(draft.to_string()), Vec::new())
    }

    #[tokio::test]
    async fn every_stage_still_records_exactly_one_step() {
        // The uniform-trail property from Phase 1: five steps, every run,
        // whatever happened. Only the payloads stopped being empty.
        let trace = architect().run(input("Happy to help.")).await;
        assert_eq!(trace.steps.len(), 5);
        let names: Vec<_> = trace.steps.iter().map(|s| s.step).collect();
        assert_eq!(
            names,
            vec![
                StepName::Inspect,
                StepName::Forward,
                StepName::Director,
                StepName::Generate,
                StepName::Verify,
            ]
        );
        assert!(
            !trace.steps.iter().any(|s| s.status == StepStatus::Unimplemented),
            "no stage reports itself unimplemented any more"
        );
    }

    #[tokio::test]
    async fn a_grounded_answer_passes() {
        let trace = architect()
            .run(input("BitRegalo will ship 3 features this quarter."))
            .await;
        assert_eq!(trace.verdict, Some(verify::TurnVerdict::Pass));
    }

    #[tokio::test]
    async fn an_invented_figure_branches_to_ask() {
        // This is the whole point of the stage: the gateway produced this
        // answer and would have sent it; shadow mode records that it should
        // have asked instead.
        let trace = architect()
            .run(input("BitRegalo shipped 47 features last quarter."))
            .await;
        assert_eq!(trace.verdict, Some(verify::TurnVerdict::Ask));
        assert!(
            trace
                .claims
                .iter()
                .any(|c| c.state == cite::EvidenceState::Unverified)
        );
    }

    #[tokio::test]
    async fn a_turn_with_no_draft_skips_verify_rather_than_failing() {
        let mut i = input("");
        i.draft = None;
        let trace = architect().run(i).await;
        assert_eq!(trace.verdict, None);
        let verify_step = trace
            .steps
            .iter()
            .find(|s| s.step == StepName::Verify)
            .expect("verify step recorded");
        assert_eq!(verify_step.status, StepStatus::Skipped);
    }

    #[tokio::test]
    async fn forward_runs_and_names_gaps() {
        let trace = architect().run(input("Happy to help.")).await;
        let view = trace.forward.expect("forward ran");
        // No missions, no docs on this entity.
        assert!(view.gaps.iter().any(|g| g.kind == forward::GapKind::NoActiveMission));
    }

    #[tokio::test]
    async fn a_phase_one_audit_row_still_deserializes() {
        // Rows written before `forward`/`verdict`/`claims` existed must still
        // read back, or the trail becomes unreadable at the upgrade boundary.
        let old = serde_json::json!({
            "turn_id": "t", "trigger": "chat", "snapshot": null, "steps": []
        });
        let trace: TurnTrace = serde_json::from_value(old).expect("old shape still parses");
        assert!(trace.forward.is_none());
        assert!(trace.claims.is_empty());
    }
}
