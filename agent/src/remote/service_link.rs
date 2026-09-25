//! Private pipes between the Agent service and the coordinator it launches. The service closes
//! the coordinator's stdin to ask it to stop, so remote sessions end and run their close actions
//! before the process exits, and reads session activity from its stdout so automatic updates
//! wait for live remote sessions.
use std::io::{Read, Write};
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Mutex};

use tokio::sync::watch;
use windows::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation};

/// Written when the first remote session starts.
pub const SESSION_ACTIVE: &str = "session-active";
/// Written when the last remote session ends.
pub const SESSION_IDLE: &str = "session-idle";

pub struct ServiceLink {
    stop: watch::Receiver<bool>,
    activity: Option<Arc<Activity>>,
}

impl ServiceLink {
    /// Follows the service's pipes for a coordinator it launched; otherwise the coordinator only
    /// stops on Ctrl+C and reports nothing.
    pub fn new(attached: bool) -> Self {
        let (sender, stop) = watch::channel(false);
        if !attached {
            return Self {
                stop,
                activity: None,
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
        let watcher = std::thread::Builder::new()
            .name("meshrmm-service-link".into())
            .spawn(move || {
                let mut input = std::io::stdin().lock();
                let mut buffer = [0_u8; 64];
                loop {
                    match input.read(&mut buffer) {
                        Ok(0) => {
                            tracing::info!(
                                "the Agent service is stopping; ending remote sessions and stopping the coordinator"
                            );
                            break;
                        }
                        Ok(_) => {}
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
        }
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
    fn an_unattached_coordinator_reports_nothing() {
        assert!(ServiceLink::new(false).session_started().is_none());
    }
}
