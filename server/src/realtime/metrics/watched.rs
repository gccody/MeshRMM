//! Which devices' live readings some website tab shows: for each device, how
//! many event sockets watch it.
//!
//! An event socket's watch set ([`WatchSet`]) joins the registry for each
//! device it adds and leaves it for each it drops, and leaves it for all of
//! them when it is dropped, which a socket's task does however the
//! connection ends. A device is in the registry only while a socket watches
//! it or a task listens for it ([`Watched::listen`]), so neither closed
//! sockets nor deleted devices leave entries behind: a socket drops a
//! deleted device from its set when presence announces the deletion.
//!
//! A task that needs to know when a device becomes watched or stops being
//! watched, such as one telling the device's Agent how often to report,
//! listens with [`Watched::listen`]: a [`tokio::sync::watch`] channel per
//! device, set on each change between unwatched and watched, so the listener
//! never falls behind and costs nothing while nothing changes.
//!
//! The lock is a plain mutex, held only while counting and never across an
//! await. The metrics feed takes it inside the store's own lock while it
//! scans for watched devices; nothing takes them the other way round.
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

use tokio::sync::watch;

/// The fleet's watched devices. Clones share the registry.
#[derive(Debug, Clone, Default)]
pub struct Watched {
    devices: Arc<Mutex<HashMap<Arc<str>, Entry>>>,
}

#[derive(Debug)]
pub(super) struct Entry {
    /// Sockets watching the device.
    sockets: usize,
    /// Listeners for the device's changes.
    listeners: usize,
    /// Whether any socket watches it, for the listeners.
    signal: watch::Sender<bool>,
}

impl Entry {
    fn new() -> Self {
        Self {
            sockets: 0,
            listeners: 0,
            signal: watch::Sender::new(false),
        }
    }
}

pub(super) type Devices = HashMap<Arc<str>, Entry>;

impl Watched {
    pub(super) fn lock(&self) -> MutexGuard<'_, Devices> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.devices.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether any website tab shows the device's live readings.
    pub fn is_watched(&self, device_id: &str) -> bool {
        is_watched(&self.lock(), device_id)
    }

    /// How many event sockets watch the device.
    pub fn sockets(&self, device_id: &str) -> usize {
        self.lock().get(device_id).map_or(0, |entry| entry.sockets)
    }

    /// How many devices any socket watches.
    pub fn len(&self) -> usize {
        self.lock()
            .values()
            .filter(|entry| entry.sockets > 0)
            .count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many devices the registry holds an entry for, watched or only
    /// listened to; for tests, to see that none is left behind.
    pub fn entries(&self) -> usize {
        self.lock().len()
    }

    /// Follows whether the device is watched, from now on.
    pub fn listen(&self, device_id: &str) -> WatchedSignal {
        let mut devices = self.lock();
        let (device_id, entry) = entry(&mut devices, device_id);
        entry.listeners += 1;
        let receiver = entry.signal.subscribe();
        WatchedSignal {
            registry: self.clone(),
            device_id,
            receiver,
        }
    }

    /// One more socket watches each of the devices. Returns the registry's
    /// copy of each ID, for the socket to keep.
    fn join(&self, device_ids: impl IntoIterator<Item = String>) -> Vec<Arc<str>> {
        let mut devices = self.lock();
        device_ids
            .into_iter()
            .map(|device_id| {
                let (key, entry) = entry(&mut devices, device_id);
                entry.sockets += 1;
                if entry.sockets == 1 {
                    entry.signal.send_replace(true);
                }
                key
            })
            .collect()
    }

    /// One socket fewer watches each of the devices.
    fn leave<'a>(&self, device_ids: impl IntoIterator<Item = &'a Arc<str>>) {
        let mut devices = self.lock();
        for device_id in device_ids {
            let Some(entry) = devices.get_mut(&**device_id) else {
                continue;
            };
            entry.sockets = entry.sockets.saturating_sub(1);
            if entry.sockets == 0 {
                entry.signal.send_replace(false);
                if entry.listeners == 0 {
                    devices.remove(&**device_id);
                }
            }
        }
    }
}

/// The device's entry, made if it has none, and the registry's copy of its
/// ID.
fn entry(
    devices: &mut Devices,
    device_id: impl AsRef<str> + Into<Arc<str>>,
) -> (Arc<str>, &mut Entry) {
    let key = match devices.get_key_value(device_id.as_ref()) {
        Some((key, _)) => Arc::clone(key),
        None => device_id.into(),
    };
    let entry = devices.entry(Arc::clone(&key)).or_insert_with(Entry::new);
    (key, entry)
}

pub(super) fn is_watched(devices: &Devices, device_id: &str) -> bool {
    devices
        .get(device_id)
        .is_some_and(|entry| entry.sockets > 0)
}

/// The devices any socket watches.
pub(super) fn watched_ids(devices: &Devices) -> impl Iterator<Item = &str> {
    devices
        .iter()
        .filter(|(_, entry)| entry.sockets > 0)
        .map(|(id, _)| &**id)
}

/// Whether a device is watched, as it changes. Dropping it stops listening.
#[derive(Debug)]
pub struct WatchedSignal {
    registry: Watched,
    device_id: Arc<str>,
    receiver: watch::Receiver<bool>,
}

impl WatchedSignal {
    /// Whether any socket watches the device now.
    pub fn is_watched(&self) -> bool {
        *self.receiver.borrow()
    }

    /// Waits until the device becomes watched or stops being watched, and
    /// returns which. Safe to cancel.
    pub async fn changed(&mut self) -> bool {
        // The registry keeps the sender while a listener exists.
        let _ = self.receiver.changed().await;
        *self.receiver.borrow_and_update()
    }
}

impl Drop for WatchedSignal {
    fn drop(&mut self) {
        let mut devices = self.registry.lock();
        if let Some(entry) = devices.get_mut(&*self.device_id) {
            entry.listeners -= 1;
            if entry.listeners == 0 && entry.sockets == 0 {
                devices.remove(&*self.device_id);
            }
        }
    }
}

/// One event socket's watched devices, each with the sequence of the last
/// reading the socket sent for it (0 for none). It holds the socket's place
/// in the registry, and gives it up when dropped.
#[derive(Debug)]
pub(super) struct WatchSet {
    registry: Watched,
    devices: HashMap<Arc<str>, u64>,
}

impl WatchSet {
    pub(super) fn new(registry: Watched) -> Self {
        Self {
            registry,
            devices: HashMap::new(),
        }
    }

    pub(super) fn len(&self) -> usize {
        self.devices.len()
    }

    pub(super) fn ids(&self) -> impl Iterator<Item = &str> {
        self.devices.keys().map(|id| &**id)
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = (&str, &mut u64)> {
        self.devices.iter_mut().map(|(id, sent)| (&**id, sent))
    }

    pub(super) fn sent_mut(&mut self, device_id: &str) -> Option<&mut u64> {
        self.devices.get_mut(device_id)
    }

    /// Watches exactly `device_ids`, and returns those it did not watch
    /// before. The registry counts the new ones before this returns, so a
    /// reading taken after it reaches the feed's next scan.
    pub(super) fn replace(&mut self, device_ids: HashSet<String>) -> Vec<Arc<str>> {
        let gone: Vec<Arc<str>> = self
            .devices
            .keys()
            .filter(|id| !device_ids.contains(&***id))
            .cloned()
            .collect();
        self.registry.leave(&gone);
        for id in &gone {
            self.devices.remove(id);
        }
        let new = device_ids
            .into_iter()
            .filter(|id| !self.devices.contains_key(id.as_str()));
        let added = self.registry.join(new);
        for id in &added {
            self.devices.insert(Arc::clone(id), 0);
        }
        added
    }

    /// Stops watching the device, if it did.
    pub(super) fn remove(&mut self, device_id: &str) {
        if let Some((id, _)) = self.devices.remove_entry(device_id) {
            self.registry.leave([&id]);
        }
    }
}

impl Drop for WatchSet {
    fn drop(&mut self) {
        self.registry.leave(self.devices.keys());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn a_device_is_watched_while_any_socket_watches_it() {
        let registry = Watched::default();
        let mut first = WatchSet::new(registry.clone());
        let mut second = WatchSet::new(registry.clone());
        first.replace(set(&["a", "b"]));
        second.replace(set(&["a"]));
        assert_eq!(registry.sockets("a"), 2);
        assert_eq!(registry.sockets("b"), 1);
        assert_eq!(registry.len(), 2);

        // Replacing a set counts only what changed.
        let added = first.replace(set(&["a", "c"]));
        assert_eq!(added.iter().map(|id| &**id).collect::<Vec<_>>(), ["c"]);
        assert_eq!(registry.sockets("a"), 2);
        assert!(!registry.is_watched("b"));
        assert!(registry.is_watched("c"));

        first.replace(HashSet::new());
        assert_eq!(registry.sockets("a"), 1);
        assert!(registry.is_watched("a"));
        // The last socket leaving, by its set being dropped as a socket's
        // task drops it however its connection ended, unwatches it.
        drop(second);
        assert!(!registry.is_watched("a"));
        assert!(registry.is_empty());
        assert_eq!(registry.entries(), 0, "nothing is left behind");
    }

    #[test]
    fn a_device_dropped_from_a_set_leaves_no_entry() {
        let registry = Watched::default();
        let mut socket = WatchSet::new(registry.clone());
        socket.replace(set(&["deleted", "kept"]));
        socket.remove("deleted");
        socket.remove("never watched");
        assert!(!registry.is_watched("deleted"));
        assert_eq!(registry.entries(), 1);
        // Dropped twice counts once.
        socket.remove("deleted");
        assert_eq!(registry.sockets("kept"), 1);
    }

    #[tokio::test]
    async fn listeners_hear_each_change_between_watched_and_not() {
        let registry = Watched::default();
        let mut signal = registry.listen("a");
        assert!(!signal.is_watched());
        assert_eq!(registry.entries(), 1);

        let mut first = WatchSet::new(registry.clone());
        let mut second = WatchSet::new(registry.clone());
        first.replace(set(&["a"]));
        assert!(signal.changed().await);
        // A second socket changes nothing a listener hears of.
        second.replace(set(&["a"]));
        drop(first);
        let pending = tokio::time::timeout(std::time::Duration::ZERO, signal.changed()).await;
        assert!(pending.is_err(), "still watched");
        drop(second);
        assert!(!signal.changed().await);
        // The listener keeps the entry while it listens, and only then.
        assert_eq!(registry.entries(), 1);
        let other = registry.listen("a");
        drop(signal);
        assert_eq!(registry.entries(), 1);
        drop(other);
        assert_eq!(registry.entries(), 0);
    }

    #[tokio::test]
    async fn a_listener_joining_a_watched_device_sees_it_watched() {
        let registry = Watched::default();
        let mut socket = WatchSet::new(registry.clone());
        socket.replace(set(&["a"]));
        let signal = registry.listen("a");
        assert!(signal.is_watched());
        drop(socket);
        assert!(!signal.is_watched());
        assert_eq!(registry.entries(), 1);
        drop(signal);
        assert_eq!(registry.entries(), 0);
    }
}
