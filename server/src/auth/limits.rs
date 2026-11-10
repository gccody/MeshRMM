//! In-memory state for sign-in: attempt limits, pending second-factor
//! challenges and the first-run setup token. The server runs as one node, so
//! process memory is enough; a restart only resets the counters.
use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use subtle::ConstantTimeEq;
use webauthn_rs::prelude::PasskeyAuthentication;

use super::ceremonies::Expiring;
use crate::secrets::{new_token, token_hash};

/// The most keys one limiter tracks, so random keys can't grow memory
/// without bound. Past it, a new key takes the place of the key with the
/// fewest attempts (see [`RateLimiter`]).
const MAX_KEYS: usize = 50_000;
/// Longer keys are kept as their hash, so a key built from client text can't
/// make an entry large.
const MAX_KEY_LENGTH: usize = 128;

/// Allows `limit` attempts per key in any `window`.
///
/// It tracks at most [`MAX_KEYS`] keys. Keys whose attempts have all aged out
/// are dropped as they age, oldest first. When every tracked key still has
/// attempts in the window, a new key is always let in, in place of the key
/// with the fewest attempts, least recently hit among those. A key at its
/// limit therefore goes only once every key is at its limit, and wiping a
/// count of `n` takes first giving every tracked key `n` attempts: about
/// [`MAX_KEYS`] times the attempts it frees. Refusing new keys instead would
/// let anyone with enough keys refuse everyone else.
///
/// Every operation takes time proportional to the attempts it expires, with
/// an occasional pass over the bookkeeping once as many changes have made
/// that much of it stale, never a pass over every key per attempt.
#[derive(Debug)]
pub struct RateLimiter {
    limit: usize,
    window: Duration,
    max_keys: usize,
    keys: Mutex<Keys>,
}

#[derive(Debug)]
struct Keys {
    entries: HashMap<Arc<str>, Entry>,
    /// Every hit as `(sequence, key)`, oldest first, to drop keys that aged
    /// out. A place whose sequence is not its key's latest is stale.
    by_hit: VecDeque<(u64, Arc<str>)>,
    /// The same hits, by how many attempts the key held after each: where to
    /// find the key with the fewest attempts, least recently hit.
    by_count: Vec<VecDeque<(u64, Arc<str>)>>,
    next_sequence: u64,
    /// Places in `by_hit` and `by_count` looked at, for tests to bound the
    /// work a hit does.
    #[cfg(test)]
    visited: usize,
}

#[derive(Debug)]
struct Entry {
    key: Arc<str>,
    times: VecDeque<Instant>,
    /// The sequence of the key's latest hit.
    sequence: u64,
    hit: Instant,
}

impl RateLimiter {
    pub fn new(limit: usize, window: Duration) -> Self {
        Self::with_max_keys(limit, window, MAX_KEYS)
    }

    fn with_max_keys(limit: usize, window: Duration, max_keys: usize) -> Self {
        Self {
            limit,
            window,
            max_keys,
            keys: Mutex::new(Keys {
                entries: HashMap::new(),
                by_hit: VecDeque::new(),
                by_count: (0..=limit).map(|_| VecDeque::new()).collect(),
                next_sequence: 0,
                #[cfg(test)]
                visited: 0,
            }),
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
        let key = bounded(key);
        let mut keys = self.lock();
        if let Some(entry) = keys.entries.get_mut(&*key) {
            entry.times.pop_back();
            // It stays filed under its count before the refund until its
            // next hit, which only keeps it longer.
            if entry.times.is_empty() {
                keys.entries.remove(&*key);
            }
        }
    }

    pub fn clear(&self, key: &str) {
        self.lock().entries.remove(&*bounded(key));
    }

    fn hit_at(&self, key: &str, now: Instant) -> Result<(), u64> {
        let key = bounded(key);
        let mut keys = self.lock();
        keys.drop_aged_out(now, self.window);
        if !keys.entries.contains_key(&*key) {
            if keys.entries.len() >= self.max_keys {
                keys.evict();
            }
            let key: Arc<str> = Arc::from(&*key);
            keys.entries.insert(
                Arc::clone(&key),
                Entry {
                    key,
                    times: VecDeque::new(),
                    sequence: 0,
                    hit: now,
                },
            );
        }
        let entry = keys
            .entries
            .get_mut(&*key)
            .expect("the key was just inserted");
        Self::expire(&mut entry.times, now, self.window);
        let allowed = self.allowed(&entry.times, now);
        if allowed.is_ok() {
            entry.times.push_back(now);
        }
        keys.file(&key, now);
        allowed
    }

    fn allowed(&self, times: &VecDeque<Instant>, now: Instant) -> Result<(), u64> {
        if times.len() < self.limit {
            return Ok(());
        }
        let oldest = times.front().copied().unwrap_or(now);
        Err((oldest + self.window)
            .saturating_duration_since(now)
            .as_secs()
            .max(1))
    }

    fn expire(times: &mut VecDeque<Instant>, now: Instant, window: Duration) {
        while times
            .front()
            .is_some_and(|time| now.saturating_duration_since(*time) >= window)
        {
            times.pop_front();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Keys> {
        // A panic while holding the lock leaves only counters behind.
        self.keys
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Keys {
    /// Records a hit on the tracked `key` at `now`, filed under the attempts
    /// it holds.
    fn file(&mut self, key: &str, now: Instant) {
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        entry.sequence = sequence;
        entry.hit = now;
        self.by_hit.push_back((sequence, Arc::clone(&entry.key)));
        self.by_count[entry.times.len()].push_back((sequence, Arc::clone(&entry.key)));
        // Each live key has one current place in each; once the stale ones
        // outnumber them, drop those in one pass, which the hits and removals
        // that made them stale pay for.
        let places = self.by_hit.len() + self.by_count.iter().map(VecDeque::len).sum::<usize>();
        if places > 4 * self.entries.len() + 64 {
            #[cfg(test)]
            {
                self.visited += places;
            }
            let entries = &self.entries;
            let current = |(sequence, key): &(u64, Arc<str>)| {
                entries
                    .get(key)
                    .is_some_and(|entry| entry.sequence == *sequence)
            };
            self.by_hit.retain(current);
            for queue in &mut self.by_count {
                queue.retain(current);
            }
        }
    }

    /// Drops keys last hit a `window` or more before `now`: every attempt
    /// they hold has aged out.
    fn drop_aged_out(&mut self, now: Instant, window: Duration) {
        while let Some((sequence, key)) = self.by_hit.front() {
            #[cfg(test)]
            {
                self.visited += 1;
            }
            match self.entries.get(key) {
                Some(entry) if entry.sequence == *sequence => {
                    if now.saturating_duration_since(entry.hit) < window {
                        return;
                    }
                    let key = Arc::clone(key);
                    self.entries.remove(&key);
                }
                _ => {}
            }
            self.by_hit.pop_front();
        }
    }

    /// Drops the key with the fewest attempts at its last hit, least
    /// recently hit among those.
    fn evict(&mut self) {
        for queue in &mut self.by_count {
            while let Some((sequence, key)) = queue.pop_front() {
                #[cfg(test)]
                {
                    self.visited += 1;
                }
                if self
                    .entries
                    .get(&key)
                    .is_some_and(|entry| entry.sequence == sequence)
                {
                    self.entries.remove(&key);
                    return;
                }
            }
        }
    }
}

/// `key`, or its hash if it is longer than [`MAX_KEY_LENGTH`]. A hash can't
/// be mistaken for a shorter key without a SHA-256 preimage.
fn bounded(key: &str) -> Cow<'_, str> {
    if key.len() > MAX_KEY_LENGTH {
        Cow::Owned(token_hash(key))
    } else {
        Cow::Borrowed(key)
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

/// The rate-limit key for an account, from its normalised email. The email
/// is whatever a client sent, so it is hashed to a fixed length, and known
/// and unknown addresses are counted alike.
pub fn account_key(email: &str) -> String {
    token_hash(email)
}

/// Second-factor challenges issued after a correct password. At most
/// [`MAX_PENDING`](super::ceremonies::MAX_PENDING) are kept; issuing one past
/// that drops the oldest, whose user signs in again.
#[derive(Debug)]
pub struct Challenges {
    max_attempts: u32,
    pending: Mutex<Expiring<Challenge>>,
}

#[derive(Debug)]
struct Challenge {
    user_id: String,
    passkey: Option<PasskeyAuthentication>,
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
            max_attempts,
            pending: Mutex::new(Expiring::new(ttl)),
        }
    }

    /// Issues a challenge for `user_id` and returns its token. `passkey` is
    /// the state of the passkey prompt sent with it.
    pub fn issue(&self, user_id: &str, passkey: Option<PasskeyAuthentication>) -> String {
        let token = new_token();
        self.lock().insert_at(
            token_hash(&token),
            Challenge {
                user_id: user_id.to_owned(),
                passkey,
                attempts: 0,
            },
            Instant::now(),
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
        let (expires, challenge) = pending.get_mut(&key)?;
        if *expires <= Instant::now() || challenge.attempts >= self.max_attempts {
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

    fn lock(&self) -> std::sync::MutexGuard<'_, Expiring<Challenge>> {
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
        for key in 0..10_000 {
            limiter.hit_at(&key.to_string(), start).unwrap();
        }
        limiter
            .hit_at("late", start + Duration::from_secs(2))
            .unwrap();
        assert_eq!(limiter.lock().entries.len(), 1);
    }

    /// Hits `key` at `now` and returns the result with the places it looked
    /// at.
    fn counted(limiter: &RateLimiter, key: &str, now: Instant) -> (Result<(), u64>, usize) {
        let before = limiter.lock().visited;
        let result = limiter.hit_at(key, now);
        (result, limiter.lock().visited - before)
    }

    #[test]
    fn a_flood_of_keys_stays_under_the_cap_without_scanning_it() {
        let limiter = RateLimiter::with_max_keys(5, Duration::from_secs(900), 1_000);
        let start = Instant::now();
        let hits = 100_000;
        let (mut visited, mut large) = (0, 0);
        for n in 0..hits {
            let now = start + Duration::from_millis(n);
            // Every key is still live, and a tenth of hits are on a key
            // already tracked.
            let key = if n % 10 == 0 {
                format!("key-{}", n.saturating_sub(1))
            } else {
                format!("key-{n}")
            };
            let (result, places) = counted(&limiter, &key, now);
            assert!(result.is_ok());
            assert!(limiter.lock().entries.len() <= 1_000);
            visited += places;
            if places > 64 {
                large += 1;
            }
        }
        assert!(
            visited <= 8 * hits as usize,
            "{visited} places in {hits} hits"
        );
        assert!(
            large <= hits as usize / 500,
            "{large} hits looked at over 64"
        );
    }

    #[test]
    fn aged_out_keys_are_dropped_as_they_age() {
        let window = Duration::from_secs(10);
        let limiter = RateLimiter::new(5, window);
        let start = Instant::now();
        let hits = 50_000;
        let mut visited = 0;
        for n in 0..hits {
            let (result, places) =
                counted(&limiter, &n.to_string(), start + Duration::from_millis(n));
            assert!(result.is_ok());
            visited += places;
            // One key per millisecond: those of the last 10 seconds.
            assert!(limiter.lock().entries.len() <= 10_001);
        }
        assert!(
            visited <= 4 * hits as usize,
            "{visited} places in {hits} hits"
        );
        assert_eq!(
            limiter
                .hit_at("late", start + Duration::from_millis(hits) + window)
                .map(|()| limiter.lock().entries.len()),
            Ok(1)
        );
    }

    #[test]
    fn a_flood_of_new_keys_does_not_reset_a_used_key() {
        let window = Duration::from_secs(900);
        let limiter = RateLimiter::with_max_keys(3, window, 100);
        let start = Instant::now();
        for _ in 0..3 {
            limiter.hit_at("exhausted", start).unwrap();
        }
        limiter.hit_at("partial", start).unwrap();
        limiter.hit_at("partial", start).unwrap();
        limiter.hit_at("normal", start).unwrap();
        for n in 0..10_000 {
            let now = start + Duration::from_millis(n);
            limiter.hit_at(&format!("flood-{n}"), now).unwrap();
        }
        let later = start + Duration::from_secs(100);
        assert_eq!(limiter.hit_at("exhausted", later), Err(800));
        assert!(limiter.hit_at("partial", later).is_ok());
        assert_eq!(limiter.hit_at("partial", later), Err(800));
        // Evicted, it starts again with every attempt.
        for _ in 0..3 {
            assert!(limiter.hit_at("normal", later).is_ok());
        }
        assert_eq!(limiter.hit_at("normal", later), Err(900));
    }

    #[test]
    fn a_new_key_gets_in_when_every_key_is_at_its_limit() {
        let window = Duration::from_secs(60);
        let limiter = RateLimiter::with_max_keys(1, window, 3);
        let start = Instant::now();
        for (n, key) in ["a", "b", "c"].into_iter().enumerate() {
            limiter
                .hit_at(key, start + Duration::from_secs(n as u64))
                .unwrap();
        }
        // A refused attempt is a hit too: "a" is now the latest.
        assert!(limiter.hit_at("a", start + Duration::from_secs(3)).is_err());
        assert!(limiter.hit_at("d", start + Duration::from_secs(4)).is_ok());
        // "b", the least recently hit, made room.
        let keys = limiter.lock();
        assert!(
            ["a", "c", "d"]
                .iter()
                .all(|key| keys.entries.contains_key(*key))
        );
        assert_eq!(keys.entries.len(), 3);
    }

    #[test]
    fn long_keys_are_kept_short() {
        let limiter = RateLimiter::new(1, Duration::from_secs(60));
        let long = "a".repeat(100_000);
        assert!(limiter.hit(&long).is_ok());
        assert!(limiter.hit(&long).is_err());
        assert!(limiter.hit(&"a".repeat(99_999)).is_ok());
        limiter.refund(&long);
        assert!(limiter.hit(&long).is_ok());
        limiter.clear(&long);
        assert!(limiter.hit(&long).is_ok());
        assert!(
            limiter
                .lock()
                .entries
                .keys()
                .all(|key| key.len() <= MAX_KEY_LENGTH)
        );
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
    fn account_keys_have_a_fixed_length() {
        assert_eq!(account_key("ada@example.com").len(), 64);
        assert_eq!(account_key(&"a".repeat(10_000)).len(), 64);
        assert_ne!(
            account_key("ada@example.com"),
            account_key("bob@example.com")
        );
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
