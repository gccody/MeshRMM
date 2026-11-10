//! Events that every website event socket sends alike, serialised once for
//! all of them.
//!
//! A [`Shared`] event makes its JSON the first time a socket sends it, and
//! every other socket sends the same bytes. Each source counts the JSON it
//! made in its [`Serializations`], so tests can see the work doesn't grow
//! with the number of sockets.
use std::{
    fmt,
    ops::Deref,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use axum::extract::ws::{Message, Utf8Bytes};
use serde::Serialize;

/// An event and its JSON. Clones share both; it reads as the event itself.
pub struct Shared<T>(Arc<Inner<T>>);

struct Inner<T> {
    event: Arc<T>,
    json: OnceLock<Utf8Bytes>,
    made: Serializations,
}

impl<T: Serialize> Shared<T> {
    /// The event, whose JSON is made when first asked for and counted in
    /// `made`.
    pub fn new(event: impl Into<Arc<T>>, made: &Serializations) -> Self {
        Self(Arc::new(Inner {
            event: event.into(),
            json: OnceLock::new(),
            made: made.clone(),
        }))
    }

    /// The event's JSON, made once however many sockets send it. Cloning the
    /// bytes copies nothing.
    pub fn json(&self) -> Utf8Bytes {
        self.0
            .json
            .get_or_init(|| {
                self.0.made.0.fetch_add(1, Ordering::Relaxed);
                serde_json::to_string(&*self.0.event)
                    .expect("events serialize")
                    .into()
            })
            .clone()
    }

    /// The event as a socket sends it.
    pub fn message(&self) -> Message {
        Message::Text(self.json())
    }
}

impl<T> Shared<T> {
    /// Whether both are the same event, sharing one JSON.
    pub fn ptr_eq(this: &Self, other: &Self) -> bool {
        Arc::ptr_eq(&this.0, &other.0)
    }
}

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0.event
    }
}

impl<T: fmt::Debug> fmt::Debug for Shared<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.event.fmt(formatter)
    }
}

/// How many times a source's events had their JSON made. Clones count
/// together.
#[derive(Debug, Clone, Default)]
pub struct Serializations(Arc<AtomicU64>);

impl Serializations {
    pub fn count(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Serialize)]
    struct Event {
        n: u32,
    }

    #[test]
    fn the_json_is_made_once_and_shared_by_every_clone() {
        let made = Serializations::default();
        let event = Shared::new(Event { n: 1 }, &made);
        assert_eq!(made.count(), 0, "nothing is made until a socket sends it");
        let copies = (0..10).map(|_| event.clone()).collect::<Vec<_>>();
        let sent = copies.iter().map(Shared::json).collect::<Vec<_>>();
        assert_eq!(made.count(), 1);
        for json in &sent {
            assert_eq!(json.as_str(), r#"{"n":1}"#);
            // The same bytes, not equal copies of them.
            assert_eq!(json.as_str().as_ptr(), sent[0].as_str().as_ptr());
        }
        assert_eq!(event.n, 1, "it reads as the event");
        Shared::new(Event { n: 2 }, &made).json();
        assert_eq!(made.count(), 2, "clones of the counter count together");
    }
}
