//! Private pipes between the Agent service and the coordinator it launches. The service closes
//! the coordinator's stdin to ask it to stop, so remote sessions end and run their close actions
//! before the process exits, and reads from its stdout whether the coordinator is busy, so
//! automatic updates wait for live remote sessions and for jobs that must not be cut off. Before
//! stopping it for an automatic update, the service first writes the release version to that
//! stdin, so the coordinator can tell the server why the Agent is going offline.
//!
//! The coordinator is busy while it holds an [`ActivityGuard`]: for a remote session, terminal
//! or file transfer while it is open, and for an update install or scan, an application run or
//! scan, a toolbox script or file, or a self-healing action from when it is accepted until its
//! result reaches the connection. A result waiting for a connection keeps it busy for at most
//! [`RESULT_HOLD`] after its job ends, since an Agent that is offline cannot deliver it.
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

/// Written when the coordinator becomes busy.
pub const BUSY: &str = "busy";
/// Written when nothing keeps the coordinator busy any longer.
pub const IDLE: &str = "idle";
/// Written by the service, followed by the release version, before it stops the coordinator to
/// install an automatic update.
pub const UPDATING_PREFIX: &str = "updating ";
/// How long a job's result keeps the Agent busy once the job has ended, if the connection has
/// not taken it by then. Results are kept only in memory, so an update that waited longer for an
/// Agent that is offline would not save them, only hold up the update.
pub const RESULT_HOLD: Duration = Duration::from_secs(10 * 60);
/// How often an attached coordinator looks for results whose hold ran out, to tell the service.
const HOLD_CHECK: Duration = Duration::from_secs(30);

pub struct ServiceLink {
    stop: Arc<watch::Sender<bool>>,
    activity: Arc<Activity>,
    update: Arc<Mutex<Option<String>>>,
}

impl ServiceLink {
    /// Follows the service's pipes for a coordinator it launched; otherwise the coordinator only
    /// stops on Ctrl+C and reports nothing.
    pub fn new(attached: bool) -> Self {
        let stop = Arc::new(watch::Sender::new(false));
        let update = Arc::new(Mutex::new(None));
        if !attached {
            return Self {
                stop,
                activity: Arc::new(Activity::new(None)),
                update,
            };
        }
        let sender = Arc::clone(&stop);
        // Helpers launched with inherited handles, some as the signed-in user, must not
        // receive the service's pipes. Unix children inherit stdio only as their own
        // stdio, which every helper launch replaces.
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;

            use windows::Win32::Foundation::{
                HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
            };

            for handle in [
                std::io::stdin().as_raw_handle(),
                std::io::stdout().as_raw_handle(),
            ] {
                if let Err(error) = unsafe {
                    SetHandleInformation(HANDLE(handle), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0))
                } {
                    tracing::warn!(%error, "could not keep the Agent service pipes from helpers");
                }
            }
        }
        let announced = Arc::clone(&update);
        let watcher = std::thread::Builder::new()
            .name("meshrmm-service-link".into())
            .spawn(move || {
                let mut input = std::io::stdin().lock();
                let mut line = Vec::new();
                loop {
                    line.clear();
                    match input.read_until(b'\n', &mut line) {
                        Ok(0) => {
                            tracing::info!(
                                "the Agent service is stopping; ending remote sessions and stopping the coordinator"
                            );
                            break;
                        }
                        Ok(_) => {
                            if let Some(version) = update_announcement(&line) {
                                tracing::info!(%version, "the Agent service is stopping to install an update");
                                *announced.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(version);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            tracing::warn!(%error, "lost the Agent service control pipe; stopping the coordinator");
                            break;
                        }
                    }
                }
                sender.send_replace(true);
            });
        if let Err(error) = watcher {
            tracing::warn!(%error, "could not follow Agent service stop requests");
        }
        let activity = Arc::new(Activity::new(Some(Box::new(std::io::stdout()))));
        // A result's hold runs out without anything happening, so the service hears of it
        // from here.
        let watched = Arc::clone(&activity);
        let expiry = std::thread::Builder::new()
            .name("meshrmm-activity".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(HOLD_CHECK);
                    watched.refresh(Instant::now());
                }
            });
        if let Err(error) = expiry {
            tracing::warn!(%error, "could not follow finished jobs; updates wait for their results until they are sent");
        }
        Self {
            stop,
            activity,
            update,
        }
    }

    /// Stops the coordinator so a staged update can replace it, as the
    /// Windows service does through the pipe.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn stop_for_update(&self, version: String) {
        *self.update.lock().unwrap_or_else(|e| e.into_inner()) = Some(version);
        self.stop.send_replace(true);
    }

    /// Whether something the Agent does must not be cut off by an update: a remote session, or
    /// a job that runs or has a result to deliver (see [`Self::activity`]).
    #[cfg_attr(windows, allow(dead_code))]
    pub fn busy(&self) -> bool {
        self.activity.lock().busy(Instant::now())
    }

    /// The release the service announced it is stopping the coordinator to install.
    pub fn update_version(&self) -> Option<String> {
        self.update
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Resolves once the service or Ctrl+C asks the coordinator to stop.
    pub async fn stopped(&self) {
        let mut stop = self.stop.subscribe();
        let service = async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        };
        // launchd stops the macOS coordinator with SIGTERM.
        let terminate = async {
            #[cfg(unix)]
            if let Ok(mut terminate) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                terminate.recv().await;
                return;
            }
            std::future::pending::<()>().await
        };
        tokio::select! {
            () = service => {}
            _ = tokio::signal::ctrl_c() => {}
            () = terminate => {}
        }
    }

    /// Keeps the Agent busy, and tells the service, until the returned guard is dropped: for
    /// the life of a remote session, terminal or transfer, or from when a job is accepted until
    /// its result reaches the connection, which [`ActivityGuard::finished`] bounds.
    pub fn activity(&self) -> ActivityGuard {
        let mut state = self.activity.lock();
        let id = state.next;
        state.next += 1;
        state.holds.insert(id, None);
        state.report(Instant::now());
        drop(state);
        ActivityGuard {
            activity: Arc::clone(&self.activity),
            id,
        }
    }
}

/// The version in an update announcement from the service. Anything that is not a plausible
/// release version is ignored rather than sent to the server.
fn update_announcement(line: &[u8]) -> Option<String> {
    let version = std::str::from_utf8(line)
        .ok()?
        .trim()
        .strip_prefix(UPDATING_PREFIX)?;
    meshrmm_protocol::is_release_version(version).then(|| version.to_owned())
}

/// Keeps the Agent busy until dropped; see [`ServiceLink::activity`].
pub struct ActivityGuard {
    activity: Arc<Activity>,
    id: u64,
}

impl ActivityGuard {
    /// The job ended and its result waits for the connection, which keeps the Agent busy for
    /// at most [`RESULT_HOLD`] more.
    pub fn finished(&self) {
        self.finished_at(Instant::now());
    }

    fn finished_at(&self, now: Instant) {
        let mut state = self.activity.lock();
        if let Some(until) = state.holds.get_mut(&self.id) {
            *until = Some(now + RESULT_HOLD);
        }
        state.report(now);
    }
}

impl std::fmt::Debug for ActivityGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActivityGuard")
            .field("id", &self.id)
            .finish()
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let mut state = self.activity.lock();
        state.holds.remove(&self.id);
        state.report(Instant::now());
    }
}

struct Activity(Mutex<ActivityState>);

struct ActivityState {
    next: u64,
    /// What keeps the Agent busy, by guard: until when, or `None` for as long as it is held.
    holds: HashMap<u64, Option<Instant>>,
    /// What the service was last told.
    reported_busy: bool,
    output: Option<Box<dyn Write + Send>>,
}

impl Activity {
    fn new(output: Option<Box<dyn Write + Send>>) -> Self {
        Self(Mutex::new(ActivityState {
            next: 0,
            holds: HashMap::new(),
            reported_busy: false,
            output,
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ActivityState> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Tells the service if a result's hold ran out by `now`.
    fn refresh(&self, now: Instant) {
        self.lock().report(now);
    }
}

impl ActivityState {
    fn busy(&self, now: Instant) -> bool {
        self.holds
            .values()
            .any(|until| until.is_none_or(|until| until > now))
    }

    /// Reports only changes between busy and idle, in the order they happen.
    fn report(&mut self, now: Instant) {
        let busy = self.busy(now);
        if busy == self.reported_busy {
            return;
        }
        self.reported_busy = busy;
        let Some(output) = &mut self.output else {
            return;
        };
        let report = if busy { BUSY } else { IDLE };
        // Once the service is gone nobody needs the report.
        let _ = writeln!(output, "{report}").and_then(|()| output.flush());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);

    impl Output {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn attached(output: &Output) -> ServiceLink {
        ServiceLink {
            stop: Arc::new(watch::Sender::new(false)),
            activity: Arc::new(Activity::new(Some(Box::new(output.clone())))),
            update: Arc::default(),
        }
    }

    #[test]
    fn reports_only_the_first_start_and_the_last_end() {
        let output = Output::default();
        let link = attached(&output);
        let first = link.activity();
        let replacement = link.activity();
        drop(first);
        drop(replacement);
        drop(link.activity());
        assert_eq!(output.text(), format!("{BUSY}\n{IDLE}\n{BUSY}\n{IDLE}\n"));
    }

    #[test]
    fn a_finished_jobs_result_keeps_the_agent_busy_until_it_is_sent_or_its_hold_runs_out() {
        let output = Output::default();
        let link = attached(&output);
        let now = Instant::now();
        let delivered = link.activity();
        delivered.finished_at(now);
        assert!(link.busy(), "a result waiting for the connection");
        drop(delivered);
        assert!(!link.busy(), "the connection took it");

        // One no connection takes holds the Agent only for a while.
        let offline = link.activity();
        offline.finished_at(now);
        let state = link.activity.lock();
        assert!(state.busy(now + RESULT_HOLD - Duration::from_millis(1)));
        assert!(!state.busy(now + RESULT_HOLD));
        drop(state);
        // A job still running is not bound by it.
        let running = link.activity();
        assert!(link.activity.lock().busy(now + RESULT_HOLD * 100));
        drop(running);
        // The service hears when the hold runs out.
        assert_eq!(output.text(), format!("{BUSY}\n{IDLE}\n{BUSY}\n"));
        link.activity.refresh(now + RESULT_HOLD);
        assert_eq!(output.text(), format!("{BUSY}\n{IDLE}\n{BUSY}\n{IDLE}\n"));
        drop(offline);
        assert_eq!(output.text(), format!("{BUSY}\n{IDLE}\n{BUSY}\n{IDLE}\n"));
    }

    #[test]
    fn reads_only_plausible_update_announcements() {
        assert_eq!(
            update_announcement(b"updating 0.3.1-rc.1+build.5\r\n").as_deref(),
            Some("0.3.1-rc.1+build.5")
        );
        assert_eq!(update_announcement(b"updating \n"), None);
        assert_eq!(update_announcement(b"updating 1.0 <script>\n"), None);
        assert_eq!(update_announcement(b"busy\n"), None);
        assert_eq!(update_announcement(&[0xff, 0xfe]), None);
    }

    #[test]
    fn an_unattached_coordinator_still_counts_what_keeps_it_busy() {
        let link = ServiceLink::new(false);
        assert!(!link.busy());
        let session = link.activity();
        assert!(link.busy());
        drop(session);
        assert!(!link.busy());
    }

    #[tokio::test]
    async fn stopping_for_an_update_ends_the_coordinator_with_the_version() {
        let link = ServiceLink::new(false);
        link.stop_for_update("1.2.3".into());
        tokio::time::timeout(std::time::Duration::from_secs(1), link.stopped())
            .await
            .unwrap();
        assert_eq!(link.update_version().as_deref(), Some("1.2.3"));
    }
}
