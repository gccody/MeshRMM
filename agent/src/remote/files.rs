//! The website's file browser on this computer. See docs/files.md.
//!
//! The server asks for listings and changes over the control connection
//! ([`FileRequest`]), and the Agent answers there ([`Files::next`]). Each
//! runs on a blocking thread, as SYSTEM or root, or as the user signed in to
//! the computer: on a thread of its own that takes on the user's identity
//! (Windows impersonation; per-thread credentials on macOS and Linux) and
//! ends with the operation, so nothing else ever runs as them. File contents
//! go over a socket of the transfer's own ([`transfer`]), never the control
//! connection.
//!
//! Each request carries how long the server waits for it, which the Agent
//! counts from its arrival ([`Deadline`]). One still waiting for its turn
//! when that runs out isn't started; a move or delete stops between
//! entries, and answers with what it did, a little before the server gives
//! up. A system call that never returns (a dead network mount, a hung
//! drive) can't be stopped: once the server has given up on it, its
//! operation is left behind as a stray that no longer holds a turn, and
//! its answer, if it ever comes, goes nowhere. While [`Limits::strays`] are
//! left behind, new requests are refused as busy.
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use meshrmm_protocol::{FileError, FileErrorCode, FileOperation, FileReply, FileRequest};
use tokio::sync::{Semaphore, mpsc};

use super::config::ExecutionMode;

mod account;
mod archive;
mod listing;
mod operations;
mod paths;
mod roots;
pub(super) mod transfer;
#[cfg(unix)]
mod unix;
mod uploads;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use self::unix as platform;
#[cfg(windows)]
use self::windows as platform;

use self::account::Account;

/// How often abandoned uploads are looked for.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// How the file browser keeps to the time the server waits.
#[derive(Debug, Clone, Copy)]
struct Limits {
    /// Operations that run at once; more wait their turn.
    concurrent: usize,
    /// Operations that never returned that are left behind at most. While
    /// there are this many, new requests are refused as busy, and one more
    /// that doesn't return keeps its turn: the Agent never has more than
    /// `concurrent + strays` threads working on files.
    strays: usize,
    /// How long before the server gives up an operation answers with what
    /// it did, so the answer arrives in time; at most a quarter of the wait.
    margin: Duration,
    /// How long past the server's wait an operation that hasn't returned
    /// keeps its turn before it is left behind.
    grace: Duration,
}

const LIMITS: Limits = Limits {
    concurrent: 4,
    strays: 8,
    margin: Duration::from_secs(5),
    grace: Duration::from_secs(5),
};

/// When an operation must answer by: a little before the server stops
/// waiting for it, counted from the request's arrival on this computer's
/// monotonic clock. Moves, deletes and the measuring of a download check it
/// between entries.
pub(super) struct Deadline {
    at: Instant,
    /// For tests, passes once it has been checked this many times more.
    #[cfg(test)]
    checks: Option<AtomicUsize>,
}

impl Deadline {
    fn at(at: Instant) -> Self {
        Self {
            at,
            #[cfg(test)]
            checks: None,
        }
    }

    /// When a request that arrived at `arrived`, for which the server waits
    /// `wait`, answers by.
    fn answering(arrived: Instant, wait: Duration, margin: Duration) -> Self {
        Self::at(arrived + wait.saturating_sub(margin.min(wait / 4)))
    }

    /// Whether the time is up.
    pub(super) fn passed(&self) -> bool {
        #[cfg(test)]
        if let Some(checks) = &self.checks {
            return checks
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                    left.checked_sub(1)
                })
                .is_err();
        }
        Instant::now() >= self.at
    }

    /// A deadline far away.
    #[cfg(test)]
    pub(super) fn distant() -> Self {
        Self::at(Instant::now() + Duration::from_secs(60 * 60))
    }

    /// A deadline that passes after `checks` checks that it hasn't.
    #[cfg(test)]
    pub(super) fn after_checks(checks: usize) -> Self {
        Self {
            checks: Some(AtomicUsize::new(checks)),
            ..Self::distant()
        }
    }
}

/// The error for work the deadline stopped, as `message` says.
fn timed_out(message: impl Into<String>) -> FileError {
    FileError::new(FileErrorCode::TimedOut, message)
}

/// Runs one request; injected by tests.
type Operate =
    dyn Fn(ExecutionMode, &listing::Cache, FileRequest, &Deadline) -> FileReply + Send + Sync;

/// The Coordinator's file browser: runs the server's requests and queues
/// their answers. It outlives a connection, so an answer that arrives after
/// a reconnect is still sent; the server ignores answers it gave up on.
pub(super) struct Files {
    replies_tx: mpsc::UnboundedSender<(String, FileReply)>,
    replies: mpsc::UnboundedReceiver<(String, FileReply)>,
    limits: Limits,
    operate: Arc<Operate>,
    running: Arc<Semaphore>,
    /// Operations left behind that haven't returned.
    strays: Arc<AtomicUsize>,
    listings: Arc<listing::Cache>,
    sweep: tokio::time::Interval,
    /// The Agent's mode, from its first request, for the sweep to find the
    /// signed-in user.
    mode: OnceLock<ExecutionMode>,
    /// The sweep running, if one is: one at a time, however long a slow
    /// drive holds one up.
    sweeping: Option<tokio::task::JoinHandle<()>>,
}

impl Files {
    pub(super) fn new() -> Self {
        Self::with(LIMITS, Arc::new(operate))
    }

    fn with(limits: Limits, operate: Arc<Operate>) -> Self {
        let (replies_tx, replies) = mpsc::unbounded_channel();
        let mut sweep = tokio::time::interval(SWEEP_INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            replies_tx,
            replies,
            limits,
            operate,
            running: Arc::new(Semaphore::new(limits.concurrent)),
            strays: Arc::new(AtomicUsize::new(0)),
            listings: Arc::new(listing::Cache::default()),
            sweep,
            mode: OnceLock::new(),
            sweeping: None,
        }
    }

    /// Runs `request` in the background; its answer comes from [`Self::next`].
    pub(super) fn request(&self, mode: ExecutionMode, request: FileRequest) {
        self.mode.get_or_init(|| mode);
        let arrived = Instant::now();
        let wait = Duration::from_millis(request.timeout_ms);
        let deadline = Deadline::answering(arrived, wait, self.limits.margin);
        // The server has given up by then.
        let give_up = arrived + wait + self.limits.grace;
        let limits = self.limits;
        let operate = Arc::clone(&self.operate);
        let replies = self.replies_tx.clone();
        let running = Arc::clone(&self.running);
        let strays = Arc::clone(&self.strays);
        let listings = Arc::clone(&self.listings);
        tokio::spawn(async move {
            let request_id = request.request_id.clone();
            let operation = request.operation.name();
            let answer = |reply: FileReply| {
                if let FileReply::Failed { error } = &reply {
                    tracing::info!(%request_id, operation, code = error.code.as_str(), message = error.message, "a file operation failed");
                }
                let _ = replies.send((request_id.clone(), reply));
            };
            if strays.load(Ordering::Relaxed) >= limits.strays {
                return answer(busy(limits.strays));
            }
            let permit = match tokio::time::timeout_at(
                deadline.at.into(),
                Arc::clone(&running).acquire_owned(),
            )
            .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return,
                Err(_) => return answer(not_started()),
            };
            if deadline.passed() {
                return answer(not_started());
            }
            // Others may have been left behind while it waited.
            if strays.load(Ordering::Relaxed) >= limits.strays {
                return answer(busy(limits.strays));
            }
            if let Some(text) = request.notification.clone() {
                super::terminal::show_notice(mode, text);
            }
            let mut work =
                tokio::task::spawn_blocking(move || operate(mode, &listings, request, &deadline));
            let finished = tokio::select! {
                finished = &mut work => finished,
                () = tokio::time::sleep_until(give_up.into()) => {
                    // The server has stopped waiting, so whatever it answers
                    // is dropped. Left behind, it stops holding its turn,
                    // unless as many are left behind already.
                    let left_behind = strays
                        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                            (count < limits.strays).then_some(count + 1)
                        })
                        .is_ok();
                    if left_behind {
                        drop(permit);
                    }
                    tracing::warn!(%request_id, operation, left_behind, "a file operation didn't return in the time the server waits");
                    let late = work.await;
                    if left_behind {
                        strays.fetch_sub(1, Ordering::Relaxed);
                    }
                    tracing::info!(%request_id, operation, finished = late.is_ok(), "a file operation the Agent gave up on returned; its answer is dropped");
                    return;
                }
            };
            drop(permit);
            answer(finished.unwrap_or_else(|error| {
                tracing::warn!(%error, operation, "a file operation's task failed");
                FileReply::failed(FileErrorCode::Failed, "The Agent could not finish it.")
            }));
        });
    }

    /// The operations left behind that haven't returned.
    #[cfg(test)]
    fn strays(&self) -> usize {
        self.strays.load(Ordering::Relaxed)
    }

    /// The next answer to send, and every hour, a look for uploads
    /// abandoned for a day.
    pub(super) async fn next(&mut self) -> (String, FileReply) {
        loop {
            tokio::select! {
                reply = self.replies.recv() => {
                    if let Some(reply) = reply {
                        return reply;
                    }
                }
                _ = self.sweep.tick() => {
                    if self.sweeping.as_ref().is_none_or(|sweep| sweep.is_finished()) {
                        let mode = self.mode.get().copied();
                        self.sweeping = Some(tokio::task::spawn_blocking(move || uploads::sweep(mode)));
                    }
                }
            }
        }
    }
}

/// The answer to a request whose time ran out before its turn came.
fn not_started() -> FileReply {
    FileReply::Failed {
        error: timed_out(
            "The device was busy with other file operations and didn't start this one in time, so nothing was changed. Try again.",
        ),
    }
}

/// The answer to a request while `strays` operations are left behind.
fn busy(strays: usize) -> FileReply {
    FileReply::failed(
        FileErrorCode::Busy,
        format!(
            "{strays} earlier file operations on this device never finished (a drive or network folder may have stopped answering), so the Agent isn't starting more until they do. Restarting the MeshRMM Agent ends them."
        ),
    )
}

/// Runs one request as its account, answering by `deadline`.
fn operate(
    mode: ExecutionMode,
    listings: &listing::Cache,
    request: FileRequest,
    deadline: &Deadline,
) -> FileReply {
    let account = match Account::resolve(mode, request.run_as) {
        Ok(account) => account,
        Err(error) => return FileReply::Failed { error },
    };
    let result = match request.operation {
        FileOperation::Roots => Ok(roots::roots(mode, &account)),
        FileOperation::List {
            path,
            offset,
            limit,
            sort,
            descending,
            hidden,
            filter,
            refresh,
        } => listing::list(
            &account,
            listings,
            &path,
            listing::Page {
                offset,
                limit,
                sort,
                descending,
                hidden,
                filter,
                refresh,
            },
        ),
        operation => {
            // Anything that changes files makes recent listings stale.
            if !matches!(
                operation,
                FileOperation::PrepareDownload { .. } | FileOperation::UploadStatus { .. }
            ) {
                listings.clear();
            }
            change(&account, operation, deadline)
        }
    };
    result.unwrap_or_else(|error| FileReply::Failed {
        error: error.sanitized(),
    })
}

fn change(
    account: &Account,
    operation: FileOperation,
    deadline: &Deadline,
) -> Result<FileReply, FileError> {
    match operation {
        FileOperation::Roots | FileOperation::List { .. } => {
            unreachable!("listings are handled by operate")
        }
        FileOperation::CreateFolder { path } => operations::create_folder(account, &path),
        FileOperation::Rename { path, name } => operations::rename(account, &path, &name),
        FileOperation::Move { paths, destination } => {
            operations::move_all(account, &paths, &destination, deadline).map(FileReply::Batch)
        }
        FileOperation::Delete { paths, trash } => {
            operations::delete_all(account, &paths, trash, deadline).map(FileReply::Batch)
        }
        FileOperation::PrepareDownload { paths, max_bytes } => {
            operations::prepare_download(account, &paths, max_bytes, deadline)
                .map(FileReply::Download)
        }
        FileOperation::PrepareUpload {
            upload_id,
            path,
            size,
            overwrite,
        } => uploads::prepare(account, &upload_id, &path, size, overwrite),
        FileOperation::UploadStatus { upload_id, path } => {
            uploads::status(account, &upload_id, &path)
        }
        FileOperation::FinishUpload {
            upload_id,
            path,
            size,
            sha256,
            overwrite,
        } => uploads::finish(account, &upload_id, &path, size, &sha256, overwrite),
        FileOperation::CancelUpload { upload_id, path } => {
            uploads::cancel(account, &upload_id, &path)
        }
    }
}

/// `time` in milliseconds since the Unix epoch.
fn unix_ms(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).ok(),
        Err(before) => i64::try_from(before.duration().as_millis())
            .ok()
            .map(|ms| -ms),
    }
}

fn now_ms() -> i64 {
    unix_ms(SystemTime::now()).unwrap_or_default()
}

/// What went wrong with `path`, for the website.
fn io_error(error: &std::io::Error, path: &Path) -> FileError {
    use std::io::ErrorKind;
    let shown = path.display();
    let (code, message) = match error.kind() {
        ErrorKind::NotFound => (FileErrorCode::NotFound, format!("{shown} doesn't exist.")),
        ErrorKind::PermissionDenied => (FileErrorCode::AccessDenied, platform::denied(error, path)),
        ErrorKind::AlreadyExists => (
            FileErrorCode::AlreadyExists,
            format!("{shown} already exists."),
        ),
        ErrorKind::NotADirectory => (
            FileErrorCode::NotADirectory,
            format!("{shown} isn't a folder."),
        ),
        ErrorKind::IsADirectory => (FileErrorCode::IsADirectory, format!("{shown} is a folder.")),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => (
            FileErrorCode::NoSpace,
            format!("There isn't enough space for {shown}."),
        ),
        ErrorKind::InvalidFilename => (
            FileErrorCode::InvalidName,
            format!("{shown} isn't a name this computer allows."),
        ),
        ErrorKind::ReadOnlyFilesystem => (
            FileErrorCode::AccessDenied,
            format!("{shown} is on a read-only volume."),
        ),
        ErrorKind::DirectoryNotEmpty => (
            FileErrorCode::AlreadyExists,
            format!("{shown} is a folder that isn't empty."),
        ),
        _ => (FileErrorCode::Failed, format!("{shown}: {error}")),
    };
    FileError::new(code, message)
}

fn invalid_path(message: impl Into<String>) -> FileError {
    FileError::new(FileErrorCode::InvalidPath, message)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Condvar, Mutex};

    use meshrmm_protocol::RunAs;

    use super::*;

    /// Operations that answer at once, except `hung` ones, which wait
    /// until they are let go. Each says when it starts.
    #[derive(Default)]
    struct Script {
        started: Mutex<Vec<String>>,
        let_go: Mutex<bool>,
        changed: Condvar,
    }

    impl Script {
        fn files(self: &Arc<Self>, limits: Limits) -> Files {
            let script = Arc::clone(self);
            Files::with(
                limits,
                Arc::new(
                    move |_: ExecutionMode,
                          _: &listing::Cache,
                          request: FileRequest,
                          _: &Deadline| {
                        script
                            .started
                            .lock()
                            .unwrap()
                            .push(request.request_id.clone());
                        script.changed.notify_all();
                        if request.request_id.starts_with("hung") {
                            let mut let_go = script.let_go.lock().unwrap();
                            while !*let_go {
                                let_go = script.changed.wait(let_go).unwrap();
                            }
                        }
                        FileReply::Upload { received: 1 }
                    },
                ),
            )
        }

        fn started(&self) -> HashSet<String> {
            self.started.lock().unwrap().iter().cloned().collect()
        }

        /// Waits, off the runtime, until `count` operations have started.
        async fn until_started(self: &Arc<Self>, count: usize) {
            let script = Arc::clone(self);
            tokio::task::spawn_blocking(move || {
                let mut started = script.started.lock().unwrap();
                while started.len() < count {
                    let (now, waited) = script.changed.wait_timeout(started, BOUND).unwrap();
                    assert!(!waited.timed_out(), "only {} started", now.len());
                    started = now;
                }
            })
            .await
            .unwrap();
        }

        fn let_go(&self) {
            *self.let_go.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    fn request(id: &str, timeout: Duration) -> FileRequest {
        FileRequest {
            request_id: id.into(),
            run_as: RunAs::System,
            notification: None,
            timeout_ms: timeout.as_millis() as u64,
            operation: FileOperation::Roots,
        }
    }

    fn limits(concurrent: usize, strays: usize) -> Limits {
        Limits {
            concurrent,
            strays,
            margin: Duration::ZERO,
            grace: Duration::ZERO,
        }
    }

    const LONG: Duration = Duration::from_secs(60 * 60);
    /// Long enough for an operation to start, short enough for a test to
    /// wait out.
    const SHORT: Duration = Duration::from_millis(300);

    fn code(reply: &FileReply) -> Option<FileErrorCode> {
        match reply {
            FileReply::Failed { error } => Some(error.code),
            _ => None,
        }
    }

    /// Lets the hung operations go however a test ends, so a failing one
    /// fails instead of waiting for ever on their threads.
    struct LetGo(Arc<Script>);

    impl Drop for LetGo {
        fn drop(&mut self) {
            self.0.let_go();
        }
    }

    /// How long a test waits for what should happen at once, or after
    /// [`SHORT`], before it fails.
    const BOUND: Duration = Duration::from_secs(60);

    async fn answered(files: &mut Files) -> (String, FileReply) {
        tokio::time::timeout(BOUND, files.next())
            .await
            .expect("no answer came")
    }

    async fn until_strays(files: &Files, count: usize) {
        let failing = Instant::now() + BOUND;
        while files.strays() != count {
            assert!(Instant::now() < failing, "{} left behind", files.strays());
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn a_request_whose_time_runs_out_before_its_turn_is_never_started() {
        let script = Arc::new(Script::default());
        let _let_go = LetGo(Arc::clone(&script));
        let mut files = script.files(limits(1, 1));
        files.request(ExecutionMode::Console, request("hung", LONG));
        script.until_started(1).await;
        files.request(
            ExecutionMode::Console,
            request("late", Duration::from_millis(1)),
        );
        let (id, reply) = answered(&mut files).await;
        assert_eq!(id, "late");
        assert_eq!(code(&reply), Some(FileErrorCode::TimedOut), "{reply:?}");

        // Its turn comes once the first is done; it still isn't started.
        script.let_go();
        assert_eq!(answered(&mut files).await.0, "hung");
        files.request(ExecutionMode::Console, request("after", LONG));
        assert_eq!(answered(&mut files).await.0, "after");
        assert_eq!(
            script.started(),
            HashSet::from(["hung".into(), "after".into()])
        );
    }

    #[tokio::test]
    async fn operations_that_never_return_stop_holding_their_turns() {
        let script = Arc::new(Script::default());
        let _let_go = LetGo(Arc::clone(&script));
        let mut files = script.files(limits(4, 8));
        for index in 0..4 {
            files.request(
                ExecutionMode::Console,
                request(&format!("hung{index}"), SHORT),
            );
        }
        script.until_started(4).await;
        files.request(ExecutionMode::Console, request("fifth", LONG));
        let (id, reply) = answered(&mut files).await;
        assert_eq!(id, "fifth");
        assert_eq!(reply, FileReply::Upload { received: 1 });
        until_strays(&files, 4).await;

        // When they return at last, their answers go nowhere.
        script.let_go();
        until_strays(&files, 0).await;
        assert!(files.replies.try_recv().is_err());
    }

    #[tokio::test]
    async fn requests_are_busy_while_as_many_operations_as_allowed_are_left_behind() {
        let script = Arc::new(Script::default());
        let _let_go = LetGo(Arc::clone(&script));
        let mut files = script.files(limits(2, 1));
        // One is left behind; the other, beyond the bound, keeps its turn.
        files.request(ExecutionMode::Console, request("hung0", SHORT));
        files.request(ExecutionMode::Console, request("hung1", SHORT));
        script.until_started(2).await;
        until_strays(&files, 1).await;
        files.request(ExecutionMode::Console, request("refused", LONG));
        let (id, reply) = answered(&mut files).await;
        assert_eq!(id, "refused");
        assert_eq!(code(&reply), Some(FileErrorCode::Busy), "{reply:?}");
        assert!(!script.started().contains("refused"));
        assert_eq!(files.strays(), 1);

        script.let_go();
        until_strays(&files, 0).await;
        files.request(ExecutionMode::Console, request("again", LONG));
        // The one that kept its turn answers only if it returned in time.
        loop {
            match answered(&mut files).await.0.as_str() {
                "again" => break,
                "hung1" => {}
                other => panic!("{other} answered"),
            }
        }
    }

    #[test]
    fn operations_answer_a_little_before_the_server_gives_up() {
        let now = Instant::now();
        let margin = LIMITS.margin;
        let by = |wait| Deadline::answering(now, wait, margin).at - now;
        assert_eq!(by(Duration::from_secs(30)), Duration::from_secs(25));
        assert_eq!(by(Duration::from_secs(120)), Duration::from_secs(115));
        assert_eq!(by(Duration::from_secs(4)), Duration::from_secs(3));
        assert_eq!(by(Duration::ZERO), Duration::ZERO);
    }
}
