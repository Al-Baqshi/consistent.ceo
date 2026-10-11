//! Resolve secrets from the credentials store (keyring or vault) at runtime.
//! TOML config must not contain secrets; they are loaded here.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tracing::warn;

use crate::config::AppConfig;
use consistent_world_credentials::keyring::KeyringStore;
use consistent_world_credentials::scope::Scope;
use consistent_world_credentials::vault::EncryptedVault;

/// Read a connection secret stored at `entity.connection.{connection_id}` (see `store_connection_secret_and_ref`).
pub fn read_entity_connection_secret(connection_id: &str) -> Option<String> {
    let key = format!("entity.connection.{connection_id}");
    KeyringStore::new("consistent-world").get(&key).ok()
}

/// Credential key for gateway Bearer auth (used when resolving from store).
pub const GATEWAY_AUTH_CREDENTIAL_KEY: &str = "consistent_world.gateway.auth_token";

/// Ensure vault is initialized or unlocked, prompting for master password on first run or when locked.
/// Returns a vault instance that is unlocked and can be used for get().
fn ensure_vault_ready(vault_path: &Path) -> Result<EncryptedVault> {
    let vault = EncryptedVault::new(vault_path.to_path_buf());
    if !vault_path.exists() {
        let password = dialoguer::Password::new()
            .with_prompt("Create master password for credentials vault")
            .with_confirmation("Confirm master password", "Passwords do not match")
            .allow_empty_password(false)
            .interact()
            .context("vault master password")?;
        vault.init(&password)?;
        eprintln!("Vault initialized at {:?}", vault_path);
    } else {
        let password = dialoguer::Password::new()
            .with_prompt("Vault master password")
            .allow_empty_password(false)
            .interact()
            .context("vault master password")?;
        vault.unlock(&password)?;
    }
    Ok(vault)
}

/// Resolve gateway auth token from the configured credentials backend.
/// `GATEWAY_AUTH_TOKEN` (env) wins over every backend — it is the only
/// source available in containerised deploys (docker, a VPS, the brain box),
/// where there is no OS keyring and no interactive vault unlock.
/// If backend is keyring, reads from keyring. If vault, prompts for master password (init or unlock) then reads.
/// Falls back to the other backend if the primary fails.
/// Returns None if not set or on error (logged).
pub fn resolve_gateway_auth_token(cfg: &AppConfig) -> Result<Option<String>> {
    if let Ok(t) = std::env::var("GATEWAY_AUTH_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(Some(t));
        }
    }
    match cfg.credentials_backend.as_str() {
        "keyring" => {
            let store = KeyringStore::new("consistent-world");
            match store.get(GATEWAY_AUTH_CREDENTIAL_KEY) {
                Ok(t) => Ok(Some(t)),
                Err(e) => {
                    warn!("Could not read gateway auth token from keyring: {e}");
                    // Fallback: try vault if configured
                    if let Some(vault_path) = &cfg.vault_path {
                        let vp = Path::new(vault_path);
                        if vp.exists() {
                            match ensure_vault_ready(vp)
                                .and_then(|v| v.get(GATEWAY_AUTH_CREDENTIAL_KEY))
                            {
                                Ok(t) => return Ok(Some(t)),
                                Err(e) => warn!("Vault fallback also failed: {e}"),
                            }
                        }
                    }
                    Ok(None)
                }
            }
        }
        "vault" => {
            let vault_path = match &cfg.vault_path {
                Some(p) => Path::new(p),
                None => {
                    warn!("credentials.backend is 'vault' but credentials.vault_path is not set");
                    return Ok(None);
                }
            };
            let vault = ensure_vault_ready(vault_path)?;
            match vault.get(GATEWAY_AUTH_CREDENTIAL_KEY) {
                Ok(t) => Ok(Some(t)),
                Err(e) => {
                    warn!("Could not read gateway auth token from vault: {e}");
                    // Fallback: try keyring
                    let store = KeyringStore::new("consistent-world");
                    if let Ok(t) = store.get(GATEWAY_AUTH_CREDENTIAL_KEY) {
                        return Ok(Some(t));
                    }
                    Ok(None)
                }
            }
        }
        _ => {
            warn!(
                "Unknown credentials backend '{}'; gateway auth token not resolved",
                cfg.credentials_backend
            );
            Ok(None)
        }
    }
}

/// Keyring entry for an LLM API key when `credentials.backend = "keyring"`.
/// Env vars (e.g. `OPENAI_API_KEY`) remain the logical name in TOML; the secret may live in the keychain under this key.
pub fn llm_api_key_keyring_entry(api_key_env: &str) -> String {
    format!("llm.api_key.{api_key_env}")
}

/// Mask a secret for operator UI: last 4 characters, never the full value.
pub fn mask_secret_tail(secret: &str) -> Option<String> {
    let t = secret.trim();
    if t.is_empty() {
        return None;
    }
    let mut tail_chars: Vec<char> = t.chars().rev().take(4).collect();
    tail_chars.reverse();
    let tail: String = tail_chars.into_iter().collect();
    Some(format!("…{tail}"))
}

/// Env-var names that stand for "this provider has no secret to hold".
///
/// `OLLAMA_API_KEY` is a local server. `CLAUDE_CODE_CLI` and `CODEX_CLI` are
/// the `claude`/`codex` binaries driven as providers, authenticated by the
/// operator's own subscription — the point of them is that there is no key.
/// All must read as *configured* when blank, or the UI reports the active
/// provider as disconnected while it is answering perfectly well.
pub fn is_keyless_llm_env(api_key_env: &str) -> bool {
    api_key_env.is_empty()
        || api_key_env == "OLLAMA_API_KEY"
        || api_key_env == "CLAUDE_CODE_CLI"
        || api_key_env == "CODEX_CLI"
}

/// Whether `name` may be stored as a provider's `api_key_env` through the API.
///
/// `api_key_env` is the name of the environment variable (or keyring entry) the
/// gateway reads the provider's key from, and it is then sent as a bearer token
/// to that provider's base URL. Left free-form, anyone who could register a
/// provider could name `AWS_SECRET_ACCESS_KEY`, `GATEWAY_AUTH_TOKEN` or
/// `DATABASE_URL` and have it delivered to a server of their choosing.
///
/// The rule, deliberately tight:
///
/// - the keyless sentinels (`""`, `OLLAMA_API_KEY`, `CLAUDE_CODE_CLI`,
///   `CODEX_CLI` — see [`is_keyless_llm_env`]); or
/// - an upper-case env-var name ending in `_API_KEY`
///   (`^[A-Z][A-Z0-9_]*_API_KEY$`), which is how every provider this product
///   knows names its key (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …),
/// - and never one of the gateway's own secrets ([`is_gateway_internal_env`]).
///
/// `*_TOKEN` is not accepted: `GITHUB_TOKEN`, `HF_TOKEN` and the gateway's own
/// `GATEWAY_AUTH_TOKEN` are general-purpose credentials, not LLM keys.
/// Config-file providers are trusted and are not held to this rule.
pub fn is_allowed_llm_key_env(name: &str) -> bool {
    if is_keyless_llm_env(name) {
        return true;
    }
    let Some(stem) = name.strip_suffix("_API_KEY") else {
        return false;
    };
    let mut chars = stem.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !is_gateway_internal_env(name)
}

/// Environment variables that hold the gateway's own secrets. Never resolved
/// as an LLM key, whatever a provider row says — the last line of defence for
/// rows written before [`is_allowed_llm_key_env`] existed.
pub fn is_gateway_internal_env(name: &str) -> bool {
    name.starts_with("CONSISTENT_WORLD_")
        || name.starts_with("CONSISTENT_RELAY_")
        || name.starts_with("GATEWAY_")
        || name.starts_with("SURREAL")
}

/// Read LLM secret from keyring, with vault fallback.
pub fn read_llm_keyring_secret(api_key_env: &str) -> Option<String> {
    if is_keyless_llm_env(api_key_env) {
        return None;
    }
    let store = KeyringStore::new("consistent-world");
    store.get(&llm_api_key_keyring_entry(api_key_env)).ok()
}

/// True if the provider can call the LLM: keyless, or non-empty env, or keyring entry.
pub fn llm_secret_configured(api_key_env: &str) -> bool {
    if is_keyless_llm_env(api_key_env) {
        return true;
    }
    if std::env::var(api_key_env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    read_llm_keyring_secret(api_key_env)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// The vault that backs unattended agent runs, if one has been nominated.
///
/// Vaults are per-principal, but an agent executing a scheduled task has no
/// request and therefore no principal. Rather than guess — picking "any unlocked
/// vault" would leak one tenant's keys into another's agents — the deployment
/// names the vault explicitly via `CONSISTENT_WORLD_AGENT_VAULT_PRINCIPAL`.
///
/// Unset means the vault is simply not consulted and resolution behaves exactly
/// as it did before, which is the right default for a shared deployment.
static AGENT_VAULT: OnceLock<AgentVault> = OnceLock::new();

struct AgentVault {
    manager: Arc<crate::vault_manager::VaultManager>,
    principal_id: String,
}

/// Nominate the vault that unattended agent runs resolve credentials from.
/// Called once at startup; later calls are ignored.
pub fn register_agent_vault(
    manager: Arc<crate::vault_manager::VaultManager>,
    principal_id: String,
) {
    let _ = AGENT_VAULT.set(AgentVault {
        manager,
        principal_id,
    });
}

/// Read a variable from the nominated vault, honouring entity scoping.
///
/// Returns `None` whenever the vault is locked, absent or does not hold the key —
/// all of which are ordinary states, not errors. A locked vault must never stall
/// an agent run waiting for someone to type a password.
pub fn resolve_scoped_secret(key: &str, entity_id: Option<&str>) -> Option<String> {
    let agent_vault = AGENT_VAULT.get()?;
    // `peek_unlocked`, not `unlocked`: an agent reading a secret is not the
    // operator being active. Touching here let a run that polls a key every few
    // minutes hold the vault open forever, voiding the 15-minute idle lock in
    // docs/VAULT.md. Locked returns None rather than blocking, as wanted.
    let vault = agent_vault.manager.peek_unlocked(&agent_vault.principal_id)?;

    let scope = match entity_id {
        Some(id) => Scope::entity(id).ok()?,
        None => Scope::Global,
    };
    // `resolve` already walks entity -> global.
    vault
        .resolve(&scope, key)
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Resolve API key for LLM HTTP calls, without entity context.
///
/// **Environment overrides keyring** when both are set. Keyless providers return
/// a placeholder; missing secrets return an empty string.
pub fn resolve_llm_api_key(api_key_env: &str) -> String {
    resolve_llm_api_key_for_entity(api_key_env, None)
}

/// Resolve an API key for one entity's agent run.
///
/// Precedence, most specific first:
///
/// 1. **The entity's own vault value.** Deliberately set for this entity and
///    nothing else, so it outranks everything — this is what makes two entities
///    able to bill different Stripe accounts.
/// 2. **The process environment.** Preserved above the vault's global scope
///    because it is the only source available in containerised deploys, where
///    there is no keychain and nobody to unlock a vault interactively.
/// 3. **The vault's global scope**, then **the keyring**.
pub fn resolve_llm_api_key_for_entity(api_key_env: &str, entity_id: Option<&str>) -> String {
    if is_keyless_llm_env(api_key_env) {
        return "dummy".to_string();
    }

    // 1. This entity's own value, if it has one of its own.
    if let Some(entity_id) = entity_id
        && let Ok(scope) = Scope::entity(entity_id)
        && let Some(vault) = AGENT_VAULT
            .get()
            .and_then(|v| v.manager.peek_unlocked(&v.principal_id))
        && let Ok(v) = vault.get_scoped(&scope, api_key_env)
        && !v.trim().is_empty()
    {
        return v;
    }

    // 2. Deployment environment. Never the gateway's own secrets: a provider
    //    row naming one would ship it to that provider's base URL.
    if !is_gateway_internal_env(api_key_env)
        && let Ok(v) = std::env::var(api_key_env)
        && !v.trim().is_empty()
    {
        return v;
    }

    // 3. Vault global scope, then the OS keyring.
    if let Some(v) = resolve_scoped_secret(api_key_env, None) {
        return v;
    }
    if let Some(v) = read_llm_keyring_secret(api_key_env) {
        let t = v.trim();
        if !t.is_empty() {
            return v;
        }
    }
    String::new()
}

/// Persist LLM API key to the keyring (Phase 2 HTTP API). Vault backend falls back to keyring.
pub fn store_llm_api_key(cfg: &AppConfig, api_key_env: &str, secret: &str) -> anyhow::Result<()> {
    if is_keyless_llm_env(api_key_env) {
        anyhow::bail!("This provider is keyless; API key storage does not apply");
    }
    let trimmed = secret.trim();
    if trimmed.is_empty() {
        anyhow::bail!("API key must not be empty");
    }
    match cfg.credentials_backend.as_str() {
        "keyring" => {
            let store = KeyringStore::new("consistent-world");
            store.set(&llm_api_key_keyring_entry(api_key_env), trimmed)?;
            Ok(())
        }
        "vault" => {
            // Vault requires interactive unlock; not supported in API context.
            // Fall back to keyring for API-based storage.
            let store = KeyringStore::new("consistent-world");
            store.set(&llm_api_key_keyring_entry(api_key_env), trimmed)?;
            warn!(
                "Vault backend does not support API-based key storage; stored in keyring as fallback"
            );
            Ok(())
        }
        other => anyhow::bail!(
            "Storing LLM keys via API requires credentials.backend = \"keyring\" or \"vault\" (current: {other})"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_key_env_names_are_allowlisted() {
        for ok in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GOOGLE_API_KEY",
            "DEEPSEEK_API_KEY",
            "X2_API_KEY",
            // Keyless sentinels.
            "",
            "OLLAMA_API_KEY",
            "CLAUDE_CODE_CLI",
            "CODEX_CLI",
        ] {
            assert!(is_allowed_llm_key_env(ok), "{ok:?} should be allowed");
        }
        for bad in [
            "AWS_SECRET_ACCESS_KEY",
            "GATEWAY_AUTH_TOKEN",
            "GITHUB_TOKEN",
            "DATABASE_URL",
            "HOME",
            "_API_KEY",
            "openai_api_key",
            "OPENAI_API_KEY ",
            "OPEN-AI_API_KEY",
            "1PASSWORD_API_KEY",
            "CONSISTENT_WORLD_RELAY_API_KEY",
            "GATEWAY_API_KEY",
        ] {
            assert!(!is_allowed_llm_key_env(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn the_gateway_never_resolves_its_own_secrets_as_an_llm_key() {
        // A provider row written before the allowlist could still name one.
        assert!(is_gateway_internal_env("GATEWAY_AUTH_TOKEN"));
        assert!(is_gateway_internal_env("CONSISTENT_WORLD_RELAY_SECRET"));
        assert!(!is_gateway_internal_env("OPENAI_API_KEY"));
    }
}
