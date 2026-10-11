//! Per-user vault handles with an idle timeout.
//!
//! Each authenticated principal gets its own vault file at
//! `~/.consistent-world/vaults/{principal_id}.json`. A vault is only usable
//! while unlocked, and an unlocked vault holds the derived `user_key` in memory,
//! so this manager exists to make sure that window is bounded:
//!
//! - Every API call a person makes touches the entry's `last_used`
//!   ([`VaultManager::unlocked`]). Reads a client makes on a timer — a polled
//!   TOTP code — go through [`VaultManager::peek_unlocked`] instead and do *not*,
//!   or an open browser tab would hold the vault unlocked forever.
//! - [`VaultManager::sweep`] locks anything idle past the TTL, and every lookup
//!   also checks the calling entry so a stale handle is never handed out even if
//!   the sweeper has not run yet.
//!
//! Locking drops the [`Vault`]'s in-memory state; the `SecretKey` zeroizes on
//! drop. The handle itself is kept so the next unlock reuses the same path.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;

use consistent_world_credentials::vault::Vault;

/// How long an unlocked vault survives without being used.
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

struct Entry {
    vault: Arc<Vault>,
    last_used: Instant,
}

/// Registry of per-principal vault handles, with idle auto-lock and unlock
/// throttling. See the module header for the lifecycle this enforces.
pub struct VaultManager {
    root: PathBuf,
    entries: DashMap<String, Entry>,
    idle_timeout: Duration,
    throttle: crate::vault_throttle::UnlockThrottle,
}

impl VaultManager {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            entries: DashMap::new(),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            throttle: crate::vault_throttle::UnlockThrottle::default(),
        }
    }

    pub fn with_idle_timeout(root: PathBuf, idle_timeout: Duration) -> Self {
        Self {
            root,
            entries: DashMap::new(),
            idle_timeout,
            throttle: crate::vault_throttle::UnlockThrottle::default(),
        }
    }

    /// Replace the unlock throttle's limits. Production always uses the default
    /// (see `vault_throttle`); tests use small numbers so a lockout costs two slow
    /// Argon2id guesses rather than six.
    pub fn with_throttle(mut self, throttle: crate::vault_throttle::UnlockThrottle) -> Self {
        self.throttle = throttle;
        self
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Seconds this principal must wait before another unlock attempt, or `None`
    /// if they may try now. The unlock handlers check this first and record the
    /// outcome via [`Self::note_unlock_failure`] / [`Self::note_unlock_success`].
    pub fn unlock_retry_after(&self, principal_id: &str) -> Option<u64> {
        self.throttle.retry_after(principal_id)
    }

    /// Record a failed unlock and return the backoff now imposed, if any.
    pub fn note_unlock_failure(&self, principal_id: &str) -> Option<u64> {
        self.throttle.record_failure(principal_id)
    }

    /// Clear a principal's backoff after a successful unlock.
    pub fn note_unlock_success(&self, principal_id: &str) {
        self.throttle.record_success(principal_id);
    }

    /// Vault file for a principal.
    ///
    /// `principal_id` becomes a filename, so it is sanitised rather than trusted:
    /// anything outside `[A-Za-z0-9_-]` becomes `_`. Note that `.` is *not* in
    /// the allowed set — permitting it would let an id like `..` or `../x` retain
    /// dot segments, and while `root.join()` would still contain them, refusing
    /// dots outright removes the question. An empty result falls back to a
    /// placeholder so the path can never collapse to the directory itself.
    fn path_for(&self, principal_id: &str) -> PathBuf {
        let safe: String = principal_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe = if safe.is_empty() {
            "unnamed".to_string()
        } else {
            safe
        };
        self.root.join(format!("{safe}.json"))
    }

    /// A handle for this principal, whether or not it is unlocked.
    pub fn handle(&self, principal_id: &str) -> Arc<Vault> {
        self.expire_if_idle(principal_id);
        if let Some(entry) = self.entries.get(principal_id) {
            return entry.vault.clone();
        }
        let vault = Arc::new(Vault::new(self.path_for(principal_id)));
        self.entries.insert(
            principal_id.to_string(),
            Entry {
                vault: vault.clone(),
                last_used: Instant::now(),
            },
        );
        vault
    }

    /// A handle that is guaranteed unlocked, or `None`.
    ///
    /// Callers should treat `None` as "prompt for the master password again".
    pub fn unlocked(&self, principal_id: &str) -> Option<Arc<Vault>> {
        let vault = self.handle(principal_id);
        if vault.is_unlocked() {
            self.touch(principal_id);
            Some(vault)
        } else {
            None
        }
    }

    /// Like [`Self::unlocked`], but does **not** count as activity.
    ///
    /// For reads a client repeats on a timer rather than because a person did
    /// something, such as the live TOTP code an open item shows. Sending those
    /// through [`Self::unlocked`] would reset the idle clock on every tick, so a
    /// forgotten tab would keep the vault unlocked indefinitely and the 15-minute
    /// auto-lock in the threat model would never fire. The idle check still
    /// applies: a vault past its window is locked here exactly as it is there.
    pub fn peek_unlocked(&self, principal_id: &str) -> Option<Arc<Vault>> {
        let vault = self.handle(principal_id);
        vault.is_unlocked().then_some(vault)
    }

    pub fn touch(&self, principal_id: &str) {
        if let Some(mut entry) = self.entries.get_mut(principal_id) {
            entry.last_used = Instant::now();
        }
    }

    pub fn lock(&self, principal_id: &str) {
        if let Some(entry) = self.entries.get(principal_id) {
            entry.vault.lock();
        }
    }

    /// Seconds until this principal's vault auto-locks, if it is unlocked.
    pub fn seconds_until_lock(&self, principal_id: &str) -> Option<u64> {
        let entry = self.entries.get(principal_id)?;
        if !entry.vault.is_unlocked() {
            return None;
        }
        let elapsed = entry.last_used.elapsed();
        Some(self.idle_timeout.saturating_sub(elapsed).as_secs())
    }

    // Lock proactively on lookup rather than waiting for `sweep`, so a handle
    // is never handed out unlocked past its own idle window.
    fn expire_if_idle(&self, principal_id: &str) {
        if let Some(entry) = self.entries.get(principal_id)
            && entry.last_used.elapsed() >= self.idle_timeout
        {
            entry.vault.lock();
        }
    }

    /// Lock every vault idle past the TTL. Called from a background loop.
    pub fn sweep(&self) -> usize {
        // Also drop stale throttle records, so a spray of distinct principal ids
        // cannot grow that map without bound.
        self.throttle.sweep();
        let mut locked = 0;
        for entry in self.entries.iter() {
            if entry.vault.is_unlocked() && entry.last_used.elapsed() >= self.idle_timeout {
                entry.vault.lock();
                locked += 1;
            }
        }
        locked
    }

    /// Lock everything, e.g. on shutdown.
    pub fn lock_all(&self) {
        for entry in self.entries.iter() {
            entry.vault.lock();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager(dir: &tempfile::TempDir, ttl: Duration) -> VaultManager {
        VaultManager::with_idle_timeout(dir.path().to_path_buf(), ttl)
    }

    /// Create and unlock a vault, then start the idle clock.
    ///
    /// `init` runs Argon2id at 64 MiB / t=3 and takes appreciable wall time, so a
    /// test must not let that count against a short TTL. This mirrors what
    /// `vault_api` does: touch once the slow work is done.
    fn init_and_touch(m: &VaultManager, principal: &str) {
        m.handle(principal).init("master").unwrap();
        m.touch(principal);
    }

    #[test]
    fn each_principal_gets_its_own_file() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(&dir, DEFAULT_IDLE_TIMEOUT);
        assert_ne!(
            m.handle("user-a").path().to_path_buf(),
            m.handle("user-b").path().to_path_buf()
        );
    }

    #[test]
    fn principal_ids_cannot_escape_the_vault_directory() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(&dir, DEFAULT_IDLE_TIMEOUT);
        for hostile in [
            "../../etc/passwd",
            "..",
            ".",
            "/etc/shadow",
            "a/../../b",
            "",
        ] {
            let path = m.handle(hostile).path().to_path_buf();
            assert_eq!(
                path.parent().unwrap(),
                dir.path(),
                "{hostile:?} escaped the vault directory"
            );
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            assert!(!name.contains(".."), "{hostile:?} produced {name:?}");
            assert!(name.ends_with(".json"), "{hostile:?} produced {name:?}");
        }
    }

    #[test]
    fn unlocked_returns_none_until_unlocked() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(&dir, DEFAULT_IDLE_TIMEOUT);
        assert!(m.unlocked("u").is_none());

        init_and_touch(&m, "u");
        assert!(m.unlocked("u").is_some());

        m.lock("u");
        assert!(m.unlocked("u").is_none());
    }

    #[test]
    fn an_idle_vault_locks_itself() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(&dir, Duration::from_millis(400));
        init_and_touch(&m, "u");
        assert!(m.unlocked("u").is_some());

        std::thread::sleep(Duration::from_millis(600));
        assert!(
            m.unlocked("u").is_none(),
            "an idle vault must not stay unlocked"
        );
    }

    #[test]
    fn use_extends_the_idle_window() {
        let dir = tempfile::tempdir().unwrap();
        let ttl = Duration::from_millis(400);
        let m = manager(&dir, ttl);
        init_and_touch(&m, "u");

        // Total elapsed here (6 x 150ms) comfortably exceeds the TTL; the vault
        // stays unlocked only because each access refreshes the window.
        for i in 0..6 {
            std::thread::sleep(Duration::from_millis(150));
            assert!(
                m.unlocked("u").is_some(),
                "activity should keep it alive (iteration {i})"
            );
        }
    }

    #[test]
    fn a_peek_does_not_extend_the_idle_window() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(&dir, Duration::from_millis(400));
        assert!(m.peek_unlocked("u").is_none(), "nothing to peek at yet");
        init_and_touch(&m, "u");
        assert!(m.peek_unlocked("u").is_some());

        // An open tab polling a TOTP code: frequent reads, no person behind them.
        // Total elapsed (3 x 150ms) exceeds the TTL, and unlike
        // `use_extends_the_idle_window` none of these reads may refresh it.
        for _ in 0..3 {
            std::thread::sleep(Duration::from_millis(150));
            let _ = m.peek_unlocked("u");
        }
        assert!(
            m.peek_unlocked("u").is_none(),
            "polling must not keep an idle vault unlocked"
        );
        assert!(m.unlocked("u").is_none());
    }

    #[test]
    fn sweep_reports_what_it_locked() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(&dir, Duration::from_millis(400));
        init_and_touch(&m, "a");
        init_and_touch(&m, "b");

        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(m.sweep(), 2);
        // Already locked, so a second sweep has nothing to do.
        assert_eq!(m.sweep(), 0);
    }
}
