# Consistent CEO — Architecture

> **Single source of truth** for how Consistent CEO is built. Onboarding doc for new engineers.
> Older planning, marketing, and whitepaper docs live in `docs/archive/` and may use legacy names (`MyIPSpace`, `MissTati`, `Consistent World`); they are no longer authoritative.

---

## 1. What Consistent is

Consistent is a **self-hosted, single-binary AI agent platform** for managing *entities* (people, businesses, projects, properties — anything you want an AI to keep working on) through a chat-driven workspace.

A user creates an entity, gives it missions, and the gateway's directors and agents pick up tasks, call LLMs, run tools, persist memory, and surface results. Everything runs locally by default — no third-party orchestration plane.

The product surface is:

- A **web dashboard** (SvelteKit) served by the gateway, used for entity management, mission tracking, agent chat, scheduled tasks, and operator oversight.
- A **Tauri desktop shell** that bundles the gateway as a sidecar so the whole product ships as one app.
- A **mobile app** (`mobile/`, Expo / React Native) for capture-on-the-go, approving sign-ins, and the operator feed.

---

## 2. Naming and core terminology

| Term | Meaning |
|---|---|
| **Consistent CEO** | Product/brand name, at `consistent.ceo`. Short form "Consistent" is fine in prose. Binary, config namespace, crate names, DB namespaces, and app bundle IDs remain `consistent-world` internally — deliberately, since renaming bundle identifiers would break update continuity and signing. |
| **Workspace** | A tenant boundary. Bearer tokens are scoped `<secret>.<workspace_id>.<principal_id>`. |
| **Entity** | A unit of long-running work (a person, business, project, property). Has a profile, resources, execution policy, optional connections. |
| **Mission** | A goal attached to an entity (status: planned/active/blocked/user-input-needed/completed). |
| **Task** | A unit of work under a mission (status: backlog/queued/in-progress/failed/completed; `task_kind` like `prompt-interpretation`, `entity-enrichment`, `mission-next-step`). |
| **Director** | A specialist role (operations, strategy-partner, full-stack-development, ...). Owns task kinds and has a heartbeat switch that controls whether its tasks are auto-picked. |
| **Agent** | A configured LLM runtime: id, model, persona, workspace path, tool registry. Each agent belongs to a director. |
| **Session** | A persistent conversation (`session_id`, `agent_id`, channel=`web`, `peer_id`, `scope_type`/`scope_id`). |
| **Capability** | Fine-grained permission (`FsRead`, `FsWrite`, `ShellExecSafe`, `ShellExecPrivileged`, ...) granted to entities and enforced by tool policy. |
| **Channel** | Inbound surface. Currently only `web` (the channels crate for Telegram/Slack/Discord/WhatsApp/Signal/iMessage was removed; see Section 16). |

---

## 3. Repository layout

```
.
├── Cargo.toml                   # Workspace root (10 crates + 1 plugin)
├── consistent_world.toml                # Default config (gateway, llm, routing, memory, plugins)
├── consistent_world.local.toml          # Local override (gitignored intent)
├── crates/
│   ├── gateway/                 # The binary — HTTP/WS server, orchestrator, scheduler, web UI host
│   ├── agents/                  # Agent loop, routing, sub-agents, persona
│   ├── llm/                     # Provider abstraction (openai, anthropic, google, ollama) + cost
│   ├── memory/                  # SurrealDB store + embeddings + vector search + compaction
│   ├── tools/                   # Built-in tools (filesystem, shell, http, browser, system, web_search) + wasm plugin shim
│   ├── plugins/                 # WASM plugin host (wasmtime) + registry
│   ├── credentials/             # Keyring + vault backends
│   ├── common/                  # Shared types (Message, Session, ChannelKind), capabilities, telemetry
│   ├── audio/                   # Mic capture, resampling, Gemini Live WS
│   └── a2f/                     # Audio2Face gRPC client + jitter buffer (MetaHuman pipeline)
├── plugins/hello-world/         # Example WASM plugin (compiled to wasm32-unknown-unknown)
├── sveltfront/                  # SvelteKit web UI (served by the gateway)
├── desktop/                     # Tauri shell that bundles the gateway binary as a sidecar
├── mobile/                      # Expo (React Native) mobile app
├── docker/, Dockerfile          # Container image
├── tests/                       # Cross-crate integration tests (routing, etc.)
└── docs/                        # Curated product/engineering notes; archive/ holds legacy docs
```

Workspace members (`Cargo.toml`):

```
gateway, agents, llm, memory, tools, plugins,
credentials, common, audio, a2f, plugins/hello-world
```

---

## 4. Runtime architecture (high level)

```
┌───────────────────────────────────────────────────────────────────────┐
│                              Consistent binary                              │
│                                                                       │
│   axum HTTP/WS server  ─┐                                             │
│                         │                                             │
│   web UI (SvelteKit)  ──┤   ServerState                               │
│   /api/* (~70 routes) ──┘  ┌─────────────────────────────────────┐    │
│                            │  ControlState (in-memory + persist) │    │
│                            │  • entities, missions, tasks        │    │
│                            │  • directors, agents, automation    │    │
│                            │  • routing/model tiers              │    │
│                            └──┬──────────────────────────────────┘    │
│                               │                                       │
│   AgentRouter ────────────────┼──► RuntimeAgent (per agent_id)        │
│   (peer pattern → agent_id)   │     • LLM provider                    │
│                               │     • Tool registry                   │
│   Scheduler (cron ticks)──────┤     • Persona                         │
│                               │     • Cost tracker                    │
│   Orchestrator loop ──────────┘     • Memory + embeddings             │
│   (15s tick, picks tasks                                              │
│    for directors with                                                 │
│    heartbeat enabled)                                                 │
│                                                                       │
│   ┌─ SurrealDB client (kv-mem | local server | cloud) ────────────┐   │
│   │  Tables: sessions, messages, memories (HNSW vector idx),      │   │
│   │  entities, feed, scheduled_tasks, tenants, credit_ledger      │   │
│   └────────────────────────────────────────────────────────────────┘  │
│                                                                       │
│   Voice WS  ─► Gemini Live (BidiGenerateContent)                      │
│                                                                       │
│   Tool sandboxes:                                                     │
│   • Filesystem (capability + root-allowlist)                          │
│   • Shell (capability + cwd allowlist + hard-deny patterns)           │
│   • HTTP (domain allow/deny)                                          │
│   • Browser, System, WebSearch                                        │
│   • WASM plugins (wasmtime sandbox; host_log/read_file/http_request)  │
└───────────────────────────────────────────────────────────────────────┘
```

The whole thing is **one process** that boots in `crates/gateway/src/main.rs`. There is no separate worker, no message bus, no external scheduler.

---

## 5. Boot sequence (`crates/gateway/src/main.rs`)

`main()` is linear and worth knowing top to bottom because it shows the dependency graph:

1. **Parse CLI** (`clap`) and resolve config path (`CONSISTENT_WORLD_CONFIG` env > `--config` flag > `~/.consistent-world/consistent_world.toml`).
2. **Handle bypass subcommands** — `--onboard` runs `wizard::run()`; `credentials get/set/list` shells through the keyring and exits.
3. **Load config**, override port if `--port` was passed, resolve `gateway_auth_token` from the keyring (never from TOML).
4. **Init tracing** (`consistent_world_common::telemetry::init_tracing`).
5. **Open SurrealDB** via `startup::create_memory_and_schedule()` — returns `MemoryStore`, `ScheduleStore`, db handle, and a backend label (`memory`, `ws://...`, `wss://...`).
6. **Init schemas** for `TenantStore`, `CreditLedger`, `SurrealEntityStore`, `FeedStore`. Each one's `init_schema()` is best-effort — failure logs a warning and continues with degraded state.
7. **Load `SessionManager`** from disk.
8. **Build `ControlState`** from `~/.consistent-world/control_state.json` (or fresh), attach entity/feed persisters, load seed entities/agents if empty, hydrate from SurrealDB. Spawn a 60-second persist loop.
9. **Build `ServerState`** with memory store, cost tracker, control state, tenant store, credit ledger.
10. **Wire Clerk verifier** if `[gateway.auth.clerk] issuer` is set.
11. **Build runtime agents** (`build_runtime_agents`): for each entry in `[agents.list]`, resolve provider+model → build LLM provider → build `ToolRegistry` (filesystem, http, system, shell, browser, optional web_search, optional WASM plugins) → load persona → attach to a `RuntimeAgent`.
12. **Build `AgentRouter`** from `[routing.rules]` (default agent = first in `agents_list`).
13. **Spawn the `Scheduler`** (cron tick every 60s, dispatches to `runtime.dispatch_scheduled_task`).
14. **Spawn the orchestrator loop** (`orchestrator::run_loop`, see Section 9).
15. **Bind axum** on `gateway_bind:gateway_port` (default `127.0.0.1:18800`).
16. **Optionally open browser** if `start --open`.
17. **Serve with graceful shutdown** — on Ctrl-C: abort persist/scheduler/orchestrator handles, flush control state + entities + sessions to disk/SurrealDB.

---

## 6. Crate responsibilities

### `crates/common`
Shared, dependency-free types and enums. Most important:

- `types::Message` — what a chat turn looks like (`session_id`, `channel`, `sender`, `content`, `timestamp`, `metadata: serde_json::Value`).
- `types::Session` — `id`, `agent_id`, `channel`, `peer_id`, `scope_type` (`principal` or `entity`), `scope_id`, `created_at`, `last_active`. **Serialized snake_case** — relevant when consuming from the frontend.
- `types::ChannelKind` — currently just `Web`.
- `capabilities::CapabilityId` — `FsRead`, `FsWrite`, `ShellExecSafe`, `ShellExecPrivileged`, etc.
- `telemetry` — `init_tracing`, `CostTracker`, `MetricsSnapshot`.

### `crates/llm`
Provider abstraction. One trait (`client::LlmProvider`), four implementations:

| Provider | Trigger | Notes |
|---|---|---|
| **OpenAI** | default; `OPENAI_API_KEY` | Chat Completions API |
| **Anthropic** | `ANTHROPIC_API_KEY` or `api.anthropic.com` | Messages API, streaming |
| **Google (Gemini)** | `GOOGLE_API_KEY` / `GEMINI_API_KEY` or `generativelanguage.googleapis.com` | `generateContent` + streaming + tools |
| **Ollama** | `OLLAMA_API_KEY` or `:11434` / `ollama` in URL | Local models, streaming |

Provider selection happens in `main::build_llm_provider()` based on `api_key_env` and `base_url`. `streaming::StreamEvent` is the common chunk type. `cost::CostCalculator` tracks per-model token/cost.

### `crates/agents`
The agent loop.

- `agent::RuntimeAgent` — the runtime object: id, model_id, workspace, memory, llm, tools, persona, cost tracker, embedding provider, **optional `ToolPolicyProvider`** (used by the gateway to inject entity-scoped policy into tool arguments at call time).
- `routing::AgentRouter` + `routing::RoutingRule` — peer-pattern routing (`*`/`?` glob). Used to map incoming messages to agents.
- `persona::Persona` — loaded from a file path or from the agent's workspace; fed in as the system prompt.
- `subagent` — spawning child agents for parallel work.

### `crates/memory`
SurrealDB-backed persistence + vector search.

- `store::MemoryStore` (trait) — `list_sessions`, `get_session_messages`, `append_message`, `recall`, etc.
- `store::SurrealMemoryStore` — the only implementation. Single DB for sessions, messages, and memories.
- `vector` + `embedding` — HNSW vector index on `memories.embedding`. `EmbeddingProvider` trait; `ApiEmbedding` (OpenAI-compatible) is the production impl.
- `compaction` — token-budget-aware history compaction.

`EMBEDDING_DIMENSION` is the canonical dim for the index.

### `crates/tools`
The built-in tool registry, each one a `Tool` implementation with sandboxing baked in:

| Tool | Sandboxing |
|---|---|
| `filesystem` | `FileSystemPolicy { deny_by_default, allowed_capabilities, read_roots, write_roots, list_roots, search_roots }` |
| `shell` | `ShellPolicy { deny_by_default, allowed_capabilities, approval_required_patterns, hard_deny_patterns, allowed_cwd_roots }` |
| `http` | Domain allow/deny list (localhost permitted by default) |
| `browser` | Headless browser harness |
| `system` | OS info, env (allowlist) |
| `web_search` | Provider auto-detected from env |
| `wasm_plugin` | Shim that exposes registered WASM plugins as tools |

`ToolRegistry::register_plugins_from_dir(host, "~/.consistent-world/plugins")` scans for compiled WASM modules and registers them.

### `crates/plugins`
WASM host.

- `host::WasmHost` — `wasmtime` runtime with a `SandboxConfig` (fuel limits, memory caps).
- `abi` — host-side functions exported to plugins: `host_log(level, msg)`, `host_read_file(path)`, `host_http_request(req_json)`.
- `registry` — manifest loading and plugin discovery.

`plugins/hello-world/` is the canonical example (compiles to `wasm32-unknown-unknown`).

### `crates/credentials`
Two backends behind a common interface:

- `keyring::KeyringStore` — OS keychain. Used for `gateway_auth_token`, LLM API keys, OAuth tokens.
- `vault::Vault` — the password manager and per-entity secret store. One encrypted
  file per principal at `~/.consistent-world/vaults/{principal_id}.json`.

The gateway never reads secrets from TOML — only from env vars or the credentials store.

The vault's design in one line: every way in (master password, passkey via WebAuthn
PRF, OS-keychain device key, printed recovery code) is an independent *wrapping* of
the same random `user_key`, so credentials can be added or revoked by rewrapping 32
bytes rather than re-encrypting anything. Secrets are scoped `global` or
`entity:{id}`, resolving one step and no further, which is what lets two entities
hold different values for the same key name.

**Read [docs/VAULT.md](docs/VAULT.md) before changing anything under this crate** —
in particular the threat model (an unlocked vault is readable by the gateway, and
necessarily so, because the gateway feeds those credentials to the agents it runs)
and the format policy governing migrations, unknown-field preservation and backups.

### `crates/audio`
Avatar pipeline Layer 1 — voice in/out.

- `capture` — `cpal` mic capture at the native rate.
- `resample` — `rubato`-based resampling to 16 kHz PCM16 mono (Gemini Live input format).
- `gemini_live` — WebSocket transport for Gemini's `BidiGenerateContent`. Used by `gateway::voice` to broker voice sessions for the browser/Tauri.

### `crates/a2f`
Avatar pipeline Layer 2 — face animation.

- `client::A2fClient` — gRPC client for an Audio2Face-compatible service. Streams PCM16 → receives blendshape coefficient frames.
- `jitter::JitterBuffer` — smooths frame cadence for the downstream UE5 `Face_AnimBP` consumer.
- `proto` module — generated from `proto/audio2face.proto` (placeholder; swap in NVIDIA's `.proto` when available).

### `crates/gateway`
The binary. Each module has one job; here are the load-bearing ones:

| Module | Role |
|---|---|
| `main` | Boot sequence (Section 5). |
| `config` | `AppConfig` parsing, `~/.consistent-world/consistent_world.toml` lifecycle, `expand_tilde_path`, `resolve_llm`. |
| `startup` | `create_memory_and_schedule` — opens SurrealDB, hands back stores. |
| `state` | `ControlState` (entities, missions, tasks, directors, agents, automation) + `ServerState` (axum app state). |
| `server` | Axum router root, `require_bearer_token` middleware, `/health`, `/api/sessions`, `/api/metrics`, `/api/runtime/status`, `/api/schedule*`. |
| `control_api` | The bulk of REST endpoints — ~70 routes for entities, missions, tasks, agents, routing config, LLM admin, capture, approvals, operator feed, storage, compat shims. |
| `clerk_auth` | JWT verification against Clerk's JWKS. |
| `router` (gateway's, not the agent router) | `AgentRuntime` — wraps `AgentRouter` + the agents map; exposes `dispatch_scheduled_task` and the message ingress used by capture pipelines. |
| `orchestrator` | The 15-second heartbeat loop (Section 9). |
| `scheduler` | Cron-based scheduled tasks (`add_task`, `list_tasks`, `remove_task`, `run` loop). |
| `session` | `SessionManager` — load/persist sessions across restarts. |
| `capture` / `capture_chat_pipeline` / `chat_agent` / `chat_tools` | Chat-driven capture flow — turns natural language into entity edits, missions, and tasks. |
| `conversational_capture` | The interactive entity-creation conversation. |
| `entity_creation` / `entity_store` / `entity_runtime` / `entity_memory` / `entity_adapters/` / `entity_onboarding` / `entity_actions` | Entity domain logic. Adapters cover sub-kinds (domain/email/social). |
| `enrichment_llm` | LLM calls that propose entity-profile fields for review. |
| `feed_store` | Operator feed events (task events, mission events, approvals). |
| `intake_policy` / `policy` | Per-entity automation policy, capability gating. |
| `interpreter` / `director_schemas` | `PromptInterpretationBrief` parsing — the structured output that drives follow-up tasks. |
| `llm_provider_probe` | "Can this provider/model actually answer?" health check used by the LLM admin endpoints. |
| `routing_engine` | Maps `task_kind` → director + model tier + fallbacks (see `[routing.task_kinds.*]` in `consistent_world.toml`). |
| `planner` | Mission decomposition + next-step suggestion. |
| `handoff` | Director-to-director task handoff. |
| `tenant_store` | Workspace bootstrap on first sign-in. |
| `billing` | `CreditLedger`, `require_credit` middleware on `/api/inference`. |
| `credentials_resolver` | Reads `gateway_auth_token` and LLM API keys from keyring/env. |
| `memory_buffer` | Short-term context windows used during agent turns. |
| `dashboard` | Serves the SvelteKit build (gated by the `dashboard` cargo feature). |
| `voice` | `/api/voice/session` WebSocket — proxies to Gemini Live (Section 12). |
| `wizard` | Interactive onboarding (`--onboard`). |

Lines of note in `lib.rs`: every module above is `pub mod`-exported, **except** `wizard` and `entity_onboarding`, which are only used from `main.rs`.

---

## 7. Data model

The shapes that matter for understanding the product are in `crates/gateway/src/state.rs` and `crates/common/src/types.rs`.

### Entity (the unit of long-running work)
- `id`, `kind`, `name`
- `path` / `default_workspace_root` — filesystem home for tools
- `resources: Vec<{ root, read_allowed, write_allowed, exec_allowed }>` — capability roots
- `execution_policies` — `allowed_capabilities: Vec<CapabilityId>`, `execution_level`
- `automation` — `AutomationMode` per task kind
- Connections (`connections/*` routes) — external integrations the entity owns

### Mission
- `id`, `entity_id`, `title`, `summary`, `goal`, `priority`
- `status: MissionStatus` (`planned`/`active`/`blocked`/`user-input-needed`/`completed`)
- `owner`, `created_by`, `source`
- `created_at`, `updated_at`, `last_reviewed_at`

### Task
- `id`, `title`, `dept`, `roles[]`, `tries`, `progress`
- `status: TaskStatus` (`backlog`/`queued`/`in-progress`/`failed`/`completed`)
- `entity_id?`, `mission_id?`, `priority?`
- `task_kind?` — drives routing (`prompt-interpretation`, `entity-enrichment`, `mission-next-step`, `coding`, `debugging`, `review`, `planning`, `operations`, `growth`, `creative`)
- `assigned_director_id?`, `assigned_agent_id?`
- `outcome: TaskOutcome?` — `completed` / `needs-review` / `blocked-by-policy` / `needs-human-approval` / `needs-stronger-model` / `retryable-failure` / `failed`
- `requires_human_approval`, `source_task_id?` (for follow-ups)
- `result?` — LLM output once completed

### Director
- `id` (e.g. `operations`, `strategy-partner`, `full-stack-development`)
- `heartbeat_enabled` — the on/off switch the orchestrator uses

### Agent (config)
- `id`, `name`, `model`, `persona_path?`, `workspace`, `max_concurrent`

### Session (`crates/common/src/types.rs`)
- `id`, `agent_id`, `channel: ChannelKind`, `peer_id`
- `scope_type: ScopeType` (`principal` | `entity`), `scope_id`
- `channel_binding_metadata?`, `resolution_reason?`
- `created_at`, `last_active`

> **Serialization note:** `Session` is serialized **snake_case** (no `#[serde(rename_all = "camelCase")]`), so frontend code consumes `s.agent_id`, `s.peer_id`, `s.last_active`. Other admin response types **do** use camelCase via `#[serde(rename_all = "camelCase")]`. Don't assume one or the other — check the struct.

---

## 8. Request flow — a web chat turn

The web UI sends each turn to `/api/capture/chat/stream` (NDJSON: a `meta`
frame with the session id, `token` frames as the model produces text, then a
`done` frame carrying the same payload as the blocking endpoint). It falls back
to blocking `/api/capture/chat` when the stream is unavailable. Both paths run
the same tool loop and finish with the same tail writes (session cache, local
vault mirror, entity conversation memory, smart routing).

```
Browser  ──HTTP──►  /api/capture/chat  ──►  capture_chat_pipeline
                                            │
                                            ▼
                                       AgentRouter.resolve(message)
                                            │ (peer pattern → agent_id)
                                            ▼
                                       RuntimeAgent.run_turn()
                                            ├─► persona (system prompt)
                                            ├─► memory.recall (vector + recent)
                                            ├─► LLM provider (streamed)
                                            │   └── tool calls
                                            │       ├─► ToolPolicyProvider enriches args
                                            │       │   (entity-scoped roots/caps)
                                            │       └─► ToolRegistry.invoke(name, args)
                                            ├─► append to session messages
                                            ├─► cost tracker += tokens
                                            └─► return streamed response
```

Key insight: **the gateway, not the agent, knows about entities.** When an entity scope is on a message, `GatewayToolPolicyProvider::enrich_tool_arguments` (defined in `main.rs`) injects `_policy` into the tool call so the tool sees the *entity's* roots and capabilities, not the agent's defaults. This is how a single agent can safely serve many entities.

---

## 9. Orchestrator loop (`crates/gateway/src/orchestrator.rs`)

A 15-second tick driven by `tokio::time::interval`. For each director with `heartbeat_enabled = true`:

1. Find a queued task that this director can run (matches `task_kind` → director mapping in `[routing.task_kinds.*]`, entity automation allows it, capabilities allow it).
2. Assign it to an agent that belongs to the director, mark `in-progress`.
3. Call `run_agent` (LLM + tools).
4. On completion:
   - Mark task `completed` with `outcome` and `result`.
   - If `task_kind == "prompt-interpretation"`, parse the structured `PromptInterpretationBrief` and **enqueue one follow-up task** (the director auto-task rule). Cooldown enforced by `MISSION_AUTOFOLLOWUP_COOLDOWN_SECS = 900`.
   - Update mission status if all its tasks are terminal.
5. Emit feed events for the operator dashboard.

The loop also handles:
- **Stalled missions** — flag if no task progress for `MISSION_INACTIVITY_STALL_HOURS = 48`.
- **Missing next-task** — if mission has no open `mission-next-step` task and isn't completed, surface to the operator feed (cooldown `MISSION_GAP_REPORT_COOLDOWN_SECS = 300`).
- **Review-due** — if `last_reviewed_at` is overdue (cooldown 3600s).
- **Suppression** — `should_suppress_interpretation_followup` honors entity overrides.

The scheduler (`crates/gateway/src/scheduler.rs`) is separate: it fires *cron-defined* tasks, calling `runtime.dispatch_scheduled_task` on a 60-second granularity.

---

## 10. LLM routing — model tiers and task kinds

Config in `consistent_world.toml` is the source of truth for *which model handles what*:

```toml
[llm.providers.openai]      # connection info per provider
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[llm.models.brain-default]  # named model handles you reference elsewhere
provider = "openai"
model = "gpt-4o"

[routing.model_tiers.hosted_brain]   # tiers are an abstraction over models
provider = "openai"
model = "brain-default"
mode = "brain"

[routing.task_kinds.coding]          # task kind → director + tier mapping
director_id = "full-stack-development"
provider = "openai"
model = "brain-default"
mode = "brain"
generation_tier = "hosted_brain"
review_tier = "hosted_brain"
fallback_tiers = ["local_code", "local_fast"]
```

The flow at runtime:

1. A task arrives with `task_kind = "coding"`.
2. `routing_engine` looks up `[routing.task_kinds.coding]` → director `full-stack-development`, primary tier `hosted_brain`.
3. `[routing.model_tiers.hosted_brain]` → `(openai, brain-default)`.
4. `[llm.models.brain-default]` → `(openai, gpt-4o)`.
5. If the call fails or returns `needs-stronger-model`, walk `fallback_tiers` in order.

`mode` (`brain` / `muscle` / `fast-fallback`) controls temperature and max_tokens via `[routing.modes.*]`.

---

## 11. Memory model

One SurrealDB database, four logical tables (Surreal "tables" — actually record sets):

| Table | Records |
|---|---|
| `sessions` | One per conversation. Keyed by `id`. Index on `(agent_id, last_active)`. |
| `messages` | Append-only chat turns. `session_id` foreign key. |
| `memories` | Long-term recall items. Has `embedding: array<float, EMBEDDING_DIMENSION>` with an **HNSW vector index**. Semantic recall uses Surreal's built-in vector search — no external vector DB. |
| `feed` | Operator feed events (one row per `task_completed`, `mission_stalled`, `approval_needed`, etc.). |
| `entities` | Persisted entities (control state is hydrated from here on startup). |
| `scheduled_tasks` | Cron schedule records. |
| `tenants` | Workspace bootstrap rows. |
| `credit_ledger` | Per-workspace credit balance (billing). |

Schema creation is idempotent — `init_schema()` is called for each store at startup and failure is logged-and-continued (the gateway still boots, just with that store disabled).

Modes (set via `[memory] surreal_url` or `SURREAL_URL` env):

| Mode | Persistence | When to use |
|---|---|---|
| `surrealkv://~/.consistent-world/db` (default) | Disk, in-process | Desktop, install.sh, the single docker container |
| `memory` (in-process kv-mem) | None | Dev / unit tests |
| `ws://127.0.0.1:8000` | Local server | A shared database (docker compose) |
| `wss://*.surreal.cloud:443/rpc` | Cloud | Shared / production |

**Embeddings** (`embedding_setup.rs`, `[memory] embedding_provider`). `auto`
tries a local Ollama model first (`nomic-embed-text`, pulled on first start),
then the hosted keys. Every HNSW index is 1536 wide; narrower local vectors are
L2-normalised and zero-padded, which preserves cosine similarity exactly. The
database records which provider wrote its vectors (`system_meta:embedding`):
that provider stays first, a failed probe disables recall for the run rather
than switching spaces, and a real switch clears the old vectors.

### Skills (on disk, not in SurrealDB)

Skills — reusable procedures in Claude's `SKILL.md` format — are the one piece of
agent knowledge that deliberately lives on the filesystem, under
`~/.consistent-world/skills/` (built-in, `workspaces/<ws>/global/`,
`workspaces/<ws>/entities/<id>/`). They are file bundles, and unattended `claude`
CLI runs read them by path. Chat gets a one-line-per-skill index as a system
message every turn and loads bodies on demand through the `load_skill` tool;
`save_skill` writes a user-approved procedure back. A project skill shadows a
workspace one of the same name, which shadows a built-in. Full reference:
`docs/SKILLS.md`.

---

## 12. Voice pipeline

The avatar pipeline has three layers; Consistent owns Layer 1 (voice ↔ Gemini Live) and Layer 2 (audio → face). Layer 3 (UE5 MetaHuman) is consumed externally.

```
Browser/Tauri
    │ WebSocket /api/voice/session
    │   binary in:  PCM16 16 kHz mono
    │   binary out: PCM16 24 kHz mono
    │   text:       JSON control frames (config, setupComplete, text, turnComplete, ...)
    ▼
gateway::voice (axum WS upgrade)
    │
    ▼
consistent_world_audio::gemini_live::LiveSession
    │ WSS to BidiGenerateContent
    ▼
Google Gemini Live
```

For the face track:

```
Audio (PCM16) ──► consistent_world_a2f::A2fClient ──gRPC──► Audio2Face service
                                                       │
                                          blendshape frames (per-frame coeffs)
                                                       │
                                                       ▼
                                          JitterBuffer (smooth cadence)
                                                       │
                                                       ▼
                                          UE5 MetaHuman Face_AnimBP
```

The `.proto` shipped in `crates/a2f/proto/` is a placeholder shaped like NVIDIA's public Audio2Face streaming API — swap in NVIDIA's actual proto when integrating with NVIDIA Inception.

---

## 13. Authentication

`server::require_bearer_token` resolves every protected request, in order:

1. **Own-auth session** (`auth_store.rs`): username + password, passkeys, backup
   codes, device-approval login, API keys. Opaque tokens stored by hash,
   revocable, idle-expiring after 14 days.
2. **External identity** (`external_identity.rs`): an EdDSA JWT from a
   configured issuer. Off unless `[gateway.auth.external] issuer` is set.
3. **Static machine token** (`gateway_auth_token`): the local operator. Never
   read from TOML; from the keyring, env, or the auto-provisioned file next to
   the config. Auto-provisioned only on a loopback bind, or in the container
   with `CONSISTENT_WORLD_PROVISION_TOKEN=1` (token kept in the data volume). A
   network-bound gateway with no token refuses to start; the static token is
   refused outright when the request arrived over the relay.

**Paired devices** (`pairing.rs`, `pair_cli.rs`). A phone pairs by presence: the
signed-in owner mints a 90 s single-use code shown only as a QR on the
computer; the phone answers with an Ed25519 public key and a signature; both
screens show six digits derived from (code, key) and the owner allows it on
the computer. The device row holds the public key, its sessions carry its
`device_id`, and revoking the device revokes them. Afterwards the phone signs
in from anywhere with a challenge signature (`/api/auth/device/*`). The
transport is not restricted: the gateway binds loopback, so phones always
arrive through Tailscale Serve or the relay.

`/health`, voice routes, public auth and pairing routes, and the dashboard SPA
are outside the bearer layer.

---

## 14. HTTP API surface

~70 routes across these prefixes (full list: `grep -nE "\.route\(" crates/gateway/src/control_api.rs crates/gateway/src/server.rs crates/gateway/src/voice.rs crates/gateway/src/billing.rs`):

| Prefix | Purpose |
|---|---|
| `/health` | Liveness + memory backend label |
| `/api/sessions`, `/api/sessions/{id}/messages` | Conversation read |
| `/api/auth/*`, `/api/me` | Accounts, sessions, passkeys, API keys, devices, device-approval login |
| `/api/pair/offer`, `/api/pair/requests` (+ `/approve`, `/deny`) | Owner side of phone pairing (protected) |
| `/api/pair/enroll`, `/api/pair/requests/{id}/collect`, `/api/auth/device/challenge`, `/api/auth/device/login` | Phone side of pairing and key sign-in (public, every call signed) |
| `/api/pair/payload`, `/api/remote/status` | Legacy address-only QR; reachability doctor |
| `/api/ollama/pull` | First run: download a local model into Ollama, with progress |
| `/api/metrics`, `/api/runtime/status` | Telemetry |
| `/api/schedule`, `/api/schedule/{id}` | Cron tasks |
| `/api/workspaces`, `/api/departments` | Tenancy + department config |
| `/api/agents` | Configured agents (list + admin) |
| `/api/directors` | Director list + heartbeat toggle |
| `/api/tasks`, `/api/tasks/{id}`, `/api/tasks/{id}/requeue` | Task CRUD |
| `/api/missions`, `/api/missions/{id}`, `/api/missions/{id}/actions`, `/api/missions/policy` | Mission CRUD + apply action |
| `/api/entities` (+ many nested) | Entity CRUD, connections, records (the entity ledger — see `docs/integrations.md`), enrichment apply/discard/regenerate, files, memory-preview, runtime-context, actions/perform |
| `/api/entities/conversational/*` | Conversational entity creation flow |
| `/api/capture/chat` | Chat ingress |
| `/api/chat/attachments` | File uploads attached to a chat turn |
| `/api/commands` | Slash-command execution |
| `/api/operator-feed` | Operator activity feed |
| `/api/approvals` | Items waiting on human approval |
| `/api/routing/task-kinds`, `/api/routing/model-tiers` | Read/edit routing config from the UI |
| `/api/llm/providers`, `/api/llm/models`, `/api/llm/mode`, `/api/llm/profile`, `/api/llm/provider-status` | LLM admin |
| `/api/cloud/config` | Cloud-mode configuration |
| `/api/storage/*` | File/blob storage |
| `/api/inference` | Direct inference (billed; `require_credit` middleware) |
| `/api/voice/session` | Voice WebSocket |
| `/api/compat/*` | Legacy shims from the removed iOS app; no current caller |
| `/` (catch-all) | SvelteKit SPA (`dashboard` cargo feature) |

---

## 15. Frontend (`sveltfront/`)

SvelteKit 2 + Svelte 5 + Vite 7 + TypeScript. Built into `sveltfront/build/` and served by the gateway when compiled with `--features dashboard`.

Top-level routes (one folder = one screen):

```
sveltfront/src/routes/
├── +layout.svelte        # Shell, nav, global theme
├── +page.svelte          # Landing
├── about/
├── agents/               # Agent groups, chat, history
├── channels/             # (legacy — channel adapters are stubbed)
├── console/              # Capture/console (chat UI)
├── dashboard/            # Operator dashboard (tasks, directors, feed, approvals)
├── data/                 # Memory / vector store admin
├── entities/             # Entity list + entities/[id] details
├── missions/             # Mission list + missions/[id] details
├── models/               # LLM model/provider admin
├── settings/
├── tasks/
└── voice/                # Voice session UI
```

Shared client lives in `sveltfront/src/lib/`:
- `api.ts` — typed `api<T>(path, options)` helper, plus exported types (`Session`, `Entity`, `Mission`, ...).
- `components/` — UI primitives, the `ConsistentWorldFace` Pixel Streaming surface, the chat list.
- `voice.ts` — `VoiceSession` class wrapping the `/api/voice/session` WebSocket.

**Serialization gotcha:** most admin endpoints serialize camelCase, but `Session` (and a few others) serialize snake_case. When in doubt, check the Rust struct's `#[serde(rename_all = …)]`.

---

## 16. Channels — current state

The `crates/channels` crate was **removed**. Files for `discord.rs`, `imessage.rs`, `signal.rs`, `slack.rs`, `telegram.rs`, `webhook.rs`, `whatsapp.rs` were deleted. The `[channels.*]` blocks still exist in `consistent_world.toml` as a forward-compatible stub but they have no effect today.

`ChannelKind` in `crates/common` is currently a single-variant enum (`Web`). All ingress goes through the web/console UI or the HTTP API. Telegram/Slack/etc. will need to be reintroduced as either a new crate or a plugin when the time comes.

---

## 17. Desktop (`desktop/`)

Tauri 2 shell that ships the gateway binary as a sidecar so the whole product is one download.

```
desktop/
├── src-tauri/
│   ├── src/
│   │   ├── main.rs            # Tauri entry, spawns the consistent-world sidecar
│   │   └── voice.rs           # Voice IPC bridge
│   ├── tauri.conf.json        # App config, sidecar declaration
│   └── Cargo.toml
└── (frontend served from sveltfront's build)
```

Build steps (see `desktop/README.md`):

```bash
cargo build -p consistent-world-gateway --release      # from repo root
cd desktop
pnpm install
npm run build:sidecar                          # copies the binary into src-tauri/
pnpm tauri dev                                 # or `pnpm tauri build` for distribution
```

The sidecar binary path is declared in `tauri.conf.json` so Tauri ships it as a bundled resource and exposes a sidecar lifecycle.

The app registers itself to start at login on first run (`autostart.rs`,
`tauri-plugin-autostart`, launched with `--hidden` into the tray). Closing the
window hides it and the gateway keeps running; the tray's Quit stops both.
Settings → System status has the start-at-login switch.

---

## 18. Mobile (`mobile/`)

Expo / React Native app: capture-on-the-go, the operator feed, vault access, and
acting as an authenticator that approves sign-ins on other devices. It talks to
the ordinary `/api/*` surface with a session token, exactly as the web UI does.

Pairing (`app/pair.tsx`, `src/lib/pairingKey.ts`): a QR with a `nonce` starts
the key flow described in §13; one without falls back to password sign-in. The
key is made from `expo-crypto` randomness and stored in SecureStore; on a 401
the client signs back in with it once and retries (`client.ts::deviceLogin`).

A native Swift app previously lived in `ios/`. It was removed: it authenticated
via Clerk, which no longer exists in this codebase, so it could not have run.
The `/api/compat/*` shims that existed for it are still served and still tested,
but now have no caller — keep or drop them deliberately rather than by accident.

---

## 19. Configuration reference

`~/.consistent-world/consistent_world.toml` is auto-created on first run from `consistent_world.toml` in the repo. The key sections:

```toml
[gateway]                              # port, bind, mode
[gateway.auth]                         # mode = "token"
[gateway.auth.clerk]                   # issuer, audience (optional)

[agents.defaults]                      # max_concurrent, preferred_provider, preferred_model
[agents.defaults.subagents]            # max_concurrent
[agents.defaults.compaction]           # mode = "safeguard"

[[agents.list]]                        # one block per configured agent
id = "..."  name = "..."  model = "..."  workspace = "..."  persona_path = "..."

[llm]                                  # default provider + base_url + api_key_env
[llm.providers.<name>]                 # per-provider base_url + api_key_env
[llm.models.<handle>]                  # named model: provider + model

[routing.modes.<mode>]                 # brain / muscle / fast-fallback
[routing.model_tiers.<tier>]           # hosted_brain / local_fast / ...
[routing.task_kinds.<kind>]            # director_id + primary tier + fallbacks
[[routing.rules]]                      # peer-pattern routing (channel, peer_pattern, agent_id)

[memory]                               # vector_search, embedding_model, surreal_url, surreal_ns, surreal_db
[plugins]                              # wasm_sandbox, registry_url
[credentials]                          # backend = "keyring" | "vault"
[telemetry]                            # enabled, cost_tracking, log_level
```

Secret resolution order: env var named in `api_key_env` → keyring → empty (warn and disable feature).

---

## 20. Running locally

```bash
# Build
cargo build -p consistent-world-gateway --release

# Run with in-memory SurrealDB (data lost on restart, fine for dev)
cargo run -p consistent-world-gateway

# Open in a browser (the gateway serves the SvelteKit UI on the same port)
cargo run -p consistent-world-gateway -- start --open

# Persistent SurrealDB
docker start surrealdb   # or: docker run -d --name surrealdb -p 8000:8000 surrealdb/surrealdb:v2.3.10 start --bind 0.0.0.0:8000 --user root --pass root
# then set [memory] surreal_url = "ws://127.0.0.1:8000" in ~/.consistent-world/consistent_world.toml

# Re-run the interactive setup
cargo run -p consistent-world-gateway -- --onboard

# Manage credentials (uses OS keyring)
cargo run -p consistent-world-gateway -- credentials set OPENAI_API_KEY sk-...
cargo run -p consistent-world-gateway -- credentials get OPENAI_API_KEY
```

Frontend dev (hot reload):

```bash
cd sveltfront
npm install
npm run dev          # vite on :5173, proxies /api/* to the gateway
```

Default gateway URL: `http://127.0.0.1:18800`.

---

## 21. Testing

```bash
cargo test --workspace --no-fail-fast        # all crates
cargo test -p consistent-world-gateway               # gateway only
cargo test -p consistent-world-agents                # agent loop + routing

cd sveltfront && npm run check               # svelte-check (type errors + svelte warnings)
```

Cross-crate integration tests live in `tests/` at the workspace root (currently `tests/routing_test.rs`; `crates/gateway/tests/routing_test.rs` `include!`s the same file).

---

## 22. Where to look first

| Need | Start here |
|---|---|
| Add a new HTTP route | `crates/gateway/src/control_api.rs` (look at `control_routes()`) |
| Add a new tool | `crates/tools/src/lib.rs` + a new module; register in `main::build_runtime_agents` |
| Add an LLM provider | `crates/llm/src/<name>.rs` + the dispatch arm in `main::build_llm_provider` |
| Tweak the orchestrator | `crates/gateway/src/orchestrator.rs` |
| Change the chat flow | `crates/gateway/src/capture_chat_pipeline.rs` + `chat_agent.rs` + `chat_tools.rs` |
| Touch the data model | `crates/gateway/src/state.rs` (control state) + `crates/common/src/types.rs` (cross-crate) |
| Modify SurrealDB schema | `crates/memory/src/store.rs::init_schema` (memory) or the relevant `*_store.rs` in `crates/gateway/src/` |
| Wire a new screen | `sveltfront/src/routes/<name>/+page.svelte` (+ optional `+page.ts`) |
| Voice / avatar | `crates/audio` (audio I/O), `crates/a2f` (face), `crates/gateway/src/voice.rs` (WS broker) |
| Skills (built-ins, scopes, `load_skill`) | `crates/gateway/src/skills.rs`, built-ins in `crates/gateway/skills/`, API in `control_api/skills_api.rs` |
| Phone pairing / device keys | `crates/gateway/src/pairing.rs` (protocol + routes), `mobile/src/lib/pairingKey.ts` (must match) |
| Which embedding model backs recall | `crates/gateway/src/embedding_setup.rs` |

---

## 23. Doc map

Legacy planning/marketing docs (old product names, superseded phases) were deleted in the 2026-07 docs purge; recover them from git history if ever needed. The curated, still-current docs are:

- `docs/VISION.md` — what Consistent is for: category, evidence-backed-execution promise, moat, parked vision
- `docs/ROADMAP.md` — the current 90-day engineering roadmap
- `docs/diagrams/consistent-world-future-state.v2.mmd` — the hardened future-state architecture spec (the target every rebuild maps to)
- `ARCHITECTURE.md` (this file) — current technical overview
- `docs/SKILLS.md` — skills: format, scopes, how chat and agent runs use them, API, limits

`AGENTS.md` is for WARP/Claude session context, not human onboarding — leave it alone.
