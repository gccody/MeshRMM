//! The toolbox in a remote session: the technician's scripts and library
//! files from the dashboard, which they run on or send to the remote
//! computer. See docs/toolbox.md.
//!
//! The viewer only talks to the server, with the session's client token. The
//! server hands each run or file to the Agent, which reports back to it, and
//! the viewer follows the result there. Nothing crosses the WebRTC session,
//! so the toolbox also works on the background desktop and while nobody is
//! signed in to the remote computer.

// Linux builds only the tests of the viewer's shared code.
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::Context;
use meshrmm_protocol::{
    FileDelivery, FileDeliveryStatus, RunAs, ScriptRun, ScriptRunStatus, StartFileDelivery,
    StartScriptRun, ToolboxListing,
};

/// A listing older than this is fetched again when the menu opens.
const LISTING_MAX_AGE: Duration = Duration::from_secs(30);
/// Runs are followed this long at most; the dashboard keeps the rest.
const RUN_FOLLOW_LIMIT: Duration = Duration::from_secs(2 * 60 * 60);
const DELIVERY_FOLLOW_LIMIT: Duration = Duration::from_secs(35 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct Client {
    server: String,
    session_id: String,
    token: String,
    runtime: tokio::runtime::Handle,
    http: reqwest::Client,
}

/// What the toolbox shows: its toolbar item and its menu.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    /// The session can reach the server's toolbox.
    pub available: bool,
    pub loading: bool,
    /// Why the listing could not be loaded.
    pub error: Option<String>,
    pub listing: Option<ToolboxListing>,
    /// The latest run's or file's progress or outcome.
    pub status: String,
    /// A run or file is in progress.
    pub busy: bool,
}

/// Something the window shows the technician.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A run finished; the window shows its output.
    RunFinished(ScriptRun),
    /// A run or file could not even start.
    Failed { title: String, message: String },
}

#[derive(Default)]
struct State {
    client: Option<Client>,
    listing: Option<ToolboxListing>,
    refreshed_at: Option<Instant>,
    loading: bool,
    error: Option<String>,
    /// The listing the open menu offers. Its commands name items by their
    /// place in it, so a refresh while the menu is open cannot change them.
    offered: Option<ToolboxListing>,
    status: String,
    in_flight: usize,
    events: VecDeque<Event>,
}

/// The session's toolbox. It outlives reconnects, which keep the session.
#[derive(Clone, Default)]
pub struct Toolbox {
    state: Arc<Mutex<State>>,
}

impl Toolbox {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Uses the session's credentials for the toolbox, and loads it the
    /// first time. Call from the session's runtime.
    pub fn connect(&self, server: &str, session_id: &str, token: &str) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let http = match crate::http::client_builder()
            .https_only(true)
            .timeout(REQUEST_TIMEOUT)
            .build()
        {
            Ok(http) => http,
            Err(error) => {
                tracing::warn!(%error, "could not create the toolbox client");
                return;
            }
        };
        let first = {
            let mut state = self.state();
            let first = state.client.is_none();
            state.client = Some(Client {
                server: server.to_owned(),
                session_id: session_id.to_owned(),
                token: token.to_owned(),
                runtime,
                http,
            });
            first
        };
        if first {
            self.refresh();
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let state = self.state();
        Snapshot {
            available: state.client.is_some(),
            loading: state.loading,
            error: state.error.clone(),
            listing: state.listing.clone(),
            status: state.status.clone(),
            busy: state.in_flight > 0,
        }
    }

    /// What the menu about to open offers. A stale listing is fetched again
    /// for next time.
    pub fn offer(&self) -> Snapshot {
        let stale = self
            .state()
            .refreshed_at
            .is_none_or(|at| at.elapsed() > LISTING_MAX_AGE);
        if stale {
            self.refresh();
        }
        let snapshot = self.snapshot();
        self.state().offered = snapshot.listing.clone();
        snapshot
    }

    /// Fetches the toolbox again, unless a fetch is running.
    pub fn refresh(&self) {
        let client = {
            let mut state = self.state();
            if state.loading {
                return;
            }
            let Some(client) = state.client.clone() else {
                return;
            };
            state.loading = true;
            client
        };
        let toolbox = self.clone();
        client.runtime.clone().spawn(async move {
            let result = fetch_listing(&client).await;
            {
                let mut state = toolbox.state();
                state.loading = false;
                match result {
                    Ok(listing) => {
                        state.listing = Some(listing);
                        state.refreshed_at = Some(Instant::now());
                        state.error = None;
                    }
                    Err(error) => {
                        tracing::warn!(error = ?error, "could not load the toolbox");
                        state.error = Some(user_message(&error));
                    }
                }
            }
            crate::platform::refresh_open_controls();
        });
    }

    pub fn take_event(&self) -> Option<Event> {
        self.state().events.pop_front()
    }

    fn start(&self) -> Option<Client> {
        let mut state = self.state();
        let client = state.client.clone()?;
        state.in_flight += 1;
        Some(client)
    }

    fn finish(&self, status: String, event: Option<Event>) {
        {
            let mut state = self.state();
            state.in_flight = state.in_flight.saturating_sub(1);
            state.status = status;
            state.events.extend(event);
        }
        // A static remote desktop sends no frames to refresh the window.
        crate::platform::refresh_open_controls();
    }

    /// Runs the offered script at `index` on the remote computer as `run_as`,
    /// and shows its output when it finishes.
    pub fn run_script(&self, index: usize, run_as: RunAs) {
        let script = self
            .state()
            .offered
            .as_ref()
            .and_then(|listing| listing.scripts.get(index).cloned());
        let Some(script) = script else {
            return;
        };
        let Some(client) = self.start() else {
            return;
        };
        self.state().status = format!("Running {}…", script.name);
        let toolbox = self.clone();
        client.runtime.clone().spawn(async move {
            let name = script.name.clone();
            let outcome = follow_run(&client, &script.id, run_as).await;
            match outcome {
                Ok(run) if run.status.finished() => {
                    let status = format!("{name}: {}", run_outcome(&run));
                    toolbox.finish(status, Some(Event::RunFinished(run)));
                }
                Ok(_) => toolbox.finish(
                    format!("{name} is still running. Its result will be in the dashboard's run history."),
                    None,
                ),
                Err(error) => {
                    tracing::warn!(error = ?error, script = %name, "could not run a toolbox script");
                    let message = user_message(&error);
                    toolbox.finish(
                        format!("Couldn't run {name}"),
                        Some(Event::Failed {
                            title: format!("Couldn't run {name}"),
                            message,
                        }),
                    );
                }
            }
        });
    }

    /// Sends the offered file at `index` to the remote computer: to the
    /// signed-in user's Documents, or to Public Documents from the background
    /// desktop.
    pub fn send_file(&self, index: usize, background: bool) {
        let file = self
            .state()
            .offered
            .as_ref()
            .and_then(|listing| listing.files.get(index).cloned());
        let Some(file) = file else {
            return;
        };
        let Some(client) = self.start() else {
            return;
        };
        self.state().status = format!("Sending {}…", file.name);
        let toolbox = self.clone();
        client.runtime.clone().spawn(async move {
            let name = file.name.clone();
            match follow_delivery(&client, &file.id, background).await {
                Ok(delivery) if delivery.status == FileDeliveryStatus::Delivered => {
                    let place = delivery.path.unwrap_or(name.clone());
                    toolbox.finish(format!("Saved {place}"), None);
                }
                Ok(delivery) => {
                    let message = delivery.error.unwrap_or_else(|| match delivery.status {
                        FileDeliveryStatus::Lost => {
                            "The remote computer didn't say whether it saved the file. It may have gone offline.".to_owned()
                        }
                        _ => "The remote computer couldn't save the file.".to_owned(),
                    });
                    toolbox.finish(
                        format!("Couldn't send {name}"),
                        Some(Event::Failed {
                            title: format!("Couldn't send {name}"),
                            message,
                        }),
                    );
                }
                Err(error) => {
                    tracing::warn!(error = ?error, file = %name, "could not send a toolbox file");
                    toolbox.finish(
                        format!("Couldn't send {name}"),
                        Some(Event::Failed {
                            title: format!("Couldn't send {name}"),
                            message: user_message(&error),
                        }),
                    );
                }
            }
        });
    }
}

/// An error as the technician reads it: the server's own message when it
/// sent one.
fn user_message(error: &anyhow::Error) -> String {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<crate::errors::ApiError>())
        .map(|api| api.message.clone())
        .unwrap_or_else(|| format!("{error:#}"))
}

fn url(client: &Client, path: &[&str]) -> anyhow::Result<url::Url> {
    let mut segments = vec!["v1", "remote", "sessions", client.session_id.as_str()];
    segments.extend_from_slice(path);
    meshrmm_signaling_client::endpoint_url(&client.server, &segments, &[], false)
}

async fn read<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> anyhow::Result<T> {
    if !response.status().is_success() {
        return Err(crate::errors::ApiError::from_response(response)
            .await
            .into());
    }
    response
        .json()
        .await
        .context("the server sent an invalid toolbox response")
}

async fn fetch_listing(client: &Client) -> anyhow::Result<ToolboxListing> {
    let response = client
        .http
        .get(url(client, &["toolbox"])?)
        .bearer_auth(&client.token)
        .send()
        .await
        .context("the toolbox could not be reached")?;
    read(response).await
}

/// How long to wait before asking again about something that started
/// `elapsed` ago: quickly at first, for short scripts.
fn poll_interval(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_secs(15) {
        Duration::from_secs(1)
    } else {
        Duration::from_secs(3)
    }
}

/// Starts a run and follows it until it finishes or [`RUN_FOLLOW_LIMIT`].
async fn follow_run(client: &Client, script_id: &str, run_as: RunAs) -> anyhow::Result<ScriptRun> {
    let response = client
        .http
        .post(url(client, &["script-runs"])?)
        .bearer_auth(&client.token)
        .json(&StartScriptRun {
            script_id: script_id.to_owned(),
            run_as,
        })
        .send()
        .await
        .context("the server could not be reached")?;
    let mut run: ScriptRun = read(response).await?;
    let started = Instant::now();
    while run.status == ScriptRunStatus::Pending && started.elapsed() < RUN_FOLLOW_LIMIT {
        tokio::time::sleep(poll_interval(started.elapsed())).await;
        let response = client
            .http
            .get(url(client, &["script-runs", &run.id])?)
            .bearer_auth(&client.token)
            .send()
            .await;
        match response {
            Ok(response) => match read::<ScriptRun>(response).await {
                Ok(next) => run = next,
                Err(error) if is_final(&error) => return Err(error),
                Err(error) => tracing::debug!(error = ?error, "toolbox run status unavailable"),
            },
            // Reconnecting sessions lose the network for a while; keep asking.
            Err(error) => tracing::debug!(%error, "toolbox run status unavailable"),
        }
    }
    Ok(run)
}

/// Sends a file and follows it until it is saved or fails.
async fn follow_delivery(
    client: &Client,
    file_id: &str,
    background: bool,
) -> anyhow::Result<FileDelivery> {
    let response = client
        .http
        .post(url(client, &["file-deliveries"])?)
        .bearer_auth(&client.token)
        .json(&StartFileDelivery {
            file_id: file_id.to_owned(),
            background,
        })
        .send()
        .await
        .context("the server could not be reached")?;
    let mut delivery: FileDelivery = read(response).await?;
    let started = Instant::now();
    while delivery.status == FileDeliveryStatus::Pending
        && started.elapsed() < DELIVERY_FOLLOW_LIMIT
    {
        tokio::time::sleep(poll_interval(started.elapsed())).await;
        let response = client
            .http
            .get(url(client, &["file-deliveries", &delivery.id])?)
            .bearer_auth(&client.token)
            .send()
            .await;
        match response {
            Ok(response) => match read::<FileDelivery>(response).await {
                Ok(next) => delivery = next,
                Err(error) if is_final(&error) => return Err(error),
                Err(error) => {
                    tracing::debug!(error = ?error, "toolbox delivery status unavailable")
                }
            },
            Err(error) => tracing::debug!(%error, "toolbox delivery status unavailable"),
        }
    }
    Ok(delivery)
}

/// The server refused for good, as when the session ended.
fn is_final(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<crate::errors::ApiError>())
        .any(|api| matches!(api.status, 400 | 401 | 403 | 404 | 410))
}

/// A run's outcome in a few words.
pub fn run_outcome(run: &ScriptRun) -> String {
    match (run.status, run.exit_code) {
        (ScriptRunStatus::Pending, _) => "running".into(),
        (ScriptRunStatus::Completed, Some(code)) => format!("exit code {code}"),
        (ScriptRunStatus::Completed, None) => "finished".into(),
        (ScriptRunStatus::Failed, _) => "couldn't run".into(),
        (ScriptRunStatus::TimedOut, _) => "timed out".into(),
        (ScriptRunStatus::Lost, _) => "no result".into(),
    }
}

/// The account a run used, noting when nobody was signed in to run it as.
fn ran_as(run: &ScriptRun) -> String {
    match &run.ran_as {
        Some(account) if run.run_as == RunAs::User && account.ends_with("\\SYSTEM") => {
            format!("{account} (nobody was signed in)")
        }
        Some(account) => account.clone(),
        None => run.run_as.label().to_owned(),
    }
}

/// The output window's title and text for a finished run. Lines end in
/// CRLF, which Windows edit controls need.
pub fn run_report(run: &ScriptRun) -> (String, String) {
    let mut lines = vec![
        format!("{}: {}", run.script_name, run_outcome(run)),
        format!("Ran as {}", ran_as(run)),
    ];
    if let Some(error) = &run.error {
        lines.push(error.clone());
    }
    lines.push(String::new());
    lines.push("Output".into());
    let stdout = run.stdout.trim_end_matches(['\r', '\n']);
    let stderr = run.stderr.trim_end_matches(['\r', '\n']);
    lines.push(if stdout.is_empty() {
        "(none)".into()
    } else {
        stdout.to_owned()
    });
    if !stderr.is_empty() {
        lines.push(String::new());
        lines.push("Errors".into());
        lines.push(stderr.to_owned());
    }
    if run.output_truncated {
        lines.push(String::new());
        lines.push("Output was cut at 512 KiB per stream.".into());
    }
    let text = lines.join("\n").replace("\r\n", "\n").replace('\n', "\r\n");
    (format!("{} — toolbox", run.script_name), text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::ScriptLanguage;

    fn run() -> ScriptRun {
        ScriptRun {
            id: "run".into(),
            device_id: "device".into(),
            script_id: "script".into(),
            script_name: "Disk report".into(),
            language: ScriptLanguage::Powershell,
            run_as: RunAs::User,
            status: ScriptRunStatus::Completed,
            ran_as: Some(r"PC\ada".into()),
            exit_code: Some(0),
            stdout: "C: 10 GB free\nD: 2 GB free\n".into(),
            stderr: String::new(),
            output_truncated: false,
            error: None,
            created_at_unix_ms: 1,
            completed_at_unix_ms: Some(2),
        }
    }

    #[test]
    fn reports_show_the_outcome_account_and_output() {
        let (title, text) = run_report(&run());
        assert_eq!(title, "Disk report — toolbox");
        assert_eq!(
            text,
            "Disk report: exit code 0\r\nRan as PC\\ada\r\n\r\nOutput\r\nC: 10 GB free\r\nD: 2 GB free"
        );
        let mut failed = run();
        failed.status = ScriptRunStatus::TimedOut;
        failed.exit_code = Some(1);
        failed.ran_as = Some(r"NT AUTHORITY\SYSTEM".into());
        failed.error = Some("The script was stopped after 60 seconds.".into());
        failed.stdout.clear();
        failed.stderr = "Access denied\r\n".into();
        failed.output_truncated = true;
        let (_, text) = run_report(&failed);
        assert_eq!(
            text,
            "Disk report: timed out\r\nRan as NT AUTHORITY\\SYSTEM (nobody was signed in)\r\nThe script was stopped after 60 seconds.\r\n\r\nOutput\r\n(none)\r\n\r\nErrors\r\nAccess denied\r\n\r\nOutput was cut at 512 KiB per stream."
        );
    }

    #[test]
    fn polling_slows_down_after_a_while() {
        assert_eq!(
            poll_interval(Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            poll_interval(Duration::from_secs(60)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn commands_need_an_offered_item_and_a_session() {
        let toolbox = Toolbox::default();
        assert!(!toolbox.snapshot().available);
        // Nothing was offered, so nothing runs, and nothing is left in flight.
        toolbox.run_script(0, RunAs::System);
        toolbox.send_file(0, false);
        let snapshot = toolbox.snapshot();
        assert!(!snapshot.busy);
        assert!(snapshot.status.is_empty());
        assert_eq!(toolbox.take_event(), None);
    }

    #[test]
    fn server_messages_are_shown_as_written() {
        let error = anyhow::Error::from(crate::errors::ApiError {
            status: 409,
            message: "The device is offline, so the script did not run.".into(),
        })
        .context("starting the run");
        assert_eq!(
            user_message(&error),
            "The device is offline, so the script did not run."
        );
        assert!(!is_final(&error));
        let ended = anyhow::Error::from(crate::errors::ApiError {
            status: 410,
            message: "the remote session has ended".into(),
        });
        assert!(is_final(&ended));
    }
}
