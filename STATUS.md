# Status

> The single honest record of where Consistent CEO actually is. Updated
> whenever `scripts/repo-health.sh` is run against a meaningful change, and
> always before cutting a release. If this file and the code disagree, the
> code is right and this file is a bug.
>
> **Last verified:** 2026-09-13 · `feat/architect-phase-2` @ `8bc455ae` + Phase 1 · verified
> locally (GitHub Actions is still refusing to start jobs — see below). This
> branch is 13 commits ahead of `main` and has not been merged or pushed.

---

## In one paragraph

A real engine, not yet a product. The world model — entities, missions,
tasks, approvals, an encrypted vault, git-diff evidence packs — is built,
persisted and differentiated. ~100k lines of Rust across 13 crates that
type-check clean with zero warnings. The thing the docs
lead with — evidence-backed execution, where every claim resolves to a cite —
now exists as behaviour rather than as a placeholder, but **only in shadow
mode**: the Architect resolves the answer the gateway already produced and
writes the verdict to `audit_trail`. Nothing a user sees has changed yet.
Two of the five stages (Director, Generate) remain deliberately unbuilt.

The number that matters next does not exist yet: **what fraction of real
chat turns would have branched to ASK.** Shadow mode was written to produce
it. Until it has run over real traffic, whether this differentiator is real
is still an open question rather than an answered one.

## Verified build state

All three CI jobs went green on `main` at `7c324525` on 9 Sep — the first
green `main` since July. Runs after that are **blocked on GitHub billing**,
not on the code: jobs fail before any step with *"the job was not started
because recent account payments have failed or your spending limit needs to
be increased"*. Until that is cleared, the gates below are the local ones —
which the toolchain pin makes equivalent rather than approximate.

| Gate | Result | Where |
|---|---|---|
| `cargo check --workspace --all-targets` | pass, 0 warnings | local (1.97) |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass | local (1.97, = CI's pin) |
| `cargo test --workspace` | 1,618 pass, 0 fail, 10 ignored | local (1.97) |
| `sveltfront && npm run build` | pass, 8.5s | local |
| `scripts/repo-health.sh` | no placeholder modules; 11 loops supervised, 0 with a discarded handle; no task writes by index, no direct status assignments; 1 pre-existing breach (red CI on `main`, a billing block) | local |

Both cargo gates now carry `--all-targets`, which they did not before — CI ran
the narrower form, so two test-only defects had accumulated unseen.

**The toolchain is pinned.** `rust-toolchain.toml` names `1.97` and CI's
`toolchain:` keys name the same, so the compiler that judges a change
is the same one everywhere. Before this, CI resolved `stable` (1.98) while
this machine ran Homebrew `rustc 1.97`: two lints added in 1.98 fired only
in CI, so clippy was green locally and red on every push, and `main` stayed
red from July to 9 September without anyone seeing a failure. Both were
fixed blind, one CI round-trip each — `chunks_exact_to_as_chunks` in
`crates/audio`, `result_large_err` in `crates/a2f`.

`repo-health.sh` now compares the local `rustc` against the pin and
breaches on a mismatch. Bumping is deliberate: raise the version in both
places, then run `--full` and fix what the newer clippy finds.

Residual gap: this machine's rustc comes from Homebrew, not rustup, so it
ignores `rust-toolchain.toml`. Parity holds today because the versions
agree; a Homebrew upgrade would break it, and the health check is what
catches that.

The root manifest defect is fixed: `[workspace.dev-dependencies]` is not a
table cargo recognises, so its four entries were silently ignored and every
invocation opened with `unused manifest key`. Nothing depended on it. The
table is gone and the crates that need those dev-deps now reference
`[workspace.dependencies]`, so `cargo check --workspace --all-targets` is
warning-free.

`cargo clippy --workspace --all-targets` now passes too, which it did not
before — CI only ever ran the form without `--all-targets`, so two test-only
defects (`items_after_test_module` in `voice.rs`, five useless conversions in
the relay tests) had accumulated unseen.

**Release is cut from a tag, not from `main`.** The release job used to run
on every push to `main`, and it mirrors CLI tarballs to the bucket
`install.sh` downloads from — so any merge could change what a stranger
installs. It is now gated on `refs/tags/v*`, matching how
`release-desktop.yml` has always worked. In practice it uploads nothing
either way: `MIRROR_ACCESS_KEY_ID` and `MIRROR_SECRET_ACCESS_KEY` are not
set on the repo, so the upload step self-skips.

## Windows

Branch `feat/windows-desktop` (on top of `feat/architect-phase-2`). The Windows
installer has **never been built**: the release matrix gained a Windows entry in
July, no tagged run has happened since, and Actions is blocked on billing. The
route that needs no CI is `desktop/scripts/build-windows.ps1` on a Windows
machine (installs the build tools with winget, builds the NSIS installer).

- **Runtime blockers fixed**, all behind `crates/common/src/platform.rs`: home
  directory (`%USERPROFILE%`, so `~/.consistent-world` no longer stays a literal
  `~`), CLI lookup with `%PATHEXT%` (`claude.exe`, `codex.cmd`), prompts to the
  Claude/Codex CLIs on stdin instead of argv (Windows command-line limits),
  PowerShell for the shell tool and verification commands, `\\?\` stripped
  from spawn working directories, process-tree kill on cancel, and
  `CREATE_NO_WINDOW` on every child process (the sidecar has no console, so
  each git/PowerShell/CLI call would otherwise flash a console window).
- **Feature gaps closed**: wake assertion (`SetThreadExecutionState`), dictation
  typing (`SendInput`), PTY shell choice, Tailscale and Git discovery, Windows
  shell risk patterns.
- **Proven**: the whole workspace, tests included, type-checks for
  `x86_64-pc-windows-msvc` with zero warnings, and so does the Tauri shell
  (cross-checked from macOS with `cargo-xwin`); macOS clippy and tests pass; the
  stdin path was verified against the real Claude CLI; the PowerShell wrapper
  (encoding, quoting, exit codes) was run against real PowerShell 7.6.
- **Not proven**: Windows clippy, any test *run* on Windows, PowerShell 5.1,
  the installer, and a first launch. The smoke test in `docs/windows-desktop.md`
  covers the last three; CI's on-demand `windows` job covers the rest once
  Actions runs.
- **Signing** is wired (Azure Trusted Signing, on when its secrets exist) but no
  Azure account exists yet.

**CI cost.** Each push used ~250 runner-minutes (three jobs rebuilding the
workspace from nothing), which is how the private repo's allowance ran out on
9 Sep. Every Rust job now uses `Swatinem/rust-cache`, and the Windows job runs
only on `workflow_dispatch` and release tags.

## Nobody can install this

Re-verified 2026-09-12. `https://www.consistent.ceo/install.sh` (the
README's one-liner) returns **404**; `dl.consistent.ceo` (the host
`install.sh` pulls from) is **NXDOMAIN**; CI has never uploaded because the
mirror credentials do not exist.

**This section used to name the wrong codebase, and so did the roadmap's fix
list.** The live site is not `sveltfront/` — it is a Next.js app from a
separate repository (`github.com/Al-Baqshi/Consistent`, checked out here at
`Consistent/`, gitignored, deployed on Vercel). Confirmed from the response
headers (`x-nextjs-prerender`) and from that checkout. Two consequences:

- `sveltfront/` is the **app UI embedded in the gateway binary**, not the
  marketing site. Its `/download` page and `src/lib/download.ts` are not what
  a visitor to consistent.ceo sees, so the honest `null` fallback documented
  here — good engineering, and still true of that file — was never load-bearing
  for the public CTA.
- the roadmap's instruction to "set `VITE_DOWNLOAD_BASE` at site build time"
  cannot work. The live site reads `NEXT_PUBLIC_DOWNLOAD_BASE`
  (`Consistent/lib/downloads.ts`).

`install.sh` does exist at `sveltfront/static/install.sh`, which is why it is
served by the gateway — but the marketing site's `public/` has no copy, which
is the actual reason the README's one-liner 404s.

This is the single most important gap in this document. Everything else
here describes software that no one outside this machine can obtain.
See P0.5 in the roadmap.

## What is built and works end to end

- **Entity / mission / task model** — persisted, canonical, with a
  deterministic definition-of-done that can actually terminate a mission.
- **Auth & identity** — password, passkeys, device-approval, API keys,
  sessions, recovery. 22 routes. The most complete subsystem here.
- **Credential vault** — encrypted, versioned, OS keychain, per-entity
  scoping, TOTP. 28 routes.
- **Policy & approval gate** — capability merge across global and entity
  layers; allow / require-approval / deny; an explicit human approval
  outranks the entity's automation default.
- **Evidence packs** (`crates/gateway/src/evidence.rs`) — git-diff proof
  that an unattended run changed something. Honest about its limits: a
  non-git directory can prove presence of change but never absence, so
  those packs never trigger a downgrade. **This is the best idea in the
  codebase.**
- **LLM routing** — six providers including the Claude CLI and Codex CLI
  subscription lanes plus Ollama; three override layers, tier fallback,
  cost debit. Never requires an API key.
- **Terminal client** (`crates/gateway/src/cli_client.rs`) — `chat`
  (interactive, one-shot, or piped stdin; streams NDJSON), `status`, `tasks`,
  `entities`, `approvals`, `approve`, `reject`. Thin HTTP calls to the running
  gateway with the machine token, never a second gateway. Verified against a
  throwaway gateway (list commands, entity lookup, errors, not-running
  message); streamed *tokens* from a live model were not exercised because
  that sandbox had no AI connected. No tests of its own yet.
- **Heartbeats** — per-entity pulse, director run with lessons recall,
  cost debit, canned fallback when the model lane is down.
- **Desktop / mobile / remote** — Tauri sidecar with signing and
  notarization, embedded PTY terminal, 19 React Native screens, QR
  pairing, Tailscale or self-hosted relay. The app starts at login into the
  tray and closing the window no longer stops the gateway (2026-10-11).
- **Pairing by presence** (2026-10-11, `pairing.rs`) — a 90 s single-use QR
  code, an Ed25519 key made on the phone, six matching digits approved on
  the computer, and key sign-in from anywhere afterwards. Revoking the device
  ends its sessions. Proven end to end through the real binary, the CLI
  (`consistent-world pair`) and the phone's own crypto module; the phone
  screen itself has only been type-checked, not run on a device, and needs a
  dev-client rebuild for `expo-crypto`.
- **One AI, chosen once, used everywhere** (2026-10-14, `ai_brain.rs`,
  `control_api/ai_api.rs`, `AiPanel.svelte`) — Settings → AI offers three
  ways: subscription (`claude`/`codex` CLI, including the copy an editor
  extension bundles, which `PATH` never had), local Ollama (chat models
  only; embedding models no longer count), or an API key (tested with one
  real turn before it is saved). The choice is persisted to
  `~/.consistent-world/ai.json` and enforced in `capture::build_llm_client`,
  so heartbeats, tasks, missions, entity analysis and agentic sessions all
  run on it, model included; with nothing chosen, a dead lane falls through
  to whatever this machine can run. Folder import no longer requires the
  `claude` CLI and falls back to a direct read when the agentic run fails.
  Also fixed: model listing built `/v1/v1/models` (every API-key save
  failed) and sent Anthropic a bearer token. Verified live: the bundled CLI
  was detected and connected with a real reply. The old `/api/llm/*`
  provider/model endpoints remain but the settings page no longer uses them.
- **AI page, second pass** (2026-10-17, `AiPanel.svelte`, `settings/+page.svelte`)
  — one screen instead of three tabs: every option with a status chip and one
  next-step button; account, phone and working style follow, and status,
  coding agents, the vault and the terminal sit under a collapsed Advanced.
  Fixed: API-key connects always sent a model, so the "find one this key can
  call" fallback never ran (now defaults to Automatic); "Pick automatically"
  swallowed its own error; the 20 s poll reset a model being chosen; a custom
  Ollama address was honoured by detection but not by the connect test or by
  runtime calls (`ai_brain::ollama_base`); a download in progress was lost
  when the page was closed. Driven end to end in a browser against a gateway
  with a stand-in Ollama (download with progress, resume after reload,
  auto-connect, persistence, pick automatically, rejected key); phone width
  and light theme checked. Not exercised this round: a real Claude or API-key
  success (the sandbox had no login; the failure messages were). The shipped
  desktop app was the Oct 13 build and predates the panel entirely, so it
  needs a rebuild to show any of this.
- **Entities, connections and the shell, third pass** (2026-10-17) — import
  and the entity setup now keep **local folder, GitHub repo and website** as
  separate fields (`EntitySetup.repo_url` / `website_url`; the import form
  prefills them and the analysis may fill them). GitHub connects with no
  environment variable: the signed-in `gh` CLI or a pasted token, verified
  against GitHub before anything is kept in the keychain
  (`POST /api/entities/{id}/connections/github/token`); the browser device flow
  still needs `CONSISTENT_WORLD_GITHUB_CLIENT_ID` and is now a side option. The
  Strategy tab is a one-question-at-a-time interview that saves each answer
  (rule-based, not yet AI follow-ups); Records, Schedule and Permissions
  explain themselves, and the Schedule cron example was wrong (`0 9 * * * *` is
  "9 past every hour") and now has local-time presets. "Write a skill" can draft
  from a plain sentence (`POST /api/skills/draft`). The Phone panel can turn on
  Tailscale remote access (`POST /api/remote/setup`). The dashboard served
  page routes as `application/octet-stream`, so a reload or deep link downloaded
  a file; fixed. Desktop: the page is a remote origin to Tauri and had no
  permissions, hence "not allowed by ACL" in the Terminal; added a scoped
  capability (`capabilities/gateway-pages.json`). New full-bleed light app icon.
  Browser control is planned, not built: `docs/plans/browser-control.md`.
  Verified: 723 gateway tests, clippy, 21 browser checks on a clean entity,
  the installed signed app (deep links, new screens, `gh` login verifies).
  Not exercised: a real GitHub connect (would store a token), the Tailscale
  setup call (would change your Tailscale), the Terminal fix and the Dock icon
  on screen.
- **The Consistent browser** (2026-10-17, `browser_manager.rs`, `tools/src/chrome.rs`,
  `docs/plans/browser-control.md` M1) — a visible Chrome window on a profile of its own that a
  person signs in to and browser tasks then drive, per-entity site lists, and the safety work
  that sharing a signed-in browser required (attaching closes only its own tab, `evaluate` off
  when attached, popups no longer escape the policy). A browser task on an entity with no sites
  now blocks instead of running, which is stricter than before. Verified with a scripted run
  against real Chrome and the real MCP server (sign in, close, reopen, still signed in;
  refusals; the person's tab untouched), 9 real-Chrome tests, 732 gateway tests and the
  executor gate. Not exercised: a visible window through the shipped app, a real site login,
  a real `claude` run. M2 (watch, pause, take over) is not started.
- **Pairing on Wi-Fi with nothing installed** (2026-10-14, `wifi_access.rs`)
  — making a pairing code with no relay or Tailscale Serve opens a second,
  guarded listener on the computer's Wi-Fi address (main listener stays on
  loopback) and remembers the choice. Wi-Fi requests are tagged and may not
  use the machine token or sign up; while it is on, the gateway stops
  counting as "self-hosted" for the admin rule and the limiters throttle
  Wi-Fi traffic. Tailscale/relay are now an optional "away from home" extra.
  Tested through the router (guard, pairing over Wi-Fi, off switch); the
  real socket bind and the macOS firewall prompt are unverified on a device.
- **Local semantic memory** (2026-10-11, `embedding_setup.rs`) — Ollama's
  `nomic-embed-text`, pulled on first start, no key. Verified live: a fresh
  install enabled recall from local Ollama alone.
- **Tenant isolation** (2026-10) — instance-wide settings (providers, keys,
  routing, directors, cloud config, backups) need the instance admin;
  everything a user owns answers 404 to anyone else; `api_key_env` is
  allowlisted; the static token is compared in constant time; uploads cannot
  escape their folder. Store deletes are explicit and tombstoned, so a sync
  can no longer delete rows it failed to read.
- **Skills** (2026-10, `skills.rs`, `docs/SKILLS.md`) — Claude-format
  SKILL.md playbooks at built-in, workspace and entity scope; chat loads them
  on demand and beats get the index. Nine built-ins. `delete_entity_skills`
  has no caller yet, so deleting an entity leaves its skills folder.
- **In-app account deletion** (2026-10) — `DELETE /api/auth/account`, password
  re-checked, vault removed first; the machine owner's account is refused.
- **Mobile, App Review pass** (2026-10) — sample data only in demo mode and
  labelled, AI consent before the first message, privacy link, push that
  survives a cold start, brand icons. Not yet submitted.
- **Nothing depends on the author** (2026-10-11) — billing is opt-in
  (`CONSISTENT_WORLD_BILLING_ENFORCE=1`), trial credits only seed where it is
  on, the Railway config is gone, downloads point at GitHub Releases, and an
  integration test boots the real binary offline with nothing installed.
- **Entity knowledge index** — an import's source material is kept as
  `entity_docs` and injected into every chat turn and heartbeat.

## What is stubbed, missing, or dead

| Thing | State | Detail |
|---|---|---|
| Architect Forward + Verify | **built, shadow only** | Deterministic, model-free, ~600 lines with 41 tests. Verify resolves the answer the gateway already produced, so a shadow run cannot spend a token — the crate has no LLM dependency. |
| Architect Director + Generate | **deliberately not built** | The placeholder files are deleted, not left shipping. `StepName` still has both, and the pipeline records them `Skipped` with a reason — shadow mode needs neither, because the gateway already routes and drafts. Decision written down in ROADMAP P2.3. |
| Claim-to-cite verification | **built, shadow only** | `EvidenceState` now exists (`crates/architect/src/cite.rs`), using VISION's four names plus `NotApplicable`. A claim's load-bearing tokens — figures, quoted strings, proper nouns — must resolve to a snapshot row or a tool result this turn, else the turn is `Ask`. Not on the wire and not in the UI yet. `gateway::verification` is still the separate, unrelated keyword gate on *task output*. |
| Evidence chips in chat | **shipped** | `Observed` when a tool grounded the turn, `Inferred` when it did not, rendered in the transcript. Provenance only — the deeper claim→cite resolution stays in shadow. |
| Activation metric | **readable** | `time_to_first_completed_mission_seconds` was always computed but no client read it. Now exposed, and both DoD-met paths emit a once-ever `mission_first_completed` feed event. |
| Revenue surface | **not built, deliberately off** | Credit ledger and 402 middleware are real but now opt-in only; a self-hosted gateway never meters its owner. Stripe appears only in test fixtures. Payments are out of scope for the open-source release (`docs/plans/open-source-local-first.md`). |
| Desktop auto-update | **not built** | Needs a Tauri updater signing key held as a CI secret; until then users download new versions from Releases. |
| Container image | **written, never built here** | Single container with the embedded database and a self-generated token (`CONSISTENT_WORLD_PROVISION_TOKEN=1`); `ci.yml` publishes it to GHCR on tags. The startup path was simulated with the real binary; no Docker was available to build the image itself. |
| Inspect memory + lessons | **real** | Entity-scoped memory, lessons and knowledge docs all read per entity. The premise that blocked this — "memory is agent-scoped" — was already false: `entity_memory` had namespaced it for months. |
| `control_api.rs` split | **done** | 12,449 → 3,741 lines across 23 child modules under `control_api/`. Pure moves — the test count was byte-identical before and after. What remains is the shared spine: request bodies, the create paths several domains share, route helpers, and the router. |
| Voice | **parked** | Gemini Live wired, needs `GEMINI_API_KEY`, page not in the sidebar. Deliberate. |
| Channels (Slack/Telegram/email) | **removed** | Crate deleted, `ChannelKind` is single-variant. No non-web ingress exists. |
| `/data`, `/models`, `/channels` | **deleted** | `/data` was a reachable "Coming soon" stub; `/models` was a redirect that could never run under adapter-static; `/channels` was a channel inventory for a product whose `ChannelKind` has one variant. 611 lines. |
| `/console` | **live — this row was wrong** | Reachable from two places (`dashboard/+page.svelte:142`, `agents/+page.svelte:328`) via `goto('/console?directorId=…')`, and it POSTs to a real `/api/commands`. It was listed as dead because the earlier sweep grepped for a bare `/console` and the links carry a query string. |
| `/api/compat/*` | **deleted** | Six routes, seven handlers, two helpers and six wire types — 483 lines. Verified uncalled across the gateway, the Svelte app, desktop and mobile before removal. |

## Structural risk

Seven background loops — orchestrator (15s), scheduler (60s cron),
heartbeat ticker, mission engine, task executor, reminders, accountability
(300s) — can all create, mutate or close a task, and none consults the
others. Coordination lives in prose and convention, not code. Heartbeat
dispatch already needed a distributed lease to stop duplicate runs; that
is the pattern repeating one race at a time.

An architecture review on 2026-09-13 measured this and found the codebase is
not tangled at the level the word "spaghetti" implies — median function 13
lines, 3.6% over 100, 14 `unwrap`/`expect` in production paths, clean
cross-crate layering — but that it has one structural root cause (`state.rs`
doing four jobs, producing 20 of 42 circular module pairs) and a class of
concurrency defects around task mutation. Diagnosis:
`docs/reviews/2026-09-13-architecture-review.html`. Seven-phase remediation
plan: `docs/plans/2026-09-13-hardening-plan.md`.

**Phase 1 of that plan is done** and closed two failure modes where the
gateway stopped working while still reporting healthy:

- **Eleven background loops were spawned with their `JoinHandle` discarded.**
  Tokio ends a panicked task silently, so one panic stopped that subsystem for
  the life of the process — no loop-level log, nothing user-visible, `/health`
  still 200. A dead orchestrator was indistinguishable from an empty queue.
  All eleven now run under `supervisor::spawn_supervised`, which restarts on
  panic with capped backoff, records it, and alerts. `/health` reports
  `status: "degraded"` plus per-loop restart counts. The two worst were the
  ones the review had not found: the 60s control-state persist loop and the
  credential-vault sweeper.
- **A three-way deadlock between `snapshot()`, the operator feed, and any
  entity writer.** `snapshot()` held all 28 read guards at once (temporary
  lifetime extension in a tail-position struct literal) while
  `append_operator_feed_event` held `operator_feed` for write across two awaits
  and took `entities` for read — the reverse order. Two readers cannot block
  each other, so it needed a third task queuing `entities.write()` for tokio's
  write-preference to close the cycle. All three run constantly. Both paths now
  hold one guard at a time; `ControlState` carries the lock-order rule; the
  regression test was verified to hang against the old code.

**Phase 2 is done** and closed the three task-mutation defects:

- **Write-by-stale-index in the orchestrator's pickup.** Candidate *positions*
  were captured under a read guard, the guard dropped, Surreal I/O awaited, then
  `tasks[i]` written by the saved index. Deleting an entity `retain`s the task
  list and shifts every later index, so the write hit a different task — silently
  hijacking it into an unattended run (including one a human had just cancelled),
  or panicking out of bounds. Pickup now claims by id through `claim_task`, which
  re-asserts the status is still pickable under the write guard.
- **The fatal-error path overwrote a cancel.** The `Ok` branch always kept a
  cancelled task cancelled; the `Err` branch assigned `Failed` raw. A test proves
  the old behaviour (`left: Failed, right: Cancelled`).
- **Chat reverted concurrent loop writes** via `*slot = clone`, a whole-row
  overwrite. It now mutates in place.

The setters are split by actor — `set_task_status_by_human` (a person reopening
outranks the stored state) and `set_task_status_by_loop` (refuses to overrule a
cancel) — and all 21 direct `task.status =` assignments outside `state.rs` are
converted, which also stops `status_changed_at`/`completed_at` drifting; the
consistency streak is computed from those. `repo-health.sh` §8 breaches on either
defect pattern and ratchets the 38 remaining direct `tasks.write()` sites.

Not yet addressed: entity sync deleting rows after a failed read (Phase 3), the
19 stores with no shared layer (Phase 4), the duplicate compile (Phase 5), and
`state.rs` still doing four jobs (Phase 6).

## Investment shape

Counted by route declarations: entities 34, vault 28, auth 22, missions
10, tasks 5. The scaffolding around the product is roughly five times the
surface of the thing the product is for.

## Process history worth remembering

`main` was frozen from 2026-07-20 to 2026-09-09 — 51 days — while 88
commits and ~4,900 uncommitted lines accumulated on a single branch. The
written plan (`docs/ROADMAP.md`) and the actual work diverged in late
August without anyone recording the pivot. `scripts/repo-health.sh` and
the `repo-health` skill exist so that cannot recur silently.
