# AGENTS.md

This file provides guidance to WARP (warp.dev) when working with code in this repository.

## Project shape

This repository is a Rust workspace centered on the `consistent-world-gateway` binary. The gateway wires together:
- inbound/outbound channel adapters (`crates/channels`)
- agent runtime and tool-calling loop (`crates/agents`)
- LLM providers and streaming (`crates/llm`)
- persistent state + semantic memory (`crates/memory`)
- built-in tools + WASM-backed plugin tools (`crates/tools`, `crates/plugins`)
- credentials backends (`crates/credentials`)
- shared types/capabilities/telemetry (`crates/common`)

`Cargo.toml` at repo root defines all workspace members and shared dependencies.

## Runtime architecture (big picture)

### Gateway startup and wiring
- Entry point: `crates/gateway/src/main.rs`.
- Config is loaded from `~/.consistent-world/consistent_world.toml` (created automatically if missing) via `config::ensure_config_file` + `config::load`.
- Memory/schedule backend is created by `startup::create_memory_and_schedule`:
  - prefers SurrealDB when configured
  - falls back to SQLite automatically if SurrealDB is unavailable.
- `ServerState` is assembled with memory backend, control state, optional embedding provider, runtime status, and agent registry.

### Two long-lived loops plus HTTP API
- HTTP server (`crates/gateway/src/server.rs`) exposes `/health`, protected `/api/*`, and control routes.
- Message routing loop (`crates/gateway/src/router.rs`):
  - receives inbound channel messages
  - resolves scope
  - chooses runtime agent via routing rules
  - executes `Agent::handle_message`
  - sends adapter replies.
- Orchestrator loop (`crates/gateway/src/orchestrator.rs`):
  - heartbeat-driven task pickup
  - director/agent assignment
  - mission follow-up and operator-feed updates.
- Heartbeat ticker (`crates/gateway/src/heartbeat_store.rs::HeartbeatTicker`):
  - polls every ~30s, picks due rows from the `heartbeats` table
  - hands each to `ChatHeartbeatDispatcher` (`crates/gateway/src/heartbeat_dispatcher.rs`)
  - dispatcher resolves the assigned director + active mission's pinned notes, persists a row in `heartbeat_results` for the entity, which the chat UI surfaces as `tone:'heartbeat'` bubbles.

### Three-pillar foundation modules (added 2026-05-24)

- **Directors as data** — `director_store.rs` (table `directors`), `director_seed.rs` (idempotent seed from `agents_seed.json`), `director_lessons_store.rs` (table `director_lessons`, HNSW recall, spliced into prompts).
- **Missions canonical** — `mission_store.rs` (table `missions` with `narrative` + `canonical_notes`; `pin_note` / `set_narrative` / `list_active_for_entity`).
- **Heartbeats** — `heartbeat_store.rs` (table `heartbeats`, `HeartbeatStore` + `HeartbeatTicker`), `heartbeat_dispatcher.rs`, `heartbeat_results_store.rs` (table `heartbeat_results`).
- **Prompt pipeline** — `prompt_assembly.rs` (pure `assemble_system_prompt(&CanonicalInputs)`; SOUL → director → entity → mission, with optional lessons). Wired into `chat_agent.rs::build_initial_messages`.
- **Hot-swap LLM** — `credentials_resolver.rs::resolve_llm_api_key` re-reads env+keyring per turn; settings UI probes new keys before keyring write.

### Agent execution model
- Core loop: `crates/agents/src/agent.rs`.
- For each message:
  - recalls semantic memories (if embedding provider enabled),
  - builds chat history from `MemoryStore`,
  - sends completion request to LLM provider,
  - executes tool calls through `ToolRegistry`,
  - persists user/assistant/tool history.
- Tool arguments can be policy-enriched via `ToolPolicyProvider` (gateway injects scope-aware policy hints).

### State boundaries
- `ControlState` (`crates/gateway/src/state.rs`) holds entities/tasks/missions/directors/approvals and is snapshotted to `~/.consistent-world/control_state.json`.
- Session/message/memory persistence is via `MemoryStore` trait:
  - `SurrealMemoryStore` supports semantic search + HNSW vector index.
  - `SqliteMemoryStore` is fallback/no-vector mode.

## Common commands

Run commands from repository root unless noted.

### Build and run
```bash
cargo build -p consistent-world-gateway --release
```

```bash
cargo run -p consistent-world-gateway
```

```bash
cargo run -p consistent-world-gateway -- start --open
```

### Quality gates (same style as CI)
```bash
cargo check --workspace
```

```bash
cargo test --workspace
```

```bash
cargo clippy --workspace -- -D warnings
```

```bash
cargo fmt --all
```

### Run tests for one crate or one test
```bash
cargo test -p consistent-world-gateway
```

```bash
cargo test -p consistent-world-gateway auth_context_uses_scoped_token_when_secret_matches_prefix
```

### Configuration and credentials
```bash
cargo run -p consistent-world-gateway -- --onboard
```

```bash
cargo run -p consistent-world-gateway -- credentials set consistent_world.gateway.auth_token <token>
```

### Data migration utility
```bash
cargo run -p consistent-world-gateway --bin migrate_sqlite_to_surreal -- --dry-run
```

### WASM plugin build example
```bash
cargo build -p consistent-world-plugin-hello-world --target wasm32-wasip1 --release
```

### Optional desktop shell workflow
From `desktop/`:
```bash
pnpm install
pnpm run build:sidecar
pnpm tauri dev
```
