# Hardening plan — seven phases from reviewed to production

Companion to `docs/reviews/2026-09-13-architecture-review.html` (the diagnosis).
This file is the executable half: what to change, in what order, and the mechanical
gate that proves each phase is done and keeps it from regressing.

- Diagnosis (published): https://claude.ai/code/artifact/a0e4ebcb-b20f-4e5d-8ff5-9880c98f4681
- This plan (published): https://claude.ai/code/artifact/e5b100cd-d52b-4b99-af5f-2e33bdc1d158
- This plan (repo copy): `docs/plans/2026-09-13-hardening-plan.html`

**Status:** Phases 1–2 landed (`d7a2c9e3`, `ab489569`). Phases 3–7 not started.
**Verified against:** `8bc455ae` on `feat/architect-phase-2`, 2026-09-13.

---

## The goal

The product's promise is unattended execution: loops that pick up work, run a real
coding agent, and report evidence, without a human watching. That promise has four
structural requirements. None of them is fully met today:

1. **No loop can silently stop.** A dead loop looks exactly like an idle one.
2. **No loop acts on stale state.** Read, release, await, write-by-position is the
   shape of every concurrency bug in this tree.
3. **Nothing deletes data it has not read.** A failed read must never become a delete.
4. **One definition per rule.** A rule written in four places is four rules.

Phases 1–4 buy those four properties. Phases 5–7 remove the structural reasons they
were easy to violate. Ship in order; each phase is a branch off `main` that compiles,
passes `cargo test --workspace`, and is releasable on its own.

---

## Corrections to the review

The review was right about the shape of the problem and wrong in four details. Fixing
the plan's premises first:

| Review said | Actually |
|---|---|
| Reminders double-fire | **No.** `reminders.rs:266-296` scans, marks `reminder_fired`, and completes under one write guard — atomic within a process. The live defect is narrower: `metadata_fired` reads `as_bool()` (`reminders.rs:230`) while `chat_tools.rs:1404-1408` writes the string `"false"` via `task_meta_set_str`. Harmless today because a non-bool reads as not-fired, which is what a reset wants; it breaks the day anything writes `"true"`. → Phase 7. |
| Heartbeat beats double-run | **Mostly no.** `heartbeat_store.rs:263` claims a beat with a conditional `UPDATE ... WHERE lease_until IS NONE OR lease_until < $now RETURN AFTER` — genuinely atomic. The real gap is a fixed 10-minute lease that is never renewed (`heartbeat_store.rs:393-396` admits it in a comment): a dispatch that runs longer is re-fired. → Phase 7. |
| Mission engine dies permanently on its third restart | **No such counter exists** anywhere in the crate. The truth is worse: *every* loop is spawned with a bare `tokio::spawn` whose handle is dropped (`main.rs:1493` `let _mission_engine_handle`, and eight siblings). A panic in any loop kills that loop for the lifetime of the process, with no restart and nothing user-visible. → Phase 1, promoted to the highest-severity item in the plan. **Phase 1 found eleven such loops, not nine.** |
| Four disagreeing mission tallies | **One canonical helper**, `derive_mission_task_counts` at `orchestrator.rs:50`. But 18 other sites filter tasks by `mission_id` inline, so it is canonical in name only. → Phase 7. |

Everything else in the review reproduced exactly as written.

---

## Phase 1 — The process cannot silently stop ✅ done

**Theme:** two ways the gateway stops doing work while still answering `/health`.

### Verified findings

- **No loop supervision.** `main.rs:1393-1538` spawns nine loops; every handle is
  discarded. `tokio` catches the panic, the task ends, nothing logs at the loop level,
  and the operator feed stays quiet. A panicking orchestrator means unattended
  execution silently ceases — the exact failure the product exists to prevent. The
  stale-index `tasks[i]` in Phase 2 is a live way to trigger it.
- **ABBA deadlock between the two hottest paths.** `state.rs:1838 snapshot()` takes 28
  read guards as temporaries in a tail-position struct literal, so Rust's temporary
  lifetime extension keeps every one alive until the function returns — including
  `entities` (guard #2) and `operator_feed` (#16), in that order.
  `state.rs:1937 append_operator_feed_event()` takes `operator_feed.write()` into a
  named binding, holds it across a Surreal `await` and a vault-mirror `await`, and
  takes `entities.read()` in the middle — the reverse order. `tokio`'s `RwLock` is
  write-preferring, so a pending writer blocks new readers: snapshot holds `entities`
  and waits for `operator_feed`; the feed holds `operator_feed` and waits for
  `entities`. Both paths run constantly (60s persist loop; every feed row).

### The change

- New `crates/gateway/src/supervisor.rs`: `spawn_supervised(name, restart_policy, factory)`
  joins the task, distinguishes clean exit from `JoinError::is_panic()`, restarts with
  capped exponential backoff, and pushes an operator-feed row on every restart so a
  crash-looping subsystem is visible in the UI rather than inferred from silence.
  Convert all nine spawns in `main.rs`.
- Rewrite `snapshot()` to clone field-by-field with each guard dropped before the next
  is taken (`let tasks = { self.tasks.read().await.clone() };` per field), so it holds
  at most one guard at a time. This also removes the 28-guard convoy that makes the
  whole snapshot serialise against every writer.
- Scope the `operator_feed` write guard in `append_operator_feed_event` to the
  push-and-trim, and `drop(feed)` before the `feed_store` await. Read `entities` after.
- Add a `//! Lock order` block to `state.rs` naming the canonical acquisition order and
  the rule: **never hold a guard across an `await`, never hold two.**

### Exit gate — all met

- `crates/gateway/tests/loop_supervision_test.rs` (3 tests): a loop that panics twice is
  restarted twice and then runs; `/health` reports `status: "degraded"` and the per-loop
  restart count; a `ServerState` with no registry omits the field rather than faking it.
- `supervisor.rs` unit tests (6): restart-and-record, clean return is not a restart,
  aborting supervision also stops the loop, panic-payload extraction, capped backoff, and
  that the public counts carry no panic text.
- `state::tests::snapshot_and_feed_append_do_not_deadlock` — **verified to fail against the
  original code** (hangs the full 20s) and pass against the fix. See the correction below.
- `scripts/repo-health.sh` section 7: breach on any loop in `main.rs` spawned with a
  discarded handle. Currently reports `11 background loops supervised, 0 with a discarded
  handle`.

### What changed from the plan, and why

1. **The deadlock needed a third actor, and the first version of the regression test did
   not catch it.** Written as planned, the test passed against the broken code — worthless
   as a guard. The reason: `snapshot()` and `append_operator_feed_event()` both take
   `entities` for **read**, and read guards are shared, so two paths in opposite orders
   cannot block each other. The cycle closes only once a third task queues
   `entities.write()` (entity creation, an edit, the periodic sync), because tokio's
   `RwLock` is write-preferring and a queued writer blocks new readers:

   ```
   snapshot holds entities.read, waits for operator_feed
     → feed holds operator_feed.write, waits for entities.read
       → blocked behind the queued writer
         → writer waits for snapshot's entities.read
   ```

   Adding that third task to the test reproduced the hang. Severity is unchanged — all
   three run constantly — but the mechanism in the review was wrong, and the code comments
   now state it correctly.

2. **Eleven loops, not nine.** Two more were found spawned bare with their handles
   discarded, and both are worse than the nine: the 60s **control-state persist** loop
   (losing it means nothing is written to disk or Surreal again, with no symptom until a
   restart reads a stale snapshot) and the **credential-vault sweeper** (losing it leaves
   vaults unlocked in memory for the life of the process — a security regression, not a
   missed convenience).

3. **No new `OperatorFeedKind`.** The plan said the supervisor should push a feed row. It
   doesn't. A crashed subsystem is system health, not work on a timeline, and a new kind
   would also fire a phone push for an infrastructure event — the feed's pushes are
   reserved for things needing a human decision (`notify.rs::is_notifiable`). It would
   also have meant editing six-plus frontend allowlists. Instead: `tracing::error!` with
   the payload, the existing `alerts::notify_failure` webhook, and restart counts on
   `/health` (`status` flips to `degraded`, so a monitor that reads only that still
   notices). Panic text stays off the public endpoint; `SubsystemRegistry::snapshot()`
   keeps it for an authenticated reader, which Phase 4's store report will expose.

4. **`spawn_supervised` aborts its inner task when aborted.** The loop runs in an inner
   `tokio::spawn` so a panic arrives as a `JoinError` instead of unwinding the supervisor.
   Dropping a `JoinHandle` only detaches, so without an abort-on-drop guard the shutdown
   path at `main.rs` would have stopped supervision and left the loop running.

**Actual blast radius:** `main.rs`, `state.rs`, `server.rs`, new `supervisor.rs`, new
integration test, `scripts/repo-health.sh`.
**Don't** try to fix the stale index here — supervision first, so Phase 2's mistakes are
loud instead of silent.

---

## Phase 2 — One way to move a task ✅ done

**Theme:** 62 `tasks.write()` sites, 6 of them in `state.rs`. The other 56, across 25
files, each re-implement the same three steps and disagree about the rules.

### Verified findings

- **Write-by-stale-index in the orchestrator.** `orchestrator.rs:340` collects
  `(index, …)` under a read guard and drops it; `:395` awaits one Surreal recall per
  candidate entity with no lock held; `:449` takes the write guard and mutates
  `tasks[i]` by that index, with no re-check of identity or status. Between the two,
  any of 56 writers can `retain` (`entity_import_api.rs:1136` deletes an entity's tasks
  and shifts every later index) or push. Two outcomes: the wrong task is hijacked into
  an unattended agent run — including one the human just cancelled — or the index is
  out of bounds and the loop panics, which before Phase 1 killed it permanently.
  Note `:457` reads `tasks[i].id` *after* mutating, so the log names the wrong task too.
- **The cancel guard is bypassed on the failure path.** `orchestrator.rs:674-681`
  re-finds the task by id (correctly) and then assigns `t.status = TaskStatus::Failed`
  raw, ignoring `set_task_status_guarded`. A run that fails after the human cancelled
  overwrites `Cancelled` with `Failed`.
- **Chat overwrites concurrent loop writes.** `chat_tools.rs:1417-1424` re-finds by id
  and does `*slot = task.clone()` — a whole-row overwrite of a snapshot taken before an
  LLM round-trip. Any field a loop changed meanwhile is reverted.
- **21 raw `.status = TaskStatus::` assignments** outside `state.rs` across 8 files,
  against 11 call sites that use the guard.

### The change

- `ControlState::update_task<F>(&self, id: &str, f: F) -> Option<Task>` — takes the
  write guard, re-finds by id, applies `f`, returns the updated clone for persistence.
  Position-based mutation stops being expressible.
- Orchestrator pickup selects a **candidate id**, not an index, and re-asserts
  `status` is still `Backlog | Queued` inside `update_task` before claiming it. This one
  change closes the stale index, the panic, and the cancel hijack.
- Split the setter by actor rather than making one of them the default. The existing doc
  comment at `state.rs:141-153` is right that a single default cannot serve both: human
  reopen and requeue legitimately un-cancel. So:
  `set_task_status_by_loop` (today's guarded behaviour, `#[must_use]`) and
  `set_task_status_by_human` (today's unguarded primitive). Renaming both forces every
  one of the 56 sites to state which actor it is.
- `chat_tools` task update moves to field-level assignment inside `update_task`.

### Exit gate — all met

- `scripts/repo-health.sh` §8, three checks, **each verified to fire against a
  deliberate violation before being kept**: a task written by saved index; a direct
  `task.status =` assignment; and a ratchet on direct `tasks.write()` sites. Test
  fixtures are skipped (they legitimately do both) and so are comments (the checks are
  explained in prose that would otherwise match itself).
- `tests/orchestrator_failure_tests.rs` — a runner crash after a human cancel leaves the
  task `Cancelled`. **Verified to fail against the old code** with
  `left: Failed, right: Cancelled`.
- Five new `state.rs` tests: `update_task` follows the id after a `retain` shifts
  positions (and the same test shows the indexed write hitting the wrong task);
  `update_task` reports a deleted task; `claim_task` refuses a status that moved and
  succeeds on one that has not; the two setters differ only on a cancelled task.

### What changed from the plan, and why

1. **The `tasks.write()` breach became a ratchet, because a blanket ban was the wrong
   gate.** 38 production sites remain, and almost all are *already correct*: they take
   the write guard and then re-find by id, which is exactly what `update_task` does
   internally. Banning them outright would have meant 36 cosmetic conversions in the
   same commit as three real fixes. The two patterns that actually caused defects —
   writing by a saved index, and assigning `task.status` directly — are now hard
   breaches instead, and neither has any production instance left. The ratchet gives
   Phase 6 a number to drive down when the consolidation is the point.
2. **The setters split by actor, not by strictness.** The plan said make the guarded one
   the only public setter; the existing doc comment was right that this cannot work,
   because human reopen and requeue legitimately un-cancel. `set_task_status_by_human`
   and `set_task_status_by_loop` force the choice at every call site instead.
3. **Converting the direct assignments fixed a second bug nobody had named:** assigning
   `task.status` skips `status_changed_at` and `completed_at`, and the consistency
   streak and per-day counts are computed from exactly those fields.
4. **One non-Task match was left alone.** `task_executor.rs:412` assigns
   `update.status` on a `TaskRunUpdate` plan struct, not a task; the gate's pattern is
   scoped to `t.status` / `task.status` so it does not flag it.
5. **`claim_task` was added alongside `update_task`.** Selection, then I/O, then claim is
   the shape every loop needs and none of them had; leaving the status re-check to each
   caller would have rebuilt the original problem one loop at a time.

**Actual blast radius:** 16 files. `state.rs` (+318), `orchestrator.rs` (+175),
`chat_tools.rs` (+166), six `control_api/*`, `task_executor.rs`, `reminders.rs`,
`mission_engine.rs`, `run_agent.rs`, the integration test, `repo-health.sh`, CLAUDE.md.
**Don't** bundle this with the `state.rs` split — one behavioural change at a time.

---

## Phase 3 — Nothing deletes what it has not read

**Theme:** three persistence mechanisms with three boot reconciliation policies, and
the destructive one is the default for entities.

### Verified findings

- `entity_store.rs:317-355 sync_all()` upserts every in-memory entity, then
  `select("entities")` and **deletes every row whose id is absent from memory**.
- `main.rs:1020` hydration failure is `warn!`-and-continue. So: Surreal read fails at
  boot → memory holds zero (or stale) entities → the next periodic `sync_all` deletes
  the rows that failed to load. A transient read error is laundered into permanent data
  loss.
- `task_store.rs:1-19` documents the correct policy for the same problem in prose —
  "upsert-only periodic sync — no pruning, so an instance that doesn't know a task yet
  can't delete it" — and entities never got it.

### The change

- `sync_all` becomes upsert-only. Pruning moves to an explicit
  `prune_absent(&self, keep: &HashSet<_>)`, callable only from a path that can prove its
  read succeeded (entity deletion already deletes its own row; nothing else needs it).
- Boot hydration failure stops being advisory: a store that fails to hydrate is marked
  degraded, its periodic sync is disabled for the process lifetime, and the operator
  sees it. A gateway running on data it could not read must not write back.
- Write the one reconciliation policy down in `ARCHITECTURE.md`, including the honest
  5-of-28 split between row-backed collections and snapshot-only ones, so the next store
  author inherits the rule instead of a coin flip.

### Exit gate

- Test: a store whose `list_all` returns `Err` at boot performs zero deletes afterwards.
- `scripts/repo-health.sh` breach on `.delete(` inside any `fn sync_`.

**Effort:** ≈1 session. **Blast radius:** `entity_store.rs`, `main.rs`, docs.

---

## Phase 4 — One store layer

**Theme:** 19 stores, no shared base. The same three problems 13–15 times.

### Verified findings

- `record_id_key` is pasted into **13** store modules.
- **15** `init_schema()` methods, each called best-effort at boot: a failure logs and
  silently disables that store. There is no single place that reports "auth_store did
  not initialise", so a half-booted gateway looks healthy.
- No schema version row and no migration mechanism anywhere. Every field addition so far
  has been absorbed by storing rows as opaque JSON strings, which works until a shape
  has to change.

### The change

- `store_common.rs`: `record_id_key` once; a `StoreInit` result type; a `schema_version`
  row plus a `migrate(from, to)` hook that is a no-op today and exists so the first real
  migration is not also the invention of the mechanism.
- One startup store report — a single log summary and a `/api/health/stores` entry —
  listing every store as `ready | degraded(reason)`.

### Exit gate

- `record_id_key` defined exactly once (grep count == 1).
- Every `init_schema` failure appears in the startup report; test asserts a forced
  failure shows up as `degraded`.

**Effort:** ≈1–2 sessions.

---

## Phase 5 — Compile once

**Theme:** the whole crate is compiled twice, on every check, by everyone.

### Verified findings

`lib.rs` declares 95 modules. `main.rs` declares 96 and references the library crate
**zero** times (`grep -c 'consistent_world_gateway::' main.rs` → 0). The bin target
therefore recompiles all ~95 modules from scratch rather than linking the lib. The CI
check that both mod lists stay in sync exists only to protect this duplication.

### The change

- Boot logic (the ~1,530 non-`mod` lines of `main.rs`) moves into the library as
  `pub async fn run(args: Cli) -> Result<()>`. `main.rs` becomes a shim that parses args
  and calls it. Visibility gets easier, not harder — the boot code ends up *inside* the
  crate it wires.
- Delete the duplicate mod list and the CI check guarding it. Deleted, not skipped.

### Exit gate

- `cargo check --workspace --all-targets` wall-clock measured before and after, recorded
  in the commit body. Expect roughly half.
- `main.rs` under 40 lines; zero `mod` declarations in it.

**Effort:** ≈1 session, but **schedule it when no other session has `main.rs` open** —
moving 1,500 lines conflicts with everything.

---

## Phase 6 — `state.rs` stops being four files in one

**Theme:** 3,253 lines doing four jobs; the root cause of 20 of the 42 circular module
pairs.

### Verified findings

`state.rs` holds **63** public `struct`/`enum` declarations (≈1,484 lines of type
definitions), **28** `RwLock` collection fields, and **40** `ControlState` methods, plus
free helper functions. The cycles are structural, not accidental: every subsystem module
imports its *types* from `state.rs`, and `state.rs` imports their *behaviour* back.

### The change

- **Step 1 (types only, no behaviour change):** move the 63 domain types into a `domain/`
  module tree — or `crates/common` where a type is genuinely shared. Mechanical, and it
  breaks most of the 20 cycles by itself, because the dependency becomes one-way.
- **Step 2:** split the 40 `ControlState` methods by aggregate — tasks, entities,
  missions, feed — each in its own `impl` block in its own file. `update_task` from
  Phase 2 lands here naturally, and the 17 push-only sites get a real creation method,
  which retires the Phase 2 allowlist.

### Exit gate

- `state.rs` under ~800 lines.
- A script that counts circular module pairs, run before and after, recorded in the
  commit body; target is 20 pairs eliminated.
- No module imports both a type and a function from `state.rs`.

**Effort:** ≈2–3 sessions. Step 1 is safe and boring; do it in one commit and review the
diff for "only moves, no edits."

---

## Phase 7 — One definition per rule

**Theme:** the residue, including the three review claims corrected above.

### The change

- `derive_mission_task_counts` (`orchestrator.rs:50`) becomes the only mission tally; the
  18 inline `mission_id` filter sites call it. Move it out of `orchestrator` into the
  mission module it belongs to.
- Heartbeat lease renewal: extend the lease while a dispatch is in flight instead of
  betting the dispatch finishes inside 10 minutes (`heartbeat_store.rs:393-396`).
- Reminder `reminder_fired` becomes a bool on every path; `chat_tools.rs:1404-1408`
  stops writing it as a string.
- Decide explicitly whether two gateway instances against one SurrealDB is a supported
  topology. If yes, reminders need the lease treatment heartbeats already have. If no,
  say so in `ARCHITECTURE.md` and stop paying for half-measures.

### Exit gate

- One tally function; grep proves no inline mission-task filtering remains.
- Test: a dispatch longer than the lease window cannot be re-fired.

**Effort:** ≈1 session.

---

## How to run a phase

```bash
git switch main && git pull && git switch -c fix/phase-N-<slug>
# ... change ...
rustfmt --edition 2024 <only the files you touched>      # never `cargo fmt --all`
cargo check  --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test   --workspace
scripts/repo-health.sh --full
```

Then: update `STATUS.md` with what became real, commit as
`fix(<scope>): <lowercase imperative>` with a body explaining *why*, and merge before
starting the next phase. Add each phase's new repo-health check in the same commit as
the fix it guards — a gate landed later never runs against the code that needed it.

**Out of scope for all seven phases:** the Architect crate stays in shadow mode; no new
features; no LLM behaviour changes; no renaming of `consistent-world` identifiers.
