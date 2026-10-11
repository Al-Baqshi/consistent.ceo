# Plan A — The heartbeat that acts (iterate until the goal is met)

Goal: the user states a goal; heartbeats then drive an iterative loop — a builder
director develops/executes, a critic director evaluates results against evidence,
and further development is queued based on those results — until the mission's
definition-of-done is met, within hard iteration and budget caps.

Status: **implemented** (2026-09-04), except where noted per phase below.
Verified against `feat/ceo-assistant-phases`.

## What shipped

| Phase | State | Where |
|---|---|---|
| 0 — safety prerequisites | done | `run_agent.rs` (no-workspace = no fs/shell tools; http/browser need `http.fetch`), `crates/tools/src/{shell,filesystem}.rs` (empty capability list denies under `deny_by_default`), `task_executor.rs` (write-through claims + `InFlightGuard`), `agent_sessions.rs` (`resume_interrupted` closes its task) |
| 1 — task lifecycle | done | `META_ATTEMPT_COUNT` + `MAX_AUTO_ATTEMPTS` (crash retries once, a `blocked` verdict never does), `requeue_task` clears the markers, heartbeat lease 2m→10m with `tokio::spawn`ed dispatch, `notify.rs` + mobile + desktop allowlists gained `task_blocked`/`mission_completed` |
| 2 — evidence | done | new `evidence.rs`; `apply_evidence_to_update` downgrades a `done` claim with zero on-disk change to `NeedsReview` (browser actions exempt, non-git dirs never downgrade) |
| 3 — critic | done | `maybe_review_loop_run` in `task_executor.rs` runs the existing `agent_review` spine in the same jail, with the evidence pack appended to the review prompt |
| 4 — iteration driver | done | new `mission_replanner.rs` (`loop_gate` + `advance_mission_loop`), `BeatIntent::Drive`, the Drive branch in `heartbeat_dispatcher.rs`, `provision_drive_beat` |
| 5 — intake + observability | done | `create_mission` chat tool takes `definition_of_done` / `max_iterations` / `budget_usd`; `OperatorFeedKind::MissionIterationAdvanced` |

## Post-audit fixes (2026-09-05)

An adversarial audit found three defects in the loop as shipped:

| Severity | Defect | Fix |
|---|---|---|
| critical | **The loop could never start.** Missions are created `Planned`, but the Drive beat resolves its mission through `list_active_for_entity`, which filters `status = 'active'`. A loop mission was therefore invisible to the beat meant to advance it — it would sit forever, silently, with no error. `advance_mission_loop` even contained Planned→Active promotion code that was unreachable. | A mission created with a `definition_of_done` starts `Active` — asking for it to be driven *is* the instruction to start. |
| critical | **The executor reversed an operator's cancel.** `apply_run_outcome_to_task` rewrote status unconditionally, so a task cancelled mid-run was resurrected by its own finishing run — and via the crash-retry branch could start a *brand-new unattended run* ten minutes after the human said stop. | A `Cancelled` task keeps its status; the run's output is recorded for the operator and nothing is rescheduled. |
| high | **Declining an approval declared victory.** `loop_gate` counted `Cancelled` tasks as success, so refusing one action could mark the whole mission "definition of done met". Separately, replanner tasks flagged `requires_approval` got no approval row — unungatable, pinning `loop_gate` at `Busy` forever. | Cancelled work stops the loop and returns it to the operator; outward-facing replanner work is never marked `executable`, so it surfaces as ordinary work instead of deadlocking. |
| high | **Evidence downgraded real work.** `dirty_delta` is a set difference, so a run editing a file that was *already* dirty added no new path and looked identical to a run that did nothing. | The pack now carries `dirty_content_changed`, a before/after diff fingerprint, so edits to already-dirty files still count. |

**One planned item deliberately not done.** Phase 0 listed "tighten
`effective_automation_mode` away from implicit AutoRun". Flipping that default
would silently stop autonomous execution for every entity created before
`entity_runtime` existed — turning a security tidy-up into a product outage for
exactly the users relying on the feature. The narrower fix shipped instead:
unconfigured entities no longer get *unrestricted* tools (Phase 0's capability
work), so the blast radius is bounded without changing who is allowed to run.
Revisit as a migration that writes an explicit mode onto every entity, not as a
default flip.

Two design decisions taken during implementation, beyond the plan:

- **`mission_goal_loop::tick` now skips loop-enabled missions entirely.** Its
  15-second task-count check would otherwise complete a mission while its
  critic was still running — retiring work nobody verified. The Drive beat
  owns terminal transitions for these missions.
- **An explicit approval outranks the entity's automation mode**
  (`META_APPROVED_AT`). Without it a phone-approved action on a Manual-mode
  entity would sit queued forever, which reads as the product ignoring you.

## Key verified facts

- The heartbeat today is text-only: `heartbeat_dispatcher.rs` does one
  non-streaming LLM completion via `director_runner.rs` (no tools, no task
  creation) and writes a `HeartbeatResult` + feed event. The heartbeat→task
  wiring is an acknowledged gap (`heartbeat_store.rs` module doc).
- The action plumbing exists end-to-end: `mission_planner.rs` is the only writer
  of `Queued + metadata.executable` tasks → `task_executor.rs` runs them
  unattended (`claude` CLI, cwd jail, one at a time) → `mission_goal_loop.rs` +
  `mission_dod.rs` account for outcomes and auto-complete missions.
- **Asset:** `agent_review.rs` already implements the builder→reviewer spine
  (`ReviewPolicy`, `start_review()` creating a linked reviewer run with a
  `VERDICT: PASS | NEEDS_REVIEW` contract, `finish_review()` downgrading the
  task to `NeedsReview` on a failed verdict). It is only wired to the manual
  HTTP agent-run path — never to task_executor or mission_engine.
- **Gap:** `GoalDecision::Verify` in `mission_goal_loop.rs` is a no-op (falls
  into the `_` branch, logs a feed event only). This is the critic's hook point.
- `report_mission_gaps` (orchestrator.rs) already auto-creates follow-up tasks
  on `Continue` — but with `metadata: None`, so they route to the in-process
  prose agent, never the executor. That is the precise break in the loop.
- `effective_automation_mode` returns `AutoRun` when `entity_runtime` is `None`
  (entity_runtime.rs) — an unconfigured entity is executor-eligible. Tighten.
- Budget-cap machinery exists: mission_engine's `META_BUDGET_USD` /
  `META_MAX_BEATS` enforced in `beat_is_due`. `Task.source_task_id` exists for
  iteration lineage.

## Design decisions

1. **Heartbeat is the trigger/deliberator, not the tool-runner.** For entities
   in the action loop, the beat enqueues the next iteration's executable tasks
   (via a replanner that consumes prior results) instead of only writing prose.
   The fast loops (goal loop 15s, executor 60s) do the work between beats.
   Rationale: the veto matrix (`task_is_auto_executable_at`) stays the single
   safety gate; a failed beat degrades to "loop pauses one beat" (Ollama-canned
   fallback safe), never "half-acted".
2. **Director-to-director iteration.** Builder = the mission's
   `assigned_director_id` (fallback: the heartbeat's director); muscle is the
   claude CLI. Critic = a seeded "Verifier" director (`role: "verifier"`),
   referenced per-mission by `loop_config.critic_director_id`. After an executor
   run finishes `done` in a loop-enabled mission, call
   `agent_review::start_review` and execute the reviewer via
   `run_claude_session` in the same cwd, with the evidence pack appended to the
   review prompt. `finish_review` already flips the task to `NeedsReview` on a
   failed verdict; the replanner then queues a bounded fix task
   (`source_task_id` = flagged task, `metadata.iteration` = N+1).
3. **Iteration state lives on the Mission** (serde-defaulted, persisted via the
   existing mission write-through):
   - `loop_config: Option<MissionLoopConfig> { definition_of_done: String,
     max_iterations: u32 (default 5, hard cap 25), budget_usd: f64,
     critic_director_id: Option<String> }`
   - `loop_state: Option<MissionLoopState> { iteration: u32, cost_usd: f64,
     last_verdict: Option<String>, last_advanced_at: Option<String> }`
   Caps are checked before every replan; exhaustion →
   `MissionStatus::UserInputNeeded` + feed event. One iteration = one replan
   round (bounded task fan-out).
4. **Real result evaluation** — new `evidence.rs`: snapshot `git rev-parse HEAD`
   + `git status --porcelain` before the run; after, collect
   `git diff --stat`, porcelain delta, optional `setup.test_command` result into
   `EvidencePack { commits, files_changed, insertions, deletions, dirty_delta,
   tests }` stored under `task.metadata.evidence`. A `done` trailer with zero
   evidence on an `executable` task downgrades to `NeedsReview`
   (`no_observable_change`). The pack is serialized into the critic's prompt.

## Phases (each shippable + testable alone)

### Phase 0 — Safety prerequisites
- `run_agent.rs`: refuse to register FS/Shell tools when `effective_workspace`
  is None instead of defaulting to `"."`; gate HttpTool/BrowserTool behind
  entity execution policy.
- `crates/tools/src/shell.rs`: empty `allowed_capabilities` must deny, not
  allow (deny-by-default like `FileSystemPolicy`).
- `task_executor.rs`: write-through the claim and completion blocks
  (`persist_task_row`); replace the raw `in_flight` release with a drop guard so
  a panic releases the slot.
- `agent_sessions.rs::resume_interrupted`: when run metadata says
  `source == "task-executor"` with a `task_id`, apply `plan_task_run_update` on
  completion (share the executor's completion block).
- Tighten `effective_automation_mode` default away from implicit AutoRun.

### Phase 1 — Task lifecycle correctness
- Replace binary `META_AUTO_RUN_ATTEMPTED` with `META_ATTEMPT_COUNT` (max 2,
  backoff via `META_NEXT_ATTEMPT_AT`).
- `requeue_task`: clear attempt/next-attempt markers, `outcome`, `result`; use
  `set_task_status`; write through.
- Heartbeat: lease 2min → 10min; `tokio::spawn` each dispatch after `try_claim`
  (persist the schedule bump before spawning).
- `notify.rs`: add `TaskBlocked` and `MissionCompleted` to `is_notifiable` +
  titles.
- `CreateTaskRequest` gains optional `executable` (default false) so chat/API
  tasks can opt into the executor shape.

### Phase 2 — Evidence verification
New `crates/gateway/src/evidence.rs`; hooks in `task_executor.rs` (snapshot at
start, collect at completion), verdict merge in `plan_task_run_update`,
extension of `verification.rs` flags. Test with a temp git repo fixture.

### Phase 3 — Critic director
Seed verifier in `director_seed.rs`; wire `agent_review::start_review/finish_review`
into the executor completion path for loop-enabled missions; reviewer runs via
`run_claude_session`; make `GoalDecision::Verify` trigger reviews for unreviewed
flagged tasks. Reviewer cost accrues to `loop_state.cost_usd`.

### Phase 4 — The iteration driver
- New `crates/gateway/src/mission_replanner.rs`: prompt carries DoD, per-task
  results, evidence packs, critic reasons, iteration N of max; emits 1–3
  executable tasks; returns None on degraded LLM (loop waits; no canned tasks).
- New `BeatIntent::Drive` (`entity_heartbeat_defaults.rs` +
  `intent_instruction` in `director_runner.rs`).
- `heartbeat_dispatcher.rs::dispatch`: when the beat is Drive and the entity has
  a loop-enabled active mission whose tasks are all terminal: check caps → call
  replanner → queue tasks → bump `loop_state` → prose summary becomes the
  iteration narration (same `HeartbeatResult` shape).
- Mission fields + `mission_store.rs` upsert + completeness guard. Suppress
  `report_mission_gaps` auto-followups for loop-enabled missions.

### Phase 5 — Intake + observability
`create_mission` chat tool gains `definition_of_done`, `max_iterations`,
`budget_usd` (presence of DoD enables the loop + provisions the Drive beat).
New `OperatorFeedKind::MissionIterationAdvanced` (feed-only); terminal
`MissionCompleted` / `MissionUserInputNeeded` are pushed. DailyDigest mentions
running loops; sveltfront timeline rendering.

## Safety rails (throughout)
Executor stays globally single-flight; `requires_human_approval` veto untouched
(replanner keeps the planner's `requires_approval` rule for outward work);
Drive beats require `heartbeat_mode == Execute` **and** `AutomationMode::AutoRun`;
iteration/budget caps hard-stop to `UserInputNeeded`; credit-exhausted
workspaces short-circuit to canned, which also pauses the loop.

## Critical files
- `crates/gateway/src/task_executor.rs`
- `crates/gateway/src/heartbeat_dispatcher.rs`
- `crates/gateway/src/mission_goal_loop.rs`
- `crates/gateway/src/agent_review.rs`
- `crates/gateway/src/mission_planner.rs`
