//! Binary entry point for the `consistent-world` gateway: parses the CLI,
//! loads config, brings up every Surreal-backed store and background loop
//! (scheduler, orchestrator, heartbeats, mission engine, task executor,
//! reminders, accountability), then serves the axum router until shutdown.
// Phase 1+ will wire config, router, session, server, scheduler, dashboard.
#![allow(dead_code)]

mod accountability;
mod activity;
mod agent_cli_probe;
mod agent_review;
mod agent_sessions;
mod alerts;
mod architect_bindings;
mod auth_api;
mod auth_store;
mod backup;
mod billing;
mod canvas_api;
mod canvas_store;
mod capture;
mod capture_chat_pipeline;
mod capture_routing;
mod chat_agent;
mod chat_tools;
mod config;
mod control_api;
mod control_snapshot_store;
mod credentials_resolver;
mod dashboard;
mod device_store;
mod director_lessons_store;
mod director_runner;
mod director_schemas;
mod director_seed;
mod director_store;
mod embedding_setup;
mod enrichment_llm;
mod entity_actions;
mod entity_adapters;
mod entity_creation;
mod entity_docs_store;
mod entity_heartbeat_defaults;
mod entity_import;
mod entity_knowledge;
mod entity_memory;
mod entity_records_store;
mod entity_runtime;
mod entity_store;
mod entity_workspace;
mod evidence;
mod external_identity;
mod feed_store;
mod heartbeat_dispatcher;
mod heartbeat_results_store;
mod heartbeat_store;
mod intake_policy;
mod interpreter;
mod lessons;
mod llm_provider_probe;
mod local_files;
mod local_vault;
mod mcp_browser;
mod memory_buffer;
mod mission_dod;
mod mission_engine;
mod mission_goal_loop;
mod mission_planner;
mod mission_replanner;
mod mission_report;
mod mission_store;
mod notify;
mod orchestrator;
mod pair_cli;
mod pairing;
mod passkey;
mod planner;
mod policy;
mod power;
mod prompt_assembly;
mod rate_limit;
mod relay_client;
mod reminders;
mod remote_doctor;
mod retry;
mod router;
mod routing_engine;
mod run_agent;
mod scheduler;
mod server;
mod session;
mod skills;
mod startup;
mod state;
mod supervisor;
mod task_executor;
mod task_store;
mod tenant_store;
mod user_profile;
mod vault_api;
mod vault_manager;
mod vault_throttle;
mod verification;
mod verification_actions;
mod voice;
mod voice_pod;
mod webhooks;
mod wizard;

use anyhow::Context;
use clap::{Parser, Subcommand};
use consistent_world_agents::agent::{Agent as RuntimeAgent, ToolPolicyProvider};
use consistent_world_agents::persona::Persona;
use consistent_world_common::capabilities::CapabilityId;
use consistent_world_credentials::keyring::KeyringStore;
use consistent_world_llm::anthropic::AnthropicProvider;
use consistent_world_llm::cost::CostCalculator;
use consistent_world_llm::google::GoogleProvider;
use consistent_world_llm::ollama::OllamaProvider;
use consistent_world_llm::openai::OpenAiProvider;
use consistent_world_plugins::host::{SandboxConfig, WasmHost};
use consistent_world_tools::ToolRegistry;
use consistent_world_tools::browser::BrowserTool;
use consistent_world_tools::coding_agent::CodingAgentTool;
use consistent_world_tools::filesystem::{FileSystemPolicy, FileSystemTool};
use consistent_world_tools::http::HttpTool;
use consistent_world_tools::shell::{self, ShellPolicy, ShellTool};
use consistent_world_tools::system::SystemTool;
use consistent_world_tools::web_search::WebSearchTool;
use dotenvy::dotenv;
use router::{AgentRuntime, build_agent_router};
use serde_json::json;
use session::SessionManager;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::{info, warn};

/// Provision a stable local gateway auth token for loopback deployments.
///
/// Stored as `gateway_auth_token` in a sibling file next to the config so it persists
/// across restarts and can be read by the desktop shell to authenticate the embedded UI.
/// Reuses an existing token if one is already on disk (keeps sessions valid across restarts).
fn provision_local_auth_token(config_path: &Path) -> std::io::Result<String> {
    let dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    match provision_token_in(dir) {
        Ok(token) => Ok(token),
        // A read-only config directory (the container's /etc): keep the token
        // in the writable state directory instead.
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            let home = config::expand_tilde_path("~/.consistent-world");
            std::fs::create_dir_all(&home)?;
            provision_token_in(Path::new(&home))
        }
        Err(e) => Err(e),
    }
}

fn provision_token_in(dir: &Path) -> std::io::Result<String> {
    let token_path = dir.join("gateway_auth_token");
    if let Ok(existing) = std::fs::read_to_string(&token_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    std::fs::write(&token_path, &token)?;
    // Best-effort tighten permissions to owner-only on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(token)
}

// Bridges the generic `scheduler` crate to this crate's `AgentRuntime` so a
// cron-fired task turns into an actual agent invocation.
struct RuntimeScheduleDispatcher {
    runtime: AgentRuntime,
}

// Looks up the message's scoped entity and injects its resource roots +
// allowed capabilities into filesystem/shell tool calls, so a tool never has
// to consult `ControlState` itself to know what it may touch.
struct GatewayToolPolicyProvider {
    control_state: Arc<state::ControlState>,
}

impl ToolPolicyProvider for GatewayToolPolicyProvider {
    /// Merges an entity-scoped `_policy` object (workspace root, allowed
    /// capabilities, read/write/exec roots) into `filesystem`/`shell` tool
    /// arguments; other tools pass through unchanged.
    fn enrich_tool_arguments(
        &self,
        message: &consistent_world_common::types::Message,
        tool_name: &str,
        arguments: &serde_json::Value,
        agent_workspace: &str,
    ) -> serde_json::Value {
        let mut enriched = arguments.clone();
        let scope = message.metadata.get("scope");
        let scope_type = scope
            .and_then(|s| s.get("scope_type"))
            .and_then(|v| v.as_str())
            .unwrap_or("principal");
        let scope_id = scope
            .and_then(|s| s.get("scope_id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if scope_type != "entity" || scope_id.is_empty() {
            return enriched;
        }

        let entities = self.control_state.entities.blocking_read();
        let Some(entity) = entities.iter().find(|e| e.id == scope_id) else {
            return enriched;
        };

        let allowed_capabilities = entity
            .execution_policies
            .as_ref()
            .map(|p| {
                p.allowed_capabilities
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let allowed_roots = if entity.resources.is_empty() {
            entity
                .default_workspace_root
                .clone()
                .or_else(|| entity.path.clone())
                .into_iter()
                .collect::<Vec<_>>()
        } else {
            entity
                .resources
                .iter()
                .map(|r| r.root.clone())
                .collect::<Vec<_>>()
        };
        let effective_workspace = entity
            .default_workspace_root
            .clone()
            .or_else(|| allowed_roots.first().cloned())
            .unwrap_or_else(|| agent_workspace.to_string());

        let policy_json = match tool_name {
            "filesystem" => json!({
                "workspaceRoot": effective_workspace,
                "allowedCapabilities": allowed_capabilities,
                "readRoots": allowed_roots,
                "writeRoots": entity.resources.iter().filter(|r| r.write_allowed).map(|r| r.root.clone()).collect::<Vec<_>>(),
                "listRoots": entity.resources.iter().filter(|r| r.read_allowed).map(|r| r.root.clone()).collect::<Vec<_>>(),
                "searchRoots": entity.resources.iter().filter(|r| r.read_allowed).map(|r| r.root.clone()).collect::<Vec<_>>()
            }),
            "shell" => json!({
                "workingDir": effective_workspace,
                "allowedCapabilities": allowed_capabilities,
                "allowedCwdRoots": entity.resources.iter().filter(|r| r.exec_allowed).map(|r| r.root.clone()).collect::<Vec<_>>(),
            }),
            _ => return enriched,
        };

        if let Some(obj) = enriched.as_object_mut() {
            obj.insert("_policy".to_string(), policy_json);
        }
        enriched
    }
}

impl scheduler::ScheduleDispatcher for RuntimeScheduleDispatcher {
    fn dispatch(
        &self,
        params: scheduler::ScheduledDispatchParams,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            runtime
                .dispatch_scheduled_task(
                    params.agent_id,
                    params.task_description,
                    params.session_id,
                    params.entity_id,
                    params.connection_id,
                )
                .await;
        })
    }
}

/// `clap` argument definitions for the `consistent-world` binary.
#[derive(Parser, Debug)]
#[command(
    name = "consistent-world",
    version,
    about = "Consistent — self-hosted AI agent platform"
)]
struct Cli {
    /// Path to config file (default: ~/.consistent-world/consistent_world.toml; created on first run if missing)
    #[arg(
        short,
        long,
        default_value = "~/.consistent-world/consistent_world.toml"
    )]
    config: String,

    /// Override gateway port
    #[arg(short, long)]
    port: Option<u16>,

    /// Run the onboarding wizard (interactive setup)
    #[arg(long)]
    onboard: bool,

    #[command(subcommand)]
    command: Option<CliCommand>,
}

/// Top-level subcommands; running with none starts the gateway.
#[derive(Subcommand, Debug)]
enum CliCommand {
    /// Start the gateway (default when no subcommand)
    Start {
        /// Open dashboard in default browser after starting
        #[arg(long)]
        open: bool,
        /// Make no outbound network call at startup (no hosted embedding
        /// probes). Same as CONSISTENT_WORLD_OFFLINE=1. Local providers
        /// (Ollama on this machine) are still used.
        #[arg(long)]
        offline: bool,
    },
    /// Manage credentials (keyring: set, get, list)
    Credentials {
        #[command(subcommand)]
        cmd: CredentialsCmd,
    },
    /// Remote access: reach this gateway from your phone, from anywhere
    Remote {
        #[command(subcommand)]
        cmd: RemoteCmd,
    },
    /// Show the QR code that pairs the mobile app with this gateway
    Pair,
    /// Serve the browser tool over MCP on stdio (used by browser-kind tasks)
    McpBrowser,
}

/// `consistent-world remote <cmd>` — Tailscale-based reachability from outside
/// the local network.
#[derive(Subcommand, Debug)]
enum RemoteCmd {
    /// Set up remote access (Tailscale serve) and print the reachable URL
    Setup,
    /// Show remote reachability status
    Status,
}

#[derive(Subcommand, Debug)]
enum CredentialsCmd {
    /// Store a credential by key
    Set { key: String, value: String },
    /// Retrieve a credential by key
    Get { key: String },
    /// List known credential keys (keyring backend may return empty)
    List,
}

/// Config path: CONSISTENT_WORLD_CONFIG env var overrides CLI default (for Docker/systemd).
/// A set-but-empty variable (easy to produce from a stale .env or `VAR= cmd`)
/// reads as unset — otherwise first run dies trying to write a config at "".
fn effective_config_path(cli_config: &str) -> String {
    std::env::var("CONSISTENT_WORLD_CONFIG")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| cli_config.to_string())
}

// Picks a concrete `LlmProvider` implementation by sniffing the base URL /
// api_key_env for known hosts, falling back to the generic OpenAI-compatible
// client for anything else.
fn build_llm_provider(
    base_url: &str,
    api_key: &str,
    api_key_env: &str,
) -> Arc<dyn consistent_world_llm::client::LlmProvider> {
    // CLI-backed subscription providers first — without these branches a
    // config-declared agent on `claude-code://` or `codex://` fell through to
    // the OpenAI client and every call died with a transport error.
    if base_url.starts_with("claude-code://")
        || api_key_env == "CLAUDE_CODE_CLI"
        || base_url == "claude-code"
    {
        if let Some(p) = consistent_world_llm::claude_code::ClaudeCodeProvider::new(None) {
            return Arc::new(p);
        }
        warn!(
            "claude-code provider requested for a config-declared agent but the `claude` CLI was not found"
        );
    }
    if base_url.starts_with("codex://") || api_key_env == "CODEX_CLI" || base_url == "codex" {
        if let Some(p) = consistent_world_llm::codex::CodexProvider::new(None) {
            return Arc::new(p);
        }
        warn!(
            "codex provider requested for a config-declared agent but the `codex` CLI was not found"
        );
    }
    let is_ollama = api_key_env == "OLLAMA_API_KEY"
        || base_url.contains(":11434")
        || base_url.contains("ollama");
    let is_google = api_key_env == "GOOGLE_API_KEY"
        || api_key_env == "GEMINI_API_KEY"
        || base_url.contains("generativelanguage.googleapis.com");
    let is_anthropic = api_key_env == "ANTHROPIC_API_KEY" || base_url.contains("api.anthropic.com");
    if is_ollama {
        Arc::new(OllamaProvider::new(Some(base_url.to_string())))
    } else if is_google {
        Arc::new(GoogleProvider::new(
            api_key.to_string(),
            Some(base_url.to_string()),
        ))
    } else if is_anthropic {
        Arc::new(AnthropicProvider::new(
            api_key.to_string(),
            Some(base_url.to_string()),
        ))
    } else {
        Arc::new(OpenAiProvider::new(
            api_key.to_string(),
            Some(base_url.to_string()),
        ))
    }
}

// Builds one `RuntimeAgent` per `[[agents.list]]` entry: resolves its LLM,
// wires up its tool registry (filesystem/http/shell/browser/coding-agent,
// plus any WASM plugins) scoped to its workspace, and loads its persona.
fn build_runtime_agents(
    cfg: &config::AppConfig,
    server_state: &state::ServerState,
) -> anyhow::Result<(HashMap<String, Arc<RuntimeAgent>>, usize)> {
    let memory = server_state.memory.clone();
    let embedding_provider = server_state
        .embedding_provider
        .as_ref()
        .map(|p| p.0.clone());
    let cost_tracker = server_state.cost_tracker.clone();
    let calculator = Arc::new(CostCalculator::new());

    let mut agents = HashMap::new();
    let mut plugin_count = 0usize;
    for agent_cfg in &cfg.agents_list {
        let model_ref = agent_cfg
            .model
            .clone()
            .unwrap_or_else(|| cfg.default_model.clone());
        let (provider_name, preferred_model) =
            if let Some((prov, model)) = cfg.llm_models_map.get(&model_ref) {
                (prov.clone(), model.clone())
            } else {
                (cfg.llm_provider.clone(), model_ref)
            };
        let Some((base_url, api_key_env, model_id)) =
            config::resolve_llm(cfg, &provider_name, &preferred_model)
        else {
            warn!(agent_id = %agent_cfg.id, "Skipping agent with unresolved LLM config");
            continue;
        };
        let api_key = credentials_resolver::resolve_llm_api_key(&api_key_env);
        let llm = build_llm_provider(&base_url, &api_key, &api_key_env);
        let workspace = agent_cfg
            .workspace
            .clone()
            .unwrap_or_else(|| ".".to_string());

        let mut tool_registry = ToolRegistry::new();
        tool_registry.register(Box::new(FileSystemTool {
            workspace_root: workspace.clone(),
            policy: Some(FileSystemPolicy {
                deny_by_default: true,
                allowed_capabilities: vec![CapabilityId::FsRead, CapabilityId::FsWrite],
                read_roots: vec![workspace.clone()],
                write_roots: vec![workspace.clone()],
                list_roots: vec![workspace.clone()],
                search_roots: vec![workspace.clone()],
            }),
        }));
        tool_registry.register(Box::new(HttpTool::new(
            None,
            vec!["localhost".to_string(), "127.0.0.1".to_string()],
        )));
        tool_registry.register(Box::new(SystemTool::new(Vec::new())));
        tool_registry.register(Box::new(ShellTool {
            allowed_commands: None,
            denied_commands: shell::default_hard_deny_patterns(),
            working_dir: workspace.clone(),
            policy: Some(ShellPolicy {
                deny_by_default: true,
                allowed_capabilities: vec![
                    CapabilityId::ShellExecSafe,
                    CapabilityId::ShellExecPrivileged,
                ],
                approval_required_patterns: shell::default_approval_required_patterns(),
                hard_deny_patterns: shell::default_hard_deny_patterns(),
                allowed_cwd_roots: vec![workspace.clone()],
            }),
        }));
        // Same denylist as the HttpTool above: the browser can reach the whole internet from the
        // user's machine, so keep loopback out of reach.
        tool_registry.register(Box::new(BrowserTool::new(
            None,
            None,
            vec!["localhost".to_string(), "127.0.0.1".to_string()],
        )));
        tool_registry.register(Box::new(CodingAgentTool {
            working_dir: workspace.clone(),
            allowed_cwd_roots: vec![workspace.clone()],
            ..CodingAgentTool::default()
        }));
        if let Some(search) = WebSearchTool::from_env() {
            tool_registry.register(Box::new(search));
        }

        if cfg.wasm_sandbox_enabled {
            let host = Arc::new(WasmHost::new(SandboxConfig::default())?);
            let plugin_dir = config::expand_tilde_path("~/.consistent-world/plugins");
            let before = tool_registry.list_definitions().len();
            if let Err(e) = tool_registry.register_plugins_from_dir(host, &plugin_dir) {
                warn!(agent_id = %agent_cfg.id, error = %e, "Plugin registration skipped");
            }
            plugin_count += tool_registry
                .list_definitions()
                .len()
                .saturating_sub(before);
        }

        let tools = Arc::new(tool_registry);
        let mut agent = RuntimeAgent::new(
            agent_cfg.id.clone(),
            model_id,
            workspace.clone(),
            memory.clone(),
            llm,
            tools,
        )
        .with_tool_policy_provider(Arc::new(GatewayToolPolicyProvider {
            control_state: server_state
                .control_state
                .as_ref()
                .expect("control_state should be attached before runtime agent build")
                .clone(),
        }));
        if let Some(provider) = embedding_provider.clone() {
            agent = agent.with_embedding_provider(provider);
        }
        if let Some(tracker) = cost_tracker.clone() {
            agent = agent.with_cost_tracking(tracker, calculator.clone());
        }
        if let Some(persona_path) = &agent_cfg.persona_path {
            let path = Path::new(persona_path);
            if path.is_file()
                && let Ok(persona_text) = std::fs::read_to_string(path)
            {
                agent.persona = Some(persona_text);
            }
        } else {
            let workspace_path = Path::new(&workspace);
            if workspace_path.is_dir()
                && let Ok(persona) = Persona::load_from_workspace(workspace_path)
            {
                let prompt = persona.system_prompt();
                if !prompt.trim().is_empty() {
                    agent.persona = Some(prompt);
                }
            }
        }
        agents.insert(agent_cfg.id.clone(), Arc::new(agent));
    }
    Ok((agents, plugin_count))
}

/// Warn loudly when the configured LLM provider cannot actually answer.
///
/// Deliberately non-fatal: a gateway with no brain is still useful for
/// capture, review and approvals, and a hard failure would strand someone
/// mid-setup. But it must never be *silent*, which is what it was.
async fn check_configured_brain(cfg: &config::AppConfig) {
    let provider = cfg.llm_provider.trim().to_ascii_lowercase();
    let cli_kind = match provider.as_str() {
        "claude-code" => "claude",
        "codex" => "codex",
        // API-key and local providers fail loudly at call time with their own
        // errors; there is nothing useful to pre-flight here.
        _ => return,
    };

    let verification = agent_cli_probe::verify(cli_kind).await;
    if verification.ok {
        info!(provider = %provider, cli = cli_kind, "LLM ready");
        return;
    }

    let detail = verification
        .detail
        .or(verification.output)
        .unwrap_or_else(|| format!("`{cli_kind}` did not answer."));
    warn!(
        provider = %provider,
        cli = cli_kind,
        detail = %detail.trim(),
        "the configured LLM is not usable — the assistant will fall back to canned replies"
    );
    // Also on stderr in plain words: the person who just installed this is
    // reading the terminal, not the structured log.
    eprintln!();
    eprintln!("  ⚠  Consistent has no working brain yet.");
    eprintln!("     Provider `{provider}` needs the `{cli_kind}` CLI, and it answered:");
    for line in detail.trim().lines().take(4) {
        eprintln!("       {line}");
    }
    eprintln!();
    if cli_kind == "claude" {
        eprintln!(
            "     Fix: install it from https://claude.com/claude-code, then run `claude login`."
        );
    } else {
        eprintln!("     Fix: install the `{cli_kind}` CLI and sign in.");
    }
    eprintln!("     Or run `consistent-world --onboard` to pick a different provider");
    eprintln!("     (an API key, or a local model via Ollama).");
    eprintln!();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenv();
    let cli = Cli::parse();
    let config_path_arg = effective_config_path(&cli.config);

    if cli.onboard {
        let config_path = config::expand_tilde_path(&config_path_arg);
        wizard::run(config_path)?;
        return Ok(());
    }

    let open_browser = match &cli.command {
        Some(CliCommand::Start { open, offline }) => {
            config::set_offline(*offline);
            *open
        }
        Some(CliCommand::Credentials { .. }) => {
            let cmd = match &cli.command {
                Some(CliCommand::Credentials { cmd }) => cmd,
                _ => unreachable!(),
            };
            let store = KeyringStore::new("consistent-world");
            match cmd {
                CredentialsCmd::Set { key, value } => {
                    store.set(key, value)?;
                    println!("OK");
                }
                CredentialsCmd::Get { key } => {
                    let v = store.get(key)?;
                    println!("{v}");
                }
                CredentialsCmd::List => {
                    let keys = store.list_keys()?;
                    if keys.is_empty() {
                        eprintln!(
                            "Keyring backend does not support listing keys; use 'get <key>' for known keys."
                        );
                    }
                    for k in &keys {
                        println!("{k}");
                    }
                }
            }
            return Ok(());
        }
        Some(CliCommand::Remote { .. }) | Some(CliCommand::Pair) | Some(CliCommand::McpBrowser) => {
            false
        }
        None => false,
    };

    // Stdio MCP server: no config, no database, no server. It is spawned as a
    // child of a `claude` CLI run and speaks JSON-RPC on stdin/stdout, so it
    // must never print anything else to stdout.
    if matches!(cli.command, Some(CliCommand::McpBrowser)) {
        return mcp_browser::serve().await;
    }

    let config_path = config::ensure_config_file(&config_path_arg)?;
    let mut cfg = config::load(&config_path)?;
    if let Some(port_override) = cli.port {
        cfg.gateway_port = port_override;
    }

    match &cli.command {
        Some(CliCommand::Remote { cmd }) => {
            let bind = &cfg.gateway_bind;
            match cmd {
                RemoteCmd::Setup => match remote_doctor::run_setup(cfg.gateway_port, bind) {
                    Ok(status) => {
                        println!("Remote access is on.");
                        if let Some(url) = &status.serve_url {
                            println!(
                                "  From anywhere (your tailnet): {url}\n  Run `consistent-world pair` and scan the QR from the mobile app."
                            );
                        } else if let Some(url) = &status.recommended_url {
                            println!("  Same network only: {url}");
                        }
                    }
                    Err(e) => {
                        eprintln!("Remote setup did not finish: {e}");
                        std::process::exit(1);
                    }
                },
                RemoteCmd::Status => {
                    let status = remote_doctor::remote_status(cfg.gateway_port, bind);
                    println!("{}", serde_json::to_string_pretty(&status)?);
                }
            }
            return Ok(());
        }
        Some(CliCommand::Pair) => {
            if let Err(e) = pair_cli::run(&cfg, &config_path).await {
                eprintln!("{e}");
                std::process::exit(1);
            }
            return Ok(());
        }
        _ => {}
    }
    local_vault::ensure_workspace_layout("default").await;
    local_vault::mirror_runtime_environment(&config_path, &cfg).await;

    // Resolve gateway auth token from credentials store (never from TOML)
    if cfg.gateway_auth_token.is_none()
        && let Ok(Some(token)) = credentials_resolver::resolve_gateway_auth_token(&cfg)
    {
        cfg.gateway_auth_token = Some(token);
    }
    // Fail-closed / auto-provision: never run an open API. If no static token is
    // configured, decide based on the bind address (own-auth sessions still work
    // either way; this only guards the anonymous fallback):
    //   * loopback (127.0.0.1 / ::1 / localhost) — auto-generate and persist a local
    //     token next to the config so the desktop shell (and CLI) can pick it up. This
    //     keeps the "double-click and it just works" path while still requiring a Bearer.
    //   * anything else — refuse to start, so a network-exposed gateway can never be open.
    let bind = cfg.gateway_bind.trim().to_lowercase();
    let is_loopback =
        bind.is_empty() || bind == "127.0.0.1" || bind == "localhost" || bind == "::1";

    // Env override, so a container can opt in without a config file edit.
    if std::env::var("CONSISTENT_WORLD_STATIC_PRINCIPALS").as_deref() == Ok("1") {
        cfg.gateway_static_principals = true;
    }

    // A container binds 0.0.0.0 so its published port works, which used to
    // make a bare `docker run` refuse to start. The image opts in with
    // CONSISTENT_WORLD_PROVISION_TOKEN=1: the gateway generates its own
    // token and keeps it in the data volume. That only closes the anonymous
    // fallback; people sign in with their own accounts either way.
    let provision_allowed =
        is_loopback || std::env::var("CONSISTENT_WORLD_PROVISION_TOKEN").as_deref() == Ok("1");

    if cfg.gateway_auth_token.is_none() {
        if provision_allowed {
            match provision_local_auth_token(Path::new(&config_path)) {
                Ok(token) => {
                    if is_loopback {
                        info!(
                            "No gateway auth token configured; using auto-generated local token (saved next to {}). Loopback-only.",
                            config_path
                        );
                    } else {
                        // The container opt-in: the token never leaves the
                        // data volume, so whoever can read it can already read
                        // the database. Not for a shared, multi-user server.
                        // stderr: tracing is not initialised yet.
                        eprintln!(
                            "No gateway auth token configured; generated one in the data directory \
                             (CONSISTENT_WORLD_PROVISION_TOKEN=1). Publish the port on 127.0.0.1 \
                             only, or set your own token for a server other people reach."
                        );
                        cfg.gateway_static_principals = true;
                    }
                    cfg.gateway_auth_token = Some(token);
                }
                Err(e) => {
                    anyhow::bail!(
                        "Failed to provision a local gateway auth token: {e}. Set one explicitly with `consistent-world credentials set consistent_world.gateway.auth_token <token>`."
                    );
                }
            }
        } else {
            anyhow::bail!(
                "Refusing to start: gateway is bound to non-loopback address `{bind}` with no auth token configured — the anonymous fallback would leave the API open to the network. \
                 Fix one of: set a token (`consistent-world credentials set consistent_world.gateway.auth_token <token>`) or bind to 127.0.0.1. (Own-auth session logins work regardless.)"
            );
        }
    }

    // A static token is a shared secret that authenticates as a principal no
    // database ever vouched for. On loopback that is exactly what the desktop
    // sidecar needs. Exposed to a network it is a permanent skeleton key, and
    // every off-localhost deployment has one by construction (the guard above
    // requires it) — so that combination has to be chosen, not inherited.
    if !is_loopback && cfg.gateway_auth_token.is_some() && !cfg.gateway_static_principals {
        anyhow::bail!(
            "Refusing to start: gateway is bound to non-loopback address `{bind}` with a static auth token. \
             That token authenticates as a service principal with no account behind it, and `<secret>.<workspace>.<principal>` \
             would impersonate any principal in any workspace. If that is intended, set `static_principals = true` under \
             `[gateway.auth]` (or CONSISTENT_WORLD_STATIC_PRINCIPALS=1). Otherwise bind to 127.0.0.1 and have users sign in \
             with their own accounts."
        );
    }

    consistent_world_common::telemetry::init_tracing(&cfg.telemetry_log_level);

    let (memory_store, schedule_store, db, memory_backend) =
        startup::create_memory_and_schedule(&cfg).await?;

    let tenant_store = tenant_store::TenantStore::new(db.clone());
    if let Err(e) = tenant_store.init_schema().await {
        warn!(error = %e, "TenantStore schema init failed; first-signin bootstrap may misbehave");
    }
    let auth_store = auth_store::AuthStore::new(db.clone());
    if let Err(e) = auth_store.init_schema().await {
        warn!(error = %e, "AuthStore schema init failed; authentication may misbehave");
    }
    // Billing enforcement is opt-in: a self-hosted gateway never meters its own
    // owner, even when docker binds it to 0.0.0.0. A hosted deployment that sells
    // inference sets CONSISTENT_WORLD_BILLING_ENFORCE=1 on a non-loopback bind.
    // CONSISTENT_WORLD_MARKUP tunes the cost multiplier (1.0–3.0).
    let billing_enforce = billing::billing_enforced(
        &cfg.gateway_bind,
        std::env::var("CONSISTENT_WORLD_BILLING_ENFORCE")
            .ok()
            .as_deref(),
    );
    let markup = std::env::var("CONSISTENT_WORLD_MARKUP")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(2.0)
        .clamp(1.0, 3.0);
    let credit_ledger =
        billing::CreditLedger::new_with_surreal(markup, db.clone(), billing_enforce);
    if let Err(e) = credit_ledger.init_schema().await {
        warn!(error = %e, "CreditLedger schema init failed; billing may misbehave");
    }
    let default_wallet_balance = credit_ledger.balance_cents("default").await;
    local_vault::mirror_wallet_balance("default", default_wallet_balance, credit_ledger.markup)
        .await;

    let entity_persister = entity_store::SurrealEntityStore::new(db.clone());
    if let Err(e) = entity_persister.init_schema().await {
        warn!(error = %e, "SurrealEntityStore schema init failed; entity persistence disabled");
    }

    let director_persister = director_store::SurrealDirectorStore::new(db.clone());
    if let Err(e) = director_persister.init_schema().await {
        warn!(error = %e, "SurrealDirectorStore schema init failed; director persistence disabled");
    } else {
        let seed_path = std::env::var("CONSISTENT_WORLD_AGENTS_SEED")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("agents_seed.json"));
        match director_seed::seed_directors_if_absent(&director_persister, &seed_path).await {
            Ok(0) => {}
            Ok(n) => info!(inserted = n, path = %seed_path.display(), "seeded directors"),
            Err(e) => warn!(error = %e, path = %seed_path.display(), "director seeding failed"),
        }
    }

    let mission_persister = mission_store::SurrealMissionStore::new(db.clone());
    if let Err(e) = mission_persister.init_schema().await {
        warn!(error = %e, "SurrealMissionStore schema init failed; mission persistence disabled");
    }

    let heartbeat_persister = heartbeat_store::SurrealHeartbeatStore::new(db.clone());
    if let Err(e) = heartbeat_persister.init_schema().await {
        warn!(error = %e, "SurrealHeartbeatStore schema init failed; heartbeat persistence disabled");
    }

    let heartbeat_results_persister =
        heartbeat_results_store::SurrealHeartbeatResultsStore::new(db.clone());
    if let Err(e) = heartbeat_results_persister.init_schema().await {
        warn!(error = %e, "SurrealHeartbeatResultsStore schema init failed; heartbeat results disabled");
    }

    let director_lessons_persister =
        director_lessons_store::SurrealDirectorLessonsStore::new(db.clone());
    if let Err(e) = director_lessons_persister.init_schema().await {
        warn!(error = %e, "SurrealDirectorLessonsStore schema init failed; director lessons disabled");
    }

    // Entity knowledge docs: what an imported source leaves behind for every
    // chat turn and heartbeat (see `entity_knowledge`).
    let entity_docs_persister = entity_docs_store::SurrealEntityDocsStore::new(db.clone());
    if let Err(e) = entity_docs_persister.init_schema().await {
        warn!(error = %e, "SurrealEntityDocsStore schema init failed; entity knowledge docs disabled");
    }

    // The entity ledger: structured, sourced facts fed by API/browser/push
    // sources (see `entity_records_store`).
    let entity_records_persister = entity_records_store::SurrealEntityRecordsStore::new(db.clone());
    if let Err(e) = entity_records_persister.init_schema().await {
        warn!(error = %e, "SurrealEntityRecordsStore schema init failed; entity records disabled");
    }

    // Phase 1 (Build 3) — Bulletproof Architect shadow path.
    let audit_persister = consistent_world_architect::SurrealAuditStore::new(db.clone());
    if let Err(e) = audit_persister.init_schema().await {
        warn!(error = %e, "SurrealAuditStore schema init failed; architect audit trail disabled");
    }

    let feed_persister = feed_store::FeedStore::new(db.clone());
    if let Err(e) = feed_persister.init_schema().await {
        warn!(error = %e, "FeedStore schema init failed; feed persistence disabled");
    }

    let canvas_persister = canvas_store::CanvasStore::new(db.clone());
    if let Err(e) = canvas_persister.init_schema().await {
        warn!(error = %e, "CanvasStore schema init failed; canvas may misbehave");
    }

    let device_persister = device_store::SurrealDeviceStore::new(db.clone());
    if let Err(e) = device_persister.init_schema().await {
        warn!(error = %e, "SurrealDeviceStore schema init failed; device registration disabled");
    }

    // Outbound connectivity: workspaces can subscribe URLs to the operator
    // feed. A failed init means webhooks are simply unavailable (503 on the
    // CRUD routes, no fan-out) — never a boot failure.
    let webhook_persister = webhooks::SurrealWebhookStore::new(db.clone());
    let webhook_store = match webhook_persister.init_schema().await {
        Ok(()) => Some(webhook_persister),
        Err(e) => {
            warn!(error = %e, "SurrealWebhookStore schema init failed; outbound webhooks disabled");
            None
        }
    };

    let session_manager = Arc::new(Mutex::new(SessionManager::new(memory_store.clone())));
    {
        let mut guard = session_manager.lock().await;
        guard.load_from_disk().await?;
    }

    // Bare IPv6 hosts (e.g. `::` for dual-stack on a cloud host whose private
    // network and healthchecks are IPv6-only) need brackets to parse.
    let bind_host = if cfg.gateway_bind.contains(':') && !cfg.gateway_bind.starts_with('[') {
        format!("[{}]", cfg.gateway_bind)
    } else {
        cfg.gateway_bind.clone()
    };
    let bind_addr: SocketAddr = format!("{}:{}", bind_host, cfg.gateway_port)
        .parse()
        .with_context(|| {
            format!(
                "Invalid bind address {}:{}",
                cfg.gateway_bind, cfg.gateway_port
            )
        })?;

    info!(%bind_addr, memory_backend = %memory_backend, "Consistent gateway starting");

    let cost_tracker = Arc::new(std::sync::Mutex::new(
        consistent_world_common::telemetry::CostTracker::default(),
    ));
    let control_state_path = config::expand_tilde_path("~/.consistent-world/control_state.json");
    let control_snapshot_store =
        control_snapshot_store::SurrealControlSnapshotStore::new(db.clone());
    // Boot order: Surreal snapshot (survives container redeploys) → local
    // file (local dev) → fresh.
    let surreal_snapshot = match control_snapshot_store.load().await {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "Surreal control snapshot load failed; falling back to file");
            None
        }
    };
    let control_state = match surreal_snapshot {
        Some(snap) => {
            info!("Loaded control state from Surreal snapshot");
            Arc::new(state::ControlState::from_snapshot(snap))
        }
        None => match state::ControlState::load_from_path(&control_state_path).await {
            Ok(Some(loaded)) => {
                info!(path = %control_state_path, "Loaded control state from disk");
                Arc::new(loaded)
            }
            Ok(None) => Arc::new(state::ControlState::new(
                &cfg.llm_provider,
                &cfg.llm_base_url,
                &cfg.default_model,
            )),
            Err(e) => {
                warn!(path = %control_state_path, error = %e, "Could not load control state; using fresh");
                Arc::new(state::ControlState::new(
                    &cfg.llm_provider,
                    &cfg.llm_base_url,
                    &cfg.default_model,
                ))
            }
        },
    };
    control_state.attach_entity_store(entity_persister.clone());
    control_state.attach_feed_store(feed_persister.clone());
    control_state.attach_task_store(task_store::SurrealTaskStore::new(db.clone()));
    control_state.attach_mission_store(mission_persister.clone());
    if let Err(e) = control_state.hydrate_tasks_from_store().await {
        warn!(error = %e, "Surreal task hydration failed; continuing with in-memory tasks");
    }
    control_state.attach_heartbeat_store(heartbeat_persister.clone());
    control_state.attach_device_store(device_persister.clone());
    control_state.attach_directors(director_persister.clone());
    // Companion notifications: one dispatcher consuming operator-feed appends.
    // Attached before any loop starts so nothing fires into a closed channel.
    // With `[notifications] mobile_push = false` (the default) this stays
    // entirely local — devices poll the feed instead.
    control_state.attach_notification_sender(
        notify::NotificationDispatcher::new(
            device_persister.clone(),
            notify::NotificationConfig {
                mobile_push: cfg.notifications.mobile_push,
            },
            webhook_store.clone(),
        )
        .spawn(),
    );
    info!(
        mobile_push = cfg.notifications.mobile_push,
        "companion notification dispatcher started"
    );
    control_state.load_entities_seed_if_empty().await;
    if let Err(e) = control_state.hydrate_entities_from_store().await {
        warn!(error = %e, "Surreal entity hydration failed; continuing with in-memory state");
    }
    if let Err(e) = control_state.hydrate_missions_from_store().await {
        warn!(error = %e, "Mission hydration failed; continuing with snapshot state");
    }
    // Defaults floor for the pulse: any entity that ended up with no heartbeat
    // at all — seeded, predating the default plan, or created through a path
    // that couldn't reach the store — gets its rhythm here. Entities that
    // already have beats (including ones the user edited) are left untouched.
    {
        let entities = control_state.entities.read().await.clone();
        match entity_heartbeat_defaults::backfill_default_heartbeats(
            heartbeat_persister.as_ref(),
            &entities,
        )
        .await
        {
            Ok(0) => {}
            Ok(n) => info!(
                beats = n,
                "default heartbeats backfilled for existing entities"
            ),
            Err(e) => {
                warn!(error = %e, "default heartbeat backfill failed; entities keep their current beats")
            }
        }
    }
    control_state.load_agents_seed_if_empty().await;
    control_state.ensure_director_states_synced().await;
    let (recovered, recovery_failed) = control_state
        .recover_after_restart(retry::RetryPolicy::default())
        .await;
    if recovered > 0 || recovery_failed > 0 {
        info!(
            requeued = recovered,
            failed = recovery_failed,
            "startup recovery: interrupted tasks recovered"
        );
    }
    // Claude Code sessions that were mid-flight keep their session id, so they
    // are resumed rather than failed — this is what lets a multi-day job
    // survive a gateway restart.
    let resumed = agent_sessions::resume_interrupted(control_state.clone()).await;
    // Runs that died before reporting a session cannot be resumed; hand their
    // tasks back rather than leaving them InProgress forever.
    let released = agent_sessions::release_unresumable_runs(control_state.clone()).await;
    if released > 0 {
        info!(released, "released tasks whose runs could not be resumed");
    }
    if resumed > 0 {
        info!(
            sessions = resumed,
            "startup recovery: resuming agent sessions"
        );
    }
    local_vault::mirror_raw_captures_from_state(&control_state).await;
    local_vault::mirror_external_agent_runs_from_state(&control_state).await;
    // Best-effort operator alerting (Slack-compatible webhook) for failures
    // that would otherwise only show up in logs nobody is watching. Read before
    // the first background loop so every supervised loop can report through it.
    let alert_config = alerts::AlertConfig::from_env();
    if alert_config.is_some() {
        info!("Operator alerting enabled (ALERT_WEBHOOK_URL)");
    }

    // Every background loop in this function runs under
    // `supervisor::spawn_supervised` rather than a bare `tokio::spawn` whose
    // handle gets dropped. Tokio ends a panicked task silently, so before this a
    // single panic stopped that subsystem for the life of the process while
    // `/health` kept answering 200 — a dead loop was indistinguishable from an
    // idle one. The registry records restarts; `/health` reports the counts.
    let subsystems = supervisor::SubsystemRegistry::new();

    let control_state_for_persist = control_state.clone();
    let persist_path = control_state_path.clone();
    let snapshot_store_for_persist = control_snapshot_store.clone();
    // Losing this loop means nothing is written to disk or Surreal again, with no
    // symptom until the next restart reads a stale snapshot.
    let persist_handle = supervisor::spawn_supervised(
        "control_state_persist",
        subsystems.clone(),
        alert_config.clone(),
        move || {
            let control_state_for_persist = control_state_for_persist.clone();
            let persist_path = persist_path.clone();
            let snapshot_store_for_persist = snapshot_store_for_persist.clone();
            async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    interval.tick().await;
                    if let Err(e) = control_state_for_persist.save_to_path(&persist_path).await {
                        tracing::warn!(path = %persist_path, error = %e, "Control state persist failed");
                    }
                    let snap = control_state_for_persist.snapshot().await;
                    if let Err(e) = snapshot_store_for_persist.save(&snap).await {
                        tracing::warn!(error = %e, "Surreal control snapshot persist failed");
                    }
                    local_vault::mirror_raw_captures_from_state(&control_state_for_persist).await;
                    local_vault::mirror_external_agent_runs_from_state(&control_state_for_persist)
                        .await;
                    if let Err(e) = control_state_for_persist.sync_entities_to_store().await {
                        tracing::warn!(error = %e, "Surreal entity sync failed");
                    }
                    if let Err(e) = control_state_for_persist.sync_tasks_to_store().await {
                        tracing::warn!(error = %e, "Surreal task sync failed");
                    }
                }
            }
        },
    );
    let memory_for_orchestrator = memory_store.clone();
    let memory_for_architect = memory_store.clone();

    // Per-user credential vaults. Unlocked vaults hold key material in memory, so
    // a background sweeper re-locks anything left idle past the timeout.
    let vault_manager = Arc::new(vault_manager::VaultManager::new(std::path::PathBuf::from(
        config::expand_tilde_path("~/.consistent-world/vaults"),
    )));
    // Nominate the vault that unattended agent runs resolve credentials from.
    // A scheduled task has no request and so no principal; rather than guess at
    // one (which in a shared deployment would mean serving another tenant's
    // keys), this is opt-in. Unset leaves resolution exactly as it was.
    if let Ok(principal) = std::env::var("CONSISTENT_WORLD_AGENT_VAULT_PRINCIPAL") {
        let principal = principal.trim().to_string();
        if !principal.is_empty() {
            tracing::info!(
                principal = %principal,
                "agent runs will resolve credentials from this principal's vault when unlocked"
            );
            credentials_resolver::register_agent_vault(vault_manager.clone(), principal);
        }
    }
    {
        let vaults = vault_manager.clone();
        // Supervised: if this dies, vaults unlocked for one request stay unlocked
        // in memory for the life of the process. A silent failure here is a
        // security regression, not a missed convenience.
        let _vault_sweeper_handle = supervisor::spawn_supervised(
            "vault_sweeper",
            subsystems.clone(),
            alert_config.clone(),
            move || {
                let vaults = vaults.clone();
                async move {
                    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                    loop {
                        ticker.tick().await;
                        let locked = vaults.sweep();
                        if locked > 0 {
                            tracing::debug!(locked, "auto-locked idle credential vaults");
                        }
                    }
                }
            },
        );
    }

    let mut server_state = state::ServerState::new(
        env!("CARGO_PKG_VERSION").to_string(),
        cfg.gateway_auth_token.clone(),
        memory_store,
    )
    .with_cost_tracker(cost_tracker)
    .with_memory_backend(memory_backend)
    .with_control_state(control_state.clone())
    .with_app_config(Arc::new(cfg.clone()))
    .with_tenant_store(tenant_store.clone())
    .with_auth_store(auth_store.clone())
    .with_credit_ledger(credit_ledger.clone())
    .with_mission_store(mission_persister.clone())
    .with_director_store(director_persister.clone())
    .with_director_lessons_store(director_lessons_persister.clone())
    .with_entity_docs_store(entity_docs_persister.clone())
    .with_entity_records_store(entity_records_persister.clone())
    .with_heartbeat_results_store(heartbeat_results_persister.clone())
    .with_heartbeat_store(heartbeat_persister.clone())
    .with_canvas_store(canvas_persister.clone())
    .with_vault_manager(vault_manager.clone())
    .with_skills(Arc::new(skills::SkillLibrary::new(
        skills::SkillLibrary::default_root(),
    )))
    .with_audit_store(audit_persister.clone())
    .with_architect({
        use std::sync::Arc;
        let sources = consistent_world_architect::SnapshotSources {
            entities: Some(Arc::new(architect_bindings::ControlStateEntityReader {
                control: control_state.clone(),
            })),
            missions: Some(Arc::new(architect_bindings::ControlStateMissionsReader {
                control: control_state.clone(),
                mission_store: Some(mission_persister.clone()),
            })),
            memories: Some(Arc::new(architect_bindings::EntityMemoryReader {
                memory: memory_for_architect,
            })),
            lessons: Some(Arc::new(architect_bindings::DirectorLessonsReader {
                store: director_lessons_persister.clone(),
                director_id: "operations".to_string(),
            })),
            docs: Some(Arc::new(architect_bindings::EntityDocsReader {
                store: entity_docs_persister.clone(),
            })),
        };
        Arc::new(consistent_world_architect::Architect::new(
            sources,
            audit_persister.clone(),
        ))
    });

    // Passkey (WebAuthn). RP id/origin from env; defaults suit localhost dev.
    // Prod must set PASSKEY_RP_ID to the associated domain + a matching origin.
    {
        let rp_id = std::env::var("PASSKEY_RP_ID").unwrap_or_else(|_| "localhost".to_string());
        let rp_origin = std::env::var("PASSKEY_RP_ORIGIN")
            .unwrap_or_else(|_| "http://localhost:18800".to_string());
        match passkey::PasskeyService::new(&rp_id, &rp_origin) {
            Ok(svc) => {
                server_state = server_state.with_passkey(svc);
                info!(rp_id = %rp_id, "Passkey (WebAuthn) enabled");
            }
            Err(e) => warn!(error = %e, "Passkey disabled (invalid RP config)"),
        }
    }

    // Local files: the only path from chat to the user's actual disk.
    //
    // Loopback only. On a network-bound gateway "your Desktop" is a server's
    // disk nobody will look at, so the tools are simply absent rather than
    // present-and-refusing. Roots are configurable; the defaults are the two
    // places a person expects — the Desktop they can see, and one folder for
    // everything else.
    if billing::is_loopback_bind(&cfg.gateway_bind) {
        match dirs::home_dir() {
            Some(home) => {
                let mut files = local_files::LocalFiles::new(
                    cfg.local_notes_root
                        .clone()
                        .map(|p| config::expand_tilde_path(&p).into())
                        .unwrap_or_else(|| home.join("Desktop")),
                    cfg.local_projects_root
                        .clone()
                        .map(|p| config::expand_tilde_path(&p).into())
                        .unwrap_or_else(|| home.join("Consistent")),
                );
                match files.ensure_roots() {
                    Ok(()) => {
                        info!(
                            notes = %files.notes_root.display(),
                            projects = %files.projects_root.display(),
                            "Local file tools enabled"
                        );
                        server_state = server_state.with_local_files(Arc::new(files));
                    }
                    Err(e) => warn!(error = %e, "Local file tools disabled (roots unusable)"),
                }
            }
            None => warn!("Local file tools disabled (no home directory)"),
        }
    }

    // External identity, if a deployment configured one. Strictly additive:
    // absent — which is every self-hosted instance — local accounts are the
    // only way in, and nothing about this path exists at runtime.
    if let Some(external) = cfg.gateway_external_identity.clone() {
        info!(
            issuer = %external.issuer,
            jwks = %external.jwks_url,
            audience = external.audience.as_deref().unwrap_or("(any)"),
            "External identity enabled alongside local accounts"
        );
        server_state = server_state
            .with_external_identity(Arc::new(external_identity::ExternalVerifier::new(external)));
    }

    if cfg.vector_search_enabled {
        // Local Ollama first, hosted keys after, and never a silent switch of
        // the vector space the database already holds (embedding_setup.rs).
        match embedding_setup::select(&cfg, Some(&db)).await {
            Some((label, provider)) => {
                server_state = server_state.with_embedding_provider(provider);
                info!(
                    provider = %label,
                    "Embedding provider enabled (semantic recall)"
                );
            }
            None => {
                warn!(
                    "vector_search enabled but no embedding provider is usable; semantic recall \
                     disabled. Install Ollama for local recall, or add an OpenAI / Gemini key"
                );
                let alerts_for_embedding = alert_config.clone();
                tokio::spawn(async move {
                    alerts::notify_failure(
                        alerts_for_embedding.as_ref(),
                        "embedding provider startup probe",
                        "no candidate passed probe; semantic recall disabled for this process",
                    )
                    .await;
                });
            }
        }
    }

    let (runtime_agents, plugin_count) = build_runtime_agents(&cfg, &server_state)?;
    if runtime_agents.is_empty() {
        warn!("No runtime agents configured in [agents.list]; channel runtime will stay idle");
    }
    let default_agent = cfg
        .agents_list
        .first()
        .map(|a| a.id.clone())
        .unwrap_or_else(|| "default".to_string());
    let agent_router = build_agent_router(&cfg.routing_rules, default_agent);

    let agent_ids: Vec<String> = runtime_agents.keys().cloned().collect();
    server_state = server_state.with_runtime_status(state::RuntimeStatus {
        messaging_enabled: !agent_ids.is_empty(),
        adapters: vec!["web".to_string()],
        agent_ids,
        plugin_count,
    });

    let runtime = AgentRuntime::new(agent_router, runtime_agents, Some(control_state.clone()));
    let schedule_dispatcher: Arc<dyn scheduler::ScheduleDispatcher> =
        Arc::new(RuntimeScheduleDispatcher {
            runtime: runtime.clone(),
        });
    let scheduler_instance = Arc::new(scheduler::Scheduler::new(
        schedule_store,
        schedule_dispatcher,
        60,
    ));
    server_state = server_state.with_scheduler(scheduler_instance.clone());

    server_state = server_state.with_subsystems(subsystems.clone());

    let scheduler_handle = supervisor::spawn_supervised(
        "scheduler",
        subsystems.clone(),
        alert_config.clone(),
        move || {
            let s = scheduler_instance.clone();
            async move {
                s.run().await;
            }
        },
    );

    let orchestrator_handle =
        supervisor::spawn_supervised("orchestrator", subsystems.clone(), alert_config.clone(), {
            let control = control_state.clone();
            let cfg = cfg.clone();
            let mem = memory_for_orchestrator;
            move || {
                let control = control.clone();
                let cfg = cfg.clone();
                let mem = mem.clone();
                async move {
                    orchestrator::run_loop(control, &cfg, Some(mem)).await;
                }
            }
        });

    // Phase 9 (Build 1): spawn the heartbeat ticker with the chat-result
    // dispatcher. Build 2 / Phase 2 swaps the dispatcher's canned summary for a
    // real `LlmDirectorRunner` call (with fallback to the canned text on any
    // error), debiting the same cost ledger the chat path uses.
    let director_runner = director_runner::LlmDirectorRunner::new_with_docs(
        Arc::new(cfg.clone()),
        control_state.clone(),
        Some(entity_docs_persister.clone()),
    );
    let heartbeat_dispatcher_impl = heartbeat_dispatcher::ChatHeartbeatDispatcher::new(
        heartbeat_results_persister.clone(),
        director_persister.clone(),
        mission_persister.clone(),
        director_lessons_persister.clone(),
        control_state.clone(),
        director_runner,
        Some(credit_ledger.clone()),
        server_state
            .embedding_provider
            .as_ref()
            .map(|p| p.0.clone()),
    );
    // Optional relay link: when the operator has pointed this gateway at a
    // relay, dial out so the phone can reach it from anywhere. Opt-in by
    // environment; absent it, nothing changes and the gateway stays local.
    if let Some(relay_cfg) = relay_client::RelayClientConfig::from_env(cfg.gateway_port) {
        info!(
            gateway_id = %relay_cfg.gateway_id,
            url = %relay_cfg.link_url(),
            "relay link enabled"
        );
        let _relay_handle = supervisor::spawn_supervised(
            "relay_client",
            subsystems.clone(),
            alert_config.clone(),
            move || {
                let relay_cfg = relay_cfg.clone();
                async move {
                    relay_client::run_loop(relay_cfg).await;
                }
            },
        );
    }

    // Hold the machine awake while work is in flight, so a closed lid can't
    // silently strand an approved action or a run the phone is waiting on.
    let _power_handle =
        supervisor::spawn_supervised("power", subsystems.clone(), alert_config.clone(), {
            let control = control_state.clone();
            move || {
                let control = control.clone();
                async move {
                    power::run_loop(control, 60).await;
                }
            }
        });

    // Drive beats reach the mission replanner through the server state.
    let heartbeat_dispatcher_impl =
        heartbeat_dispatcher_impl.with_server_state(server_state.clone());
    let heartbeat_ticker = Arc::new(heartbeat_store::HeartbeatTicker::new(
        heartbeat_persister.clone(),
        heartbeat_dispatcher_impl,
        30,
    ));
    let _heartbeat_ticker_handle = supervisor::spawn_supervised(
        "heartbeat_ticker",
        subsystems.clone(),
        alert_config.clone(),
        move || {
            let t = heartbeat_ticker.clone();
            async move {
                t.run().await;
            }
        },
    );

    // Off-box daily backups: SurrealDB export → S3 bucket, when configured.
    match backup::BackupConfig::from_env() {
        Some(s3) => {
            backup::spawn_daily(Arc::new(cfg.clone()), s3, alert_config.clone());
            info!("Daily database backups enabled (BACKUP_S3_*)");
        }
        None => info!("BACKUP_S3_* not configured; off-box backups disabled"),
    }

    // Retry embedding lessons stuck at `embedding: None` from a past provider
    // outage — a no-op (returns immediately) if no provider is enabled now.
    lessons::spawn_embedding_backfill_sweep(
        director_lessons_persister.clone(),
        server_state
            .embedding_provider
            .as_ref()
            .map(|p| p.0.clone()),
    );

    // Reminders: fire chat-created reminders (task_kind "reminder" with a
    // resolved due_at) into the operator feed.
    // Mission engines: wake engaged missions on their cadence and let Claude
    // Code push them forward while the operator is away.
    let mission_engine_ticker = mission_engine::MissionEngineTicker::with_skills(
        control_state.clone(),
        60,
        server_state.skills.clone(),
    );
    let _mission_engine_handle = supervisor::spawn_supervised(
        "mission_engine",
        subsystems.clone(),
        alert_config.clone(),
        move || {
            let t = mission_engine_ticker.clone();
            async move {
                t.run().await;
            }
        },
    );

    // Task executor: run the tasks the planner marked as plain code/file work,
    // unattended, inside the owning entity's directory jail. One at a time —
    // it shares the operator's single Claude window with the mission engines.
    let task_executor_ticker = task_executor::TaskExecutorTicker::with_skills(
        control_state.clone(),
        60,
        server_state.skills.clone(),
    );
    let _task_executor_handle = supervisor::spawn_supervised(
        "task_executor",
        subsystems.clone(),
        alert_config.clone(),
        move || {
            let t = task_executor_ticker.clone();
            async move {
                t.run().await;
            }
        },
    );

    let reminder_ticker = reminders::ReminderTicker::new(control_state.clone(), 20);
    let _reminder_ticker_handle = supervisor::spawn_supervised(
        "reminders",
        subsystems.clone(),
        alert_config.clone(),
        move || {
            let t = reminder_ticker.clone();
            async move {
                t.run().await;
            }
        },
    );

    // Accountability: notice slipped deadlines, escalate once, and brief the
    // operator each morning. Five minutes is fine — nothing here is urgent to
    // the second, and a slow poll keeps the write lock contention low.
    let accountability_ticker =
        accountability::AccountabilityTicker::new(control_state.clone(), 300);
    let _accountability_ticker_handle = supervisor::spawn_supervised(
        "accountability",
        subsystems.clone(),
        alert_config.clone(),
        move || {
            let t = accountability_ticker.clone();
            async move {
                t.run().await;
            }
        },
    );

    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("Failed to bind {bind_addr}"))?;

    if open_browser {
        let port = cfg.gateway_port;
        let url = format!("http://127.0.0.1:{port}/");
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            if let Err(e) = open_url_in_browser(&url) {
                warn!("Failed to open browser: {e}");
            }
        });
    }

    // Does this install actually have a brain?
    //
    // A CLI provider that is installed but not signed in fails every call with
    // an opaque `api_error`, and the product degrades to canned heartbeats and
    // template plans with nothing but a log line to say why. That reads as
    // "the software is broken" to someone who just downloaded it. So check it
    // once, out loud, at startup — `agent_cli_probe::verify` runs the CLI for
    // real and passes the vendor's own instructions through untouched.
    //
    // In the background: the probe runs a real completion, which needs the
    // network and can take many seconds. Nothing waits on its verdict, so it
    // must never delay the port opening (the desktop shell gives up waiting).
    {
        let cfg = cfg.clone();
        tokio::spawn(async move { check_configured_brain(&cfg).await });
    }

    let router = server::build_router_with_webhooks(server_state, webhook_store);
    let shutdown_sessions = session_manager.clone();
    let control_state_final = control_state;
    let control_state_path_final = control_state_path;
    let snapshot_store_final = control_snapshot_store.clone();

    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        // Docker (and most orchestrators) stop containers with SIGTERM;
        // Ctrl-C covers local runs. Whichever arrives first wins.
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                r = tokio::signal::ctrl_c() => {
                    if let Err(err) = r {
                        warn!("Failed to listen for Ctrl-C: {err}");
                    }
                }
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        if let Err(err) = tokio::signal::ctrl_c().await {
            warn!("Failed to listen for Ctrl-C: {err}");
        }
        persist_handle.abort();
        scheduler_handle.abort();
        orchestrator_handle.abort();
        if let Err(e) = control_state_final
            .save_to_path(&control_state_path_final)
            .await
        {
            warn!(error = %e, "Failed to persist control state on shutdown");
        }
        let snap = control_state_final.snapshot().await;
        if let Err(e) = snapshot_store_final.save(&snap).await {
            warn!(error = %e, "Failed to persist control snapshot to Surreal on shutdown");
        }
        if let Err(e) = control_state_final.sync_entities_to_store().await {
            warn!(error = %e, "Failed to flush entities to Surreal on shutdown");
        }
        if let Err(e) = control_state_final.sync_tasks_to_store().await {
            warn!(error = %e, "Failed to flush tasks to Surreal on shutdown");
        }
        if let Err(err) = shutdown_sessions.lock().await.persist_all().await {
            warn!("Failed to persist sessions on shutdown: {err:?}");
        }
        info!("Graceful shutdown: control state flushed");
    })
    .await?;

    info!("Consistent gateway shut down cleanly");
    Ok(())
}

/// Open a URL in the default browser (macOS: open, Windows: start, Linux: xdg-open).
fn open_url_in_browser(url: &str) -> std::io::Result<()> {
    let (cmd, args) = if cfg!(target_os = "windows") {
        ("cmd", vec!["/C", "start", "", url])
    } else if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else {
        ("xdg-open", vec![url])
    };
    let mut child = consistent_world_common::platform::std_command(cmd)
        .args(&args)
        .spawn()?;
    let _ = child.wait();
    Ok(())
}
