//! Onboarding wizard: interactive CLI to set up Consistent CEO in under 3 minutes.
//! Run with `consistent-world --onboard`.
//!
//! Walks the operator through picking an LLM provider, storing secrets in the
//! OS keyring (or an optional encrypted vault), and writing the resulting
//! `consistent_world.toml`. Ends with a quick connectivity check.

use anyhow::{Context, Result};
use consistent_world_credentials::keyring::KeyringStore;
use consistent_world_credentials::vault::EncryptedVault;
use dialoguer::{Confirm, Input, Password, Select};
use std::fs;
use std::path::Path;
use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const LLM_KEYRING_KEY: &str = "consistent_world.llm.api_key";
const GATEWAY_AUTH_KEYRING_KEY: &str = "consistent_world.gateway.auth_token";

/// Wizard outcome: non-secret config to write to consistent_world.toml and credential keys we stored.
pub struct WizardResult {
    pub config_path: String,
    pub llm_provider: String,
    pub llm_base_url: String,
    pub llm_api_key_env: String,
    pub default_model: String,
    pub gateway_port: u16,
    pub surreal_url: String,
    pub surreal_ns: String,
    pub surreal_db: String,
}

/// Run the interactive onboarding wizard. Returns config to write and stores secrets in keyring.
pub fn run(config_path: String) -> Result<WizardResult> {
    let console = console::Term::stderr();
    console.clear_screen()?;

    eprintln!("╔══════════════════════════════════════════════════════════╗");
    eprintln!("║  Consistent CEO — Onboarding Wizard  v{VERSION:<17}  ║");
    eprintln!("╚══════════════════════════════════════════════════════════╝");
    eprintln!();
    eprintln!("This wizard will create a config file and store secrets in your");
    eprintln!("system keychain. No secrets are written to the config file.");
    eprintln!();

    if !Confirm::new()
        .with_prompt("Continue?")
        .default(true)
        .interact_on(&console)?
    {
        anyhow::bail!("Onboarding cancelled.");
    }

    // Keyless options first: they are the ones that need nothing beyond what
    // is already installed on the machine.
    let llm_choice = Select::new()
        .with_prompt("Choose LLM provider")
        .items(&[
            "Claude Code — your Claude subscription via the `claude` CLI (no API key)",
            "Codex — your ChatGPT subscription via the `codex` CLI (no API key)",
            "Ollama (local, no API key)",
            "Anthropic API (Claude, with an API key)",
            "OpenAI API (or OpenRouter, Azure OpenAI)",
            "Other (OpenAI-compatible API)",
        ])
        .default(0)
        .interact_on(&console)?;

    // `keyless` providers skip the base-URL/env/API-key prompts entirely.
    let (llm_provider, default_base_url, default_env_key, default_model, keyless) = match llm_choice
    {
        0 => (
            "claude-code".to_string(),
            "claude-code://".to_string(),
            "CLAUDE_CODE_CLI".to_string(),
            "opus".to_string(),
            true,
        ),
        1 => (
            "codex".to_string(),
            "codex://".to_string(),
            "CODEX_CLI".to_string(),
            String::new(), // empty = whatever the CLI defaults to
            true,
        ),
        2 => (
            "ollama".to_string(),
            "http://localhost:11434".to_string(),
            "OLLAMA_API_KEY".to_string(),
            "llama3.2".to_string(),
            true,
        ),
        3 => (
            "anthropic".to_string(),
            "https://api.anthropic.com".to_string(),
            "ANTHROPIC_API_KEY".to_string(),
            "claude-sonnet-4".to_string(),
            false,
        ),
        4 => (
            "openai".to_string(),
            "https://api.openai.com/v1".to_string(),
            "OPENAI_API_KEY".to_string(),
            "gpt-4o".to_string(),
            false,
        ),
        _ => (
            "openai".to_string(),
            "https://api.openai.com/v1".to_string(),
            "OPENAI_API_KEY".to_string(),
            "gpt-4o".to_string(),
            false,
        ),
    };

    if llm_provider == "claude-code" && which_cli("claude").is_none() {
        eprintln!(
            "  ⚠ `claude` CLI not found on PATH — install it (https://claude.com/claude-code)"
        );
        eprintln!("    or the gateway will fall back to Ollama at runtime.");
    }
    if llm_provider == "codex" && which_cli("codex").is_none() {
        eprintln!("  ⚠ `codex` CLI not found on PATH — install it or the gateway will fall back.");
    }

    let store = KeyringStore::new("consistent-world");
    let (llm_base_url, llm_api_key_env, api_key) = if keyless {
        // Ollama still benefits from a configurable host; the CLI providers
        // have nothing to configure.
        if llm_provider == "ollama" {
            let url: String = Input::new()
                .with_prompt("Ollama base URL")
                .with_initial_text(&default_base_url)
                .interact_on(&console)?;
            (url, default_env_key, String::new())
        } else {
            (default_base_url, default_env_key, String::new())
        }
    } else {
        let llm_base_url: String = Input::new()
            .with_prompt("LLM API base URL")
            .with_initial_text(&default_base_url)
            .interact_on(&console)?;

        let llm_api_key_env: String = Input::new()
            .with_prompt("Environment variable name for API key")
            .with_initial_text(&default_env_key)
            .interact_on(&console)?;

        let api_key = Password::new()
            .with_prompt("API key (will be stored in keychain)")
            .interact_on(&console)?;

        store
            .set(LLM_KEYRING_KEY, &api_key)
            .context("Store LLM API key in keyring")?;
        eprintln!("  → Stored in keyring as '{LLM_KEYRING_KEY}'");
        (llm_base_url, llm_api_key_env, api_key)
    };

    let default_model: String = if llm_provider == "claude-code" {
        let idx = Select::new()
            .with_prompt("Claude model")
            .items(&["opus (strongest)", "sonnet (faster)", "haiku (fastest)"])
            .default(0)
            .interact_on(&console)?;
        ["opus", "sonnet", "haiku"][idx].to_string()
    } else if llm_provider == "codex" {
        // Whatever the CLI defaults to; nothing to ask.
        default_model
    } else if llm_provider == "ollama" {
        match fetch_ollama_models(&llm_base_url) {
            Ok(ref names) if !names.is_empty() => {
                let mut items: Vec<String> = names.clone();
                items.push("Other (type model name)".to_string());
                let idx = Select::new()
                    .with_prompt("Default agent model (from Ollama)")
                    .items(&items)
                    .default(0)
                    .interact_on(&console)?;
                if idx == items.len() - 1 {
                    Input::new()
                        .with_prompt("Model name")
                        .with_initial_text(&default_model)
                        .interact_on(&console)?
                } else {
                    items[idx].clone()
                }
            }
            _ => {
                eprintln!("  (Ollama not reachable or no models; enter model name)");
                Input::new()
                    .with_prompt("Default agent model")
                    .with_initial_text(&default_model)
                    .interact_on(&console)?
            }
        }
    } else {
        Input::new()
            .with_prompt("Default agent model")
            .with_initial_text(&default_model)
            .interact_on(&console)?
    };

    eprintln!();
    let want_gateway_auth = Confirm::new()
        .with_prompt("Protect API with a Bearer token? (recommended if exposed)")
        .default(true)
        .interact_on(&console)?;

    if want_gateway_auth {
        let auth_token = Password::new()
            .with_prompt("Gateway auth token (Bearer)")
            .interact_on(&console)?;
        store
            .set(GATEWAY_AUTH_KEYRING_KEY, &auth_token)
            .context("Store gateway auth token in keyring")?;
        eprintln!("  → Stored in keyring as '{GATEWAY_AUTH_KEYRING_KEY}'");
    }

    let (credentials_backend, vault_path) = {
        let want_vault = Confirm::new()
            .with_prompt("Use a vault with a master password (passkey) for credentials? (Secrets from this run stay in keyring; vault can be used after next run.)")
            .default(false)
            .interact_on(&console)?;
        if want_vault {
            let vault_path = expand_tilde("~/.consistent-world/vault");
            if !Path::new(&vault_path).exists() {
                let password = Password::new()
                    .with_prompt("Vault master password")
                    .with_confirmation("Confirm master password", "Passwords do not match")
                    .allow_empty_password(false)
                    .interact_on(&console)?;
                let vault = EncryptedVault::new(std::path::PathBuf::from(&vault_path));
                if let Some(parent) = Path::new(&vault_path).parent() {
                    fs::create_dir_all(parent).context("Create vault directory")?;
                }
                vault.init(&password).context("Initialize vault")?;
                eprintln!("  → Vault created at {}", vault_path);
            } else {
                eprintln!("  → Vault already exists at {}", vault_path);
            }
            ("vault".to_string(), Some(vault_path))
        } else {
            ("keyring".to_string(), None)
        }
    };

    let surreal_url = "ws://127.0.0.1:8000".to_string();
    let surreal_ns = "consistent-world".to_string();
    let surreal_db = "main".to_string();

    eprintln!();
    let want_persona = Confirm::new()
        .with_prompt("Create a default persona (SOUL.md) and edit it now?")
        .default(false)
        .interact_on(&console)?;

    if want_persona {
        let workspace = expand_tilde("~/.consistent-world/workspace/default");
        fs::create_dir_all(&workspace).context("Create workspace dir")?;
        let soul_path = format!("{workspace}/SOUL.md");
        let default_soul = "# Agent persona\n\nYou are a helpful assistant.\n";
        if !Path::new(&soul_path).exists() {
            fs::write(&soul_path, default_soul).context("Write SOUL.md")?;
        }
        let fallback_editor = if cfg!(windows) { "notepad" } else { "nano" };
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| fallback_editor.to_string());
        let _ = Command::new(editor.split_whitespace().next().unwrap_or(fallback_editor))
            .arg(&soul_path)
            .status();
        eprintln!("  → Persona at {soul_path}");
    }

    let config_dir = Path::new(&config_path)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    let toml = build_toml(
        &llm_provider,
        &llm_base_url,
        &llm_api_key_env,
        &default_model,
        want_gateway_auth,
        &credentials_backend,
        vault_path.as_deref(),
        18800,
        &surreal_url,
        &surreal_ns,
        &surreal_db,
    );
    fs::create_dir_all(&config_dir).ok();
    fs::write(&config_path, &toml).context("Write consistent_world.toml")?;
    eprintln!();
    eprintln!("  → Wrote config to {}", config_path);

    eprintln!();
    eprintln!("Testing connectivity...");
    let llm_ok = test_llm(&llm_base_url, &api_key, &llm_provider, &default_model);
    if llm_ok {
        eprintln!("  ✓ LLM API reachable");
    } else {
        eprintln!("  ✗ LLM API check failed (wrong URL or key?)");
    }
    report_coding_agents();

    eprintln!();
    eprintln!("═══════════════════════════════════════════════════════════");
    eprintln!("  Setup complete. Start the gateway:");
    eprintln!();
    if !keyless {
        eprintln!(
            "    export {}=$(consistent-world credentials get {})",
            llm_api_key_env, LLM_KEYRING_KEY
        );
    }
    if want_gateway_auth {
        eprintln!(
            "    export CONSISTENT_WORLD_AUTH=$(consistent-world credentials get {})",
            GATEWAY_AUTH_KEYRING_KEY
        );
        eprintln!("  (Use CONSISTENT_WORLD_AUTH in your API requests as Bearer token.)");
    }
    eprintln!();
    eprintln!("    consistent-world --config {}", config_path);
    eprintln!();
    eprintln!("  For production: run as a service (systemd, launchd) and set");
    eprintln!("  the env vars above in the service environment.");
    eprintln!("═══════════════════════════════════════════════════════════");

    Ok(WizardResult {
        config_path,
        llm_provider,
        llm_base_url,
        llm_api_key_env,
        default_model,
        gateway_port: 18800,
        surreal_url,
        surreal_ns,
        surreal_db,
    })
}

// Expands a leading `~/` to the home directory; any other path is returned unchanged.
fn expand_tilde(s: &str) -> String {
    consistent_world_common::platform::expand_tilde(s)
}

// Renders the final consistent_world.toml from the wizard's collected answers.
// TODO: take a WizardAnswers-style params struct.
#[allow(clippy::too_many_arguments)]
fn build_toml(
    llm_provider: &str,
    llm_base_url: &str,
    llm_api_key_env: &str,
    default_model: &str,
    _gateway_auth: bool,
    credentials_backend: &str,
    vault_path: Option<&str>,
    gateway_port: u16,
    surreal_url: &str,
    surreal_ns: &str,
    surreal_db: &str,
) -> String {
    let auth_note = if _gateway_auth {
        "# Auth token is in keyring: consistent-world credentials get consistent_world.gateway.auth_token\n# Set gateway.auth.token in config or use env for Bearer.\n"
    } else {
        ""
    };
    let credentials_section = match vault_path {
        Some(path) if credentials_backend == "vault" => {
            format!("[credentials]\nbackend = \"vault\"\nvault_path = \"{path}\"\n")
        }
        _ => "[credentials]\nbackend = \"keyring\"\n".to_string(),
    };
    format!(
        r#"# Consistent config — generated by onboarding wizard. Secrets are in keychain or vault.

[gateway]
port = {gateway_port}
bind = "127.0.0.1"
{auth_note}
[agents.defaults]
model = "{default_model}"
max_concurrent = 4

[agents.defaults.subagents]
max_concurrent = 8

[memory]
backend = "surreal"
db_path = "~/.consistent-world/memory.db"
surreal_url = "{surreal_url}"
surreal_ns = "{surreal_ns}"
surreal_db = "{surreal_db}"
surreal_user = "root"
surreal_pass = "root"
vector_search = true

[plugins]
wasm_sandbox = true

{credentials_section}
[telemetry]
log_level = "info"

[llm]
provider = "{llm_provider}"
base_url = "{llm_base_url}"
api_key_env = "{llm_api_key_env}"
"#,
    )
}

/// Find a CLI on PATH, like `which`. Returns the first hit's full path.
fn which_cli(name: &str) -> Option<std::path::PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

// Lists locally-pulled Ollama model names via its /api/tags endpoint.
fn fetch_ollama_models(base_url: &str) -> Result<Vec<String>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let resp = client.get(&url).send()?;
    if !resp.status().is_success() {
        anyhow::bail!("Ollama API returned {}", resp.status());
    }
    let json: serde_json::Value = resp.json()?;
    let models = json
        .get("models")
        .and_then(|m| m.as_array())
        .ok_or_else(|| anyhow::anyhow!("Invalid Ollama /api/tags response"))?;
    let names: Vec<String> = models
        .iter()
        .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();
    Ok(names)
}

/// Tell the operator which coding-agent CLIs this machine can already drive.
///
/// Purely informational — a missing CLI is not a setup failure, it just means
/// the execution harness for that agent isn't available yet. Sync (like the
/// rest of the wizard) rather than reusing the async
/// [`crate::agent_cli_probe`], which would need a runtime here for what is two
/// `--version` calls.
fn report_coding_agents() {
    eprintln!();
    eprintln!("Coding agents on this machine...");
    let mut found_any = false;
    for kind in crate::agent_cli_probe::KNOWN_KINDS {
        match consistent_world_tools::coding_agent::locate_program(kind) {
            Some(path) => {
                found_any = true;
                let version = consistent_world_common::platform::std_command(&path)
                    .arg("--version")
                    .output()
                    .ok()
                    .filter(|o| o.status.success())
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .filter(|v| !v.is_empty());
                match version {
                    Some(v) => eprintln!("  ✓ {kind} — {v}"),
                    None => eprintln!("  ✓ {kind} — found at {}", path.display()),
                }
            }
            None => eprintln!("  · {kind} — not installed"),
        }
    }
    if found_any {
        eprintln!("  → Sign-in is checked separately, from Settings → Coding agents.");
    } else {
        eprintln!("  → Install one (e.g. the `claude` CLI) to run work on this machine.");
    }
}

// Best-effort reachability check: a CLI-backed provider just needs the CLI on
// PATH, Ollama needs its tags endpoint, everything else gets a tiny chat call
// (a 401 still counts as "reachable" — it means the URL is right).
fn test_llm(base_url: &str, api_key: &str, provider: &str, default_model: &str) -> bool {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    if provider == "claude-code" {
        return which_cli("claude").is_some();
    }
    if provider == "codex" {
        return which_cli("codex").is_some();
    }
    if provider == "ollama" {
        let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
        let resp = client.get(&url).send();
        match resp {
            Ok(r) => r.status().is_success(),
            Err(_) => false,
        }
    } else {
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": default_model,
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 2
        });
        let req = client.post(&url).json(&body).bearer_auth(api_key);
        let resp = req.send();
        match resp {
            Ok(r) => r.status().is_success() || r.status().as_u16() == 401,
            Err(_) => false,
        }
    }
}
