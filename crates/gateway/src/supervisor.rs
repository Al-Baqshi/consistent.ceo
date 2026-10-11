//! Restart supervision for the gateway's background loops.
//!
//! Nine loops drive everything this product promises to do while nobody is
//! watching — task pickup, mission engines, the task executor, reminders,
//! heartbeats, accountability. Each used to be started with a bare
//! `tokio::spawn` whose `JoinHandle` was dropped on the floor. Tokio catches a
//! panic and quietly ends that task, so a panicking orchestrator stopped picking
//! up work for the entire life of the process: nothing logged at loop level,
//! nothing in the operator feed, and `/health` still answering 200. **A dead
//! loop looked exactly like an idle one** — which is indistinguishable from "you
//! have no work queued", the reading an operator is most likely to accept.
//!
//! [`spawn_supervised`] takes a *factory* instead of a future, because restarting
//! a loop means building a fresh one. A panic is logged with the payload,
//! recorded in the [`SubsystemRegistry`], pushed to the alert webhook when one is
//! configured, and then the loop is started again after a backoff.
//!
//! Design notes:
//!
//! - **Backoff is capped and self-resetting.** A loop that panics on every tick
//!   must not spin the CPU, so delays double up to [`RESTART_BACKOFF_MAX`]. But a
//!   loop that ran for an hour before dying shouldn't inherit an hour-old
//!   penalty, so any run lasting longer than [`BACKOFF_RESET_AFTER`] resets it.
//! - **A clean return is not a crash.** These loops are `loop { interval.tick() }`
//!   forever, so returning `Ok(())` means something deliberately stopped — a
//!   shutdown path, or a guard clause deciding the subsystem is disabled. That is
//!   respected: no restart, and the registry records it as stopped rather than
//!   failed.
//! - **Cancellation is not a crash either.** Task shutdown at process exit
//!   arrives as a non-panic `JoinError`; restarting then would fight teardown.
//! - **No new operator-feed kind.** A crashed subsystem is system health, not
//!   work on a timeline, and a new `OperatorFeedKind` would push a phone
//!   notification for an infrastructure event — the feed's pushes are reserved
//!   for things needing a human decision. Restart counts surface on `/health`
//!   instead; per-loop detail joins the store report in the health-surface phase.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::alerts::AlertConfig;

/// First delay after a panic. Short enough that a one-off blip barely registers.
pub const RESTART_BACKOFF_MIN: Duration = Duration::from_secs(1);
/// Ceiling on the delay, so a loop panicking on every tick retries twice a minute
/// rather than in a hot spin.
pub const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(30);
/// A run that lasted at least this long counts as healthy: the next panic starts
/// its backoff from [`RESTART_BACKOFF_MIN`] again.
pub const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(60);
/// Minimum gap between outbound alerts for the same loop. A subsystem panicking on
/// every tick restarts every [`RESTART_BACKOFF_MAX`] forever — correct, since
/// transient causes do clear — but webhooking that ~2,900 times a day is not. Every
/// restart is still logged and counted; only the notification is throttled.
pub const ALERT_MIN_INTERVAL: Duration = Duration::from_secs(300);

/// What the supervisor knows about one background loop.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SubsystemHealth {
    /// `true` while a run is in flight; `false` after a clean exit or cancel.
    pub running: bool,
    /// How many times this loop has panicked and been restarted.
    pub restarts: u32,
    /// The most recent panic payload, truncated. `None` if it has never panicked.
    pub last_panic: Option<String>,
    /// RFC3339 timestamp of the most recent restart.
    pub last_restart_at: Option<String>,
}

/// Every supervised loop's health, keyed by name.
///
/// Held behind one `RwLock` over a small map. The guard is never held across an
/// `await` — the same rule [`crate::state::ControlState`] documents, for the same
/// reason.
#[derive(Debug, Default)]
pub struct SubsystemRegistry {
    inner: RwLock<BTreeMap<&'static str, SubsystemHealth>>,
}

impl SubsystemRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    async fn mark_running(&self, name: &'static str) {
        let mut map = self.inner.write().await;
        map.entry(name).or_default().running = true;
    }

    async fn mark_stopped(&self, name: &'static str) {
        let mut map = self.inner.write().await;
        map.entry(name).or_default().running = false;
    }

    async fn record_panic(&self, name: &'static str, detail: &str) {
        let mut map = self.inner.write().await;
        let entry = map.entry(name).or_default();
        entry.running = false;
        entry.restarts += 1;
        entry.last_panic = Some(detail.to_string());
        entry.last_restart_at = Some(chrono::Utc::now().to_rfc3339());
    }

    /// Full health for every known loop. Carries panic payloads, so this belongs
    /// behind auth.
    pub async fn snapshot(&self) -> BTreeMap<&'static str, SubsystemHealth> {
        self.inner.read().await.clone()
    }

    /// `name → restart count`, with no panic text. Safe for the public `/health`
    /// endpoint: a monitoring check only needs to see a non-zero count.
    pub async fn restart_counts(&self) -> BTreeMap<&'static str, u32> {
        self.inner
            .read()
            .await
            .iter()
            .map(|(name, h)| (*name, h.restarts))
            .collect()
    }

    /// Total restarts across every loop — the one number worth alerting on.
    pub async fn total_restarts(&self) -> u32 {
        self.inner.read().await.values().map(|h| h.restarts).sum()
    }
}

/// Aborts the task it holds when dropped.
///
/// The supervisor runs the loop in an inner `tokio::spawn` so a panic arrives as a
/// `JoinError` instead of unwinding the supervisor. That inner task is
/// independent, and dropping a bare `JoinHandle` only *detaches* — so aborting
/// the supervisor handle at shutdown would otherwise leave the loop running with
/// nothing watching it. This makes the two die together.
struct AbortOnDrop(Option<JoinHandle<()>>);

impl AbortOnDrop {
    fn handle(&mut self) -> &mut JoinHandle<()> {
        self.0.as_mut().expect("inner handle is taken only on drop")
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(h) = self.0.take() {
            // A no-op if the task already finished, which is the common path.
            h.abort();
        }
    }
}

/// Spawn `factory()` and keep it running: restart it whenever it panics, with
/// capped backoff, recording every restart in `registry`.
///
/// `factory` is called once per run, so it must be able to build the loop's
/// future more than once — clone the `Arc`s it needs inside the closure rather
/// than moving them in.
///
/// The returned handle is the *supervisor's*. Dropping it is safe — the supervisor
/// keeps running — but calling `abort()` on it stops both supervision and the loop
/// it is watching, which is what the shutdown path wants.
///
/// Restarts are unlimited on purpose: giving up would put us back where we started,
/// with a subsystem that is silently gone. Outbound alerts are throttled to one per
/// [`ALERT_MIN_INTERVAL`] per loop instead, so a crash-loop is reported rather than
/// used as a siren.
pub fn spawn_supervised<F, Fut>(
    name: &'static str,
    registry: Arc<SubsystemRegistry>,
    alerts: Option<AlertConfig>,
    factory: F,
) -> JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = RESTART_BACKOFF_MIN;
        let mut last_alert: Option<Instant> = None;
        loop {
            registry.mark_running(name).await;
            let started = Instant::now();

            // Inner spawn so a panic surfaces as a JoinError instead of
            // unwinding the supervisor itself; the guard ties the two lifetimes
            // together so aborting supervision also stops the loop.
            let mut inner = AbortOnDrop(Some(tokio::spawn(factory())));
            let result = inner.handle().await;
            let ran_for = started.elapsed();

            match result {
                Ok(()) => {
                    registry.mark_stopped(name).await;
                    info!(
                        subsystem = name,
                        ran_for_secs = ran_for.as_secs(),
                        "background loop returned; not restarting"
                    );
                    return;
                }
                Err(e) if e.is_panic() => {
                    let detail = panic_detail(e.into_panic());
                    registry.record_panic(name, &detail).await;

                    if ran_for >= BACKOFF_RESET_AFTER {
                        backoff = RESTART_BACKOFF_MIN;
                    }
                    error!(
                        subsystem = name,
                        panic = %detail,
                        ran_for_secs = ran_for.as_secs(),
                        restart_in_secs = backoff.as_secs(),
                        "background loop panicked; restarting"
                    );
                    let due = last_alert.is_none_or(|at| at.elapsed() >= ALERT_MIN_INTERVAL);
                    if due {
                        last_alert = Some(Instant::now());
                        crate::alerts::notify_failure(
                            alerts.as_ref(),
                            name,
                            &format!("panicked after {}s: {detail}", ran_for.as_secs()),
                        )
                        .await;
                    }

                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RESTART_BACKOFF_MAX);
                }
                Err(e) => {
                    // Cancelled — process teardown. Restarting would fight it.
                    registry.mark_stopped(name).await;
                    warn!(
                        subsystem = name,
                        error = %e,
                        "background loop was cancelled; not restarting"
                    );
                    return;
                }
            }
        }
    })
}

/// Best-effort readable form of a panic payload. `panic!("msg")` yields a
/// `&str`, `panic!("{x}")` a `String`, and anything else is opaque.
fn panic_detail(payload: Box<dyn std::any::Any + Send>) -> String {
    let raw = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    };
    let mut detail: String = raw.chars().filter(|c| *c != '\n').take(400).collect();
    if raw.chars().count() > 400 {
        detail.push('…');
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn a_panicking_loop_is_restarted_and_recorded() {
        let registry = SubsystemRegistry::new();
        let attempts = Arc::new(AtomicU32::new(0));

        let handle = {
            let attempts = attempts.clone();
            spawn_supervised("flaky", registry.clone(), None, move || {
                let attempts = attempts.clone();
                async move {
                    let n = attempts.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        panic!("tick {n} exploded");
                    }
                    // Third run returns cleanly, which ends supervision.
                }
            })
        };

        tokio::time::timeout(Duration::from_secs(20), handle)
            .await
            .expect("supervisor should finish once the loop returns cleanly")
            .expect("supervisor task itself must not panic");

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            3,
            "two restarts then success"
        );

        let health = registry.snapshot().await;
        let flaky = health.get("flaky").expect("registry knows the loop");
        assert_eq!(flaky.restarts, 2);
        assert!(!flaky.running, "a returned loop is not running");
        assert!(
            flaky
                .last_panic
                .as_deref()
                .is_some_and(|d| d.contains("tick 1 exploded")),
            "the latest panic payload is kept: {:?}",
            flaky.last_panic
        );
        assert!(flaky.last_restart_at.is_some());
    }

    #[tokio::test]
    async fn a_clean_return_is_not_a_restart() {
        let registry = SubsystemRegistry::new();
        let handle = spawn_supervised("tidy", registry.clone(), None, || async {});

        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("returns promptly")
            .expect("no panic");

        assert_eq!(registry.total_restarts().await, 0);
        let health = registry.snapshot().await;
        assert!(!health["tidy"].running);
        assert!(health["tidy"].last_panic.is_none());
    }

    #[tokio::test]
    async fn aborting_the_supervisor_also_stops_the_loop() {
        let registry = SubsystemRegistry::new();
        let ticks = Arc::new(AtomicU32::new(0));

        let handle = {
            let ticks = ticks.clone();
            spawn_supervised("busy", registry.clone(), None, move || {
                let ticks = ticks.clone();
                async move {
                    loop {
                        ticks.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }
            })
        };

        // Let it get going, then abort supervision the way the shutdown path does.
        tokio::time::sleep(Duration::from_millis(60)).await;
        let before = ticks.load(Ordering::SeqCst);
        assert!(before > 0, "the loop should have ticked at least once");

        handle.abort();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let settled = ticks.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(
            ticks.load(Ordering::SeqCst),
            settled,
            "the inner loop must stop with the supervisor, not detach and keep running"
        );
    }

    #[tokio::test]
    async fn restart_counts_carry_no_panic_text() {
        let registry = SubsystemRegistry::new();
        registry
            .record_panic("leaky", "secret: /Users/someone/private")
            .await;

        let counts = registry.restart_counts().await;
        assert_eq!(counts.get("leaky"), Some(&1));
        // The public shape is name → count; nothing else can leak through it.
        let json = serde_json::to_string(&counts).unwrap();
        assert!(
            !json.contains("secret"),
            "public counts must not carry detail"
        );
    }

    #[test]
    fn panic_detail_handles_both_payload_shapes() {
        assert_eq!(panic_detail(Box::new("literal")), "literal");
        assert_eq!(
            panic_detail(Box::new(String::from("formatted"))),
            "formatted"
        );
        assert_eq!(panic_detail(Box::new(7u8)), "non-string panic payload");

        let long = "x".repeat(500);
        let out = panic_detail(Box::new(long));
        assert_eq!(
            out.chars().count(),
            401,
            "truncated to 400 plus an ellipsis"
        );
        assert!(out.ends_with('…'));
    }

    #[tokio::test]
    async fn backoff_is_capped() {
        // Sanity-check the arithmetic the loop relies on rather than sleeping for it.
        let mut backoff = RESTART_BACKOFF_MIN;
        for _ in 0..20 {
            backoff = (backoff * 2).min(RESTART_BACKOFF_MAX);
        }
        assert_eq!(backoff, RESTART_BACKOFF_MAX);
    }
}
