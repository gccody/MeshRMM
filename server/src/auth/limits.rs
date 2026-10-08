//! In-memory state for sign-in: attempt limits, pending second-factor
//! challenges and the first-run setup token. The server runs as one node, so
//! process memory is enough; a restart only resets the counters.
use std::{
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use subtle::ConstantTimeEq;
use webauthn_rs::prelude::PasskeyAuthentication;

use crate::secrets::{new_token, token_hash};

/// Above this many tracked keys, keys whose attempts have all aged out are
/// dropped, so random keys can't grow memory without bound.
const PRUNE_THRESHOLD: usize = 10_000;

/// Allows `limit` attempts per key in any `window`.
#[derive(Debug)]
pub struct RateLimiter {
    limit: usize,
    window: Duration,
    attempts: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl RateLimiter {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self {
            limit,
            window,
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// Counts an attempt, or returns `Err(seconds until one ages out)` if
    /// the key has used every attempt. Check and count are one step, so
    /// parallel requests can't all pass before any is counted. An attempt
    /// that turns out to be legitimate is handed back with [`Self::refund`].
    pub fn hit(&self, key: &str) -> Result<(), u64> {
        self.hit_at(key, Instant::now())
    }

    /// Hands back the key's most recent attempt.
    pub fn refund(&self, key: &str) {
        if let Some(times) = self.lock().get_mut(key) {
            times.pop_back();
        }
    }

    pub fn clear(&self, key: &str) {
        self.lock().remove(key);
    }

    fn hit_at(&self, key: &str, now: Instant) -> Result<(), u64> {
        let mut attempts = self.lock();
        if attempts.len() >= PRUNE_THRESHOLD {
            attempts.retain(|_, times| {
                Self::expire(times, now, self.window);
                !times.is_empty()
            });
        }
        let times = attempts.entry(key.to_owned()).or_default();
        Self::expire(times, now, self.window);
        if times.len() >= self.limit {
            let oldest = times.front().copied().unwrap_or(now);
            return Err((oldest + self.window)
                .saturating_duration_since(now)
                .as_secs()
                .max(1));
        }
        times.push_back(now);
        Ok(())
    }

    fn expire(times: &mut VecDeque<Instant>, now: Instant, window: Duration) {
        while times
            .front()
            .is_some_and(|time| now.saturating_duration_since(*time) >= window)
        {
            times.pop_front();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, VecDeque<Instant>>> {
        // A panic while holding the lock leaves only counters behind.
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The rate-limit key for a client address. An IPv6 client usually controls
/// a whole /64, so its addresses share one key.
pub fn ip_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(address) => address.to_string(),
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => {
                let segments = address.segments();
                format!(
                    "{:x}:{:x}:{:x}:{:x}::/64",
                    segments[0], segments[1], segments[2], segments[3]
                )
            }
        },
    }
}

/// Second-factor challenges issued after a correct password.
#[derive(Debug)]
pub struct Challenges {
    ttl: Duration,
    max_attempts: u32,
    pending: Mutex<HashMap<String, Challenge>>,
}

#[derive(Debug)]
struct Challenge {
    user_id: String,
    passkey: Option<PasskeyAuthentication>,
    expires: Instant,
    attempts: u32,
}

/// An attempt at a challenge: whose it is, and the passkey prompt it
/// offered, if the user has passkeys.
#[derive(Debug)]
pub struct ChallengeAttempt {
    pub user_id: String,
    pub passkey: Option<PasskeyAuthentication>,
}

impl Challenges {
    pub fn new(ttl: Duration, max_attempts: u32) -> Self {
        Self {
            ttl,
            max_attempts,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Issues a challenge for `user_id` and returns its token. `passkey` is
    /// the state of the passkey prompt sent with it.
    pub fn issue(&self, user_id: &str, passkey: Option<PasskeyAuthentication>) -> String {
        let token = new_token();
        let now = Instant::now();
        let mut pending = self.lock();
        pending.retain(|_, challenge| challenge.expires > now);
        pending.insert(
            token_hash(&token),
            Challenge {
                user_id: user_id.to_owned(),
                passkey,
                expires: now + self.ttl,
                attempts: 0,
            },
        );
        token
    }

    /// Counts an attempt at the challenge and returns it, or `None` if it is
    /// unknown, expired, or out of attempts (which also removes it). With
    /// `take_passkey`, the passkey prompt is taken out in the same step, so
    /// one answer to it can't be checked twice, even by parallel requests.
    pub fn attempt(&self, token: &str, take_passkey: bool) -> Option<ChallengeAttempt> {
        let key = token_hash(token);
        let mut pending = self.lock();
        let challenge = pending.get_mut(&key)?;
        if challenge.expires <= Instant::now() || challenge.attempts >= self.max_attempts {
            pending.remove(&key);
            return None;
        }
        challenge.attempts += 1;
        Some(ChallengeAttempt {
            user_id: challenge.user_id.clone(),
            passkey: if take_passkey {
                challenge.passkey.take()
            } else {
                None
            },
        })
    }

    pub fn complete(&self, token: &str) {
        self.lock().remove(&token_hash(token));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Challenge>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The one-time token that lets the first visitor create the administrator.
#[derive(Debug, Default)]
pub struct SetupGate {
    token_hash: Mutex<Option<String>>,
}

impl SetupGate {
    /// Issues a new token, replacing any earlier one.
    pub fn issue(&self) -> String {
        let token = new_token();
        *self.lock() = Some(token_hash(&token));
        token
    }

    /// Whether `token` is the current token, without using it up.
    pub fn matches(&self, token: &str) -> bool {
        self.lock()
            .as_deref()
            .is_some_and(|hash| hash.as_bytes().ct_eq(token_hash(token).as_bytes()).into())
    }

    /// Takes the token if `token` is it, so only one request can use it.
    pub fn consume(&self, token: &str) -> bool {
        let mut current = self.lock();
        let matches = current
            .as_deref()
            .is_some_and(|hash| hash.as_bytes().ct_eq(token_hash(token).as_bytes()).into());
        if matches {
            *current = None;
        }
        matches
    }

    /// Puts back a consumed token after setup failed for another reason.
    pub fn restore(&self, token: &str) {
        let mut current = self.lock();
        if current.is_none() {
            *current = Some(token_hash(token));
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.token_hash
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limit_blocks_until_attempts_age_out() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        let start = Instant::now();
        assert!(limiter.hit_at("a", start).is_ok());
        assert!(limiter.hit_at("a", start + Duration::from_secs(10)).is_ok());
        assert_eq!(
            limiter.hit_at("a", start + Duration::from_secs(20)),
            Err(40)
        );
        assert!(limiter.hit_at("b", start).is_ok());
        assert!(limiter.hit_at("a", start + Duration::from_secs(60)).is_ok());
        limiter.clear("a");
        assert!(limiter.hit_at("a", start + Duration::from_secs(20)).is_ok());
    }

    #[test]
    fn refunded_attempts_dont_count() {
        let limiter = RateLimiter::new(1, Duration::from_secs(60));
        let start = Instant::now();
        assert!(limiter.hit_at("a", start).is_ok());
        limiter.refund("a");
        assert!(limiter.hit_at("a", start).is_ok());
        assert!(limiter.hit_at("a", start).is_err());
    }

    #[test]
    fn stale_keys_are_pruned() {
        let limiter = RateLimiter::new(1, Duration::from_secs(1));
        let start = Instant::now();
        for key in 0..PRUNE_THRESHOLD {
            limiter.hit_at(&key.to_string(), start).unwrap();
        }
        limiter
            .hit_at("late", start + Duration::from_secs(2))
            .unwrap();
        assert_eq!(limiter.lock().len(), 1);
    }

    #[test]
    fn ipv6_clients_share_a_key_per_64() {
        let key = |text: &str| ip_key(text.parse().unwrap());
        assert_eq!(key("203.0.113.9"), "203.0.113.9");
        assert_eq!(key("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(key("2001:db8:1:2:aaaa::1"), key("2001:db8:1:2:bbbb::2"));
        assert_ne!(key("2001:db8:1:2::1"), key("2001:db8:1:3::1"));
    }

    #[test]
    fn challenges_allow_limited_attempts() {
        let user = |attempt: Option<ChallengeAttempt>| attempt.map(|attempt| attempt.user_id);
        let challenges = Challenges::new(Duration::from_secs(60), 2);
        let token = challenges.issue("user-1", None);
        assert_eq!(
            user(challenges.attempt(&token, false)).as_deref(),
            Some("user-1")
        );
        assert_eq!(
            user(challenges.attempt(&token, false)).as_deref(),
            Some("user-1")
        );
        assert_eq!(user(challenges.attempt(&token, false)), None);
        assert_eq!(user(challenges.attempt(&token, false)), None);
        assert_eq!(user(challenges.attempt("unknown", false)), None);

        let token = challenges.issue("user-2", None);
        challenges.complete(&token);
        assert_eq!(user(challenges.attempt(&token, false)), None);

        let expired = Challenges::new(Duration::ZERO, 5);
        let token = expired.issue("user-3", None);
        assert_eq!(user(expired.attempt(&token, false)), None);
    }

    #[test]
    fn the_setup_token_works_once() {
        let gate = SetupGate::default();
        assert!(!gate.consume("anything"));
        let token = gate.issue();
        assert!(!gate.consume("wrong"));
        assert!(gate.matches(&token) && !gate.matches("wrong"));
        assert!(gate.consume(&token));
        assert!(!gate.matches(&token));
        assert!(!gate.consume(&token));
        gate.restore(&token);
        assert!(gate.consume(&token));
        let replaced = gate.issue();
        let newer = gate.issue();
        assert!(!gate.consume(&replaced));
        assert!(gate.consume(&newer));
    }
}
