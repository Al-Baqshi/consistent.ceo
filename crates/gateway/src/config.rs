//! TOML config loading (`consistent.toml`) into [`AppConfig`], plus the model
//! routing helpers (provider/model/tier resolution) other modules call to
//! turn a preferred provider/model/task-kind into a concrete LLM endpoint.

use anyhow::{Context, Result};
use consistent_world_common::capabilities::{CapabilityId, ExecutionLevel};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::{env, fs};
use tracing::warn;

/// Default TOML content written when config file is missing on first run.
/// Matches the schema parsed by this module (no secrets).
///
/// Model routing follows the two-layer product architecture:
///   * `operator` tier — deep reasoning / entity-aware work → strongest model (Claude Sonnet).
///   * `interpreter` tier — intake / fast classification / prompt interpretation → Gemini Flash.
///
/// Both tiers are explicit so the runtime never silently falls back to a wrong model.
pub fn default_toml_content() -> &'static str {
    r#"# Consistent — default configuration (created on first run)
# Edit this file or run: consistent-world --onboard
#
# Architecture:
#   operator    = strongest model (deep reasoning, entity-aware work)
#   interpreter = Gemini Flash (intake, fast classification, prompt-interpretation)

[gateway]
port = 18800
bind = "127.0.0.1"
# Proxies whose X-Forwarded-For header is believed when rate limiting. IPs or
# CIDRs. Unset means the header is ignored and the socket peer is used — the
# only safe default, since otherwise any client picks its own rate-limit bucket.
# trusted_proxies = ["10.0.0.0/8"]

# Where the assistant may write on your machine. Loopback binds only; on a
# network-bound gateway these tools are not offered at all.
#   notes_root    — "put a note on my desktop" lands here (default ~/Desktop)
#   projects_root — folders and files it creates (default ~/Consistent)
# Nothing outside these two is writable, and there is no setting that widens it.
# notes_root = "~/Desktop"
# projects_root = "~/Consistent"

# [gateway.auth]
# The static auth token normally authenticates as one fixed service principal.
# Turning this on additionally accepts `<secret>.<workspace>.<principal>`, which
# impersonates ANY principal in ANY workspace with no account behind it. That is
# how the desktop sidecar and multi-tenant dev setups work; it is a skeleton key
# anywhere reachable from a network. Required to bind off-localhost with a
# static token at all.
# static_principals = false

# [gateway.auth.external]
# An outside identity provider, for accounts that follow you between machines.
# Strictly additive and off by default: unset, local username + password is the
# only way in, which is what a self-hosted instance runs. Setting `issuer` turns
# on a third bearer path — an EdDSA JWT verified against the issuer's JWKS and
# mapped onto a local account. Set `audience` too, or a token this issuer minted
# for a different service is accepted here.
# issuer = "https://id.consistent.ceo"
# audience = "consistent-gateway"
# jwks_url = "https://id.consistent.ceo/.well-known/jwks.json"  # defaults to this

[agents.defaults]
# Default model alias for runtime agents. Resolves through [llm.models.operator-default]
# below, so chat/runtime work uses the strongest model unless overridden per-agent.
model = "operator-default"
max_concurrent = 4

[agents.defaults.subagents]
max_concurrent = 8

[memory]
backend = "surreal"
db_path = "~/.consistent-world/memory.db"
# Embedded persistent store (default): SurrealDB runs in-process, no Docker, and
# data survives restarts. The directory is created automatically on first run.
surreal_url = "surrealkv://~/.consistent-world/db"
surreal_ns = "consistent-world"
surreal_db = "main"
# For a REMOTE SurrealDB instead, point surreal_url at the server and set root auth:
#   surreal_url = "ws://127.0.0.1:8000"
#   surreal_user = "root"   # or set SURREAL_USER / SURREAL_PASS
#   surreal_pass = "root"
vector_search = true
# Semantic recall. "auto" uses a local Ollama embedding model when Ollama runs
# (pulled on first start, no key, nothing leaves the machine), else an
# OpenAI / Gemini key if one is configured. "off" disables it.
# embedding_provider = "auto"
# embedding_model = "nomic-embed-text"
# SurrealDB is the sole memory backend. If unreachable at startup, the gateway exits.

[plugins]
wasm_sandbox = true

[credentials]
backend = "keyring"

[telemetry]
log_level = "info"

# ---- LLM providers ---------------------------------------------------------
# Keyless-first, same as the desktop app: the default rides an installed
# `claude` CLI (your existing Claude subscription — no API key), then falls
# back to local Ollama. Add ANTHROPIC_API_KEY / GEMINI_API_KEY etc. to your
# env to graduate individual tiers to cloud APIs.
[llm]
provider = "claude-code"
base_url = "claude-code://"
api_key_env = "CLAUDE_CODE_CLI"

# The locally-installed `claude` CLI driven as a provider. No API key: it uses
# the operator's existing Claude subscription. If the CLI is not installed the
# provider construction falls through and the tiers below drop to Ollama.
[llm.providers.claude-code]
base_url = "claude-code://"
api_key_env = "CLAUDE_CODE_CLI"

[llm.providers.codex]
base_url = "codex://"
api_key_env = "CODEX_CLI"

[llm.providers.ollama]
base_url = "http://127.0.0.1:11434"
api_key_env = "OLLAMA_API_KEY"

[llm.providers.anthropic]
base_url = "https://api.anthropic.com/v1"
api_key_env = "ANTHROPIC_API_KEY"

[llm.providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"

[llm.providers.google]
base_url = "https://generativelanguage.googleapis.com/v1beta"
api_key_env = "GEMINI_API_KEY"

# ---- Semantic model aliases ------------------------------------------------
# Use these aliases everywhere instead of raw model strings. Swapping a model
# is a one-line change here; nothing in app code or per-task routing needs to
# move.
[llm.models.operator-default]
provider = "claude-code"
model = "opus"

[llm.models.operator-fallback]
provider = "ollama"
model = "gemma4:latest"

[llm.models.interpreter-default]
provider = "claude-code"
model = "sonnet"

[llm.models.interpreter-fallback]
provider = "google"
model = "gemini-2.5-flash"

# ---- Mode presets (temperature / max_tokens) -------------------------------
[routing.modes.deep]
temperature = 0.2
max_tokens = 8192

[routing.modes.fast]
temperature = 0.2
max_tokens = 2048

# ---- Model tiers (the two semantic lanes the runtime resolves against) -----
[routing.model_tiers.operator]
provider = "ollama"
model = "operator-default"
mode = "deep"

[routing.model_tiers.interpreter]
provider = "ollama"
model = "interpreter-default"
mode = "fast"

# ---- Task-kind routing ----------------------------------------------------
# Lightweight intake / classification → interpreter tier (Gemini Flash).
# The orchestrator uses prompt-interpretation as the gate before deeper work,
# so this is the load-bearing route for the fast layer.
[routing.task_kinds.prompt-interpretation]
generation_tier = "interpreter"
review_tier     = "interpreter"
fallback_tiers  = ["operator"]
mode            = "fast"

# Anything else runs on the operator tier by default. Override per-kind below
# if a task should route somewhere else.
[routing.task_kinds.entity-enrichment]
generation_tier = "operator"
review_tier     = "operator"
fallback_tiers  = ["interpreter"]
mode            = "deep"

# Optional: mission policy controls
# [missions]
# starter_task_auto_queueing_enabled = true
# followup_task_proposal_enabled = true
# followup_auto_create_enabled = true
# default_review_cadence_hours = 24

# How the companion reaches you. Reminders, approvals, heartbeat summaries and
# finished agent runs always land on the operator feed; these settings control
# whether they also chase you.
#
# mobile_push = false (the default) keeps everything on your own machines: the
# mobile app raises alerts by polling the feed while it is open, and the desktop
# shell raises native notifications while it is running. Setting it to true lets
# the gateway push to registered phones with the app closed, which routes the
# notification title and body through Expo's push service.
# [notifications]
# mobile_push = false
"#
}

static OFFLINE_FLAG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Record `start --offline` for the rest of the process.
pub fn set_offline(on: bool) {
    if on {
        OFFLINE_FLAG.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// True when this gateway must make no outbound call it can avoid: started
/// with `--offline`, or `CONSISTENT_WORLD_OFFLINE` set to a truthy value.
/// Hosted probes are skipped; anything on this machine (Ollama on loopback,
/// the embedded database) keeps working.
pub fn offline_mode() -> bool {
    OFFLINE_FLAG.load(std::sync::atomic::Ordering::Relaxed)
        || env::var("CONSISTENT_WORLD_OFFLINE")
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
}

/// Expands `~` in a path using `$HOME`. Public for use by main/wizard.
pub fn expand_tilde_path(path: &str) -> String {
    expand_tilde(path)
}

/// If the config file does not exist, create its parent directory and write default TOML.
/// Returns the expanded path (with `~` resolved) for loading.
pub fn ensure_config_file(path: &str) -> Result<String> {
    let expanded = expand_tilde(path);
    if Path::new(&expanded).exists() {
        return Ok(expanded);
    }
    if let Some(parent) = Path::new(&expanded).parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Create config directory {}", parent.display()))?;
    }
    fs::write(&expanded, default_toml_content())
        .with_context(|| format!("Write default config to {}", expanded))?;
    tracing::info!(path = %expanded, "Created default config file");
    Ok(expanded)
}

/// The fully-resolved runtime configuration: TOML file contents merged with
/// env-var overrides and defaults. Built by [`load`]; see [`AppConfig::default`]
/// for what every field is when the config file is missing or a section is absent.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub gateway_port: u16,
    pub gateway_bind: String,
    pub gateway_auth_token: Option<String>,
    /// Networks whose `X-Forwarded-For` is believed when rate limiting. Empty
    /// (the default) means the header is ignored and the socket peer is used —
    /// the only safe default, since otherwise any client picks its own bucket.
    pub gateway_trusted_proxies: Vec<ipnet::IpNet>,
    /// Where the chat agent may write notes (default `~/Desktop`) and folders
    /// (default `~/Consistent`). Only consulted on a loopback bind.
    pub local_notes_root: Option<String>,
    pub local_projects_root: Option<String>,
    /// External identity, when a deployment has one. `None` — the default, and
    /// what every self-hosted instance runs — means local accounts are the only
    /// way in. See `external_identity.rs`.
    pub gateway_external_identity: Option<crate::external_identity::ExternalIdentityConfig>,
    /// Allow the static deploy token to stand in for a *user*: the
    /// `<secret>.<workspace>.<principal>` form, which fabricates a principal
    /// with no database lookup. Legitimate for the local desktop sidecar,
    /// deliberate opt-in everywhere else. See `server::parse_scoped_bearer_token`.
    pub gateway_static_principals: bool,
    pub telemetry_log_level: String,
    pub default_model: String,
    pub max_concurrent_agents: usize,
    pub max_concurrent_subagents: usize,
    /// Legacy field retained for `/health` reporting; always reads as `"surreal"` after the SQLite removal.
    pub memory_backend: String,
    pub surreal_url: String,
    pub surreal_ns: String,
    pub surreal_db: String,
    /// Root auth for remote SurrealDB (Docker `--user` / `--pass`). None = try env, then local URL default.
    pub surreal_user: Option<String>,
    pub surreal_pass: Option<String>,
    pub vector_search_enabled: bool,
    /// `[memory] embedding_provider`: `auto` (local Ollama first, then hosted
    /// keys), `ollama`, `openai`, `gemini`, or `off`. See `embedding_setup.rs`.
    pub embedding_provider: String,
    /// `[memory] embedding_model`: the Ollama embedding model to use or pull.
    pub embedding_model: String,
    pub wasm_sandbox_enabled: bool,
    pub credentials_backend: String,
    /// Path to encrypted vault file (when backend is "vault"). Supports ~ expansion.
    pub vault_path: Option<String>,
    pub llm_provider: String,
    pub llm_base_url: String,
    pub llm_api_key_env: String,
    /// Named providers from [llm.providers.NAME]: name -> (base_url, api_key_env)
    pub llm_providers_map: std::collections::HashMap<String, (String, String)>,
    /// Model aliases from [llm.models.ALIAS]: alias -> (provider_name, model_id)
    pub llm_models_map: std::collections::HashMap<String, (String, String)>,
    pub agents_list: Vec<AgentListEntry>,
    pub routing_rules: Vec<RoutingRuleEntry>,
    /// Task-kind routing: task_kind (e.g. "entity-enrichment") -> director, provider, model, mode.
    pub task_kind_routing: std::collections::HashMap<String, TaskKindRoutingEntry>,
    /// Mode presets: "fast" | "balanced" | "deep" -> temperature/max_tokens.
    pub mode_presets: std::collections::HashMap<String, ModePreset>,
    /// Model tier routes: local_fast | local_deep | hosted_smart | hosted_brain.
    pub model_tier_routes: std::collections::HashMap<String, ModelTierRoute>,
    /// Mission policy controls.
    pub mission_policy: MissionPolicyConfig,
    /// Optional global execution defaults merged with per-entity `execution_policies`.
    pub global_policy: GlobalPolicyLayer,
    /// Companion notification delivery controls.
    pub notifications: NotificationsConfig,
}

/// TOML `[notifications]` — how the companion reaches the operator.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NotificationsConfig {
    /// Push notifications to registered mobile devices via Expo's push service.
    ///
    /// Off by default: with this false nothing leaves the operator's machines,
    /// and devices pick events up by polling `/api/operator-feed` while
    /// foregrounded. Turning it on lets the phone be notified with the app
    /// closed, at the cost of routing notification titles/bodies through
    /// Expo's servers.
    pub mobile_push: bool,
}

/// TOML `[policy]` — intersected with entity policies in `policy::merge_global_and_entity`.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GlobalPolicyLayer {
    #[serde(default)]
    pub allowed_capabilities: Vec<CapabilityId>,
    #[serde(default)]
    pub allowed_execution_levels: Vec<ExecutionLevel>,
    #[serde(default)]
    pub approval_required_capabilities: Vec<CapabilityId>,
}

/// TOML `[missions]` — toggles for the mission auto-pilot behaviors.
#[derive(Debug, Clone, Deserialize)]
pub struct MissionPolicyConfig {
    pub starter_task_auto_queueing_enabled: bool,
    pub followup_task_proposal_enabled: bool,
    pub followup_auto_create_enabled: bool,
    pub default_review_cadence_hours: u32,
}

impl Default for MissionPolicyConfig {
    fn default() -> Self {
        Self {
            starter_task_auto_queueing_enabled: true,
            followup_task_proposal_enabled: true,
            followup_auto_create_enabled: true,
            default_review_cadence_hours: 24,
        }
    }
}

/// Per-task-kind routing: optional director and optional LLM override.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TaskKindRoutingEntry {
    /// Prefer this director (agent group id) for this task kind.
    pub director_id: Option<String>,
    /// Override LLM provider name (from [llm.providers] or default).
    pub provider: Option<String>,
    /// Override model (alias from [llm.models] or raw model id).
    pub model: Option<String>,
    /// Mode preset name (from [routing.modes], e.g. fast | balanced | deep).
    pub mode: Option<String>,
    /// Preferred generation tier (e.g. local_fast, hosted_smart).
    pub generation_tier: Option<String>,
    /// Review tier for escalation/review passes.
    pub review_tier: Option<String>,
    /// Ordered fallback tiers when generation fails.
    pub fallback_tiers: Option<Vec<String>>,
}

/// Mode preset for temperature and max_tokens (e.g. fast / balanced / deep).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModePreset {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModelTierRoute {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
}

/// Output of [`resolve_tiers_for_task`] / [`apply_mode_to_tier_resolution`]:
/// which tier generates, which reviews, and the ordered fallback chain.
#[derive(Debug, Clone, Default)]
pub struct TierResolution {
    pub generation_tier: Option<String>,
    pub review_tier: Option<String>,
    pub fallback_tiers: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            gateway_port: 18800,
            gateway_bind: "127.0.0.1".into(),
            gateway_auth_token: None,
            gateway_trusted_proxies: Vec::new(),
            local_notes_root: None,
            local_projects_root: None,
            gateway_external_identity: None,
            gateway_static_principals: false,
            telemetry_log_level: "info".into(),
            // Semantic alias resolved through [llm.models.operator-default].
            // The runtime never compares this string against a provider's model
            // catalog directly — it's a key into the alias map.
            default_model: "operator-default".into(),
            max_concurrent_agents: 4,
            max_concurrent_subagents: 8,
            memory_backend: "surreal".into(),
            // Embedded persistent default: runs SurrealDB in-process (no Docker),
            // survives restarts. Override with a `ws://` URL for a remote server.
            surreal_url: "surrealkv://~/.consistent-world/db".into(),
            surreal_ns: "consistent-world".into(),
            surreal_db: "main".into(),
            surreal_user: None,
            surreal_pass: None,
            vector_search_enabled: true,
            embedding_provider: "auto".into(),
            embedding_model: consistent_world_memory::embedding::DEFAULT_OLLAMA_EMBED_MODEL.into(),
            wasm_sandbox_enabled: true,
            credentials_backend: "keyring".into(),
            vault_path: None,
            // Default to Anthropic because the operator (deep reasoning) tier is
            // the dominant runtime lane. The interpreter tier resolves through
            // [llm.models.interpreter-default] independently.
            llm_provider: "anthropic".into(),
            llm_base_url: "https://api.anthropic.com/v1".into(),
            llm_api_key_env: "ANTHROPIC_API_KEY".into(),
            llm_providers_map: std::collections::HashMap::new(),
            llm_models_map: std::collections::HashMap::new(),
            agents_list: Vec::new(),
            routing_rules: Vec::new(),
            task_kind_routing: std::collections::HashMap::new(),
            mode_presets: default_mode_presets(),
            model_tier_routes: std::collections::HashMap::new(),
            mission_policy: MissionPolicyConfig::default(),
            notifications: NotificationsConfig::default(),
            global_policy: GlobalPolicyLayer::default(),
        }
    }
}

fn default_mode_presets() -> std::collections::HashMap<String, ModePreset> {
    let mut m = std::collections::HashMap::new();
    // Deep reasoning lane — paired with the `operator` tier.
    m.insert(
        "deep".to_string(),
        ModePreset {
            temperature: Some(0.2),
            max_tokens: Some(8192),
        },
    );
    // Fast classification / intake lane — paired with the `interpreter` tier.
    m.insert(
        "fast".to_string(),
        ModePreset {
            temperature: Some(0.2),
            max_tokens: Some(2048),
        },
    );
    // Legacy aliases retained so existing user configs keep working.
    m.insert(
        "balance".to_string(),
        ModePreset {
            temperature: Some(0.3),
            max_tokens: Some(4096),
        },
    );
    m.insert(
        "sleeping".to_string(),
        ModePreset {
            temperature: Some(0.2),
            max_tokens: Some(2048),
        },
    );
    m
}

/// Task kinds that should run on the `interpreter` (Gemini Flash) tier.
///
/// Keep this list narrow: only flows that are pure intake / classification /
/// brief-extraction belong here. Anything that needs entity-aware reasoning,
/// tool calls, or planning belongs on the `operator` tier.
pub fn is_interpreter_task_kind(task_kind: &str) -> bool {
    matches!(
        task_kind,
        "prompt-interpretation"
            | "intake"
            | "classification"
            | "task-classification"
            | "extract"
            | "extract-task"
    )
}

/// Returns the semantic tier that should run a task by default, before any
/// per-task override is applied.
///
/// Returns `None` when the relevant tier is not configured in
/// `model_tier_routes` — in that case the caller falls back to its existing
/// resolution path (mode preferences, default provider/model).
pub fn semantic_tier_for_task(config: &AppConfig, task_kind: Option<&str>) -> Option<String> {
    let want = match task_kind {
        Some(k) if is_interpreter_task_kind(k) => "interpreter",
        _ => "operator",
    };
    if config.model_tier_routes.contains_key(want) {
        Some(want.to_string())
    } else {
        None
    }
}

/// One `[[agents.list]]` entry: a runtime agent config-declared rather than
/// created through the director UI.
#[derive(Debug, Clone, Deserialize)]
pub struct AgentListEntry {
    pub id: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub workspace: Option<String>,
    pub persona_path: Option<String>,
    pub max_concurrent: Option<usize>,
}

/// A `[[agents.routing]]` entry: routes messages on a channel/peer pattern
/// to a specific agent (see [`crate::router::build_agent_router`]).
#[derive(Debug, Clone, Deserialize)]
pub struct RoutingRuleEntry {
    pub channel: Option<String>,
    pub peer: Option<String>,
    pub agent_id: String,
}

// Raw shape of the TOML file, deserialized as-is (every field optional so a
// partial or missing file still parses). `load` below merges this over
// `AppConfig::default()` field by field, warning on anything absent.
#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    gateway: Option<GatewaySection>,
    agents: Option<AgentsSection>,
    memory: Option<MemorySection>,
    plugins: Option<PluginsSection>,
    credentials: Option<CredentialsSection>,
    telemetry: Option<TelemetrySection>,
    llm: Option<LlmSection>,
    routing: Option<RoutingSection>,
    missions: Option<MissionsSection>,
    notifications: Option<NotificationsSection>,
    #[serde(default)]
    policy: Option<GlobalPolicyLayer>,
}

#[derive(Debug, Default, Deserialize)]
struct NotificationsSection {
    mobile_push: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct MissionsSection {
    starter_task_auto_queueing_enabled: Option<bool>,
    followup_task_proposal_enabled: Option<bool>,
    followup_auto_create_enabled: Option<bool>,
    default_review_cadence_hours: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct RoutingSection {
    /// [routing.task_kinds."entity-enrichment"] etc.
    #[serde(default)]
    task_kinds: Option<std::collections::HashMap<String, TaskKindRoutingEntry>>,
    /// [routing.modes.fast], [routing.modes.balanced], [routing.modes.deep]
    #[serde(default)]
    modes: Option<std::collections::HashMap<String, ModePreset>>,
    /// [routing.model_tiers.local_fast], etc.
    #[serde(default)]
    model_tiers: Option<std::collections::HashMap<String, ModelTierRoute>>,
}

#[derive(Debug, Default, Deserialize)]
struct GatewaySection {
    port: Option<u16>,
    bind: Option<String>,
    /// Override where chat-created notes and folders land.
    notes_root: Option<String>,
    projects_root: Option<String>,
    /// IPs or CIDRs of proxies allowed to set `X-Forwarded-For`.
    #[serde(default)]
    trusted_proxies: Option<Vec<String>>,
    #[serde(default)]
    auth: Option<AuthSection>,
}

/// Auth section in TOML. Secret fields (e.g. token) are not in schema;
/// secrets are resolved from the credentials store at runtime.
#[derive(Debug, Default, Deserialize)]
struct AuthSection {
    /// Opt in to static/service principals — see
    /// [`AppConfig::gateway_static_principals`].
    #[serde(default)]
    static_principals: Option<bool>,
    /// `[gateway.auth.external]` — an outside JWT issuer, when there is one.
    #[serde(default)]
    external: Option<ExternalAuthSection>,
}

/// An external identity provider. Absent (the default) means local accounts are
/// the only way in, which is what every self-hosted instance runs.
#[derive(Debug, Default, Deserialize)]
struct ExternalAuthSection {
    /// Required to enable the path at all: the `iss` claim to trust.
    issuer: Option<String>,
    /// The `aud` to require. Omitting it accepts any token this issuer signed,
    /// including ones minted for a different service.
    audience: Option<String>,
    /// Defaults to the issuer's `/.well-known/jwks.json`.
    jwks_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentsSection {
    defaults: Option<AgentDefaults>,
    list: Option<Vec<AgentListEntry>>,
    routing: Option<Vec<RoutingRuleEntry>>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentDefaults {
    model: Option<String>,
    max_concurrent: Option<usize>,
    subagents: Option<SubAgentSection>,
}

#[derive(Debug, Default, Deserialize)]
struct SubAgentSection {
    max_concurrent: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct MemorySection {
    backend: Option<String>,
    surreal_url: Option<String>,
    surreal_ns: Option<String>,
    surreal_db: Option<String>,
    surreal_user: Option<String>,
    surreal_pass: Option<String>,
    vector_search: Option<bool>,
    embedding_provider: Option<String>,
    embedding_model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct PluginsSection {
    wasm_sandbox: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct CredentialsSection {
    backend: Option<String>,
    vault_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct TelemetrySection {
    log_level: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LlmSection {
    provider: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    /// [llm.default] — overrides flat provider/base_url/api_key_env when present
    default: Option<LlmDefaultSection>,
    /// [llm.providers.NAME] — named providers (e.g. local-ollama) for config-driven resolution
    #[serde(default)]
    providers: Option<std::collections::HashMap<String, LlmProviderEntry>>,
    /// [llm.models.ALIAS] — model aliases (e.g. strategy-analyst -> provider + model id)
    #[serde(default)]
    models: Option<std::collections::HashMap<String, LlmModelEntry>>,
}

#[derive(Debug, Default, Deserialize)]
struct LlmDefaultSection {
    provider: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LlmProviderEntry {
    base_url: Option<String>,
    api_key_env: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LlmModelEntry {
    provider: Option<String>,
    model: Option<String>,
}

/// Reads `path`, parses it as [`FileConfig`] TOML, and merges each present
/// section onto [`AppConfig::default`] (missing sections/fields just warn and
/// keep the default). A missing or empty file returns defaults outright —
/// this never fails startup on its own. A few settings are then overridable
/// by env var for container/systemd deploys.
pub fn load(path: &str) -> Result<AppConfig> {
    let mut config = AppConfig::default();
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => {
            warn!("Config file not found at {path}: {err}; using defaults");
            return Ok(apply_path_expansion(config));
        }
    };

    if content.trim().is_empty() {
        warn!("Config file at {path} is empty; using defaults");
        return Ok(apply_path_expansion(config));
    }

    let file_cfg: FileConfig = toml::from_str(&content)
        .with_context(|| format!("Failed to parse configuration file {path}"))?;

    if let Some(gateway) = file_cfg.gateway {
        assign_or_warn(&mut config.gateway_port, gateway.port, "gateway.port");
        assign_or_warn(&mut config.gateway_bind, gateway.bind, "gateway.bind");
        config.local_notes_root = gateway.notes_root.or(config.local_notes_root);
        config.local_projects_root = gateway.projects_root.or(config.local_projects_root);
        if let Some(proxies) = gateway.trusted_proxies {
            config.gateway_trusted_proxies = crate::rate_limit::parse_trusted_proxies(&proxies);
        }
        // gateway_auth_token is never read from TOML; resolve from credentials store at runtime.
        if let Some(auth) = gateway.auth {
            config.gateway_static_principals = auth.static_principals.unwrap_or(false);
            config.gateway_external_identity = auth.external.and_then(|ext| {
                let issuer = ext.issuer?.trim().to_string();
                if issuer.is_empty() {
                    return None;
                }
                if ext.audience.is_none() {
                    warn!(
                        "[gateway.auth.external] has no audience; any token this issuer signs will be accepted"
                    );
                }
                Some(crate::external_identity::ExternalIdentityConfig::new(
                    issuer,
                    ext.audience,
                    ext.jwks_url,
                ))
            });
        }
    } else {
        warn_missing("gateway", "section");
    }

    if let Some(agents) = file_cfg.agents.as_ref() {
        if let Some(ref defaults) = agents.defaults {
            assign_or_warn(
                &mut config.default_model,
                defaults.model.clone(),
                "agents.defaults.model",
            );
            assign_or_warn(
                &mut config.max_concurrent_agents,
                defaults.max_concurrent,
                "agents.defaults.max_concurrent",
            );
            if let Some(ref subagents) = defaults.subagents {
                assign_or_warn(
                    &mut config.max_concurrent_subagents,
                    subagents.max_concurrent,
                    "agents.defaults.subagents.max_concurrent",
                );
            } else {
                warn_missing("agents.defaults.subagents", "section");
            }
        } else {
            warn_missing("agents.defaults", "section");
        }
        if let Some(ref list) = agents.list {
            config.agents_list = list.clone();
        }
        if let Some(ref routing) = agents.routing {
            config.routing_rules = routing.clone();
        }
    } else {
        warn_missing("agents", "section");
    }

    if let Some(memory) = file_cfg.memory {
        assign_or_warn(&mut config.memory_backend, memory.backend, "memory.backend");
        assign_or_warn(
            &mut config.surreal_url,
            memory.surreal_url,
            "memory.surreal_url",
        );
        assign_or_warn(
            &mut config.surreal_ns,
            memory.surreal_ns,
            "memory.surreal_ns",
        );
        assign_or_warn(
            &mut config.surreal_db,
            memory.surreal_db,
            "memory.surreal_db",
        );
        if let Some(v) = memory.surreal_user {
            config.surreal_user = Some(v);
        }
        if let Some(v) = memory.surreal_pass {
            config.surreal_pass = Some(v);
        }
        assign_or_warn(
            &mut config.vector_search_enabled,
            memory.vector_search,
            "memory.vector_search",
        );
        if let Some(v) = memory.embedding_provider.filter(|v| !v.trim().is_empty()) {
            config.embedding_provider = v;
        }
        // "local" is what older desktop and sample configs wrote, back when the
        // key was ignored. It means "the default local model", not a model
        // called `local`.
        if let Some(v) = memory
            .embedding_model
            .filter(|v| !v.trim().is_empty() && !v.trim().eq_ignore_ascii_case("local"))
        {
            config.embedding_model = v;
        }
    } else {
        warn_missing("memory", "section");
    }

    if let Some(plugins) = file_cfg.plugins {
        assign_or_warn(
            &mut config.wasm_sandbox_enabled,
            plugins.wasm_sandbox,
            "plugins.wasm_sandbox",
        );
    } else {
        warn_missing("plugins", "section");
    }

    if let Some(credentials) = file_cfg.credentials {
        assign_or_warn(
            &mut config.credentials_backend,
            credentials.backend,
            "credentials.backend",
        );
        config.vault_path = credentials.vault_path;
    } else {
        warn_missing("credentials", "section");
    }

    if let Some(telemetry) = file_cfg.telemetry {
        assign_or_warn(
            &mut config.telemetry_log_level,
            telemetry.log_level,
            "telemetry.log_level",
        );
    } else {
        warn_missing("telemetry", "section");
    }

    if let Some(llm) = file_cfg.llm {
        if let Some(ref d) = llm.default {
            assign_or_warn(
                &mut config.llm_provider,
                d.provider.clone(),
                "llm.default.provider",
            );
            assign_or_warn(
                &mut config.llm_base_url,
                d.base_url.clone(),
                "llm.default.base_url",
            );
            assign_or_warn(
                &mut config.llm_api_key_env,
                d.api_key_env.clone(),
                "llm.default.api_key_env",
            );
        } else {
            assign_or_warn(&mut config.llm_provider, llm.provider, "llm.provider");
            assign_or_warn(&mut config.llm_base_url, llm.base_url, "llm.base_url");
            assign_or_warn(
                &mut config.llm_api_key_env,
                llm.api_key_env,
                "llm.api_key_env",
            );
        }
        if let Some(providers) = llm.providers {
            for (name, entry) in providers {
                let base_url = entry
                    .base_url
                    .unwrap_or_else(|| config.llm_base_url.clone());
                let api_key_env = entry
                    .api_key_env
                    .unwrap_or_else(|| config.llm_api_key_env.clone());
                config
                    .llm_providers_map
                    .insert(name, (base_url, api_key_env));
            }
        }
        if let Some(models) = llm.models {
            for (alias, entry) in models {
                if let (Some(provider), Some(model_id)) = (entry.provider, entry.model) {
                    config.llm_models_map.insert(alias, (provider, model_id));
                }
            }
        }
        // Ensure default provider is in the map so it can be resolved by name
        config
            .llm_providers_map
            .entry(config.llm_provider.clone())
            .or_insert_with(|| (config.llm_base_url.clone(), config.llm_api_key_env.clone()));
    } else {
        warn_missing("llm", "section");
    }

    if let Some(routing) = file_cfg.routing {
        if let Some(task_kinds) = routing.task_kinds {
            config.task_kind_routing = task_kinds;
        }
        if let Some(modes) = routing.modes {
            for (k, v) in modes {
                config.mode_presets.insert(k, v);
            }
        }
        if let Some(model_tiers) = routing.model_tiers {
            config.model_tier_routes = model_tiers;
        }
    }

    if let Some(missions) = file_cfg.missions {
        assign_or_warn(
            &mut config.mission_policy.starter_task_auto_queueing_enabled,
            missions.starter_task_auto_queueing_enabled,
            "missions.starter_task_auto_queueing_enabled",
        );
        assign_or_warn(
            &mut config.mission_policy.followup_task_proposal_enabled,
            missions.followup_task_proposal_enabled,
            "missions.followup_task_proposal_enabled",
        );
        assign_or_warn(
            &mut config.mission_policy.followup_auto_create_enabled,
            missions.followup_auto_create_enabled,
            "missions.followup_auto_create_enabled",
        );
        assign_or_warn(
            &mut config.mission_policy.default_review_cadence_hours,
            missions.default_review_cadence_hours,
            "missions.default_review_cadence_hours",
        );
    }

    if let Some(notifications) = file_cfg.notifications {
        assign_or_warn(
            &mut config.notifications.mobile_push,
            notifications.mobile_push,
            "notifications.mobile_push",
        );
    }

    if let Some(p) = file_cfg.policy {
        config.global_policy = p;
    }

    // Env-var overrides for the few settings deploys (Docker / systemd) need
    // to swap without rewriting the TOML.
    if let Ok(v) = env::var("SURREAL_URL")
        && !v.trim().is_empty()
    {
        config.surreal_url = v;
    }
    if let Ok(v) = env::var("SURREAL_NS")
        && !v.trim().is_empty()
    {
        config.surreal_ns = v;
    }
    if let Ok(v) = env::var("SURREAL_DB")
        && !v.trim().is_empty()
    {
        config.surreal_db = v;
    }
    if let Ok(v) = env::var("CONSISTENT_WORLD_PORT")
        && let Ok(p) = v.parse::<u16>()
    {
        config.gateway_port = p;
    }
    if let Ok(v) = env::var("CONSISTENT_WORLD_BIND")
        && !v.trim().is_empty()
    {
        config.gateway_bind = v;
    }

    Ok(apply_path_expansion(config))
}

/// Resolves agent's preferred_provider + preferred_model to (base_url, api_key_env, model_id).
/// Model can be an alias from [llm.models.ALIAS] or a raw model id (e.g. llama3.1:8b).
pub fn resolve_llm(
    config: &AppConfig,
    preferred_provider: &str,
    preferred_model: &str,
) -> Option<(String, String, String)> {
    resolve_llm_for_task(config, None, preferred_provider, preferred_model).0
}

/// Resolves LLM (base_url, api_key_env, model_id) and optional mode preset for a task.
/// If task_kind has a routing entry with provider/model, those override the agent's preferred provider/model.
/// If routing specifies mode, returns the corresponding ModePreset (temperature/max_tokens).
pub fn resolve_llm_for_task(
    config: &AppConfig,
    task_kind: Option<&str>,
    preferred_provider: &str,
    preferred_model: &str,
) -> (Option<(String, String, String)>, Option<ModePreset>) {
    let (provider_name, model_id) = if let Some(kind) = task_kind {
        if let Some(entry) = config.task_kind_routing.get(kind) {
            if let (Some(prov), Some(model)) = (entry.provider.as_deref(), entry.model.as_deref()) {
                (prov.to_string(), model.to_string())
            } else {
                resolve_provider_model(config, preferred_provider, preferred_model)
            }
        } else {
            resolve_provider_model(config, preferred_provider, preferred_model)
        }
    } else {
        resolve_provider_model(config, preferred_provider, preferred_model)
    };
    let (base_url, api_key_env) = config
        .llm_providers_map
        .get(&provider_name)
        .cloned()
        .unwrap_or_else(|| (config.llm_base_url.clone(), config.llm_api_key_env.clone()));
    let model_resolved = Some((base_url, api_key_env, model_id));
    let mode_preset = task_kind
        .and_then(|k| config.task_kind_routing.get(k))
        .and_then(|e| e.mode.as_deref())
        .and_then(|m| config.mode_presets.get(m))
        .cloned();
    (model_resolved, mode_preset)
}

/// True when an `entity-enrichment` task can call the LLM without an empty API key.
///
/// Matches the key resolution used in `run_agent`: Ollama / empty `api_key_env` is treated as
/// keyless; cloud providers require a non-empty value for the configured env var.
pub fn llm_ready_for_entity_enrichment(config: &AppConfig) -> bool {
    let (resolved, _) = resolve_llm_for_task(
        config,
        Some("entity-enrichment"),
        config.llm_provider.as_str(),
        config.default_model.as_str(),
    );
    let Some((_base, api_key_env, _model)) = resolved else {
        return false;
    };
    crate::credentials_resolver::llm_secret_configured(&api_key_env)
}

/// Resolve a named LLM provider from TOML (`[llm.providers.NAME]`) or the default `[llm]` provider.
/// Returns `(canonical_name, base_url, api_key_env)`.
pub fn resolve_llm_provider_from_config(
    config: &AppConfig,
    name: &str,
) -> Option<(String, String, String)> {
    let name_trim = name.trim();
    if name_trim.is_empty() {
        return None;
    }
    for (k, (base, key_env)) in &config.llm_providers_map {
        if k.eq_ignore_ascii_case(name_trim) {
            return Some((k.clone(), base.clone(), key_env.clone()));
        }
    }
    if config.llm_provider.eq_ignore_ascii_case(name_trim) {
        let (base, key_env) = config
            .llm_providers_map
            .get(&config.llm_provider)
            .cloned()
            .unwrap_or_else(|| (config.llm_base_url.clone(), config.llm_api_key_env.clone()));
        return Some((config.llm_provider.clone(), base, key_env));
    }
    None
}

/// Default local model when no `local-fast` alias is configured. gemma4 is
/// what onboarding pulls, so it is the model most likely to actually exist in
/// a fresh install's Ollama.
pub const LOCAL_FALLBACK_MODEL: &str = "gemma4:latest";

/// The registered local (Ollama) lane, for falling back when a cloud/CLI
/// provider is unavailable: `(base_url, api_key_env, model_id)`.
///
/// Model id comes from the `local-fast` alias when `[llm.models.local-fast]`
/// exists (same hop rule as everywhere else, minus the provider swap — this
/// lane is pinned to Ollama by construction), else [`LOCAL_FALLBACK_MODEL`].
/// `None` when no `ollama` provider is registered: an unregistered local lane
/// is dormant and must not be resurrected by a fallback.
pub fn resolve_local_ollama_lane(config: &AppConfig) -> Option<(String, String, String)> {
    let (_name, base_url, api_key_env) = resolve_llm_provider_from_config(config, "ollama")?;
    let mut model = config
        .llm_models_map
        .get("local-fast")
        .map(|(_, m)| m.clone())
        .filter(|m| !m.trim().is_empty())
        .unwrap_or_else(|| LOCAL_FALLBACK_MODEL.to_string());
    let mut hops = 0;
    while let Some((_, real)) = config.llm_models_map.get(&model) {
        if *real == model || hops >= 4 {
            break;
        }
        model = real.clone();
        hops += 1;
    }
    Some((base_url, api_key_env, model))
}

/// Resolve preferred generation/review/fallback tiers for a task.
pub fn resolve_tiers_for_task(
    config: &AppConfig,
    task_kind: Option<&str>,
    preferred_generation_tier: Option<&str>,
) -> TierResolution {
    let mut out = TierResolution::default();
    if let Some(kind) = task_kind
        && let Some(entry) = config.task_kind_routing.get(kind)
    {
        out.generation_tier = entry.generation_tier.clone();
        out.review_tier = entry.review_tier.clone();
        out.fallback_tiers = entry.fallback_tiers.clone().unwrap_or_default();
    }
    if out.generation_tier.is_none() {
        out.generation_tier = preferred_generation_tier.map(str::to_string);
    }
    out
}

/// Ordered tier preferences for a given mode + task kind.
///
/// The two semantic tiers (`operator`, `interpreter`) always lead the list so
/// that, when configured, they win over legacy `hosted_*` / `local_*` tiers.
/// The legacy tiers stay in the tail purely as fallbacks for existing user
/// configs.
pub fn mode_tier_preferences(mode: &str, task_kind: Option<&str>) -> Vec<String> {
    let is_interpreter = task_kind.map(is_interpreter_task_kind).unwrap_or(false);
    let is_code = task_kind.map(|k| k.contains("code")).unwrap_or(false);
    let is_extract = task_kind
        .map(|k| k.contains("extract") || k.contains("ocr"))
        .unwrap_or(false);

    let semantic_lead: &[&str] = if is_interpreter {
        &["interpreter", "operator"]
    } else {
        &["operator", "interpreter"]
    };

    let legacy: Vec<&str> = match mode {
        "sleeping" => {
            if is_code {
                vec!["local_code", "hosted_flash", "hosted_smart", "hosted_brain"]
            } else if is_extract {
                vec![
                    "local_extract",
                    "hosted_flash",
                    "hosted_smart",
                    "hosted_brain",
                ]
            } else {
                vec!["local_fast", "hosted_flash", "hosted_smart", "hosted_brain"]
            }
        }
        "fast" => {
            if is_interpreter {
                vec!["hosted_flash", "local_fast", "hosted_smart", "hosted_brain"]
            } else if is_code {
                vec![
                    "hosted_brain",
                    "hosted_smart",
                    "local_code",
                    "hosted_flash",
                    "local_fast",
                ]
            } else {
                vec![
                    "hosted_brain",
                    "hosted_smart",
                    "hosted_flash",
                    "local_fast",
                    "local_extract",
                ]
            }
        }
        _ => {
            if is_interpreter {
                vec!["hosted_flash", "local_fast", "hosted_smart", "hosted_brain"]
            } else if is_code {
                vec![
                    "hosted_smart",
                    "local_code",
                    "hosted_flash",
                    "hosted_brain",
                    "local_fast",
                ]
            } else if is_extract {
                vec![
                    "hosted_flash",
                    "local_extract",
                    "hosted_smart",
                    "hosted_brain",
                    "local_fast",
                ]
            } else {
                vec![
                    "hosted_smart",
                    "hosted_brain",
                    "hosted_flash",
                    "local_fast",
                    "local_extract",
                ]
            }
        }
    };

    let mut out: Vec<String> = Vec::with_capacity(semantic_lead.len() + legacy.len());
    for t in semantic_lead {
        out.push((*t).to_string());
    }
    for t in legacy {
        let s = t.to_string();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

pub fn apply_mode_to_tier_resolution(
    config: &AppConfig,
    mode: &str,
    task_kind: Option<&str>,
    mut resolution: TierResolution,
) -> TierResolution {
    let preferences = mode_tier_preferences(mode, task_kind);

    // Step 1: pick a generation tier. The semantic helper wins first so that
    // operator/interpreter routing is explicit, not implied by mode names.
    if resolution.generation_tier.is_none() {
        resolution.generation_tier = semantic_tier_for_task(config, task_kind).or_else(|| {
            preferences
                .iter()
                .find(|tier| config.model_tier_routes.contains_key(tier.as_str()))
                .cloned()
        });
    }

    // Step 2: build the fallback chain from mode preferences + any explicit
    // entries already provided in `resolution.fallback_tiers`.
    let mut fallbacks = Vec::new();
    for tier in preferences {
        if Some(tier.as_str()) != resolution.generation_tier.as_deref()
            && config.model_tier_routes.contains_key(tier.as_str())
            && !fallbacks.contains(&tier)
        {
            fallbacks.push(tier);
        }
    }
    for tier in &resolution.fallback_tiers {
        if Some(tier.as_str()) != resolution.generation_tier.as_deref()
            && config.model_tier_routes.contains_key(tier.as_str())
            && !fallbacks.contains(tier)
        {
            fallbacks.push(tier.clone());
        }
    }
    resolution.fallback_tiers = fallbacks;

    // Step 3: review tier defaults to the operator (deep reasoning) tier when
    // available, otherwise to the legacy mode-specific picks for back-compat.
    if resolution.review_tier.is_none() {
        let preferred = if config.model_tier_routes.contains_key("operator") {
            Some("operator".to_string())
        } else {
            match mode {
                "fast" => Some("hosted_brain".to_string()),
                "sleeping" => Some("hosted_flash".to_string()),
                _ => Some("hosted_smart".to_string()),
            }
        };
        resolution.review_tier =
            preferred.filter(|tier| config.model_tier_routes.contains_key(tier.as_str()));
    }
    resolution
}

/// Resolve LLM route for a specific model tier.
pub fn resolve_llm_for_tier(
    config: &AppConfig,
    tier: &str,
    preferred_provider: &str,
    preferred_model: &str,
) -> (Option<(String, String, String)>, Option<ModePreset>) {
    if let Some(route) = config.model_tier_routes.get(tier)
        && let (Some(provider), Some(model)) = (route.provider.as_deref(), route.model.as_deref())
    {
        let (provider_name, model_id) = resolve_provider_model(config, provider, model);
        let (base_url, api_key_env) = config
            .llm_providers_map
            .get(&provider_name)
            .cloned()
            .unwrap_or_else(|| (config.llm_base_url.clone(), config.llm_api_key_env.clone()));
        let mode = route
            .mode
            .as_deref()
            .and_then(|m| config.mode_presets.get(m))
            .cloned();
        return (Some((base_url, api_key_env, model_id)), mode);
    }
    resolve_llm_for_task(config, None, preferred_provider, preferred_model)
}

/// Local / edge-runtime provider ids: not honored for agent preferences — use global `[llm]` default instead.
/// Keeps local codepaths available in config without letting stale seeded prefs break cloud-first runs.
fn is_dormant_local_provider_name(name: &str) -> bool {
    let n = name.trim().to_ascii_lowercase();
    matches!(
        n.as_str(),
        "ollama"
            | "vllm"
            | "local-llms"
            | "local_llms"
            | "custom-local"
            | "custom_local"
            | "lmstudio"
            | "local"
            | "local-ollama"
            | "local_ollama"
    )
}

// Turns a preferred (provider, model) pair into a canonical one: resolves
// model aliases from `[llm.models]`, and redirects a dormant local provider
// (Ollama et al., unless the user explicitly registered it) to the
// configured cloud default instead.
fn resolve_provider_model(
    config: &AppConfig,
    preferred_provider: &str,
    preferred_model: &str,
) -> (String, String) {
    if let Some((prov, model)) = config.llm_models_map.get(preferred_model).cloned() {
        // Treat the provider as dormant only when it's *not* explicitly
        // configured in `[llm.providers.<name>]`. If the user has actively
        // registered an ollama (or other local) provider, that's an
        // intentional choice — honor it instead of silently downgrading
        // to the cloud default.
        if is_dormant_local_provider_name(&prov) && !config.llm_providers_map.contains_key(&prov) {
            return (config.llm_provider.clone(), config.default_model.clone());
        }
        return (prov, model);
    }

    if is_dormant_local_provider_name(preferred_provider)
        && !config.llm_providers_map.contains_key(preferred_provider)
    {
        return (config.llm_provider.clone(), config.default_model.clone());
    }

    let provider_known = !preferred_provider.trim().is_empty()
        && (preferred_provider == config.llm_provider
            || config.llm_providers_map.contains_key(preferred_provider));
    let model_looks_explicit = !preferred_model.trim().is_empty();

    if provider_known && model_looks_explicit {
        (preferred_provider.to_string(), preferred_model.to_string())
    } else {
        (config.llm_provider.clone(), config.default_model.clone())
    }
}

// Sets `*target` when the TOML provided a value; otherwise leaves the
// existing default in place and logs which key was missing.
fn assign_or_warn<T>(target: &mut T, value: Option<T>, key: &str)
where
    T: Clone + std::fmt::Display,
{
    if let Some(val) = value {
        *target = val;
    } else {
        warn!("Missing `{key}` in config; using default {target}");
    }
}

fn warn_missing(section: &str, kind: &str) {
    warn!("Missing `{section}` {kind}; using defaults");
}

fn apply_path_expansion(mut config: AppConfig) -> AppConfig {
    if let Some(ref p) = config.vault_path {
        config.vault_path = Some(expand_tilde(p));
    }
    config
}

fn expand_tilde(path: &str) -> String {
    consistent_world_common::platform::expand_tilde(path)
}

#[cfg(test)]
mod llm_ready_tests {
    use super::*;

    #[test]
    fn llm_ready_entity_enrichment_keyless_empty_api_key_env() {
        let mut c = AppConfig::default();
        c.llm_api_key_env.clear();
        assert!(llm_ready_for_entity_enrichment(&c));
    }

    #[test]
    fn llm_ready_entity_enrichment_keyless_ollama_api_key_env() {
        let c = AppConfig {
            llm_api_key_env: "OLLAMA_API_KEY".into(),
            ..AppConfig::default()
        };
        assert!(llm_ready_for_entity_enrichment(&c));
    }

    #[test]
    fn llm_ready_entity_enrichment_requires_nonempty_secret_env() {
        let key = "CONSISTENT_WORLD_UNITTEST_ENTITY_ENRICH_SECRET_A";
        let c = AppConfig {
            llm_api_key_env: key.into(),
            ..AppConfig::default()
        };
        // SAFETY: test-only env vars; no concurrent access to these keys.
        unsafe {
            env::remove_var(key);
        }
        assert!(!llm_ready_for_entity_enrichment(&c));
        unsafe {
            env::set_var(key, "not-empty");
        }
        assert!(llm_ready_for_entity_enrichment(&c));
        unsafe {
            env::remove_var(key);
        }
    }

    #[test]
    fn llm_ready_entity_enrichment_whitespace_only_secret_is_missing() {
        let key = "CONSISTENT_WORLD_UNITTEST_ENTITY_ENRICH_SECRET_B";
        let c = AppConfig {
            llm_api_key_env: key.into(),
            ..AppConfig::default()
        };
        unsafe {
            env::set_var(key, "   \t");
        }
        assert!(!llm_ready_for_entity_enrichment(&c));
        unsafe {
            env::remove_var(key);
        }
    }
}

#[cfg(test)]
mod provider_resolve_tests {
    use super::*;

    #[test]
    fn resolve_llm_provider_case_insensitive_map_key() {
        let mut c = AppConfig {
            llm_provider: "openai".into(),
            ..AppConfig::default()
        };
        c.llm_providers_map.insert(
            "openai".into(),
            ("https://api.example/v1".into(), "OPENAI_API_KEY".into()),
        );
        let r = resolve_llm_provider_from_config(&c, "OPENAI");
        assert_eq!(
            r,
            Some((
                "openai".into(),
                "https://api.example/v1".into(),
                "OPENAI_API_KEY".into()
            ))
        );
    }

    #[test]
    fn resolve_llm_provider_default_fallback_when_not_in_map() {
        let mut c = AppConfig {
            llm_provider: "acme".into(),
            llm_base_url: "https://acme.test/v1".into(),
            llm_api_key_env: "ACME_KEY".into(),
            ..AppConfig::default()
        };
        c.llm_providers_map.clear();
        let r = resolve_llm_provider_from_config(&c, "acme");
        assert_eq!(
            r,
            Some((
                "acme".into(),
                "https://acme.test/v1".into(),
                "ACME_KEY".into()
            ))
        );
    }

    #[test]
    fn resolve_llm_ignores_dormant_local_agent_provider() {
        let mut c = AppConfig {
            llm_provider: "openai".into(),
            default_model: "gpt-4o-mini".into(),
            ..AppConfig::default()
        };
        c.llm_providers_map.insert(
            "openai".into(),
            ("https://api.openai.com/v1".into(), "OPENAI_API_KEY".into()),
        );
        let (base, _env, model) = resolve_llm(&c, "local-llms", "qwen3:8b").expect("resolved");
        assert_eq!(base, "https://api.openai.com/v1");
        assert_eq!(model, "gpt-4o-mini");
    }

    #[test]
    fn resolve_llm_ignores_alias_pointing_at_local_provider() {
        let mut c = AppConfig {
            llm_provider: "openai".into(),
            default_model: "gpt-4o-mini".into(),
            ..AppConfig::default()
        };
        c.llm_providers_map.insert(
            "openai".into(),
            ("https://api.openai.com/v1".into(), "OPENAI_API_KEY".into()),
        );
        c.llm_models_map
            .insert("legacy-local".into(), ("ollama".into(), "llama3".into()));
        let (base, _env, model) = resolve_llm(&c, "openai", "legacy-local").expect("resolved");
        assert_eq!(model, "gpt-4o-mini");
        assert_eq!(base, "https://api.openai.com/v1");
    }

    #[test]
    fn local_ollama_lane_requires_registered_provider() {
        let c = AppConfig {
            llm_provider: "openai".into(),
            ..AppConfig::default()
        };
        assert_eq!(resolve_local_ollama_lane(&c), None);
    }

    #[test]
    fn local_ollama_lane_uses_local_fast_alias_else_stock_model() {
        let mut c = AppConfig {
            llm_provider: "openai".into(),
            ..AppConfig::default()
        };
        c.llm_providers_map.insert(
            "ollama".into(),
            ("http://127.0.0.1:11434".into(), "OLLAMA_API_KEY".into()),
        );
        let (base, env, model) = resolve_local_ollama_lane(&c).expect("registered lane");
        assert_eq!(base, "http://127.0.0.1:11434");
        assert_eq!(env, "OLLAMA_API_KEY");
        assert_eq!(model, LOCAL_FALLBACK_MODEL);

        c.llm_models_map
            .insert("local-fast".into(), ("ollama".into(), "gemma4:e4b".into()));
        let (_base, _env, model) = resolve_local_ollama_lane(&c).expect("registered lane");
        assert_eq!(model, "gemma4:e4b", "alias must beat the stock default");
    }
}

#[cfg(test)]
mod semantic_tier_tests {
    use super::*;

    fn cfg_with_both_tiers() -> AppConfig {
        let mut c = AppConfig::default();
        c.model_tier_routes.insert(
            "operator".into(),
            ModelTierRoute {
                provider: Some("anthropic".into()),
                model: Some("operator-default".into()),
                mode: Some("deep".into()),
            },
        );
        c.model_tier_routes.insert(
            "interpreter".into(),
            ModelTierRoute {
                provider: Some("google".into()),
                model: Some("interpreter-default".into()),
                mode: Some("fast".into()),
            },
        );
        c
    }

    #[test]
    fn prompt_interpretation_routes_to_interpreter_tier() {
        let c = cfg_with_both_tiers();
        assert_eq!(
            semantic_tier_for_task(&c, Some("prompt-interpretation")).as_deref(),
            Some("interpreter"),
            "prompt-interpretation MUST resolve to the interpreter (Gemini Flash) tier"
        );
    }

    #[test]
    fn classification_and_intake_route_to_interpreter_tier() {
        let c = cfg_with_both_tiers();
        for kind in ["intake", "classification", "task-classification"] {
            assert_eq!(
                semantic_tier_for_task(&c, Some(kind)).as_deref(),
                Some("interpreter"),
                "{kind} should route to interpreter tier"
            );
        }
    }

    #[test]
    fn entity_work_routes_to_operator_tier() {
        let c = cfg_with_both_tiers();
        for kind in ["entity-enrichment", "content-draft", "code-task", "general"] {
            assert_eq!(
                semantic_tier_for_task(&c, Some(kind)).as_deref(),
                Some("operator"),
                "{kind} should route to operator (deep reasoning) tier"
            );
        }
    }

    #[test]
    fn semantic_tier_is_none_when_tier_not_configured() {
        let c = AppConfig::default(); // no model_tier_routes entries
        assert!(semantic_tier_for_task(&c, Some("prompt-interpretation")).is_none());
        assert!(semantic_tier_for_task(&c, Some("entity-enrichment")).is_none());
    }

    #[test]
    fn apply_mode_to_tier_resolution_picks_operator_for_deep_work() {
        let c = cfg_with_both_tiers();
        let resolved = apply_mode_to_tier_resolution(
            &c,
            "deep",
            Some("entity-enrichment"),
            TierResolution::default(),
        );
        assert_eq!(resolved.generation_tier.as_deref(), Some("operator"));
    }

    #[test]
    fn apply_mode_to_tier_resolution_picks_interpreter_for_intake() {
        let c = cfg_with_both_tiers();
        let resolved = apply_mode_to_tier_resolution(
            &c,
            "fast",
            Some("prompt-interpretation"),
            TierResolution::default(),
        );
        assert_eq!(resolved.generation_tier.as_deref(), Some("interpreter"));
    }
}

#[cfg(test)]
mod embedding_config_tests {
    use super::*;

    fn load_memory_section(body: &str) -> AppConfig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("consistent_world.toml");
        std::fs::write(&path, format!("[memory]\n{body}\n")).unwrap();
        load(path.to_str().unwrap()).unwrap()
    }

    #[test]
    fn legacy_local_means_the_default_model() {
        // Every desktop install before 2026-10-11 wrote this line.
        let c = load_memory_section("embedding_model = \"local\"");
        assert_eq!(
            c.embedding_model,
            consistent_world_memory::embedding::DEFAULT_OLLAMA_EMBED_MODEL
        );
        assert_eq!(c.embedding_provider, "auto");
    }

    #[test]
    fn a_named_model_and_provider_are_kept() {
        let c = load_memory_section(
            "embedding_model = \"mxbai-embed-large\"\nembedding_provider = \"off\"",
        );
        assert_eq!(c.embedding_model, "mxbai-embed-large");
        assert_eq!(c.embedding_provider, "off");
    }
}
