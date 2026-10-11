//! Brute-force backoff for vault unlock attempts.
//!
//! Argon2id at 64 MiB is already a throttle — each guess costs real wall-clock
//! time and real memory, so an attacker cannot spray millions of attempts a
//! second. But "slow per attempt" is not "bounded total": someone who has stolen
//! a valid session token can still grind the master password one expensive guess
//! at a time, indefinitely, and a self-hosted gateway on the open internet is a
//! reachable target. This adds the second layer every serious password manager
//! has — an explicit, escalating lockout per principal.
//!
//! # Shape
//!
//! Failures are counted per principal. The first few are free (fat-fingering your
//! own password should not lock you out), then each further failure imposes a
//! delay that doubles, capped. A success clears the record immediately. The delay
//! is *advisory to the client* (returned as `retryAfterSeconds`) and *enforced at
//! the server* (the unlock is refused until it elapses), so a hostile client that
//! ignores the hint gains nothing.
//!
//! # Why in-memory
//!
//! The counter lives in memory, not the vault file. Persisting it would let an
//! attacker with disk access reset it, and would risk locking a user out across a
//! restart they did not cause. A process restart clearing the counter is an
//! acceptable amount of forgiveness: the expensive KDF still bounds what that
//! window is worth, and a restart is not something a remote attacker controls.

use std::time::{Duration, Instant};

use dashmap::DashMap;

/// Failures before any delay is imposed. Covers ordinary mistyping.
const FREE_ATTEMPTS: u32 = 5;
/// Base delay applied to the first throttled failure; doubles thereafter.
const BASE_DELAY: Duration = Duration::from_secs(1);
/// Ceiling on the backoff. Fifteen minutes is long enough to make grinding
/// pointless without permanently bricking a vault whose owner really did forget
/// which password they set.
const MAX_DELAY: Duration = Duration::from_secs(15 * 60);
/// A record with no failure this long is forgotten, so an honest user who trips
/// the limit and walks away comes back to a clean slate.
const RECORD_TTL: Duration = Duration::from_secs(60 * 60);

struct Record {
    failures: u32,
    /// When the most recent failure's penalty ends.
    locked_until: Option<Instant>,
    last_failure: Instant,
}

// Free attempts, base delay and cap are configurable so tests can use small
// numbers; production always goes through `Default` (see the constants above).
/// Per-principal unlock-attempt throttle.
pub struct UnlockThrottle {
    records: DashMap<String, Record>,
    free_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
}

impl Default for UnlockThrottle {
    fn default() -> Self {
        Self::new(FREE_ATTEMPTS, BASE_DELAY, MAX_DELAY)
    }
}

impl UnlockThrottle {
    pub fn new(free_attempts: u32, base_delay: Duration, max_delay: Duration) -> Self {
        Self {
            records: DashMap::new(),
            free_attempts,
            base_delay,
            max_delay,
        }
    }

    /// Seconds the principal must wait before another unlock attempt, or `None`
    /// if they may try now. Call before attempting an unlock.
    pub fn retry_after(&self, principal_id: &str) -> Option<u64> {
        let record = self.records.get(principal_id)?;
        let locked_until = record.locked_until?;
        let now = Instant::now();
        if now >= locked_until {
            None
        } else {
            Some((locked_until - now).as_secs().max(1))
        }
    }

    /// Record a failed attempt and return the delay now imposed, if any.
    ///
    /// The delay escalates: free attempts cost nothing, then `base * 2^n` capped
    /// at `max`. The doubling is what turns a stolen-session grind from feasible
    /// into hopeless — a dozen wrong guesses already means quarter-hour waits.
    pub fn record_failure(&self, principal_id: &str) -> Option<u64> {
        let now = Instant::now();
        let mut record = self
            .records
            .entry(principal_id.to_string())
            .or_insert(Record {
                failures: 0,
                locked_until: None,
                last_failure: now,
            });

        // Forgive a long-idle record before counting this failure against it.
        if now.duration_since(record.last_failure) > RECORD_TTL {
            record.failures = 0;
        }
        record.failures += 1;
        record.last_failure = now;

        if record.failures <= self.free_attempts {
            record.locked_until = None;
            return None;
        }

        let over = record.failures - self.free_attempts - 1;
        // Saturating shift so a large streak cannot overflow into a small delay.
        let multiplier = 1u64.checked_shl(over.min(32)).unwrap_or(u64::MAX);
        let delay = self
            .base_delay
            .saturating_mul(multiplier.min(u32::MAX as u64) as u32)
            .min(self.max_delay);

        record.locked_until = Some(now + delay);
        Some(delay.as_secs().max(1))
    }

    /// Clear a principal's record after a successful unlock.
    pub fn record_success(&self, principal_id: &str) {
        self.records.remove(principal_id);
    }

    /// Drop records that have been idle past the TTL. Called from the same sweep
    /// loop that re-locks idle vaults, so the map cannot grow without bound under
    /// a spray of distinct principal ids.
    pub fn sweep(&self) {
        let now = Instant::now();
        self.records
            .retain(|_, r| now.duration_since(r.last_failure) <= RECORD_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn throttle() -> UnlockThrottle {
        UnlockThrottle::new(3, Duration::from_secs(2), Duration::from_secs(60))
    }

    #[test]
    fn the_first_attempts_are_free() {
        let t = throttle();
        for _ in 0..3 {
            assert_eq!(t.record_failure("u"), None);
        }
        assert_eq!(t.retry_after("u"), None, "still free after the allowance");
    }

    #[test]
    fn delays_escalate_after_the_allowance() {
        let t = throttle();
        for _ in 0..3 {
            t.record_failure("u");
        }
        // 4th failure: base (2s). 5th: 4s. 6th: 8s.
        assert_eq!(t.record_failure("u"), Some(2));
        assert_eq!(t.record_failure("u"), Some(4));
        assert_eq!(t.record_failure("u"), Some(8));
    }

    #[test]
    fn the_delay_is_capped() {
        let t = throttle();
        // Enough failures to blow well past the 60s cap.
        for _ in 0..40 {
            t.record_failure("u");
        }
        let last = t.record_failure("u").unwrap();
        assert!(last <= 60, "delay {last} exceeded the cap");
    }

    #[test]
    fn retry_after_reports_a_pending_lock() {
        let t = throttle();
        for _ in 0..4 {
            t.record_failure("u");
        }
        // A lock is in effect right after the throttled failure.
        assert!(t.retry_after("u").is_some());
    }

    #[test]
    fn success_clears_the_record() {
        let t = throttle();
        for _ in 0..5 {
            t.record_failure("u");
        }
        assert!(t.retry_after("u").is_some());

        t.record_success("u");
        assert_eq!(t.retry_after("u"), None, "a success must wipe the penalty");
        // And the count resets, so the allowance is whole again.
        assert_eq!(t.record_failure("u"), None);
    }

    #[test]
    fn principals_are_throttled_independently() {
        let t = throttle();
        for _ in 0..5 {
            t.record_failure("attacker");
        }
        assert!(t.retry_after("attacker").is_some());
        assert_eq!(
            t.retry_after("victim"),
            None,
            "one user must not lock another"
        );
    }

    #[test]
    fn a_huge_streak_does_not_overflow_into_a_small_delay() {
        let t = throttle();
        for _ in 0..100 {
            t.record_failure("u");
        }
        // Must still be the cap, never a wrapped-around tiny value.
        assert_eq!(t.record_failure("u"), Some(60));
    }
}
