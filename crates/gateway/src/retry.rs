//! Task retry policy for the durable overnight loop.
//!
//! Replaces the old hardcoded "retry twice" behavior with an explicit policy:
//! a max attempt count and exponential backoff. Only `RetryableFailure` outcomes
//! are retried; `Failed` / `BlockedByPolicy` are terminal (non-retryable) and
//! never reach this policy. Pure and deterministic.

/// Retry configuration.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts allowed (including the first). After this many failed
    /// attempts, the task is exhausted and marked `Failed`.
    pub max_attempts: u32,
    /// Base backoff in seconds; delay is `base * 2^(attempt-1)`, capped.
    pub base_backoff_secs: u64,
    /// Upper bound on a single backoff delay.
    pub max_backoff_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            max_attempts: 3,
            base_backoff_secs: 30,
            max_backoff_secs: 3600,
        }
    }
}

/// Result of evaluating a failed attempt against a [`RetryPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// Schedule another attempt after `delay_secs`.
    Retry { attempt: u32, delay_secs: u64 },
    /// No attempts left; the task should be failed.
    Exhausted { attempts: u32 },
}

impl RetryPolicy {
    /// Decide what to do after `attempt_number` (1-based) has just failed.
    pub fn evaluate(&self, attempt_number: u32) -> RetryOutcome {
        if attempt_number >= self.max_attempts {
            return RetryOutcome::Exhausted {
                attempts: attempt_number,
            };
        }
        let shift = attempt_number.saturating_sub(1).min(16);
        let delay = self
            .base_backoff_secs
            .saturating_mul(1u64 << shift)
            .min(self.max_backoff_secs);
        RetryOutcome::Retry {
            attempt: attempt_number,
            delay_secs: delay,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_with_exponential_backoff_then_exhausts() {
        let p = RetryPolicy {
            max_attempts: 3,
            base_backoff_secs: 30,
            max_backoff_secs: 3600,
        };
        assert_eq!(
            p.evaluate(1),
            RetryOutcome::Retry {
                attempt: 1,
                delay_secs: 30
            }
        );
        assert_eq!(
            p.evaluate(2),
            RetryOutcome::Retry {
                attempt: 2,
                delay_secs: 60
            }
        );
        assert_eq!(p.evaluate(3), RetryOutcome::Exhausted { attempts: 3 });
        assert_eq!(p.evaluate(9), RetryOutcome::Exhausted { attempts: 9 });
    }

    #[test]
    fn backoff_is_capped() {
        let p = RetryPolicy {
            max_attempts: 20,
            base_backoff_secs: 60,
            max_backoff_secs: 300,
        };
        // 60 * 2^9 would be huge, but it's capped at 300.
        assert_eq!(
            p.evaluate(10),
            RetryOutcome::Retry {
                attempt: 10,
                delay_secs: 300
            }
        );
    }
}
