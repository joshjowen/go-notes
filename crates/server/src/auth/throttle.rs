//! Rate limiting for password logins.
//!
//! Argon2 makes offline cracking expensive, but it does nothing about someone
//! working through a password list against the live endpoint — and because the
//! hash is deliberately slow, an unthrottled login endpoint is also a cheap way
//! to exhaust the server's CPU.
//!
//! State is in memory rather than in Postgres. A lockout that resets when the
//! process restarts is a real limitation, but the alternative — a database write
//! on every failed attempt — turns the login endpoint into an amplification
//! vector, which is worse. This is a speed bump against guessing, not a defence
//! against a determined distributed attacker; that is what Authelia is for, and
//! why the OIDC path exists.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Failures tolerated before the first lockout.
const FREE_ATTEMPTS: u32 = 5;

/// Lockout after exceeding the allowance, doubling with each further failure.
const BASE_LOCKOUT: Duration = Duration::from_secs(15);
const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// How long a quiet key is remembered before being forgotten entirely.
const FORGET_AFTER: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone)]
struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

#[derive(Debug, Default)]
pub struct LoginThrottle {
    entries: Mutex<HashMap<String, Attempts>>,
}

impl LoginThrottle {
    pub fn new() -> LoginThrottle {
        LoginThrottle::default()
    }

    /// The key a login attempt is counted against.
    ///
    /// Both the client address and the username are included. Keying on the
    /// address alone would let one attacker lock out everyone behind a shared
    /// NAT; keying on the username alone would let an attacker lock a specific
    /// person out of their own account at will. Combining them means an attacker
    /// slows only their own attempts against one account.
    pub fn key(client: &str, username: &str) -> String {
        format!("{client}|{}", username.to_lowercase())
    }

    /// Returns how long the caller must wait, or `None` if they may proceed.
    pub fn check(&self, key: &str) -> Option<Duration> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = entries.get(key)?;
        let locked_until = entry.locked_until?;

        let now = Instant::now();
        if locked_until > now {
            Some(locked_until - now)
        } else {
            None
        }
    }

    pub fn record_failure(&self, key: &str) {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        Self::prune(&mut entries, now);

        let entry = entries.entry(key.to_string()).or_insert(Attempts {
            failures: 0,
            locked_until: None,
            last_seen: now,
        });

        entry.failures += 1;
        entry.last_seen = now;

        if entry.failures > FREE_ATTEMPTS {
            // Each failure past the allowance doubles the wait, capped so a key
            // is never locked out permanently.
            let over = entry.failures - FREE_ATTEMPTS - 1;
            let lockout = BASE_LOCKOUT
                .saturating_mul(2u32.saturating_pow(over.min(16)))
                .min(MAX_LOCKOUT);
            entry.locked_until = Some(now + lockout);
        }
    }

    /// Clears the counter after a successful sign-in, so a person who mistypes
    /// their password twice and then gets it right starts fresh.
    pub fn record_success(&self, key: &str) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.remove(key);
    }

    fn prune(entries: &mut HashMap<String, Attempts>, now: Instant) {
        // Bounded so a flood of distinct usernames cannot grow the map without
        // limit; the oldest entries are the least useful to keep.
        if entries.len() < 10_000 {
            entries.retain(|_, entry| now.duration_since(entry.last_seen) < FORGET_AFTER);
        } else {
            entries.retain(|_, entry| {
                entry.locked_until.is_some_and(|until| until > now)
                    || now.duration_since(entry.last_seen) < Duration::from_secs(300)
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_attempts_up_to_the_allowance() {
        let throttle = LoginThrottle::new();
        let key = LoginThrottle::key("10.0.0.1", "josh");

        for _ in 0..FREE_ATTEMPTS {
            assert!(throttle.check(&key).is_none());
            throttle.record_failure(&key);
        }
        assert!(throttle.check(&key).is_none(), "should still be free here");

        throttle.record_failure(&key);
        assert!(throttle.check(&key).is_some(), "should now be locked out");
    }

    #[test]
    fn lockout_grows_with_repeated_failures() {
        let throttle = LoginThrottle::new();
        let key = LoginThrottle::key("10.0.0.1", "josh");

        for _ in 0..=FREE_ATTEMPTS {
            throttle.record_failure(&key);
        }
        let first = throttle.check(&key).unwrap();

        throttle.record_failure(&key);
        let second = throttle.check(&key).unwrap();

        assert!(second > first, "{second:?} should exceed {first:?}");
        assert!(second <= MAX_LOCKOUT);
    }

    #[test]
    fn lockout_is_capped() {
        let throttle = LoginThrottle::new();
        let key = LoginThrottle::key("10.0.0.1", "josh");
        for _ in 0..200 {
            throttle.record_failure(&key);
        }
        assert!(throttle.check(&key).unwrap() <= MAX_LOCKOUT);
    }

    #[test]
    fn success_clears_the_counter() {
        let throttle = LoginThrottle::new();
        let key = LoginThrottle::key("10.0.0.1", "josh");
        for _ in 0..=FREE_ATTEMPTS {
            throttle.record_failure(&key);
        }
        assert!(throttle.check(&key).is_some());

        throttle.record_success(&key);
        assert!(throttle.check(&key).is_none());
    }

    /// The property that makes this safe to deploy: an attacker hammering one
    /// account from one address must not affect anybody else.
    #[test]
    fn lockouts_do_not_leak_between_users_or_addresses() {
        let throttle = LoginThrottle::new();
        let attacker = LoginThrottle::key("10.0.0.1", "josh");
        for _ in 0..=FREE_ATTEMPTS {
            throttle.record_failure(&attacker);
        }
        assert!(throttle.check(&attacker).is_some());

        // The real user, at a different address.
        assert!(throttle.check(&LoginThrottle::key("10.0.0.2", "josh")).is_none());
        // A different account from the attacker's own address.
        assert!(throttle.check(&LoginThrottle::key("10.0.0.1", "alice")).is_none());
    }

    #[test]
    fn keys_ignore_username_case() {
        assert_eq!(
            LoginThrottle::key("10.0.0.1", "Josh"),
            LoginThrottle::key("10.0.0.1", "josh")
        );
    }
}
