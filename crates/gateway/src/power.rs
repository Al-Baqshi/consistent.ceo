//! Keep the machine awake while — and only while — there is work to reach.
//!
//! A closed laptop lid is the quiet way the whole remote story fails: the
//! phone gets a push, the operator taps it, and nothing answers. But pinning a
//! Mac awake 24/7 to serve an idle gateway is a battery bug, not a feature.
//!
//! So the assertion is conditional: hold it while runs are in flight or a
//! fresh approval is waiting for an answer, drop it when everything is quiet.
//! macOS holds it with `caffeinate -is` (which dies with the gateway thanks to
//! `-w`), Windows with `SetThreadExecutionState` on a thread of its own; it is a
//! no-op everywhere else.

use std::sync::Arc;

use chrono::{DateTime, Duration as ChronoDuration, Utc};

use crate::state::{ApprovalStatus, ControlState, TaskStatus};

/// How long a pending approval keeps the machine awake. An approval nobody
/// answers within the hour is not an emergency; the push already landed.
const APPROVAL_WAKE_WINDOW_MINS: i64 = 60;

/// Should the machine be held awake right now?
///
/// Pure so the policy is testable without a process, a clock, or a Mac.
pub fn should_hold_awake(
    in_flight_runs: usize,
    fresh_approvals: usize,
    executable_queued: usize,
) -> bool {
    in_flight_runs > 0 || fresh_approvals > 0 || executable_queued > 0
}

/// Count the reasons to stay awake, from live state.
pub async fn wake_reasons(
    control: &Arc<ControlState>,
    now: DateTime<Utc>,
) -> (usize, usize, usize) {
    let in_flight = control
        .external_agent_runs
        .read()
        .await
        .iter()
        .filter(|r| r.status == "running")
        .count();
    let cutoff = now - ChronoDuration::minutes(APPROVAL_WAKE_WINDOW_MINS);
    let fresh_approvals = control
        .approvals
        .read()
        .await
        .iter()
        .filter(|a| matches!(a.status, ApprovalStatus::Pending))
        .filter(|a| {
            DateTime::parse_from_rfc3339(&a.created_at)
                .map(|t| t.with_timezone(&Utc) > cutoff)
                .unwrap_or(false)
        })
        .count();
    // Work already approved and queued: sleeping now would just stall it.
    let executable_queued = control
        .tasks
        .read()
        .await
        .iter()
        .filter(|t| t.status == TaskStatus::Queued && !t.requires_human_approval)
        .filter(|t| {
            t.metadata
                .as_ref()
                .and_then(|m| m.get(crate::task_executor::META_EXECUTABLE))
                .map(|v| v.as_bool().unwrap_or_else(|| v.as_str() == Some("true")))
                .unwrap_or(false)
        })
        .count();
    (in_flight, fresh_approvals, executable_queued)
}

/// Owns the platform's wake assertion, if held. Dropping it releases it.
#[derive(Default)]
pub struct PowerAssertion {
    hold: Option<Hold>,
}

impl PowerAssertion {
    pub fn held(&self) -> bool {
        self.hold.is_some()
    }

    fn start(&mut self) {
        if self.hold.is_some() {
            return;
        }
        if let Some(hold) = Hold::acquire() {
            tracing::info!("power: holding the machine awake (work in flight)");
            self.hold = Some(hold);
        }
    }

    fn stop(&mut self) {
        // `take()` drops the hold at the end of this statement, releasing the
        // assertion. (Not `drop(hold)`: on platforms without one, `Hold` has no
        // `Drop`, and clippy's `drop_non_drop` rejects that in CI.)
        if self.hold.take().is_some() {
            tracing::info!("power: released the wake assertion (idle)");
        }
    }

    pub fn apply(&mut self, hold: bool) {
        if hold {
            self.start();
        } else {
            self.stop();
        }
    }
}

impl Drop for PowerAssertion {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The `caffeinate` child holding the Mac awake.
#[cfg(target_os = "macos")]
struct Hold(std::process::Child);

#[cfg(target_os = "macos")]
impl Hold {
    fn acquire() -> Option<Self> {
        // `-i` prevents idle sleep, `-s` prevents system sleep on AC, and
        // `-w <pid>` ties the assertion's life to ours: if the gateway dies,
        // the Mac is free to sleep immediately.
        match std::process::Command::new("/usr/bin/caffeinate")
            .args(["-is", "-w", &std::process::id().to_string()])
            .spawn()
        {
            Ok(child) => Some(Self(child)),
            Err(e) => {
                tracing::warn!(error = %e, "power: caffeinate unavailable");
                None
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for Hold {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[cfg(windows)]
mod win {
    pub const ES_CONTINUOUS: u32 = 0x8000_0000;
    pub const ES_SYSTEM_REQUIRED: u32 = 0x0000_0001;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn SetThreadExecutionState(flags: u32) -> u32;
    }
}

/// A thread holding `ES_SYSTEM_REQUIRED`.
///
/// Windows scopes an execution state to the thread that set it, and the power
/// loop is a tokio task that can resume on any worker — so setting it from the
/// loop would leak the state onto whichever worker ran `start` and never clear
/// it from `stop`. A dedicated thread sets it, parks until released, clears it,
/// and exits (which would also clear it).
#[cfg(windows)]
struct Hold {
    release: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(windows)]
impl Hold {
    fn acquire() -> Option<Self> {
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (ready, acquired) = std::sync::mpsc::channel::<bool>();
        let thread = std::thread::Builder::new()
            .name("power-assertion".into())
            .spawn(move || {
                // SAFETY: plain Win32 call with documented flag values; no
                // pointers cross the boundary.
                let ok = unsafe {
                    win::SetThreadExecutionState(win::ES_CONTINUOUS | win::ES_SYSTEM_REQUIRED)
                } != 0;
                let _ = ready.send(ok);
                if ok {
                    // Blocks until the sender drops, i.e. `Hold` is released.
                    let _ = released.recv();
                    // SAFETY: as above.
                    unsafe { win::SetThreadExecutionState(win::ES_CONTINUOUS) };
                }
            })
            .ok()?;
        if acquired.recv().unwrap_or(false) {
            Some(Self {
                release: Some(release),
                thread: Some(thread),
            })
        } else {
            tracing::warn!("power: SetThreadExecutionState refused the wake assertion");
            let _ = thread.join();
            None
        }
    }
}

#[cfg(windows)]
impl Drop for Hold {
    fn drop(&mut self) {
        drop(self.release.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Platforms without a wake assertion never hold one.
#[cfg(not(any(target_os = "macos", windows)))]
struct Hold;

#[cfg(not(any(target_os = "macos", windows)))]
impl Hold {
    fn acquire() -> Option<Self> {
        None
    }
}

/// Poll live state and hold/release the assertion accordingly.
pub async fn run_loop(control: Arc<ControlState>, poll_secs: u64) {
    let mut assertion = PowerAssertion::default();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(poll_secs.max(15)));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        let (runs, approvals, queued) = wake_reasons(&control, Utc::now()).await;
        assertion.apply(should_hold_awake(runs, approvals, queued));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_gateway_lets_the_machine_sleep() {
        assert!(!should_hold_awake(0, 0, 0));
    }

    #[test]
    fn any_live_reason_holds_the_machine_awake() {
        assert!(should_hold_awake(1, 0, 0), "a running agent");
        assert!(should_hold_awake(0, 1, 0), "an approval waiting");
        assert!(should_hold_awake(0, 0, 1), "approved work queued");
    }

    #[test]
    fn the_assertion_is_released_when_work_ends() {
        let mut assertion = PowerAssertion::default();
        assertion.apply(true);
        // Holding is best-effort (a platform may refuse or not support it), but
        // releasing must always leave nothing held.
        if cfg!(any(target_os = "macos", windows)) {
            assert!(assertion.held(), "a supported platform should hold");
        }
        assertion.apply(false);
        assert!(!assertion.held());
    }
}
