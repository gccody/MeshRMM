//! What authenticating a request needs to know about its user besides the
//! session or token row: the user row, their roles with permissions, and
//! their second factors.
//!
//! Every request with a session or an API token needs these, and they
//! change a few times a day, so they are kept here and a warm request reads
//! only its session or token row. Those rows are never kept: ending a
//! session or revoking a token works on the next request without a signal.
//!
//! An entry is good only for the access generation it was loaded in. Every
//! committed change that can alter what is kept here bumps that generation
//! with [`Presence::recheck_access`](crate::realtime::Presence::recheck_access)
//! before its response is sent, so the next request reads the database
//! again, and a load that began before the change isn't kept. The generation
//! is presence's own counter, so the signal that wakes open sockets is the
//! one that empties this cache, and a socket it wakes reads fresh data.
//!
//! A change from another process can't bump it: `meshrmm-server admin` edits
//! the database directly. So entries also last at most [`ACCESS_RECHECK`],
//! the interval at which open sockets check their access anyway.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use tokio::sync::watch;

use crate::{
    access::Access,
    db::{self, Database},
    rbac::{self, Permissions, Role},
    users::{self, SecondFactors, User},
};

/// How long a change made outside the server, which can't signal, may take
/// to apply: open event sockets and terminals check their user's access this
/// often, and a kept account lasts this long.
pub const ACCESS_RECHECK: Duration = Duration::from_secs(30);
/// An instance has few users; past this many, the oldest entries go.
const MAX_ENTRIES: usize = 1000;

/// A user as authentication sees them.
#[derive(Debug)]
pub struct Account {
    pub user: User,
    pub roles: Vec<Role>,
    /// Every permission the roles grant somewhere.
    pub permissions: Permissions,
    /// The companies each one reaches.
    pub access: Access,
    pub second_factors: SecondFactors,
}

impl Account {
    /// The user `user_id` from the database, or `None` if there is none.
    pub async fn load(database: &Database, user_id: &str) -> db::Result<Option<Self>> {
        let mut database = database;
        let Some(user) = users::by_id(&mut database, user_id).await? else {
            return Ok(None);
        };
        let roles = rbac::user_roles(&mut database, user_id).await?;
        let second_factors = users::second_factors(&mut database, user_id).await?;
        let access = Access::of(&roles);
        Ok(Some(Self {
            permissions: access.permissions(),
            access,
            user,
            roles,
            second_factors,
        }))
    }
}

/// The accounts that authenticated requests recently.
#[derive(Debug)]
pub struct Accounts {
    /// Presence's access generation.
    access: watch::Receiver<u64>,
    entries: Mutex<HashMap<String, Entry>>,
}

#[derive(Debug)]
struct Entry {
    generation: u64,
    loaded_at: Instant,
    account: Arc<Account>,
}

impl Accounts {
    /// Keeps accounts valid while `access` (from
    /// [`Presence::access_changes`](crate::realtime::Presence::access_changes))
    /// stays at the generation they were loaded in.
    pub fn new(access: watch::Receiver<u64>) -> Self {
        Self {
            access,
            entries: Mutex::default(),
        }
    }

    /// The user `user_id`, read from `database` unless a current entry is
    /// kept. `None` if there is no such user.
    pub async fn get(
        &self,
        database: &Database,
        user_id: &str,
    ) -> db::Result<Option<Arc<Account>>> {
        // Read before the database, so a change committed after this point
        // has bumped it by the time the load is kept.
        let generation = self.generation();
        if let Some(account) = self.cached(user_id, generation, Instant::now()) {
            return Ok(Some(account));
        }
        let Some(account) = Account::load(database, user_id).await? else {
            return Ok(None);
        };
        let account = Arc::new(account);
        self.keep(user_id, generation, Instant::now(), &account);
        Ok(Some(account))
    }

    fn generation(&self) -> u64 {
        *self.access.borrow()
    }

    /// The entry for `user_id`, if it was loaded in `generation` and less
    /// than [`ACCESS_RECHECK`] before `now`.
    fn cached(&self, user_id: &str, generation: u64, now: Instant) -> Option<Arc<Account>> {
        let entries = self.lock();
        let entry = entries.get(user_id)?;
        current(entry, generation, now).then(|| Arc::clone(&entry.account))
    }

    /// Keeps `account`, loaded in `generation` at `loaded_at`, unless the
    /// generation has moved on since: then the load may predate a change.
    fn keep(&self, user_id: &str, generation: u64, loaded_at: Instant, account: &Arc<Account>) {
        let mut entries = self.lock();
        // Read under the lock, so an entry is never kept after a bump that
        // happened before it was inserted.
        if self.generation() != generation {
            return;
        }
        entries.retain(|_, entry| current(entry, generation, loaded_at));
        if entries.len() >= MAX_ENTRIES
            && let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.loaded_at)
                .map(|(id, _)| id.clone())
        {
            entries.remove(&oldest);
        }
        entries.insert(
            user_id.to_owned(),
            Entry {
                generation,
                loaded_at,
                account: Arc::clone(account),
            },
        );
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Entry>> {
        // Nothing panics while holding it, so a poisoned one is whole.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn current(entry: &Entry, generation: u64, now: Instant) -> bool {
    entry.generation == generation
        && now.saturating_duration_since(entry.loaded_at) < ACCESS_RECHECK
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(id: &str) -> Arc<Account> {
        Arc::new(Account {
            user: User {
                id: id.to_owned(),
                email: format!("{id}@example.com"),
                display_name: id.to_owned(),
                password_hash: None,
                password_changed_at: None,
                disabled: false,
                oidc_subject: None,
                scim_external_id: None,
                scim_managed: false,
                created_at: 0,
                updated_at: 0,
                last_sign_in_at: None,
            },
            roles: Vec::new(),
            permissions: Permissions::new(),
            access: Access::default(),
            second_factors: SecondFactors::default(),
        })
    }

    #[test]
    fn an_account_is_kept_until_the_recheck_interval_passes() {
        let (_signal, access) = watch::channel(0);
        let accounts = Accounts::new(access);
        let loaded = Instant::now();
        assert!(accounts.cached("ada", 0, loaded).is_none());
        accounts.keep("ada", 0, loaded, &account("ada"));
        let kept = accounts.cached("ada", 0, loaded + ACCESS_RECHECK - Duration::from_millis(1));
        assert_eq!(kept.unwrap().user.id, "ada");
        // A change made in the database by another process applies by now.
        assert!(accounts.cached("ada", 0, loaded + ACCESS_RECHECK).is_none());
        assert!(accounts.cached("bob", 0, loaded).is_none());
    }

    #[test]
    fn an_access_change_drops_every_account() {
        let (signal, access) = watch::channel(0);
        let accounts = Accounts::new(access);
        let now = Instant::now();
        accounts.keep("ada", 0, now, &account("ada"));
        accounts.keep("bob", 0, now, &account("bob"));
        signal.send_modify(|generation| *generation += 1);
        let generation = accounts.generation();
        assert_eq!(generation, 1);
        assert!(accounts.cached("ada", generation, now).is_none());
        assert!(accounts.cached("bob", generation, now).is_none());
        accounts.keep("ada", generation, now, &account("ada"));
        assert!(accounts.cached("ada", generation, now).is_some());
        // Keeping a new entry clears out those of the old generation.
        assert_eq!(accounts.lock().len(), 1);
    }

    #[test]
    fn a_load_that_began_before_an_access_change_is_not_kept() {
        let (signal, access) = watch::channel(0);
        let accounts = Accounts::new(access);
        let now = Instant::now();
        // A request reads the generation and starts loading; meanwhile a
        // change commits and signals.
        let generation = accounts.generation();
        signal.send_modify(|generation| *generation += 1);
        accounts.keep("ada", generation, now, &account("ada"));
        assert!(accounts.cached("ada", generation, now).is_none());
        assert!(accounts.cached("ada", accounts.generation(), now).is_none());
        assert!(accounts.lock().is_empty());
    }

    #[test]
    fn the_oldest_account_goes_when_the_cache_is_full() {
        let (_signal, access) = watch::channel(0);
        let accounts = Accounts::new(access);
        let start = Instant::now();
        for n in 0..MAX_ENTRIES {
            let at = start + Duration::from_millis(n as u64);
            accounts.keep(&format!("user-{n}"), 0, at, &account(&format!("user-{n}")));
        }
        let later = start + Duration::from_secs(1);
        accounts.keep("newcomer", 0, later, &account("newcomer"));
        let entries = accounts.lock();
        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(!entries.contains_key("user-0"));
        assert!(entries.contains_key("user-1"));
        assert!(entries.contains_key("newcomer"));
    }
}
