//! Server-side state for multi-step sign-in ceremonies (passkey challenges,
//! SSO redirects). Each entry is used once and expires; the browser holds
//! only a random token naming it.
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use crate::secrets::{new_token, token_hash};

/// Above this many pending ceremonies the oldest is dropped, so unfinished
/// ones can't grow memory without bound.
const MAX_PENDING: usize = 10_000;

#[derive(Debug)]
pub struct Ceremonies<T> {
    ttl: Duration,
    pending: Mutex<HashMap<String, (Instant, T)>>,
}

impl<T> Ceremonies<T> {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Keeps `value` and returns the token that takes it back.
    pub fn start(&self, value: T) -> String {
        self.start_at(value, Instant::now())
    }

    fn start_at(&self, value: T, now: Instant) -> String {
        let token = new_token();
        let mut pending = self.lock();
        pending.retain(|_, (expires, _)| *expires > now);
        if pending.len() >= MAX_PENDING
            && let Some(oldest) = pending
                .iter()
                .min_by_key(|(_, (expires, _))| *expires)
                .map(|(key, _)| key.clone())
        {
            pending.remove(&oldest);
        }
        pending.insert(token_hash(&token), (now + self.ttl, value));
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

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, (Instant, T)>> {
        // A panic while holding the lock leaves only pending ceremonies.
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
}
