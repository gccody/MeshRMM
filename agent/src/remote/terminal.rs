//! Remote terminals: a shell on a pseudo-terminal that a signed-in user
//! drives from the website. See docs/terminal.md.
//!
//! The server asks for one over the control connection. The Agent connects a
//! WebSocket of its own for it, starts the shell as SYSTEM or root, or as the
//! signed-in user, and relays between the two ([`relay`]) until the shell
//! exits or the socket closes. Closing either way ends the shell and
//! everything it started: its session on macOS and Linux, its job object on
//! Windows. Terminal traffic never touches the control connection, so a
//! flood of output can't hold up metrics or scripts.
//!
//! A terminal for the signed-in user is refused when nobody is signed in;
//! unlike a script, it never falls back to SYSTEM or root.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use meshrmm_protocol::{DevicePlatform, TerminalEvent, TerminalRequest, valid_terminal_size};
use tokio::sync::{mpsc, oneshot};

use super::config::{Config, ExecutionMode};

mod notice;
mod relay;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use self::unix as platform;
#[cfg(windows)]
use self::windows as platform;

#[cfg(windows)]
pub(crate) use self::notice::run_notice_helper;
#[cfg(unix)]
pub(crate) use self::notice::show_here as show_notice_here;

/// Terminals past this many at once are refused.
const MAX_OPEN_TERMINALS: usize = 8;

static OPEN_TERMINALS: AtomicUsize = AtomicUsize::new(0);

/// A place among the open terminals, given back on drop.
struct Slot;

impl Slot {
    fn take() -> Option<Self> {
        OPEN_TERMINALS
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |open| {
                (open < MAX_OPEN_TERMINALS).then_some(open + 1)
            })
            .ok()
            .map(|_| Self)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        OPEN_TERMINALS.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A running shell. Dropping `process` ends it and everything it started.
pub(super) struct Shell {
    /// The account it runs as, such as `NT AUTHORITY\SYSTEM` or `alice`.
    pub account: String,
    /// The program, such as `/bin/zsh`.
    pub program: String,
    pub io: relay::Io,
    pub process: platform::Process,
}

/// The shell threads' ends of the pipes to the relay.
pub(super) struct ShellEnds {
    pub output: mpsc::Sender<Vec<u8>>,
    pub input: mpsc::Receiver<Vec<u8>>,
    pub exited: oneshot::Sender<Option<i32>>,
}

/// The pipes between a shell's threads and the relay.
pub(super) fn channels() -> (relay::Io, ShellEnds) {
    // A few chunks each way: a stalled reader stops the shell, not memory.
    let (output_tx, output) = mpsc::channel(4);
    let (input, input_rx) = mpsc::channel(8);
    let (exited_tx, exited) = oneshot::channel();
    (
        relay::Io {
            output,
            input,
            exited,
        },
        ShellEnds {
            output: output_tx,
            input: input_rx,
            exited: exited_tx,
        },
    )
}

/// Shows the company's connection notification to the computer's user, as
/// a terminal does when it opens; the file browser shows it too.
pub(in crate::remote) fn show_notice(mode: ExecutionMode, text: String) {
    notice::show(mode, text);
}

/// The shells a terminal can run on this computer, for its system
/// information.
pub(crate) fn available_shells() -> Vec<meshrmm_protocol::TerminalShell> {
    platform::available_shells()
}

/// Why `request` can't be opened, before anything starts.
fn problem(request: &TerminalRequest) -> Option<String> {
    if !valid_id(&request.terminal_id) {
        Some("The terminal has an invalid ID.".into())
    } else if !valid_terminal_size(request.columns, request.rows) {
        Some("The terminal's size is out of range.".into())
    } else if !request.shell.runs_on(DevicePlatform::current()) {
        Some(format!(
            "{} doesn't run on {}.",
            request.shell.label(),
            DevicePlatform::current().label()
        ))
    } else {
        None
    }
}

/// Terminal IDs go in a URL path, so they must be plain.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Opens `request`'s terminal on a task of its own, holding `activity`
/// while it is open.
pub(crate) fn open(
    config: &Config,
    mode: ExecutionMode,
    request: TerminalRequest,
    activity: impl Send + 'static,
) {
    let config = config.clone();
    tokio::spawn(async move {
        let _activity = activity;
        run(config, mode, request).await;
    });
}

async fn run(config: Config, mode: ExecutionMode, request: TerminalRequest) {
    let terminal_id = request.terminal_id.clone();
    let slot = Slot::take();
    let refusal = if slot.is_none() {
        Some(format!(
            "This computer already has {MAX_OPEN_TERMINALS} terminals open."
        ))
    } else {
        problem(&request)
    };
    let url = match meshrmm_signaling_client::endpoint_url(
        &config.server,
        &[
            "v1",
            "agents",
            &config.device_id,
            "terminals",
            &request.terminal_id,
        ],
        &[],
        true,
    ) {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(%terminal_id, error = %error, "could not build the terminal's address");
            return;
        }
    };
    let mut socket = match meshrmm_signaling_client::authenticated_websocket(
        url,
        &config.agent_token,
    )
    .await
    {
        Ok((socket, _)) => socket,
        Err(error) => {
            tracing::warn!(%terminal_id, error = %format!("{error:#}"), "could not connect the terminal's socket");
            return;
        }
    };
    if let Some(message) = refusal {
        tracing::info!(%terminal_id, message, "refused a terminal");
        fail(&mut socket, message).await;
        return;
    }
    let started = {
        let request = request.clone();
        tokio::task::spawn_blocking(move || platform::start(mode, &request)).await
    };
    let shell = match started {
        Ok(Ok(shell)) => shell,
        Ok(Err(message)) => {
            tracing::info!(%terminal_id, shell = request.shell.as_str(), run_as = request.run_as.as_str(), message, "could not open a terminal");
            fail(&mut socket, message).await;
            return;
        }
        Err(error) => {
            tracing::warn!(%terminal_id, error = %error, "the terminal's start task failed");
            fail(&mut socket, "The Agent could not start the shell.".into()).await;
            return;
        }
    };
    let Shell {
        account,
        program,
        io,
        process,
    } = shell;
    let opened = Instant::now();
    let event = TerminalEvent::Started {
        account: account.clone(),
        shell: program.clone(),
    };
    if relay::send_event(&mut socket, &event).await.is_err() {
        tracing::info!(%terminal_id, "the terminal's socket closed before the shell started");
        return;
    }
    tracing::info!(
        %terminal_id,
        shell = request.shell.as_str(),
        run_as = request.run_as.as_str(),
        account,
        program,
        "opened a terminal"
    );
    if let Some(text) = request.notification.clone() {
        notice::show(mode, text);
    }
    let ending = relay::relay(&mut socket, io, &process).await;
    // Ends the shell and what it started before anything else.
    drop(process);
    drop(slot);
    let duration_seconds = opened.elapsed().as_secs();
    match ending {
        relay::Ending::Exited(code) => {
            tracing::info!(%terminal_id, account, ?code, duration_seconds, "the terminal's shell exited")
        }
        relay::Ending::Closed => {
            tracing::info!(%terminal_id, account, duration_seconds, "closed a terminal")
        }
        relay::Ending::Failed(error) => tracing::warn!(
            %terminal_id,
            account,
            duration_seconds,
            error = %format!("{error:#}"),
            "a terminal ended with an error"
        ),
    }
}

/// Tells the browser why the terminal didn't open, and closes.
async fn fail(socket: &mut meshrmm_signaling_client::Socket, message: String) {
    let _ = relay::send_event(socket, &TerminalEvent::Failed { message }).await;
    let _ = tokio::time::timeout(relay::SEND_TIMEOUT, socket.close(None)).await;
}

#[cfg(test)]
mod tests {
    use meshrmm_protocol::{RunAs, TerminalShell};

    use super::*;

    fn request() -> TerminalRequest {
        TerminalRequest {
            terminal_id: "0195f3a2-7c1d-7e4b-9a3f-2b8c4d5e6f70".into(),
            shell: TerminalShell::defaults(DevicePlatform::current())[0],
            run_as: RunAs::System,
            columns: 80,
            rows: 24,
            notification: None,
        }
    }

    #[test]
    fn requests_are_checked_before_anything_starts() {
        assert_eq!(problem(&request()), None);
        for id in ["", "../x", "a/b", &"a".repeat(65)] {
            let mut bad = request();
            bad.terminal_id = id.into();
            assert!(problem(&bad).is_some(), "{id:?}");
        }
        let mut wide = request();
        wide.columns = 0;
        assert!(problem(&wide).is_some());
        let mut foreign = request();
        foreign.shell = if cfg!(windows) {
            TerminalShell::Zsh
        } else {
            TerminalShell::Cmd
        };
        assert!(problem(&foreign).unwrap().contains("doesn't run on"));
    }

    #[test]
    fn open_terminals_are_limited() {
        let slots = (0..MAX_OPEN_TERMINALS)
            .map_while(|_| Slot::take())
            .collect::<Vec<_>>();
        // Other tests may hold slots; this one fills the rest.
        assert!(Slot::take().is_none());
        drop(slots);
        assert!(Slot::take().is_some());
    }

    #[test]
    fn this_computer_offers_its_default_shells() {
        let shells = available_shells();
        for shell in TerminalShell::defaults(DevicePlatform::current()) {
            assert!(shells.contains(shell), "{shell:?} in {shells:?}");
        }
    }
}
