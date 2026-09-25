//! Private pipes between the Agent service and the coordinator it launches. The service closes
//! the coordinator's stdin to ask it to stop, so remote sessions end and run their close actions
//! before the process exits, and reads session activity from its stdout so automatic updates
//! wait for live remote sessions. Before stopping it for an automatic update, the service first
//! writes the release version to that stdin, so the coordinator can tell the server why the
//! Agent is going offline.
use std::io::{BufRead, Write};
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use windows::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};

/// Written when the first remote session starts.
pub const SESSION_ACTIVE: &str = "session-active";
/// Written when the last remote session ends.
pub const SESSION_IDLE: &str = "session-idle";
/// Written by the service, followed by the release version, before it stops the coordinator to
/// install an automatic update.
pub const UPDATING_PREFIX: &str = "updating ";

pub struct ServiceLink {
    stop: watch::Receiver<bool>,
    activity: Option<Arc<Activity>>,
    update: Arc<Mutex<Option<String>>>,
}

impl ServiceLink {
    /// Follows the service's pipes for a coordinator it launched; otherwise the coordinator only
    /// stops on Ctrl+C and reports nothing.
    pub fn new(attached: bool) -> Self {
        let (sender, stop) = watch::channel(false);
        let update = Arc::new(Mutex::new(None));
        if !attached {
            return Self {
                stop,
                activity: None,
                update,
            };
        }
        // Helpers launched with inherited handles, some as the signed-in user, must not
        // receive the service's pipes.
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
                let _ = sender.send(true);
            });
        if let Err(error) = watcher {
            tracing::warn!(%error, "could not follow Agent service stop requests");
        }
        Self {
            stop,
            activity: Some(Arc::new(Activity::new(Box::new(std::io::stdout())))),
            update,
        }
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
        let mut stop = self.stop.clone();
        let service = async move {
            // Without a watcher thread the service can only stop the coordinator by terminating it.
            if stop.wait_for(|stopped| *stopped).await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        tokio::select! {
            () = service => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }

    /// Reports a live remote session to the service until the returned guard is dropped.
    pub fn session_started(&self) -> Option<SessionGuard> {
        self.activity.as_ref().map(|activity| {
            activity.change(true);
            SessionGuard(Arc::clone(activity))
        })
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

pub struct SessionGuard(Arc<Activity>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.change(false);
    }
}

struct Activity(Mutex<ActivityState>);

struct ActivityState {
    sessions: usize,
    output: Box<dyn Write + Send>,
}

impl Activity {
    fn new(output: Box<dyn Write + Send>) -> Self {
        Self(Mutex::new(ActivityState {
            sessions: 0,
            output,
        }))
    }

    /// Reports only transitions between no sessions and some, in the order they happen.
    fn change(&self, started: bool) {
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let was_active = state.sessions > 0;
        state.sessions = if started {
            state.sessions + 1
        } else {
            state.sessions.saturating_sub(1)
        };
        let active = state.sessions > 0;
        if active == was_active {
            return;
        }
        let report = if active { SESSION_ACTIVE } else { SESSION_IDLE };
        // Once the service is gone nobody needs the report.
        let _ = writeln!(state.output, "{report}").and_then(|()| state.output.flush());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Output(Arc<Mutex<Vec<u8>>>);

    impl Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reports_only_the_first_start_and_the_last_end() {
        let output = Output::default();
        let activity = Arc::new(Activity::new(Box::new(output.clone())));
        let link = ServiceLink {
            stop: watch::channel(false).1,
            activity: Some(activity),
            update: Arc::default(),
        };
        let first = link.session_started();
        let replacement = link.session_started();
        drop(first);
        drop(replacement);
        drop(link.session_started());
        assert_eq!(
            String::from_utf8(output.0.lock().unwrap().clone()).unwrap(),
            format!("{SESSION_ACTIVE}\n{SESSION_IDLE}\n{SESSION_ACTIVE}\n{SESSION_IDLE}\n")
        );
    }

    #[test]
    fn reads_only_plausible_update_announcements() {
        assert_eq!(
            update_announcement(b"updating 0.3.1-rc.1+build.5\r\n").as_deref(),
            Some("0.3.1-rc.1+build.5")
        );
        assert_eq!(update_announcement(b"updating \n"), None);
        assert_eq!(update_announcement(b"updating 1.0 <script>\n"), None);
        assert_eq!(update_announcement(b"session-active\n"), None);
        assert_eq!(update_announcement(&[0xff, 0xfe]), None);
    }

    #[test]
    fn an_unattached_coordinator_reports_nothing() {
        assert!(ServiceLink::new(false).session_started().is_none());
    }
}
