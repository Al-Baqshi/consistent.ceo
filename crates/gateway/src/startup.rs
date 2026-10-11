//! Startup helpers: create memory and schedule stores from config (used by main and tests).
//!
//! SurrealDB is the sole backend. If the connection fails, startup fails — there is no
//! silent fallback. Point `SURREAL_URL` (env) or `[memory] surreal_url` (TOML) at either a
//! local `surreal start` instance for dev or Surreal Cloud for production.

use anyhow::Context;
use consistent_world_memory::store::{MemoryStore, SurrealMemoryStore};
use std::sync::Arc;
use surrealdb::Surreal;
use surrealdb::engine::any::Any;
use tracing::info;

use crate::config::AppConfig;

/// Root user/password for SurrealDB when the server requires auth (e.g. Docker `--user` / `--pass`).
/// Order: `SURREAL_USER` / `SURREAL_PASS` (env), then `[memory] surreal_user` / `surreal_pass`,
/// then for local URLs only (`127.0.0.1` / `localhost`) default `root`/`root` to match `docs/run.md`.
/// Env wins over TOML: baked-in container configs carry placeholder credentials, and the
/// deploy environment (container env vars, docker compose) must be able to override them.
pub(crate) fn surreal_root_credentials(cfg: &AppConfig) -> Option<(String, String)> {
    use std::env;
    let u = cfg.surreal_url.to_lowercase();
    // Embedded engines run in-process and have no root auth to sign in against.
    if is_embedded_surreal_url(&u) {
        return None;
    }
    let env_nonempty = |key: &str| {
        env::var(key)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let user = env_nonempty("SURREAL_USER").or_else(|| cfg.surreal_user.clone());
    let pass = env_nonempty("SURREAL_PASS").or_else(|| cfg.surreal_pass.clone());
    if let (Some(u), Some(p)) = (user, pass) {
        return Some((u, p));
    }
    if cfg.surreal_user.is_none()
        && cfg.surreal_pass.is_none()
        && env::var("SURREAL_USER").is_err()
        && env::var("SURREAL_PASS").is_err()
        && (u.contains("127.0.0.1") || u.contains("localhost"))
    {
        return Some(("root".into(), "root".into()));
    }
    None
}
use crate::scheduler;

/// True for in-process SurrealDB engines (no server, no root auth):
/// `memory` / `mem://` (ephemeral) and persistent file engines `surrealkv://` / `rocksdb://`
/// (optionally `file://`, which Surreal maps to the default embedded engine).
fn is_embedded_surreal_url(url_lower: &str) -> bool {
    url_lower == "memory"
        || url_lower.starts_with("mem://")
        || url_lower.starts_with("surrealkv://")
        || url_lower.starts_with("rocksdb://")
        || url_lower.starts_with("file://")
}

/// Persistent embedded scheme prefixes whose path component is a real filesystem
/// path — it needs `~` expansion and a pre-created parent directory.
const EMBEDDED_FILE_SCHEMES: [&str; 3] = ["surrealkv://", "rocksdb://", "file://"];

/// Split a persistent embedded URL into `(scheme_prefix, path)`, preserving the
/// caller's original casing. Returns `None` for remote engines (`ws://`/`http://`)
/// and the ephemeral in-memory engine (`memory`/`mem://`), which have no disk path.
fn embedded_file_url_parts(url: &str) -> Option<(&str, &str)> {
    let lower = url.to_lowercase();
    EMBEDDED_FILE_SCHEMES.iter().find_map(|scheme| {
        lower
            .strip_prefix(scheme)
            .map(|rest| (&url[..scheme.len()], &url[url.len() - rest.len()..]))
    })
}

/// Resolve a SurrealDB URL for connecting. For persistent embedded engines, expand
/// `~` in the path so `surrealkv://~/.consistent-world/db` lands in the real home directory
/// instead of creating a literal `~` folder in the cwd. Other URLs pass through.
fn resolve_surreal_url(url: &str) -> String {
    match embedded_file_url_parts(url) {
        Some((scheme, path)) => format!("{scheme}{}", crate::config::expand_tilde_path(path)),
        None => url.to_string(),
    }
}

/// For persistent embedded engines, make sure the on-disk directory exists before
/// connecting — surrealkv/rocksdb won't create missing parent directories themselves.
/// Expects an already-resolved (tilde-expanded) URL.
fn ensure_embedded_db_dir(resolved_url: &str) -> anyhow::Result<()> {
    if let Some((_, path)) = embedded_file_url_parts(resolved_url)
        && let Some(parent) = std::path::Path::new(path).parent()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "Failed to create SurrealDB data directory {}",
                parent.display()
            )
        })?;
    }
    Ok(())
}

/// Create memory and schedule stores backed by SurrealDB.
/// Fails fast if Surreal is unreachable; the gateway will not start.
pub async fn create_memory_and_schedule(
    cfg: &AppConfig,
) -> anyhow::Result<(
    Arc<dyn MemoryStore>,
    Arc<dyn scheduler::ScheduleStore>,
    Arc<Surreal<Any>>,
    String,
)> {
    let surreal_url = resolve_surreal_url(&cfg.surreal_url);
    ensure_embedded_db_dir(&surreal_url)?;
    let root_creds = surreal_root_credentials(cfg);
    let surreal_store = SurrealMemoryStore::connect(
        &surreal_url,
        &cfg.surreal_ns,
        &cfg.surreal_db,
        root_creds.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
    )
    .await
    .with_context(|| {
        format!(
            "SurrealDB connect failed at {} (ns={}, db={}). SurrealDB is the required backend — the desktop default `surrealkv://~/.consistent-world/db` runs in-process; for a remote server start it with `surreal start memory --user root --pass root` or point SURREAL_URL at your cloud instance.",
            surreal_url, cfg.surreal_ns, cfg.surreal_db
        )
    })?;

    let db = surreal_store.db();
    let memory_store: Arc<dyn MemoryStore> = Arc::new(surreal_store);
    let schedule_store: Arc<dyn scheduler::ScheduleStore> =
        Arc::new(scheduler::SurrealScheduleStore::new(Arc::clone(&db)));
    info!("SurrealDB connected at {}", surreal_url);

    Ok((memory_store, schedule_store, db, "surreal".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // SAFETY: env mutation isn't thread-safe; serialize this module's tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_surreal_env() {
        unsafe {
            std::env::remove_var("SURREAL_USER");
            std::env::remove_var("SURREAL_PASS");
        }
    }

    #[test]
    fn env_credentials_win_over_toml_placeholders() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_surreal_env();
        // SAFETY: guarded by ENV_LOCK above.
        unsafe {
            std::env::set_var("SURREAL_USER", "root");
            std::env::set_var("SURREAL_PASS", "real-deploy-secret");
        }
        // TOML placeholder credentials a baked-in container config might carry.
        let cfg = AppConfig {
            surreal_url: "ws://surrealdb:8000".into(),
            surreal_user: Some("root".into()),
            surreal_pass: Some("root".into()),
            ..AppConfig::default()
        };

        let creds = surreal_root_credentials(&cfg);

        clear_surreal_env();
        assert_eq!(creds, Some(("root".into(), "real-deploy-secret".into())));
    }

    #[test]
    fn falls_back_to_toml_when_env_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_surreal_env();
        let cfg = AppConfig {
            surreal_url: "ws://surrealdb:8000".into(),
            surreal_user: Some("root".into()),
            surreal_pass: Some("toml-pass".into()),
            ..AppConfig::default()
        };

        let creds = surreal_root_credentials(&cfg);

        assert_eq!(creds, Some(("root".into(), "toml-pass".into())));
    }

    #[test]
    fn embedded_engines_have_no_root_auth() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_surreal_env();
        let cfg = AppConfig {
            surreal_url: "surrealkv://~/.consistent-world/db".into(),
            surreal_user: Some("root".into()),
            surreal_pass: Some("root".into()),
            ..AppConfig::default()
        };

        assert_eq!(surreal_root_credentials(&cfg), None);
    }
}
