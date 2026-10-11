# Roadmap — to production-ready

> Replaces the 90-day plan dated 2026-07-16, which was abandoned in late
> August without being rewritten. See **What happened to the last plan**
> at the bottom; that history is kept deliberately, because losing it is
> how the repo ended up shipping docs for a product that did not exist.
>
> Current state: [`STATUS.md`](../STATUS.md). Vision: [`VISION.md`](VISION.md).
> Architecture: [`ARCHITECTURE.md`](../ARCHITECTURE.md).
>
> **Written:** 2026-09-09 · `main` @ `7d8eac58`

## What "production-ready" means here

This is a local-first desktop app, not a hosted service, so readiness is
not uptime. It is this:

> A stranger downloads the installer, connects a model lane in one click,
> states a goal in their own words, closes the laptop, and comes back to
> finished work with proof of what changed — without hitting a dead
> surface, an unreachable page, or a claim the software cannot back up.

Every phase below is scored against that sentence. Anything that does not
move it is parked, explicitly, rather than carried as quiet debt.

---

## P0 — Truth and green

*This week. Nothing else starts until this is done.*

The repo currently makes claims it cannot support and has a red pipeline
nobody was watching. Both are cheap to fix and both undermine everything
built on top of them.

1. **Get CI green on `main`.** ✅ *done 2026-09-09.* CI had been red since
   July. The cause was not platform but toolchain: CI resolves `stable`
   via rustup (1.98) while this machine runs Homebrew rustc 1.97, so the
   `chunks_exact_to_as_chunks` lint added in 1.98 fired only in CI —
   local clippy was green the whole time. Fixed in
   `crates/audio/src/capture.rs`, and `repo-health.sh` now warns when the
   local toolchain is not rustup-managed. **Treat a red `main` as a
   stop-the-line event from here.**
2. **Stop the docs overselling.** `VISION.md` calls evidence-backed
   execution "the non-negotiable promise" and describes four evidence
   states. The code has none of them. Either soften the claim to what
   `evidence.rs` actually does, or ship P1.2 in the same week. Do not
   leave them contradicting each other.
3. **Fix the root manifest.** ✅ *done 2026-09-12.* `[workspace.dev-dependencies]`
   is not a table cargo recognises. Nothing depended on it — every crate that
   needs those dev-deps already declared its own. Table removed, those
   declarations pointed at `[workspace.dependencies]`. While there:
   `cargo clippy --workspace --all-targets` now passes, which it never had,
   because CI only ever ran the form without `--all-targets`.
4. **Delete the dead surfaces.** Partly done 2026-09-12, and **this item was
   partly wrong**. Deleted: `/data` ("Coming soon"), `/models` (a redirect that
   could never run — `+layout.ts` sets `ssr = false` and the adapter is static),
   and `/channels`. `/channels` was *not* unreachable, which is why the honest
   reason for deleting it is different: it is a channel inventory for a product
   whose `ChannelKind` has exactly one variant.

   **`/console` must NOT be deleted.** It is live and reachable from two places
   — `dashboard/+page.svelte:142` and `agents/+page.svelte:328`, both via
   `goto('/console?directorId=…')` — and it POSTs to a real `/api/commands`. It
   was listed as dead because the earlier sweep grepped for a bare `/console`
   and missed the query string.

   Also done: the six `/api/compat/*` endpoints (verified uncalled across the
   gateway, the Svelte app, desktop and mobile) and the `[channels.*]` config
   block, which parsed into nothing because no `channels` key exists anywhere in
   `config.rs`.

5. **Nobody can install this.** Verified 2026-09-09 — the whole
   acquisition path is dead end to end:

   | Step | State |
   |---|---|
   | `curl https://www.consistent.ceo/install.sh` (the README's one-liner) | **404** |
   | `dl.consistent.ceo` (the host `install.sh` downloads from) | **NXDOMAIN** |
   | Desktop installer buttons on `/download` | null — `NEXT_PUBLIC_DOWNLOAD_BASE` unset at build time |
   | CI mirror upload | never ran — `MIRROR_ACCESS_KEY_ID` / `MIRROR_SECRET_ACCESS_KEY` are not set, so the step self-skips |
   | `github.com/consistent-ceo` (linked from the site) | **404**; the real repo is private |

   **Corrected 2026-09-12 — this item named the wrong codebase.** The live site
   is *not* `sveltfront/`. It is a Next.js app from a separate repository
   (`github.com/Al-Baqshi/Consistent`, checked out here at `Consistent/`,
   gitignored, deployed on Vercel), confirmed from the response headers
   (`x-nextjs-prerender`). So:

   - the original instruction "set `VITE_DOWNLOAD_BASE` at site build time"
     **cannot work** — that variable belongs to the Vite build of the in-app UI.
     The live site reads `NEXT_PUBLIC_DOWNLOAD_BASE` (`Consistent/lib/downloads.ts`).
   - `sveltfront/src/lib/download.ts` is still deliberately honest, and that is
     still good engineering — but it is the **in-app** download page, embedded in
     the gateway binary, not the public CTA. It was never what a visitor sees.
   - `install.sh` exists at `sveltfront/static/install.sh`, so the *gateway*
     serves it. The marketing site's `public/` has no copy, which is the actual
     reason the README's one-liner 404s.

   Fix in order: **copy `install.sh` into the marketing repo's `public/`** (the
   one change that is purely in-repo and makes the README's one-liner true) →
   create the `dl.consistent.ceo` DNS record and bucket → set the two mirror
   secrets so CI can upload → set `NEXT_PUBLIC_DOWNLOAD_BASE` on the Vercel
   project → fix or remove the dead GitHub org link.

   Everything after the first step needs DNS, bucket and Vercel access, so it
   cannot be done from this repository alone. Until then the README's "grab the
   installer, open it, and you're running" is not true, and every other item in
   this roadmap is moot: there is no way for a user to reach any of it.

**Exit:** `scripts/repo-health.sh --full` returns 0. CI green on `main`.
No document describes an unbuilt feature in the present tense. A person
on a clean machine can install and launch the app from a public URL.

---

## P1 — Make the one job land

*Weeks 2–4. This is the product.*

The strongest, most defensible thing here already works: state a goal with
a definition of done, and the system works on it unattended inside a
directory jail, stops at an approval gate for anything outward-facing, and
proves with a git diff what it changed. A first-time user currently has no
way to find that.

1. **Put the one job on the first screen.** The workspace opens on chat
   among twenty routes. It should open on: state a goal → see it become a
   mission with a definition of done → watch tasks execute → see the
   evidence. The vault, canvas, terminal and authenticator move behind it.
   They are excellent scaffolding and they should not be what a user meets
   first.
2. **Make the evidence claim true, cheaply.** Do *not* build the full
   claim-to-cite resolver — that is where the last plan died. Extend the
   posture `evidence.rs` already takes into chat: label a claim
   **Observed** when a tool produced it during this turn, **Inferred**
   otherwise, and render the chip in the transcript. Small, honest, and it
   turns the headline promise from marketing into behaviour.

   *Done 2026-09-12,* and the instruction was honoured: the deterministic
   resolver that was also built lives in `crates/architect` and runs **in shadow
   mode only** — it never touches the wire, the UI, or the user's answer, and it
   cannot spend a token.

   The chip ships the cheap version asked for here: `Observed` when a tool
   produced something this turn, `Inferred` otherwise. It reuses `EvidenceState`
   from `crates/architect/src/cite.rs` rather than defining a second vocabulary,
   so that when Verify eventually reaches the wire it upgrades the label instead
   of competing with it. A failed tool call grounds nothing but does not
   downgrade the turn below `Inferred` — provenance is not entitled to make a
   claim about resolution.
3. **A first-run that cannot fail silently.** Install → connect a lane →
   first mission completes. The Connect card already runs a live
   completion rather than trusting a probe; extend that honesty to the
   whole first-run path so a user never reaches an empty workspace
   wondering whether it is working.
4. **Instrument time-to-first-completed-mission.** ✅ *done 2026-09-12.* **The
   original wording, "still unmeasured", was stale** — the counter was already
   real, write-once, wired into both DoD-met paths, persisted in the snapshot,
   and served at `GET /api/missions/metrics`. The genuine gaps were narrower and
   are now closed: the recorder returns `Some(elapsed)` only on the call that
   actually records, so both completion paths emit a `mission_first_completed`
   feed event at the activation moment; and `api.ts` now carries
   `timeToFirstCompletedMissionSeconds`, which it previously omitted, so a
   client can read the number at all.

   The new kind is **feed-only, deliberately not push-notifiable**: it fires at
   the same instant as `mission_completed`, which already pushes, and
   double-notifying the single most important moment in the product is how a
   companion gets muted.

**Exit:** someone who has never seen this app installs it and completes a
real mission without being told what to click. That is the test — run it
on an actual person, not a description of one.

---

## P2 — Structural debt that is compounding

*Weeks 5–8. Not urgent, genuinely compounding.*

1. **One arbiter for the seven loops.** Orchestrator, scheduler, heartbeat
   ticker, mission engine, task executor, reminders and accountability all
   create, mutate or close tasks with nothing deciding who wins. Heartbeat
   dispatch already needed a distributed lease to stop duplicate runs;
   that is the pattern repeating one race at a time. This was "Seam C" in
   the old plan and it is the part of the Architect with real value — it
   needs no model work at all.
2. **Split `control_api.rs`.** ✅ *done 2026-09-12.* 12,449 → 3,741 lines across
   23 child modules. Pure moves, verified by an unchanged test count.

   One deviation from the instruction, deliberately: they went into
   `control_api/` as **child** modules, not a sibling `src/api/`. A child opens
   with `use super::*` and so reaches every parent-private helper unchanged,
   which is what makes the move genuinely pure. A sibling would have forced
   every shared helper to `pub(crate)` and rewritten every call site — a
   visibility refactor wearing a pure-move costume, and exactly the "mixed with
   behaviour changes" this item warns against.

   What is left in the parent is the shared spine (request bodies, the create
   paths several domains share, route helpers, the router). Splitting that
   further would mean deciding who owns shared types, which is a design question,
   not a move.
3. **Decide the Architect's fate in writing.** ✅ *decided 2026-09-12 — this is
   the decision.* **Build Verify, keep the crate, do not build Director or
   Generate.**

   Verify is the piece with real value and it is buildable deterministically:
   it resolves the answer the gateway *already produced* against the snapshot
   Inspect just read, so a shadow run costs nothing and needs no model. Forward
   came with it because it is the same shape — pure functions over the snapshot
   — and it is what makes Verify's output legible.

   Director and Generate are **deliberately not built**. Shadow mode needs
   neither: the gateway already routes (`routing_engine`, `build_provider`) and
   already drafts. A second router consulted by nobody is exactly the ceremony
   this item warns about. They now record `Skipped` with a reason rather than
   `Unimplemented`, so the trail says "nobody is waiting on this" instead of
   "this is missing".

   Keeping `crates/architect` free of `consistent-world-llm` is what makes
   "a shadow run cannot spend a token" provable from the manifest rather than
   from an argument about control flow. Do not add that dependency.

   **What is owed next:** run shadow mode over real traffic and write down what
   fraction of turns would have branched to ASK. That number is the entire
   justification for ever putting this inline, and it does not exist yet.
4. **Give Inspect a real memory.** ✅ *done 2026-09-12.* **The premise here was
   wrong, and that is why this looked expensive.** "Memory is agent-scoped; the
   entity-scoped view needs building" — `entity_memory` had already namespaced
   an entity's notes under `entity_mem:{id}:short` / `:long`, with six
   production call sites using it. The adapters were the only missing piece, and
   they read exactly what the live chat prompt reads, with a test pinning each so
   the two cannot drift. Knowledge documents were added as a fifth slice at the
   same time — they are the most citable rows the system holds.

**Exit:** no module can mutate a task without going through one path.
`control_api.rs` under 4k lines. Zero placeholder modules in `crates/`.

---

## P3 — Reach and revenue

*Weeks 9–12. Only after P1 holds up with real users.*

1. **A way to pay.** The credit ledger and 402 middleware are built;
   Stripe appears only in test fixtures. Cost precheck before generate,
   per-mission and per-day caps, then Stripe on the top-up endpoint.
   Billing stays off for loopback binds — self-hosted is free, always.
2. **Ingress beyond the browser.** `ChannelKind` is a single-variant enum
   and the channels crate was deleted. Email or Slack as a plugin, not a
   revived crate — one channel done properly beats seven half-wired.
3. **Approval inbox on mobile with push.** Task approvals and policy-gate
   approvals in one feed. The phone is already the authenticator; this is
   the natural second job.
4. **Unpark voice** only if P1 and P2 hold. Gemini Live is wired and needs
   a key.

---

## Explicitly parked

Not "later" — parked, with a reason, so they stop appearing as debt:

- **Voice** — until the core loop is trustworthy. Wired, keyless, unlinked.
- **A2F face and screen perception** — same condition.
- **Shared team workspaces, RBAC, SSO/SCIM** — no customer needs them yet.
- **External agent bridges (MCP/ACP/A2A)** — the moat is the graph, not
  model access; revisit when someone asks.

---

## What happened to the last plan

The 2026-07-16 roadmap set Milestone 2 (days 26–60) as "Architect Becomes
the Brain": four seams that would route every turn through Inspect →
Forward → Director → Generate → Verify. None were started. The architect
crate's only commit in that window was a documentation pass.

What was actually built between 26 August and 9 September: the CEO
assistant operating loop, heartbeats that act, phone-driven Mac control,
the local Ollama muscle, the Codex subscription lane, streaming chat,
desktop signing and notarization, on-device dictation, the entity
knowledge index, and an embedded terminal.

That is a coherent and defensible pivot — from *governance first* toward
*make it actually run on the operator's own machine, on the operator's own
subscriptions*. The problem was never the pivot. The problem was that
nothing recorded it, so for six weeks the repository shipped a plan nobody
was following next to a product nobody had described.

`scripts/repo-health.sh` and the `repo-health` skill exist to make that
gap visible within a fortnight instead of six weeks. Run it before
writing any status update, and rewrite this file when the plan changes —
changing the plan is fine, leaving the old one standing is not.
