//! Remote terminals: a shell on a device, reached from the website.
//!
//! A user asks for a terminal (`POST /v1/agents/{id}/terminals`), which
//! reserves it here, then opens the browser's socket for it
//! (`/v1/terminals/{id}`) within [`Timeouts::browser_connect`]. That socket's
//! task asks the Agent over its control connection to open the terminal, and
//! the Agent connects a socket of its own for it
//! (`/v1/agents/{id}/terminals/{terminal_id}`), which this registry hands to
//! the waiting task. The task then relays between the two sockets
//! ([`relay`]) until either ends, and the terminal is forgotten. Nothing is
//! stored: terminals live as long as the browser's socket, and the audit log
//! records each one's opening and closing.
//!
//! A device has at most [`MAX_TERMINALS_PER_DEVICE`] terminals and a user at
//! most [`MAX_TERMINALS_PER_USER`], counting those still being opened.
pub mod relay;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::extract::ws::WebSocket;
use meshrmm_protocol_types::{RunAs, TerminalShell};
use tokio::sync::oneshot;

use crate::{audit::Actor, rbac::Permission};

pub const MAX_TERMINALS_PER_DEVICE: usize = 5;
pub const MAX_TERMINALS_PER_USER: usize = 10;

/// How long each step of opening a terminal may take.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// From the request until the browser's socket connects.
    pub browser_connect: Duration,
    /// From asking the Agent until its socket connects.
    pub agent_connect: Duration,
    /// Replaces the company's idle time, for tests.
    pub idle: Option<Duration>,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            browser_connect: Duration::from_secs(30),
            agent_connect: Duration::from_secs(20),
            idle: None,
        }
    }
}

/// The permission a terminal for `run_as` needs.
pub fn permission(run_as: RunAs) -> Permission {
    match run_as {
        RunAs::User => Permission::TerminalConnect,
        RunAs::System => Permission::TerminalConnectSystem,
    }
}

/// A terminal a user asked for.
#[derive(Debug, Clone)]
pub struct Requested {
    pub id: String,
    pub device_id: String,
    pub user_id: String,
    pub actor: Actor,
    pub shell: TerminalShell,
    pub run_as: RunAs,
    pub columns: u16,
    pub rows: u16,
    /// The connection notification the device's user sees, if any.
    pub notification: Option<String>,
}

/// Why a terminal can't be reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    DeviceLimit,
    UserLimit,
}

#[derive(Clone)]
pub struct Terminals {
    inner: Arc<Inner>,
}

struct Inner {
    timeouts: Timeouts,
    open: Mutex<HashMap<String, Entry>>,
}

struct Entry {
    device_id: String,
    user_id: String,
    stage: Stage,
}

enum Stage {
    /// Waiting for the browser's socket until `expires_at`.
    Requested {
        requested: Box<Requested>,
        expires_at: Instant,
    },
    /// The browser connected; the Agent's socket goes to its task.
    Connecting { agent: oneshot::Sender<WebSocket> },
    /// Relaying, or closing.
    Open,
}

/// A terminal whose browser connected. Dropping it forgets the terminal,
/// whether its socket's task ended or never started.
pub struct Claim {
    terminals: Terminals,
    id: String,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.terminals.remove(&self.id);
    }
}

impl std::fmt::Debug for Terminals {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Terminals")
            .field("timeouts", &self.inner.timeouts)
            .field("open", &self.lock().len())
            .finish()
    }
}

impl Default for Terminals {
    fn default() -> Self {
        Self::new(Timeouts::default())
    }
}

impl Terminals {
    pub fn new(timeouts: Timeouts) -> Self {
        Self {
            inner: Arc::new(Inner {
                timeouts,
                open: Mutex::default(),
            }),
        }
    }

    pub fn timeouts(&self) -> Timeouts {
        self.inner.timeouts
    }

    /// Reserves `requested`, unless its device or user has as many
    /// terminals as allowed.
    pub fn reserve(&self, requested: Requested) -> Result<(), Refusal> {
        let mut open = self.lock();
        let now = Instant::now();
        // A request whose browser never came is gone.
        open.retain(|_, entry| {
            !matches!(entry.stage, Stage::Requested { expires_at, .. } if expires_at <= now)
        });
        let count = |matches: &dyn Fn(&Entry) -> bool| open.values().filter(|e| matches(e)).count();
        if count(&|entry| entry.device_id == requested.device_id) >= MAX_TERMINALS_PER_DEVICE {
            return Err(Refusal::DeviceLimit);
        }
        if count(&|entry| entry.user_id == requested.user_id) >= MAX_TERMINALS_PER_USER {
            return Err(Refusal::UserLimit);
        }
        open.insert(
            requested.id.clone(),
            Entry {
                device_id: requested.device_id.clone(),
                user_id: requested.user_id.clone(),
                stage: Stage::Requested {
                    expires_at: now + self.inner.timeouts.browser_connect,
                    requested: Box::new(requested),
                },
            },
        );
        Ok(())
    }

    /// The terminal `user_id` asked for, for its browser's socket, with
    /// where the Agent's socket will arrive. A terminal is claimed once, and
    /// forgotten when the claim is dropped.
    pub fn claim_browser(
        &self,
        id: &str,
        user_id: &str,
    ) -> Option<(Claim, Requested, oneshot::Receiver<WebSocket>)> {
        let mut open = self.lock();
        let entry = open.get_mut(id).filter(|entry| entry.user_id == user_id)?;
        let Stage::Requested { expires_at, .. } = &entry.stage else {
            return None;
        };
        if *expires_at <= Instant::now() {
            open.remove(id);
            return None;
        }
        let (agent, arrived) = oneshot::channel();
        let Stage::Requested { requested, .. } =
            std::mem::replace(&mut entry.stage, Stage::Connecting { agent })
        else {
            unreachable!("the stage was just matched");
        };
        let claim = Claim {
            terminals: self.clone(),
            id: id.to_owned(),
        };
        Some((claim, *requested, arrived))
    }

    /// Where to hand the socket `device_id`'s Agent connected for the
    /// terminal, if it is waiting for one.
    pub fn claim_agent(&self, id: &str, device_id: &str) -> Option<oneshot::Sender<WebSocket>> {
        let mut open = self.lock();
        let entry = open
            .get_mut(id)
            .filter(|entry| entry.device_id == device_id)?;
        if !matches!(entry.stage, Stage::Connecting { .. }) {
            return None;
        }
        match std::mem::replace(&mut entry.stage, Stage::Open) {
            Stage::Connecting { agent } => Some(agent),
            _ => unreachable!("the stage was just matched"),
        }
    }

    /// Forgets a terminal that ended.
    pub fn remove(&self, id: &str) {
        self.lock().remove(id);
    }

    /// The terminals reserved or open on `device_id`.
    pub fn count_on(&self, device_id: &str) -> usize {
        self.lock()
            .values()
            .filter(|entry| entry.device_id == device_id)
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        // The map stays consistent even if a holder panicked.
        self.inner
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested(id: &str, device_id: &str, user_id: &str) -> Requested {
        Requested {
            id: id.into(),
            device_id: device_id.into(),
            user_id: user_id.into(),
            actor: Actor::user(user_id, "ada@example.com", None),
            shell: TerminalShell::Bash,
            run_as: RunAs::System,
            columns: 80,
            rows: 24,
            notification: None,
        }
    }

    #[test]
    fn devices_and_users_are_limited() {
        let terminals = Terminals::default();
        for index in 0..MAX_TERMINALS_PER_DEVICE {
            terminals
                .reserve(requested(
                    &format!("d{index}"),
                    "device",
                    &format!("user{index}"),
                ))
                .unwrap();
        }
        assert_eq!(
            terminals.reserve(requested("extra", "device", "someone")),
            Err(Refusal::DeviceLimit)
        );
        for index in 0..MAX_TERMINALS_PER_USER - 1 {
            terminals
                .reserve(requested(
                    &format!("u{index}"),
                    &format!("device{index}"),
                    "user0",
                ))
                .unwrap();
        }
        assert_eq!(
            terminals.reserve(requested("extra", "other", "user0")),
            Err(Refusal::UserLimit)
        );
        terminals.remove("d0");
        terminals
            .reserve(requested("again", "device", "user1"))
            .unwrap();
        assert_eq!(terminals.count_on("device"), MAX_TERMINALS_PER_DEVICE);
    }

    #[tokio::test]
    async fn each_side_claims_once_and_only_its_own() {
        let terminals = Terminals::default();
        terminals
            .reserve(requested("one", "device", "ada"))
            .unwrap();
        assert!(
            terminals.claim_agent("one", "device").is_none(),
            "no browser yet"
        );
        assert!(terminals.claim_browser("one", "eve").is_none(), "not eve's");
        let (claim, claimed, _arrived) = terminals.claim_browser("one", "ada").unwrap();
        assert_eq!(claimed.device_id, "device");
        assert!(
            terminals.claim_browser("one", "ada").is_none(),
            "claimed once"
        );
        assert!(
            terminals.claim_agent("one", "other").is_none(),
            "not its device"
        );
        assert!(terminals.claim_agent("one", "device").is_some());
        assert!(
            terminals.claim_agent("one", "device").is_none(),
            "claimed once"
        );
        assert_eq!(terminals.count_on("device"), 1);
        drop(claim);
        assert_eq!(terminals.count_on("device"), 0);
    }

    #[test]
    fn unclaimed_requests_expire() {
        let terminals = Terminals::new(Timeouts {
            browser_connect: Duration::ZERO,
            ..Timeouts::default()
        });
        terminals
            .reserve(requested("one", "device", "ada"))
            .unwrap();
        assert!(terminals.claim_browser("one", "ada").is_none());
        for index in 0..MAX_TERMINALS_PER_DEVICE {
            terminals
                .reserve(requested(&format!("n{index}"), "device", "ada"))
                .unwrap();
        }
    }
}
