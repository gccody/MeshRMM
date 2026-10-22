//! The website's file browser: a device's folders and files, without a
//! remote session. See docs/files.md.
//!
//! Listings and changes go to the Agent over its control connection
//! ([`ask`]), and the answer comes back there ([`Files::answered`]). A
//! request whose device disconnects fails at once. File contents never take
//! that path: a download or an upload reserves a transfer here, the Agent
//! connects a socket of its own for it
//! (`/v1/agents/{id}/file-transfers/{transfer_id}`), and [`relay`] streams
//! between that socket and the browser's HTTP request, one piece at a time.
//!
//! Downloads and uploads are named by IDs a user creates first
//! ([`Files::add_download`], [`Files::add_upload`]), so the browser can
//! restart or resume one with a plain request. They live in memory, for
//! [`Timeouts::download`] and [`Timeouts::upload`] after their last use.
//! A device has at most [`MAX_TRANSFERS_PER_DEVICE`] transfers streaming,
//! and a user at most [`MAX_TRANSFERS_PER_USER`].
pub mod relay;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::extract::ws::WebSocket;
use meshrmm_protocol_types::{
    AgentCommand, FileDownloadInfo, FileError, FileErrorCode, FileOperation, FileReply,
    FileRequest, RunAs, render_connection_notification,
};
use serde::Serialize;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{
    audit::Actor,
    auth::Authorized,
    http::{ApiError, AppState},
    rbac::Permission,
    settings,
};

pub const MAX_TRANSFERS_PER_DEVICE: usize = 4;
pub const MAX_TRANSFERS_PER_USER: usize = 6;
/// Downloads and uploads a user may have waiting at once.
const MAX_WAITING_PER_USER: usize = 200;
/// A technician's file operations on a device show the company's
/// connection notification once in this long.
const NOTIFY_EVERY: Duration = Duration::from_secs(30 * 60);

/// How long each step may take.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// A listing or a quick change.
    pub request: Duration,
    /// A change that walks or copies folders, or reads a whole file.
    pub slow_request: Duration,
    /// From asking the Agent for a transfer until its socket connects.
    pub agent_connect: Duration,
    /// A download's ID lasts this long after its last use.
    pub download: Duration,
    /// An unfinished upload's ID lasts this long after its last use.
    pub upload: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(30),
            slow_request: Duration::from_secs(120),
            agent_connect: Duration::from_secs(20),
            download: Duration::from_secs(12 * 60 * 60),
            upload: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// The permission working with files as `run_as` needs, besides
/// `files.browse` or `files.manage`.
pub fn account_permission(run_as: RunAs) -> Option<Permission> {
    match run_as {
        RunAs::System => Some(Permission::FilesSystem),
        RunAs::User => None,
    }
}

/// Checks that `actor` may do what `needed` allows as `run_as`.
pub fn require(actor: &Authorized, needed: Permission, run_as: RunAs) -> Result<(), ApiError> {
    actor.require(needed)?;
    if let Some(account) = account_permission(run_as) {
        actor.require(account)?;
    }
    Ok(())
}

/// A download a user set up: what to send, and how its latest attempt is
/// going.
#[derive(Debug, Clone)]
pub struct Download {
    pub id: String,
    pub device_id: String,
    pub user_id: String,
    pub actor: Actor,
    pub run_as: RunAs,
    pub paths: Vec<String>,
    pub info: FileDownloadInfo,
}

/// How a download's latest attempt went, for the website's list of
/// transfers.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Progress {
    pub state: ProgressState,
    /// Bytes sent so far in this attempt, from `offset`.
    pub bytes: u64,
    pub offset: u64,
    /// What this attempt sends, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressState {
    /// Nothing has been asked for yet.
    #[default]
    Ready,
    Sending,
    Done,
    Failed,
}

/// An upload a user started: where its bytes go.
#[derive(Debug, Clone)]
pub struct Upload {
    pub id: String,
    pub device_id: String,
    pub user_id: String,
    pub actor: Actor,
    pub run_as: RunAs,
    pub path: String,
    pub size: u64,
    pub overwrite: bool,
}

/// Why a transfer can't start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    DeviceLimit,
    UserLimit,
}

impl From<Refusal> for ApiError {
    fn from(refusal: Refusal) -> Self {
        let message = match refusal {
            Refusal::DeviceLimit => format!(
                "the device is already transferring {MAX_TRANSFERS_PER_DEVICE} files; try again when one finishes"
            ),
            Refusal::UserLimit => format!(
                "you are already transferring {MAX_TRANSFERS_PER_USER} files; try again when one finishes"
            ),
        };
        ApiError::new(axum::http::StatusCode::TOO_MANY_REQUESTS, message)
            .with_code("too_many_transfers")
    }
}

#[derive(Clone)]
pub struct Files {
    inner: Arc<Inner>,
}

struct Inner {
    timeouts: Timeouts,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Requests waiting for the Agent's answer, by request ID.
    pending: HashMap<String, Pending>,
    /// Transfers streaming or waiting for the Agent's socket.
    transfers: HashMap<String, Transfer>,
    downloads: HashMap<String, (Download, Progress, Instant)>,
    uploads: HashMap<String, (Upload, Instant)>,
    /// When each user's operations on each device last notified its user.
    notified: HashMap<(String, String, RunAs), Instant>,
}

struct Pending {
    device_id: String,
    reply: oneshot::Sender<FileReply>,
}

struct Transfer {
    device_id: String,
    user_id: String,
    /// Where the Agent's socket goes, until it arrives.
    agent: Option<oneshot::Sender<WebSocket>>,
    /// Cancelled when the device goes offline.
    offline: CancellationToken,
}

/// A reserved transfer: its ID, and where the Agent's socket will arrive.
/// Dropping it forgets the transfer.
pub struct TransferClaim {
    files: Files,
    pub id: String,
    pub agent: Option<oneshot::Receiver<WebSocket>>,
    /// Cancelled when the device's control connection ends, which notices
    /// a device that vanished (powered off, off the network) within a
    /// minute; its transfer socket may otherwise hang far longer.
    pub offline: CancellationToken,
}

impl Drop for TransferClaim {
    fn drop(&mut self) {
        self.files.lock().transfers.remove(&self.id);
    }
}

impl std::fmt::Debug for Files {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        formatter
            .debug_struct("Files")
            .field("pending", &state.pending.len())
            .field("transfers", &state.transfers.len())
            .finish()
    }
}

impl Default for Files {
    fn default() -> Self {
        Self::new(Timeouts::default())
    }
}

impl Files {
    pub fn new(timeouts: Timeouts) -> Self {
        Self {
            inner: Arc::new(Inner {
                timeouts,
                state: Mutex::default(),
            }),
        }
    }

    pub fn timeouts(&self) -> Timeouts {
        self.inner.timeouts
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // The maps stay consistent even if a holder panicked.
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hands `device_id`'s answer to the request waiting for it.
    pub fn answered(&self, device_id: &str, request_id: &str, reply: FileReply) {
        let mut state = self.lock();
        if state
            .pending
            .get(request_id)
            .is_some_and(|pending| pending.device_id == device_id)
            && let Some(pending) = state.pending.remove(request_id)
        {
            let _ = pending.reply.send(reply);
        }
    }

    /// Fails the requests waiting on a device that disconnected, and its
    /// transfers.
    pub fn device_disconnected(&self, device_id: &str) {
        let mut state = self.lock();
        state
            .pending
            .retain(|_, pending| pending.device_id != device_id);
        for transfer in state.transfers.values() {
            if transfer.device_id == device_id {
                transfer.offline.cancel();
            }
        }
    }

    fn forget_request(&self, request_id: &str) {
        self.lock().pending.remove(request_id);
    }

    /// Reserves a transfer for `user_id` on `device_id`, unless either has
    /// as many as allowed.
    pub fn reserve(&self, device_id: &str, user_id: &str) -> Result<TransferClaim, Refusal> {
        let mut state = self.lock();
        let on_device = state
            .transfers
            .values()
            .filter(|transfer| transfer.device_id == device_id)
            .count();
        if on_device >= MAX_TRANSFERS_PER_DEVICE {
            return Err(Refusal::DeviceLimit);
        }
        let by_user = state
            .transfers
            .values()
            .filter(|transfer| transfer.user_id == user_id)
            .count();
        if by_user >= MAX_TRANSFERS_PER_USER {
            return Err(Refusal::UserLimit);
        }
        let id = uuid::Uuid::new_v4().to_string();
        let (agent, arrived) = oneshot::channel();
        let offline = CancellationToken::new();
        state.transfers.insert(
            id.clone(),
            Transfer {
                device_id: device_id.to_owned(),
                user_id: user_id.to_owned(),
                agent: Some(agent),
                offline: offline.clone(),
            },
        );
        Ok(TransferClaim {
            files: self.clone(),
            id,
            agent: Some(arrived),
            offline,
        })
    }

    /// Where to hand the socket `device_id`'s Agent connected for a
    /// transfer, if one is waiting for it.
    pub fn claim_agent(&self, id: &str, device_id: &str) -> Option<oneshot::Sender<WebSocket>> {
        self.lock()
            .transfers
            .get_mut(id)
            .filter(|transfer| transfer.device_id == device_id)
            .and_then(|transfer| transfer.agent.take())
    }

    /// The transfers streaming or waiting on `device_id`.
    pub fn transfers_on(&self, device_id: &str) -> usize {
        self.lock()
            .transfers
            .values()
            .filter(|transfer| transfer.device_id == device_id)
            .count()
    }

    fn expire(&self, state: &mut State) {
        let timeouts = self.inner.timeouts;
        state
            .downloads
            .retain(|_, (_, _, used)| used.elapsed() < timeouts.download);
        state
            .uploads
            .retain(|_, (_, used)| used.elapsed() < timeouts.upload);
        state
            .notified
            .retain(|_, when| when.elapsed() < NOTIFY_EVERY);
    }

    fn waiting_for(state: &State, user_id: &str) -> usize {
        state
            .downloads
            .values()
            .filter(|(download, ..)| download.user_id == user_id)
            .count()
            + state
                .uploads
                .values()
                .filter(|(upload, _)| upload.user_id == user_id)
                .count()
    }

    fn too_many_waiting() -> ApiError {
        ApiError::new(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            format!(
                "you have {MAX_WAITING_PER_USER} downloads and uploads waiting; finish or cancel some first"
            ),
        )
        .with_code("too_many_transfers")
    }

    pub fn add_download(&self, download: Download) -> Result<(), ApiError> {
        let mut state = self.lock();
        self.expire(&mut state);
        if Self::waiting_for(&state, &download.user_id) >= MAX_WAITING_PER_USER {
            return Err(Self::too_many_waiting());
        }
        state.downloads.insert(
            download.id.clone(),
            (download, Progress::default(), Instant::now()),
        );
        Ok(())
    }

    /// `user_id`'s download `id`, marked used now.
    pub fn download(&self, id: &str, user_id: &str) -> Option<(Download, Progress)> {
        let mut state = self.lock();
        self.expire(&mut state);
        let (download, progress, used) = state
            .downloads
            .get_mut(id)
            .filter(|(download, ..)| download.user_id == user_id)?;
        *used = Instant::now();
        Some((download.clone(), progress.clone()))
    }

    pub fn set_progress(&self, id: &str, change: impl FnOnce(&mut Progress)) {
        if let Some((_, progress, used)) = self.lock().downloads.get_mut(id) {
            change(progress);
            *used = Instant::now();
        }
    }

    pub fn add_upload(&self, upload: Upload) -> Result<(), ApiError> {
        let mut state = self.lock();
        self.expire(&mut state);
        if Self::waiting_for(&state, &upload.user_id) >= MAX_WAITING_PER_USER {
            return Err(Self::too_many_waiting());
        }
        state
            .uploads
            .insert(upload.id.clone(), (upload, Instant::now()));
        Ok(())
    }

    /// `user_id`'s upload `id`, marked used now.
    pub fn upload(&self, id: &str, user_id: &str) -> Option<Upload> {
        let mut state = self.lock();
        self.expire(&mut state);
        let (upload, used) = state
            .uploads
            .get_mut(id)
            .filter(|(upload, _)| upload.user_id == user_id)?;
        *used = Instant::now();
        Some(upload.clone())
    }

    pub fn remove_upload(&self, id: &str) {
        self.lock().uploads.remove(id);
    }

    /// Whether `user_id`'s operations as `run_as` on `device_id` should show
    /// its user the connection notification now: the first in a while.
    fn notify_now(&self, user_id: &str, device_id: &str, run_as: RunAs) -> bool {
        let mut state = self.lock();
        self.expire(&mut state);
        let key = (user_id.to_owned(), device_id.to_owned(), run_as);
        if state.notified.contains_key(&key) {
            return false;
        }
        state.notified.insert(key, Instant::now());
        true
    }
}

/// The company's connection notification for `actor`'s file operation on
/// `device_id`, when policy shows one and this is their first in a while.
/// Files as the signed-in user follow the remote session notification;
/// files as SYSTEM or root, like background mode, the background one.
async fn notification(
    state: &AppState,
    actor: &Authorized,
    device_id: &str,
    run_as: RunAs,
) -> Result<Option<String>, ApiError> {
    let settings = settings::load(&mut &state.database).await?;
    let notify = match run_as {
        RunAs::User => settings.connection_notification,
        RunAs::System => settings.background_connection_notification,
    };
    if !notify || !state.files.notify_now(&actor.user.id, device_id, run_as) {
        return Ok(None);
    }
    Ok(Some(render_connection_notification(
        &settings.connection_notification_message,
        &actor.user.display_name,
    )))
}

/// Whether `operation` may take a while on the device.
fn slow(operation: &FileOperation) -> bool {
    matches!(
        operation,
        FileOperation::Move { .. }
            | FileOperation::Delete { .. }
            | FileOperation::PrepareDownload { .. }
            | FileOperation::FinishUpload { .. }
    )
}

pub fn device_offline() -> ApiError {
    ApiError::conflict("the device is offline").with_code("device_offline")
}

/// Asks `device_id`'s Agent to do `operation` as `run_as` for `actor`, and
/// waits for the answer. A failure the Agent reports is an error with its
/// code.
pub async fn ask(
    state: &AppState,
    actor: &Authorized,
    device_id: &str,
    run_as: RunAs,
    operation: FileOperation,
) -> Result<FileReply, ApiError> {
    if !state.agents.is_connected(device_id) {
        return Err(device_offline());
    }
    let timeouts = state.files.timeouts();
    let wait = if slow(&operation) {
        timeouts.slow_request
    } else {
        timeouts.request
    };
    let notification = notification(state, actor, device_id, run_as).await?;
    let request_id = uuid::Uuid::new_v4().to_string();
    let (reply, answer) = oneshot::channel();
    state.files.lock().pending.insert(
        request_id.clone(),
        Pending {
            device_id: device_id.to_owned(),
            reply,
        },
    );
    let name = operation.name();
    let command = AgentCommand::FileRequest {
        request: FileRequest {
            request_id: request_id.clone(),
            run_as,
            notification,
            operation,
        },
    };
    if !state.agents.send(device_id, command) {
        state.files.forget_request(&request_id);
        return Err(device_offline());
    }
    let answer = tokio::time::timeout(wait, answer).await;
    state.files.forget_request(&request_id);
    match answer {
        Ok(Ok(FileReply::Failed { error })) => Err(agent_error(error)),
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(_)) => Err(ApiError::conflict(
            "the device disconnected; try again once it's back online",
        )
        .with_code("device_disconnected")),
        Err(_) => {
            tracing::warn!(
                device_id,
                operation = name,
                "the device didn't answer a file request in time"
            );
            Err(ApiError::new(
                axum::http::StatusCode::GATEWAY_TIMEOUT,
                "the device didn't answer in time; it may still be working on it, so refresh in a moment",
            )
            .with_code("device_timeout"))
        }
    }
}

/// An error the Agent reported, with the status that suits it.
pub fn agent_error(error: FileError) -> ApiError {
    use axum::http::StatusCode;
    let error = error.sanitized();
    let status = match error.code {
        FileErrorCode::NotFound => StatusCode::NOT_FOUND,
        FileErrorCode::AccessDenied => StatusCode::FORBIDDEN,
        FileErrorCode::AlreadyExists
        | FileErrorCode::NoUser
        | FileErrorCode::OffsetMismatch
        | FileErrorCode::Changed
        | FileErrorCode::TrashUnavailable => StatusCode::CONFLICT,
        FileErrorCode::NotADirectory
        | FileErrorCode::IsADirectory
        | FileErrorCode::InvalidPath
        | FileErrorCode::InvalidName
        | FileErrorCode::InvalidMove => StatusCode::BAD_REQUEST,
        FileErrorCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        FileErrorCode::NoSpace => StatusCode::INSUFFICIENT_STORAGE,
        FileErrorCode::ChecksumMismatch => StatusCode::UNPROCESSABLE_ENTITY,
        FileErrorCode::Busy => StatusCode::TOO_MANY_REQUESTS,
        FileErrorCode::Failed => StatusCode::BAD_GATEWAY,
    };
    ApiError::new(status, error.message).with_code(error.code.as_str())
}

/// The largest transfer the company allows, in bytes.
pub async fn transfer_limit(state: &AppState) -> Result<u64, ApiError> {
    let settings = settings::load(&mut &state.database).await?;
    Ok(settings.file_transfer_limit_mb.max(1) as u64 * 1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfers_are_limited_per_device_and_user() {
        let files = Files::default();
        let claims = (0..MAX_TRANSFERS_PER_DEVICE)
            .map(|index| files.reserve("device", &format!("user{index}")).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            files.reserve("device", "other").err(),
            Some(Refusal::DeviceLimit)
        );
        let mut mine = (0..MAX_TRANSFERS_PER_USER)
            .map(|index| files.reserve(&format!("d{index}"), "ada").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            files.reserve("elsewhere", "ada").err(),
            Some(Refusal::UserLimit)
        );
        mine.pop();
        files.reserve("elsewhere", "ada").unwrap();
        assert_eq!(files.transfers_on("device"), MAX_TRANSFERS_PER_DEVICE);
        drop(claims);
        assert_eq!(files.transfers_on("device"), 0);
    }

    #[test]
    fn agents_claim_only_their_own_transfers_once() {
        let files = Files::default();
        let claim = files.reserve("device", "ada").unwrap();
        assert!(files.claim_agent(&claim.id, "other").is_none());
        assert!(files.claim_agent(&claim.id, "device").is_some());
        assert!(files.claim_agent(&claim.id, "device").is_none());
        let id = claim.id.clone();
        drop(claim);
        assert!(files.claim_agent(&id, "device").is_none());
    }

    #[test]
    fn a_device_going_offline_stops_its_transfers() {
        let files = Files::default();
        let mine = files.reserve("device", "ada").unwrap();
        let other = files.reserve("other", "ada").unwrap();
        files.device_disconnected("device");
        assert!(mine.offline.is_cancelled());
        assert!(!other.offline.is_cancelled());
    }

    #[test]
    fn answers_reach_only_the_request_from_their_device() {
        let files = Files::default();
        let (reply, mut answer) = oneshot::channel();
        files.lock().pending.insert(
            "r".into(),
            Pending {
                device_id: "device".into(),
                reply,
            },
        );
        files.answered("other", "r", FileReply::Upload { received: 1 });
        assert!(answer.try_recv().is_err());
        files.answered("device", "r", FileReply::Upload { received: 2 });
        assert_eq!(
            answer.try_recv().unwrap(),
            FileReply::Upload { received: 2 }
        );
        let (reply, mut answer) = oneshot::channel();
        files.lock().pending.insert(
            "s".into(),
            Pending {
                device_id: "device".into(),
                reply,
            },
        );
        files.device_disconnected("device");
        assert!(matches!(
            answer.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
    }

    #[test]
    fn notifications_come_once_in_a_while() {
        let files = Files::default();
        assert!(files.notify_now("ada", "device", RunAs::System));
        assert!(!files.notify_now("ada", "device", RunAs::System));
        assert!(files.notify_now("ada", "device", RunAs::User));
        assert!(files.notify_now("bob", "device", RunAs::System));
    }

    #[test]
    fn agent_errors_keep_their_code() {
        let error = agent_error(FileError::new(
            FileErrorCode::AccessDenied,
            "Access to /root is denied.",
        ));
        assert_eq!(error.status(), axum::http::StatusCode::FORBIDDEN);
        assert_eq!(error.code(), Some("access_denied"));
    }
}
