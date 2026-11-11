//! Remote tools: a device's processes and services, listed and changed
//! from the website without a remote session. See docs/remote-tools.md.
//!
//! As with the file browser's listings, each request goes to the Agent over
//! its control connection ([`ask`]) and the answer comes back there
//! ([`Tools::answered`]). A request whose device disconnects fails at once;
//! one it doesn't answer in time fails as `device_timeout`. A device has at
//! most [`MAX_PENDING_PER_DEVICE`] requests waiting, so a page left open in
//! many tabs can't pile requests up on it; more are `device_busy`, as when
//! its connection's backlog is full.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::http::StatusCode;
use meshrmm_protocol_types::{
    AgentCommand, ToolError, ToolErrorCode, ToolOperation, ToolReply, ToolRequest,
};
use tokio::sync::oneshot;

use crate::http::{ApiError, AppState};

/// Requests that may wait for one device's answers at once.
pub const MAX_PENDING_PER_DEVICE: usize = 16;

/// How long the server waits for the Agent.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// A list.
    pub request: Duration,
    /// Ending processes, or starting, stopping or changing a service.
    pub slow_request: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(30),
            slow_request: Duration::from_secs(120),
        }
    }
}

#[derive(Clone)]
pub struct Tools {
    inner: Arc<Inner>,
}

struct Inner {
    timeouts: Timeouts,
    pending: Mutex<HashMap<String, Pending>>,
}

struct Pending {
    device_id: String,
    reply: oneshot::Sender<ToolReply>,
}

/// A request waiting for its Agent's answer. Dropping it forgets the
/// request, as when the browser goes away while it waits.
struct PendingRequest {
    tools: Tools,
    id: String,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.tools.lock().remove(&self.id);
    }
}

impl std::fmt::Debug for Tools {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Tools")
            .field("pending", &self.lock().len())
            .finish()
    }
}

impl Default for Tools {
    fn default() -> Self {
        Self::new(Timeouts::default())
    }
}

impl Tools {
    pub fn new(timeouts: Timeouts) -> Self {
        Self {
            inner: Arc::new(Inner {
                timeouts,
                pending: Mutex::default(),
            }),
        }
    }

    pub fn timeouts(&self) -> Timeouts {
        self.inner.timeouts
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        // The map stays consistent even if a holder panicked.
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hands `device_id`'s answer to the request waiting for it.
    pub fn answered(&self, device_id: &str, request_id: &str, reply: ToolReply) {
        let mut pending = self.lock();
        if pending
            .get(request_id)
            .is_some_and(|waiting| waiting.device_id == device_id)
            && let Some(waiting) = pending.remove(request_id)
        {
            let _ = waiting.reply.send(reply);
        }
    }

    /// Fails the requests waiting on a device that disconnected.
    pub fn device_disconnected(&self, device_id: &str) {
        self.lock()
            .retain(|_, waiting| waiting.device_id != device_id);
    }

    /// The requests waiting for an Agent's answer.
    pub fn pending_requests(&self) -> usize {
        self.lock().len()
    }

    /// Waits for `device_id`'s answer to a new request, unless as many wait
    /// on it as may.
    fn expect_answer(
        &self,
        device_id: &str,
    ) -> Option<(PendingRequest, oneshot::Receiver<ToolReply>)> {
        let mut pending = self.lock();
        let waiting = pending
            .values()
            .filter(|waiting| waiting.device_id == device_id)
            .count();
        if waiting >= MAX_PENDING_PER_DEVICE {
            return None;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (reply, answer) = oneshot::channel();
        pending.insert(
            id.clone(),
            Pending {
                device_id: device_id.to_owned(),
                reply,
            },
        );
        drop(pending);
        let guard = PendingRequest {
            tools: self.clone(),
            id,
        };
        Some((guard, answer))
    }
}

const DEVICE_TIMEOUT: &str = "device_timeout";
const DEVICE_DISCONNECTED: &str = "device_disconnected";

/// Whether `error` from [`ask`] leaves unknown what the device did: it
/// didn't answer in time, or disconnected after it was asked.
pub fn unconfirmed(error: &ApiError) -> bool {
    matches!(error.code(), Some(DEVICE_TIMEOUT | DEVICE_DISCONNECTED))
}

pub fn device_offline() -> ApiError {
    ApiError::conflict("the device is offline").with_code("device_offline")
}

/// Whether `error` from [`ask`] means the request never reached the device:
/// it was offline, or had as many requests waiting as it may.
pub fn never_sent(error: &ApiError) -> bool {
    matches!(error.code(), Some("device_offline" | "device_busy"))
}

/// Asks `device_id`'s Agent to do `operation`, and waits for the answer. A
/// failure the Agent reports is an error with its code.
pub async fn ask(
    state: &AppState,
    device_id: &str,
    operation: ToolOperation,
) -> Result<ToolReply, ApiError> {
    if !state.agents.is_connected(device_id) {
        return Err(device_offline());
    }
    let timeouts = state.tools.timeouts();
    let wait = if operation.slow() {
        timeouts.slow_request
    } else {
        timeouts.request
    };
    let (request, answer) = state
        .tools
        .expect_answer(device_id)
        .ok_or_else(ApiError::device_busy)?;
    let name = operation.name();
    let command = AgentCommand::ToolRequest {
        request: ToolRequest {
            request_id: request.id.clone(),
            timeout_ms: u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
            operation,
        },
    };
    if let Err(unsent) = state.agents.send(device_id, command) {
        return Err(unsent.answer(device_offline()));
    }
    let answer = tokio::time::timeout(wait, answer).await;
    drop(request);
    match answer {
        Ok(Ok(ToolReply::Failed { error })) => Err(agent_error(error)),
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(_)) => Err(ApiError::conflict(
            "the device disconnected; try again once it's back online",
        )
        .with_code(DEVICE_DISCONNECTED)),
        Err(_) => {
            tracing::warn!(
                device_id,
                operation = name,
                "the device didn't answer a tool request in time"
            );
            Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "the device didn't answer in time; it may have done this, or still be doing it, so refresh in a moment to see",
            )
            .with_code(DEVICE_TIMEOUT))
        }
    }
}

/// An error the Agent reported, with the status that suits it.
pub fn agent_error(error: ToolError) -> ApiError {
    let error = error.sanitized();
    let status = match error.code {
        ToolErrorCode::NotFound => StatusCode::NOT_FOUND,
        ToolErrorCode::AccessDenied | ToolErrorCode::Protected => StatusCode::FORBIDDEN,
        ToolErrorCode::Changed
        | ToolErrorCode::Dependents
        | ToolErrorCode::Exists
        | ToolErrorCode::NotEmpty => StatusCode::CONFLICT,
        ToolErrorCode::Invalid | ToolErrorCode::Unsupported => StatusCode::BAD_REQUEST,
        ToolErrorCode::Busy => StatusCode::TOO_MANY_REQUESTS,
        ToolErrorCode::TimedOut => StatusCode::GATEWAY_TIMEOUT,
        ToolErrorCode::Failed => StatusCode::BAD_GATEWAY,
    };
    ApiError::new(status, error.message).with_code(error.code.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed() -> ToolReply {
        ToolReply::failed(ToolErrorCode::Failed, "x")
    }

    #[test]
    fn answers_reach_only_the_request_from_their_device() {
        let tools = Tools::default();
        let (request, mut answer) = tools.expect_answer("device").unwrap();
        tools.answered("other", &request.id, failed());
        assert!(answer.try_recv().is_err());
        tools.answered("device", &request.id, failed());
        assert_eq!(answer.try_recv().unwrap(), failed());

        let (_request, mut answer) = tools.expect_answer("device").unwrap();
        tools.device_disconnected("device");
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn a_device_has_only_so_many_requests_waiting() {
        let tools = Tools::default();
        let waiting = (0..MAX_PENDING_PER_DEVICE)
            .map(|_| tools.expect_answer("device").unwrap())
            .collect::<Vec<_>>();
        assert!(tools.expect_answer("device").is_none());
        assert!(tools.expect_answer("other").is_some());
        drop(waiting);
        assert_eq!(tools.pending_requests(), 0);
        assert!(tools.expect_answer("device").is_some());
    }

    #[test]
    fn agent_errors_keep_their_code() {
        let error = agent_error(ToolError::new(ToolErrorCode::Protected, "no"));
        assert_eq!(error.status(), StatusCode::FORBIDDEN);
        assert_eq!(error.code(), Some("protected"));
    }
}
