//! Best-effort operator alerting for failures that would otherwise only show
//! up in a server's logs — where nobody is watching in real time.
//!
//! Fires a Slack-compatible incoming-webhook POST (`{"text": "..."}`, which
//! Slack, Mattermost, and most generic webhook catchers accept directly) when
//! `ALERT_WEBHOOK_URL` is configured. Disabled (silent no-op) otherwise, same
//! pattern as [`crate::backup::BackupConfig`] — this is optional infra, not
//! a required one.

use std::time::Duration;

/// Parsed alert config. Its mere existence means alerting is enabled.
#[derive(Clone)]
pub struct AlertConfig {
    webhook_url: String,
}

impl AlertConfig {
    /// Load from `ALERT_WEBHOOK_URL`; `None` when unset or blank, which
    /// leaves alerting disabled.
    pub fn from_env() -> Option<Self> {
        std::env::var("ALERT_WEBHOOK_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .map(|webhook_url| Self { webhook_url })
    }
}

/// Fire an alert for a named failure. Best-effort: logs and swallows its own
/// errors rather than letting a dead webhook cascade into the caller's error
/// path (an alert about a failure should never itself become a new failure).
pub async fn notify_failure(cfg: Option<&AlertConfig>, kind: &str, detail: &str) {
    let Some(cfg) = cfg else { return };
    let text = format!(":warning: Consistent gateway — {kind} failed: {detail}");
    let result = reqwest::Client::new()
        .post(&cfg.webhook_url)
        .timeout(Duration::from_secs(10))
        .json(&serde_json::json!({ "text": text }))
        .send()
        .await;
    match result {
        Ok(resp) if !resp.status().is_success() => {
            tracing::warn!(status = %resp.status(), "alert webhook returned non-2xx");
        }
        Err(e) => tracing::warn!(error = %e, "alert webhook request failed"),
        Ok(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn disabled_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("ALERT_WEBHOOK_URL");
        }
        assert!(AlertConfig::from_env().is_none());
    }

    #[test]
    fn enabled_when_set() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::set_var("ALERT_WEBHOOK_URL", "https://hooks.example.com/x");
        }
        let cfg = AlertConfig::from_env();
        assert!(cfg.is_some());
        assert_eq!(cfg.unwrap().webhook_url, "https://hooks.example.com/x");
        // SAFETY: guarded by ENV_LOCK.
        unsafe {
            std::env::remove_var("ALERT_WEBHOOK_URL");
        }
    }

    #[tokio::test]
    async fn notify_is_noop_without_config() {
        // Must not panic or block when disabled.
        notify_failure(None, "test", "detail").await;
    }
}
