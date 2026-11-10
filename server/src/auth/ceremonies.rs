//! Server-side state for multi-step sign-in ceremonies (passkey challenges,
//! SSO redirects). Each entry is used once and expires; the browser holds
//! only a random token naming it.
use std::{
    collections::{HashMap, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::secrets::{new_token, token_hash};

/// Above this many pending entries the oldest is dropped, so unfinished
/// ones can't grow memory without bound.
pub(super) const MAX_PENDING: usize = 10_000;

#[derive(Debug)]
pub struct Ceremonies<T> {
    pending: Mutex<Expiring<T>>,
}

impl<T> Ceremonies<T> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            pending: Mutex::new(Expiring::new(ttl)),
        }
    }

    /// Keeps `value` and returns the token that takes it back.
    pub fn start(&self, value: T) -> String {
        self.start_at(value, Instant::now())
    }

    fn start_at(&self, value: T, now: Instant) -> String {
        let token = new_token();
        self.lock().insert_at(token_hash(&token), value, now);
        token
    }

    /// Removes and returns the ceremony `token` names, unless it expired.
    pub fn take(&self, token: &str) -> Option<T> {
        self.take_at(token, Instant::now())
    }

    fn take_at(&self, token: &str, now: Instant) -> Option<T> {
        let (expires, value) = self.lock().remove(&token_hash(token))?;
        (expires > now).then_some(value)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Expiring<T>> {
        // A panic while holding the lock leaves only pending ceremonies.
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// At most [`MAX_PENDING`] entries, each expiring `ttl` after it was added.
/// Adding one drops what expired and, when full, the oldest, in time
/// proportional to what it drops rather than to how many are kept.
#[derive(Debug)]
pub(super) struct Expiring<T> {
    ttl: Duration,
    entries: HashMap<String, (Instant, T)>,
    /// Keys and expiry times in the order they were added, which with one
    /// `ttl` is the order they expire. Keys removed since are skipped.
    order: VecDeque<(Instant, String)>,
    /// Entries looked at, for tests to bound the work an insert does.
    #[cfg(test)]
    pub(super) visited: usize,
}

impl<T> Expiring<T> {
    pub(super) fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: HashMap::new(),
            order: VecDeque::new(),
            #[cfg(test)]
            visited: 0,
        }
    }

    pub(super) fn insert_at(&mut self, key: String, value: T, now: Instant) {
        while let Some((expires, _)) = self.order.front()
            && *expires <= now
        {
            self.pop_oldest();
        }
        while self.entries.len() >= MAX_PENDING && self.pop_oldest() {}
        // Removed entries leave their place in `order`; when those are most
        // of it, they are dropped in one pass, once per as many removals.
        if self.order.len() > 2 * self.entries.len() + 64 {
            #[cfg(test)]
            {
                self.visited += self.order.len();
            }
            let entries = &self.entries;
            self.order
                .retain(|(expires, key)| Self::current(entries, key, *expires));
        }
        let expires = now + self.ttl;
        self.order.push_back((expires, key.clone()));
        self.entries.insert(key, (expires, value));
    }

    pub(super) fn get_mut(&mut self, key: &str) -> Option<&mut (Instant, T)> {
        self.entries.get_mut(key)
    }

    pub(super) fn remove(&mut self, key: &str) -> Option<(Instant, T)> {
        self.entries.remove(key)
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Takes the oldest place in `order`, removing its entry if it is still
    /// there. Returns whether there was a place.
    fn pop_oldest(&mut self) -> bool {
        let Some((expires, key)) = self.order.pop_front() else {
            return false;
        };
        #[cfg(test)]
        {
            self.visited += 1;
        }
        if Self::current(&self.entries, &key, expires) {
            self.entries.remove(&key);
        }
        true
    }

    fn current(entries: &HashMap<String, (Instant, T)>, key: &str, expires: Instant) -> bool {
        entries.get(key).is_some_and(|(kept, _)| *kept == expires)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ceremony_is_taken_once() {
        let ceremonies = Ceremonies::new(Duration::from_secs(60));
        let token = ceremonies.start("state");
        assert_eq!(ceremonies.take("unknown"), None);
        assert_eq!(ceremonies.take(&token), Some("state"));
        assert_eq!(ceremonies.take(&token), None);
    }

    #[test]
    fn expired_ceremonies_are_refused_and_pruned() {
        let ceremonies = Ceremonies::new(Duration::from_secs(60));
        let start = Instant::now();
        let token = ceremonies.start_at(1, start);
        assert_eq!(
            ceremonies.take_at(&token, start + Duration::from_secs(60)),
            None
        );
        ceremonies.start_at(2, start);
        ceremonies.start_at(3, start + Duration::from_secs(61));
        assert_eq!(ceremonies.lock().len(), 1);
    }

    #[test]
    fn the_oldest_is_dropped_when_full() {
        let ceremonies = Ceremonies::new(Duration::from_secs(60));
        let start = Instant::now();
        let first = ceremonies.start_at(0, start);
        for n in 1..MAX_PENDING {
            ceremonies.start_at(n, start + Duration::from_millis(1));
        }
        let last = ceremonies.start_at(MAX_PENDING, start + Duration::from_millis(2));
        assert_eq!(ceremonies.lock().len(), MAX_PENDING);
        assert_eq!(ceremonies.take_at(&first, start), None);
        assert_eq!(ceremonies.take_at(&last, start), Some(MAX_PENDING));
    }

    #[test]
    fn starting_one_when_full_does_not_scan_the_rest() {
        let mut pending = Expiring::new(Duration::from_secs(60));
        let start = Instant::now();
        for n in 0..MAX_PENDING {
            pending.insert_at(n.to_string(), n, start);
        }
        pending.visited = 0;
        let starts = 10 * MAX_PENDING;
        for n in 0..starts {
            pending.insert_at(format!("new-{n}"), n, start + Duration::from_secs(1));
            assert_eq!(pending.len(), MAX_PENDING);
        }
        assert!(pending.visited <= 2 * starts, "{}", pending.visited);
        assert_eq!(pending.get_mut("0").map(|(_, n)| *n), None);
        assert!(pending.get_mut(&format!("new-{}", starts - 1)).is_some());
    }

    #[test]
    fn taken_ceremonies_leave_nothing_behind() {
        let mut pending = Expiring::new(Duration::from_secs(60));
        let start = Instant::now();
        pending.insert_at("kept".to_owned(), 0, start);
        for n in 0..100_000 {
            pending.insert_at(n.to_string(), n, start);
            assert_eq!(pending.remove(&n.to_string()).map(|(_, n)| n), Some(n));
            assert!(pending.order.len() <= 2 * pending.len() + 65);
        }
        assert!(pending.visited <= 300_000, "{}", pending.visited);
        assert_eq!(pending.len(), 1);
        assert!(pending.get_mut("kept").is_some());
    }
}
