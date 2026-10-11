//! On-demand lifecycle for the RunPod voice pod (PersonaPlex brain box).
//!
//! The pod bills by the hour while running, so it must only run while someone
//! is actually talking. The gateway is the natural owner of that lifecycle:
//! every voice WebSocket session calls [`VoicePodManager::on_session_start`]
//! (which starts the pod if it is stopped) and [`on_session_end`]; a
//! background reaper stops the pod once there have been zero active sessions
//! for the configured idle window.
//!
//! Enabled by env: `RUNPOD_API_KEY` + `VOICE_POD_ID`, optional
//! `VOICE_POD_IDLE_STOP_SECS` (default 600). Absent env → feature off, zero
//! behaviour change.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::{info, warn};

const DEFAULT_IDLE_STOP_SECS: u64 = 600;

// Lazily built once per process; `None` means the feature isn't configured.
static GLOBAL: std::sync::OnceLock<Option<Arc<VoicePodManager>>> = std::sync::OnceLock::new();

/// Process-wide manager, lazily initialised on first voice session (inside
/// the tokio runtime, so the idle reaper can spawn). `None` when the env
/// isn't configured.
pub fn global() -> Option<Arc<VoicePodManager>> {
    GLOBAL.get_or_init(VoicePodManager::from_env).clone()
}

/// Starts/stops the RunPod voice pod based on the active voice-session count.
pub struct VoicePodManager {
    api_key: String,
    pod_id: String,
    idle_stop: Duration,
    active_sessions: AtomicUsize,
    /// Instant of the most recent session end (idle clock starts here).
    last_activity: Mutex<Instant>,
    client: reqwest::Client,
}

impl VoicePodManager {
    /// Builds the manager from env, or `None` when not configured.
    pub fn from_env() -> Option<Arc<Self>> {
        let api_key = std::env::var("RUNPOD_API_KEY")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        let pod_id = std::env::var("VOICE_POD_ID")
            .ok()
            .filter(|v| !v.trim().is_empty())?;
        let idle_stop = std::env::var("VOICE_POD_IDLE_STOP_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_IDLE_STOP_SECS);
        let manager = Arc::new(Self {
            api_key,
            pod_id,
            idle_stop: Duration::from_secs(idle_stop),
            active_sessions: AtomicUsize::new(0),
            last_activity: Mutex::new(Instant::now()),
            client: reqwest::Client::new(),
        });
        manager.clone().spawn_idle_reaper();
        info!(pod_id = %manager.pod_id, idle_stop_secs = idle_stop, "Voice pod on-demand lifecycle enabled");
        Some(manager)
    }

    // Fires a RunPod REST action ("start"/"stop") against this pod.
    async fn pod_action(&self, action: &str) -> Result<(), String> {
        let url = format!("https://rest.runpod.io/v1/pods/{}/{}", self.pod_id, action);
        let res = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if res.status().is_success() {
            Ok(())
        } else {
            Err(format!("{} → {}", action, res.status()))
        }
    }

    /// Called when a voice session opens. Fire-and-forget pod start — the
    /// session proceeds regardless (the upstream connect retries cover the
    /// pod's boot window).
    pub fn on_session_start(self: &Arc<Self>) {
        let n = self.active_sessions.fetch_add(1, Ordering::SeqCst) + 1;
        info!(active = n, "voice session opened; ensuring pod is running");
        let this = self.clone();
        tokio::spawn(async move {
            match this.pod_action("start").await {
                Ok(()) => info!(pod_id = %this.pod_id, "voice pod start requested"),
                // "not enough free GPUs" and "already running" both land here;
                // the session itself will surface a user-visible error if the
                // upstream stays unreachable.
                Err(e) => warn!(pod_id = %this.pod_id, error = %e, "voice pod start failed"),
            }
        });
    }

    /// Called when a voice session closes; starts the idle clock.
    pub fn on_session_end(self: &Arc<Self>) {
        let n = self
            .active_sessions
            .fetch_sub(1, Ordering::SeqCst)
            .saturating_sub(1);
        info!(active = n, "voice session closed");
        let this = self.clone();
        tokio::spawn(async move {
            *this.last_activity.lock().await = Instant::now();
        });
    }

    // Background task: stops the pod once idle_stop has elapsed with zero active sessions.
    fn spawn_idle_reaper(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(60));
            loop {
                tick.tick().await;
                if self.active_sessions.load(Ordering::SeqCst) > 0 {
                    continue;
                }
                let idle_for = self.last_activity.lock().await.elapsed();
                if idle_for < self.idle_stop {
                    continue;
                }
                match self.pod_action("stop").await {
                    Ok(()) => {
                        info!(pod_id = %self.pod_id, idle_secs = idle_for.as_secs(), "voice pod stopped (idle)");
                        // Reset the clock so we don't re-issue stop every minute.
                        *self.last_activity.lock().await = Instant::now();
                    }
                    Err(e) => {
                        warn!(pod_id = %self.pod_id, error = %e, "voice pod idle stop failed")
                    }
                }
            }
        });
    }
}
