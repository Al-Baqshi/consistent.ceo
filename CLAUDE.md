# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Consistent CEO (brand, `consistent.ceo`) is a self-hosted, single-binary Rust agent platform: entities (businesses, projects, people) get missions, and directors/agents push tasks forward under human approval. One `consistent-world` gateway process serves the HTTP API, embeds the SvelteKit dashboard, and runs every background loop. A Tauri desktop shell wraps the gateway as a sidecar; an Expo mobile app is a remote control surface.

**Naming rule:** public/SEO copy says "Consistent CEO"; every internal identifier stays `consistent-world` (crate names, `~/.consistent-world/`, DB namespaces, bundle IDs). Do not rename identifiers.

## Read these before broad changes

- `ARCHITECTURE.md` — the technical source of truth (boot sequence, data model, request flow, API prefixes, "where to look first"). Some bits lag the code: there is no root `tests/` dir, no SQLite fallback, `control_api.rs` is now split.
- `STATUS.md` — the honest record of what is real vs. shadow-mode vs. stubbed. Update it after meaningful changes; if it disagrees with the code, the code wins.
- `docs/ROADMAP.md`, `docs/VISION.md` — current plan and product promise (evidence-backed execution).
- `docs/VAULT.md` — **mandatory before touching `crates/credentials/`** (threat model, format policy).
- `docs/integrations.md` — API keys (inbound) and webhooks (outbound); asserted by `crates/gateway/tests/connectivity_test.rs`.
- Module header comments (`//!`) are unusually thorough and are the intended entry point for each subsystem.
- `AGENTS.md` is WARP's context file and is partly stale (SQLite, `cargo fmt --all`). Leave it alone.
- `Consistent/` is the marketing site: a separate repo, gitignored, deployed on Vercel. Not part of this workspace.

## Commands

### Toolchain

Pinned to Rust **1.97** in `rust-toolchain.toml` and in every `toolchain:` key of `.github/workflows/ci.yml` (five, with the Windows job). The local `rustc` is Homebrew, so it ignores the pin; parity holds only while the versions agree (`scripts/repo-health.sh` breaches on a mismatch). To bump: raise both places, then run `scripts/repo-health.sh --full` and fix new lints in their own commit.

### Build and run the gateway

```bash
cargo build -p consistent-world-gateway --release
cargo run -p consistent-world-gateway -- start --open       # API only, no UI
# With the embedded dashboard: the SPA must exist BEFORE cargo runs
cd sveltfront && npm ci && npm run build && cd ..
cargo run -p consistent-world-gateway --features dashboard -- start --open
```

Gateway listens on `http://127.0.0.1:18800`. Config: `CONSISTENT_WORLD_CONFIG` env > `--config` > `~/.consistent-world/consistent_world.toml` (underscore; auto-created). The repo-root `consistent-world.toml` is a **sample only, loaded by nothing**; `docker/*.toml` are the container configs.

Other CLI entry points: `--onboard` (wizard), `credentials set|get|list <key>`, `remote setup|status` (Tailscale), `pair` (QR for the phone), `mcp-browser` (browser tool over MCP stdio).

### Quality gates (identical to CI)

```bash
cargo check  --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test   --workspace
scripts/repo-health.sh           # ~2s drift checks: unlanded work, doc lag, dead routes, toolchain
scripts/repo-health.sh --full    # + the three cargo/npm gates above
```

CI also fails if any `crates/gateway/src/*.rs` is not declared with `mod` in **both** `lib.rs` and `main.rs` (the gateway is a lib and a bin with duplicate module lists; an undeclared file is silently never compiled). If linking test binaries exhausts memory, set `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0` as CI does.

### Formatting

**Never run `cargo fmt --all`** — the tree is not rustfmt-clean (~70 files would churn). Format only files you touched:

```bash
rustfmt --edition 2024 crates/gateway/src/foo.rs
```

**And never hand rustfmt a file that declares modules.** rustfmt follows every `mod` declaration and reformats the whole reachable subtree, so it silently churns files you never opened. `src/main.rs` or `lib.rs` reaches all 95 gateway modules (and reorders the `mod` list that CI compares between the two); `src/control_api.rs` reaches its 18 `control_api/*_api.rs` submodules. Safe only for a leaf file — if it contains `mod `, expect the subtree. Either way, run `git status` straight after and `git checkout --` anything you did not edit.

### Tests

```bash
cargo test -p consistent-world-gateway                          # one crate
cargo test -p consistent-world-gateway some_test_name           # one test by substring
cargo test -p consistent-world-gateway --test gateway_test      # one integration file (crates/*/tests/*.rs)
cargo test -p consistent-world-tools -- --ignored                # ignored tests need real Chrome / a signed-in claude CLI
```

Unit tests live inline (`#[cfg(test)]`, heavy use of `#[tokio::test]`); integration tests in `crates/<crate>/tests/`. Surreal-backed tests use the in-process `memory` engine.

### Frontend (`sveltfront/`)

```bash
npm ci
npm run dev       # Vite on :5173, proxies /api and /health to :18800 — use this for UI iteration
npm run build     # static SPA into sveltfront/build (what build.rs embeds)
npm run check     # svelte-check
```

### Desktop (`desktop/`, Tauri 2)

```bash
npm ci
npm run build:sidecar   # builds sveltfront, then the gateway with --features dashboard, copies it to src-tauri/binaries/
npm run dev             # tauri dev
npm run build           # tauri build (beforeBuildCommand also builds the dictation + whisper sidecars)
npm run build:signed    # signed + notarized macOS build; see docs/macos-signing.md
```

### Mobile (`mobile/`, Expo 57 / RN 0.86)

Needs the custom dev client (Skia); Expo Go will not work. Metro runs on **8088** (8081 is taken on this machine).

```bash
npm install
npx expo run:ios                              # first time: build + install the dev client
npx expo start --dev-client --port 8088
npm run typecheck
```

### Other

```bash
docker compose up --build                                   # SurrealDB v2.3.10 + gateway; Ollama on the host
cargo run --release -p consistent-world-relay               # self-hosted rendezvous relay (crates/relay/README.md)
cargo build -p consistent-world-plugin-hello-world --target wasm32-wasip1 --release   # WASM plugin
bash scripts/test-consistent-world-capture.sh               # offline test of the shell capture bridge
ollama create consistent-beat -f local-models/consistent-beat.Modelfile   # branded local models
```

## Architecture

### Workspace shape and dependency direction

```
common ─► llm, memory, tools(+plugins), credentials ─► agents ─► gateway (the binary)
architect ─► gateway   (no LLM dependency by design; shadow mode)
relay      ─► gateway   (also its own binary; gateway links relay_client)
audio, a2f, screen      (voice / face / screen-perception layer; parked)
plugins/hello-world     (example WASM plugin, wasm32-wasip1)
```

Only `crates/gateway` knows about entities, missions, directors, policy and approvals. `agents`/`tools` are entity-agnostic; the gateway injects entity scope at call time (below).

### One process, one database

`crates/gateway/src/main.rs` boots everything linearly: config → SurrealDB (`startup::create_memory_and_schedule`) → per-store `init_schema()` (each best-effort: failure logs and disables that store) → `ControlState` → runtime agents → background loops → axum. **SurrealDB is the sole backend**; if it cannot connect, startup fails. Default is embedded `surrealkv://~/.consistent-world/db`; `memory` for tests; `ws://` for a server.

State has two homes that must stay in sync:
- `ControlState` (`state.rs`) — in-memory entities/missions/tasks/directors/approvals, snapshotted to `control_state.json` every 60s and to a Surreal snapshot.
- `*_store.rs` modules — each owns a Surreal table (`entity_store`, `mission_store`, `task_store`, `heartbeat_store`, `director_lessons_store`, `feed_store`, `auth_store`, `entity_docs_store`, …).

### Background loops (all spawned in `main.rs`)

| Loop | Cadence | Module |
|---|---|---|
| Orchestrator (director task pickup) | 15s | `orchestrator.rs` |
| Scheduler (cron tasks) | 60s | `scheduler.rs` |
| Heartbeat ticker → dispatcher | 30s | `heartbeat_store.rs`, `heartbeat_dispatcher.rs` |
| Mission engine | 60s | `mission_engine.rs` |
| Task executor | 60s | `task_executor.rs` |
| Reminders | 20s | `reminders.rs` |
| Accountability | 300s | `accountability.rs` |
| Power (caffeinate while work is in flight) | 60s | `power.rs` |
| Relay client (only if `CONSISTENT_WORLD_RELAY_*` set) | — | `relay_client.rs` |

None of these coordinate through code; several can create, mutate, or close a task. Heartbeat dispatch already needed a lease to stop duplicate runs. When adding a loop-side task mutation, check the others (see the "four loop-side closers" fix in git history).

### A chat turn

`POST /api/capture/chat/stream` (NDJSON: `meta`, `token`…, `done`; blocking `/api/capture/chat` is the fallback) → `capture_chat_pipeline.rs` → `chat_agent.rs` → `prompt_assembly::assemble_system_prompt` (pure; order: SOUL → agent operating contract → director block + recalled lessons → entity canonical → mission narrative + pinned notes) → LLM → `chat_tools.rs` tool loop → tail writes (session, vault mirror, entity memory, smart routing). Entity knowledge docs (`entity_docs`) ride every chat turn and heartbeat.

`GatewayToolPolicyProvider` in `main.rs` merges an entity-scoped `_policy` (roots, capabilities) into `filesystem`/`shell` tool arguments, which is how one agent safely serves many entities.

### LLM lanes and routing

Providers in `crates/llm`: OpenAI, Anthropic, Google, Ollama, plus two **keyless subscription lanes** that shell out to a signed-in CLI: `claude_code.rs` (`claude`) and `codex.rs` (`codex`). The product never requires an API key; use the Claude subscription via the CLI rather than proposing `ANTHROPIC_API_KEY`.

Three override layers resolve `(provider, model)`, most specific wins: `[routing.task_kinds.<kind>]` → `[routing.model_tiers.<tier>]` → `[llm.models.<handle>]` → `[llm.providers.<name>]`. Entry points are `config::resolve_llm*`. A model-alias hop beats `[llm] provider`, so check aliases before blaming the provider. Providers registered from the settings UI still need a config entry or resolution silently falls back to Ollama. Three lanes by cost: Brain (hosted), Muscle (local Ollama, heartbeats/fallbacks), Reflex (canned). Heartbeats fall back to a canned summary when the model lane fails.

The Claude CLI adapter is **text completion only** (`--max-turns 1`, no tools). Agentic, long-running work goes through `agent_sessions.rs` (`claude -p --output-format stream-json`, persisted `session_id` for `--resume`, cancellable, resumable after restart). Both `mission_engine` and `task_executor` use it, and only one executor run happens at a time globally.

### Unattended execution posture

Code/file work runs unprompted inside a **directory jail** (`resolve_entity_agent_run_cwd`; an entity with no resolvable root is marked and never retried; `entity_workspace.rs` asks once if the gateway invented the directory). Outward-facing work carries `requires_human_approval` and goes through the approvals queue. `evidence.rs` builds git-diff evidence packs: a "done" claim with zero diff is downgraded to `NeedsReview` (non-git dirs are informational only). Missions never self-complete: the agent's `MISSION_STATUS:` trailer → reviewer → `UserInputNeeded`. Definition-of-done loops cap at 5 iterations (25 max) plus an optional spend cap.

### Architect (`crates/architect`)

Inspect → Forward → Director → Generate → Verify. Inspect/Forward/Verify are built and run in **shadow mode** (resolve the answer the gateway already produced, write a verdict to `audit_trail`; nothing user-visible changes). Director and Generate are deliberately unbuilt and record `Skipped`. The crate has no LLM dependency so a shadow run cannot spend tokens. `gateway/src/architect_bindings.rs` is the only place gateway stores meet architect traits. `gateway::verification` is a separate, unrelated keyword gate on task output.

### HTTP API and auth

`server.rs` builds the axum router and merges `control_api.rs` (the spine: shared request bodies, `control_routes()`, helpers) with domain handlers in `control_api/<domain>_api.rs`, plus `auth_api`, `vault_api`, `billing`, `voice`, `webhooks`, `dashboard`. To add a route: handler in the matching `control_api/*_api.rs`, register it in `control_routes()`.

Bearer auth (`require_bearer_token`): no token configured → anonymous/default workspace; token equals `gateway_auth_token` → single-tenant; scoped `<secret>.<workspace_id>.<principal_id>` → multi-tenant `AuthContext`. The token comes from keyring/env or the auto-provisioned `gateway_auth_token` file next to the config, **never from TOML**. Auth subsystem: password, passkeys (WebAuthn), device approval from the phone, API keys, sessions, recovery. `/health`, voice, and the SPA are public. `/api/inference` is credit-gated (`billing.rs`).

### Wire-format gotchas

- Most gateway response types are `#[serde(rename_all = "camelCase")]`; `common::types::Session` and friends are **snake_case**. Check the struct.
- Operator-feed kinds cross the wire as snake_case. Desktop and mobile keep their own allowlists: `sveltfront/src/lib/components/chat/ChatMessageList.svelte`, `mobile/src/components/MessageBubble.tsx`, `mobile/src/api/types.ts`/`endpoints.ts`, and `notify.rs::is_notifiable` for push. A new kind means updating all of them.

### Frontend, desktop, mobile

- `sveltfront/`: SvelteKit 2 + Svelte 5 runes, `adapter-static` with `index.html` fallback, `ssr = false`. `crates/gateway/build.rs` copies `sveltfront/build` into the gitignored `crates/gateway/dashboard_build/` for `include_dir!`; without a prior `npm run build` the binary serves a "Dashboard not embedded" stub. `src/lib/api.ts` is the client (token from `sessionStorage`).
- `desktop/`: Tauri 2, identifier `world.consistent.desktop`, config in the app-data dir. The embedded terminal PTY lives in Tauri (`src-tauri/src/pty.rs`), never in the gateway. Stale-UI symptoms are usually an install gap: compare `nodes/0.*.js` hashes across build/sidecar/bundle.
- `mobile/`: real-gateway-first API layer with mock fallback (`src/api/`); `ios/`/`android/` are regenerated, edit `app.json` only.

## Conventions

- Commits: `type(scope): lowercase imperative subject` (`feat(missions): …`, `fix(repo-health): …`, `docs: …`). Bodies explain *why*; the repo's comments and commit history are deliberately narrative.
- `crates/gateway` has `#![allow(dead_code)]` at the crate root, so unreachable code does not warn. Verify callers before adding, and delete deliberately (`repo-health.sh` checks dead routes and placeholder modules).
- Release is cut from a `v*` tag, never from a push to `main` (`ci.yml` release job, `release-desktop.yml`).
- Other sessions may edit this tree concurrently; blame moving-target compile errors on in-flight work before hunting phantom bugs.
