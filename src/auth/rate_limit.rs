//! In-memory backoff on the login endpoint (ADR-0003). One account means
//! one counter: guessing is slowed for everyone at once.

use std::time::{Duration, SystemTime};

/// Failures tolerated before the first lockout.
pub const FREE_ATTEMPTS: u32 = 5;

/// How long the first lockout lasts; each further failure doubles it.
pub const FIRST_LOCKOUT: Duration = Duration::from_secs(5);

/// The longest a lockout grows to.
pub const LONGEST_LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Tracks consecutive login failures and the lockout they earned.
#[derive(Debug, Default)]
pub struct LoginLimiter {
    failures: u32,
    locked_until: Option<SystemTime>,
}

impl LoginLimiter {
    /// Whether login attempts are being refused right now.
    #[must_use]
    pub fn is_locked(&self, _now: SystemTime) -> bool {
        todo!()
    }

    /// Count a rejected attempt, extending the lockout.
    pub fn record_failure(&mut self, _now: SystemTime) {
        todo!()
    }

    /// Forget past failures after a successful login.
    pub fn record_success(&mut self) {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn fail(limiter: &mut LoginLimiter, times: u32, now: SystemTime) {
        for _ in 0..times {
            limiter.record_failure(now);
        }
    }

    #[test]
    fn a_fresh_limiter_allows_attempts() {
        assert!(!LoginLimiter::default().is_locked(at(0)));
    }

    #[test]
    fn failures_below_the_threshold_stay_open() {
        let mut limiter = LoginLimiter::default();

        fail(&mut limiter, FREE_ATTEMPTS - 1, at(0));

        assert!(!limiter.is_locked(at(0)));
    }

    #[test]
    fn the_threshold_failure_locks_further_attempts() {
        let mut limiter = LoginLimiter::default();

        fail(&mut limiter, FREE_ATTEMPTS, at(0));

        assert!(limiter.is_locked(at(0)));
    }

    #[test]
    fn the_lockout_lifts_once_it_elapses() {
        let mut limiter = LoginLimiter::default();
        fail(&mut limiter, FREE_ATTEMPTS, at(0));

        assert!(!limiter.is_locked(at(0) + FIRST_LOCKOUT));
    }

    #[test]
    fn each_further_failure_doubles_the_lockout() {
        let mut limiter = LoginLimiter::default();
        fail(&mut limiter, FREE_ATTEMPTS + 1, at(0));

        assert!(limiter.is_locked(at(0) + FIRST_LOCKOUT));
        assert!(!limiter.is_locked(at(0) + FIRST_LOCKOUT * 2));
    }

    #[test]
    fn the_lockout_stops_growing_at_the_cap() {
        let mut limiter = LoginLimiter::default();
        fail(&mut limiter, FREE_ATTEMPTS + 20, at(0));

        assert!(!limiter.is_locked(at(0) + LONGEST_LOCKOUT));
    }

    #[test]
    fn a_success_forgets_past_failures() {
        let mut limiter = LoginLimiter::default();
        fail(&mut limiter, FREE_ATTEMPTS, at(0));

        limiter.record_success();

        assert!(!limiter.is_locked(at(0)));
    }
}
