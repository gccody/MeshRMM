//! The viewer's windows and the loop that runs them. The loop has the main
//! thread; the session runs on another and reaches the windows through
//! [`post`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use meshrmm_protocol::{CursorShape, Display, DisplayId, VideoFormat};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowAttributes, WindowId};

use super::dialog::{Dialog, Kind};
use super::gpu::{Canvas, Gpu};
use super::presenter::Shared;
use super::session::{self, SessionWindow};
use crate::reconnect::ReconnectStatus;

/// What desktops know the viewer by. It matches the name of the launcher
/// the viewer installs, which gives its windows their icon.
pub(super) const APPLICATION_ID: &str = "meshrmm-remote";

/// How long the viewer waits for a session it was told to end before it
/// exits anyway. Ending a session retries for up to about 16 seconds.
const EXIT_TIMEOUT: Duration = Duration::from_secs(20);

/// The stream a presenter wants the session window to show.
pub(super) struct SessionRequest {
    pub(super) shared: Arc<Shared>,
    pub(super) format: VideoFormat,
    pub(super) display: Display,
    pub(super) displays: Vec<Display>,
}

/// What a presenter tells its window.
pub(super) enum SessionCommand {
    Reset {
        format: VideoFormat,
        display: Display,
        displays: Vec<Display>,
        reply: tokio::sync::oneshot::Sender<anyhow::Result<()>>,
    },
    AgentPointerDisplay(Option<DisplayId>),
    Displays(Vec<Display>),
    Refresh,
    ReconnectStatus(Option<ReconnectStatus>),
    CursorShape(CursorShape),
    /// The decoder has a picture newer than the one on screen.
    Picture,
    Close,
}

pub(super) enum UiEvent {
    /// Opens the session window, or moves it to a new connection's stream,
    /// and says whether that worked.
    OpenSession(Box<SessionRequest>, SyncSender<anyhow::Result<()>>),
    /// For the window of the presenter `id`; a window that has moved on to
    /// another presenter ignores it.
    Session {
        id: u64,
        command: SessionCommand,
    },
    RefreshSession,
    LaunchStatus {
        message: String,
        cancellable: bool,
    },
    Notice {
        title: String,
        message: String,
        closed: SyncSender<()>,
    },
    /// The session ended, with what went wrong if it failed.
    NetworkFinished(Option<String>),
}

static PROXY: OnceLock<EventLoopProxy<UiEvent>> = OnceLock::new();
/// Whether the session's thread is still running, so the loop goes on.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// Hands `event` to the window loop. `false` when there is none.
pub(super) fn post(event: UiEvent) -> bool {
    PROXY
        .get()
        .is_some_and(|proxy| proxy.send_event(event).is_ok())
}

/// A window titled `title` that desktops file under the viewer.
pub(super) fn window_attributes(title: &str) -> WindowAttributes {
    use winit::platform::wayland::WindowAttributesExtWayland;
    // Wayland's application ID and X11's window class are set as one.
    Window::default_attributes()
        .with_title(title)
        .with_name(APPLICATION_ID, APPLICATION_ID)
}

/// Shows what the launch is waiting on, in the connecting window.
pub fn show_launch_status(message: String, cancellable: bool) {
    post(UiEvent::LaunchStatus {
        message,
        cancellable,
    });
}

/// Shows a notice from the session's thread, such as where a recording was
/// saved, and waits until the technician dismisses it.
pub fn show_notice(title: &'static str, message: &str) {
    let (closed, dismissed) = std::sync::mpsc::sync_channel(1);
    if post(UiEvent::Notice {
        title: title.to_owned(),
        message: message.to_owned(),
        closed,
    }) {
        // An error means the window could not open, or the loop has ended.
        let _ = dismissed.recv();
    } else {
        println!("{title}: {message}");
    }
}

struct App {
    session: Option<SessionWindow>,
    connecting: Option<Dialog>,
    dialogs: Vec<Dialog>,
    /// The session is over; the loop ends once its last notice is read.
    finished: bool,
    /// When to exit although the session has not ended.
    exit_at: Option<Instant>,
    /// Last, so the windows go before the device they draw with.
    gpu: Option<Gpu>,
}

impl App {
    /// Opens a window and what draws in it. The first window decides which
    /// GPU the viewer uses.
    fn canvas(
        &mut self,
        event_loop: &ActiveEventLoop,
        attributes: WindowAttributes,
    ) -> anyhow::Result<Canvas> {
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .context("the desktop did not open a window for the viewer")?,
        );
        let surface = match &self.gpu {
            Some(gpu) => gpu.create_surface(Arc::clone(&window))?,
            None => {
                let (gpu, surface) =
                    Gpu::new(event_loop.owned_display_handle(), Arc::clone(&window))?;
                self.gpu = Some(gpu);
                surface
            }
        };
        let gpu = self.gpu.as_ref().context("the GPU device is missing")?;
        Canvas::new(gpu, window, surface)
    }

    fn dialog(&mut self, event_loop: &ActiveEventLoop, kind: Kind) -> anyhow::Result<Dialog> {
        let canvas = self.canvas(event_loop, kind.attributes())?;
        Ok(Dialog::new(canvas, kind))
    }

    fn open_session(
        &mut self,
        event_loop: &ActiveEventLoop,
        request: SessionRequest,
    ) -> anyhow::Result<()> {
        // The remote desktop replaces the connecting window.
        self.connecting = None;
        if let Some(session) = &mut self.session {
            session.adopt(request);
            return Ok(());
        }
        let canvas = self.canvas(
            event_loop,
            session::attributes(event_loop, request.format, &request.display),
        )?;
        let gpu = self.gpu.as_ref().context("the GPU device is missing")?;
        self.session = Some(SessionWindow::open(gpu, canvas, request));
        Ok(())
    }

    fn launch_status(&mut self, event_loop: &ActiveEventLoop, message: String, cancellable: bool) {
        // Reconnecting shows in the session window instead.
        if self.session.is_some() || self.finished {
            return;
        }
        if let Some(connecting) = &mut self.connecting {
            connecting.set_status(message, cancellable);
            return;
        }
        match self.dialog(
            event_loop,
            Kind::Connecting {
                message,
                cancellable,
            },
        ) {
            Ok(dialog) => self.connecting = Some(dialog),
            Err(error) => tracing::warn!(error = ?error, "could not show the connecting window"),
        }
    }

    /// Cancel, Esc, or the close box of the connecting window: end the
    /// launch without an error.
    fn cancel_connection(&mut self) {
        let Some(connecting) = &mut self.connecting else {
            return;
        };
        if !matches!(
            connecting.kind(),
            Kind::Connecting {
                cancellable: true,
                ..
            }
        ) {
            return;
        }
        connecting.set_status("Cancelling…".into(), false);
        crate::shutdown::request("the user cancelled the connection");
        self.exit_at = Some(Instant::now() + EXIT_TIMEOUT);
    }

    fn show(&mut self, event_loop: &ActiveEventLoop, kind: Kind) {
        match self.dialog(event_loop, kind) {
            Ok(dialog) => self.dialogs.push(dialog),
            // Dropping the notice tells whoever waits that it is gone.
            Err(error) => tracing::warn!(error = ?error, "could not show a viewer window"),
        }
    }

    fn network_finished(&mut self, event_loop: &ActiveEventLoop, error: Option<String>) {
        self.finished = true;
        self.session = None;
        self.connecting = None;
        // What is left to read is how the session ended.
        self.dialogs
            .retain(|dialog| matches!(dialog.kind(), Kind::Notice { .. }));
        if let Some(error) = error {
            tracing::error!(%error, "showing the viewer's connection error");
            self.show(
                event_loop,
                Kind::Notice {
                    title: "Remote connection failed".into(),
                    message: error,
                    closed: None,
                },
            );
        }
        if self.dialogs.is_empty() {
            event_loop.exit();
        }
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::OpenSession(request, reply) => {
                let _ = reply.send(self.open_session(event_loop, *request));
            }
            UiEvent::Session { id, command } => {
                let Some(gpu) = &self.gpu else {
                    return;
                };
                if let Some(session) = &mut self.session
                    && session.presenter() == id
                    && session.command(gpu, command)
                {
                    self.session = None;
                }
            }
            UiEvent::RefreshSession => {
                if let Some(session) = &mut self.session {
                    session.invalidate();
                }
            }
            UiEvent::LaunchStatus {
                message,
                cancellable,
            } => self.launch_status(event_loop, message, cancellable),
            UiEvent::Notice {
                title,
                message,
                closed,
            } => {
                self.connecting = None;
                self.show(
                    event_loop,
                    Kind::Notice {
                        title,
                        message,
                        closed: Some(closed),
                    },
                );
            }
            UiEvent::NetworkFinished(error) => self.network_finished(event_loop, error),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(gpu) = &self.gpu else {
            return;
        };
        if let Some(session) = &mut self.session
            && session.window_id() == id
        {
            session.window_event(gpu, event);
            let outputs = session.take_outputs();
            if session.closed() {
                self.session = None;
            }
            for (title, text) in outputs {
                self.show(
                    event_loop,
                    Kind::Output {
                        title,
                        text: text.replace("\r\n", "\n"),
                    },
                );
            }
        } else if let Some(connecting) = &mut self.connecting
            && connecting.window_id() == id
        {
            if connecting.window_event(gpu, event) {
                self.cancel_connection();
            }
        } else if let Some(index) = self
            .dialogs
            .iter()
            .position(|dialog| dialog.window_id() == id)
            && self.dialogs[index].window_event(gpu, event)
        {
            self.dialogs.remove(index);
            if self.finished && self.dialogs.is_empty() {
                event_loop.exit();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = Instant::now();
        if let Some(exit_at) = self.exit_at
            && now >= exit_at
            && RUNNING.load(Ordering::Acquire)
        {
            tracing::warn!("the session did not end in time; exiting without its cleanup");
            std::process::exit(0);
        }
        let mut wake = self.exit_at;
        let mut sooner = |at: Instant| wake = Some(wake.map_or(at, |wake| wake.min(at)));
        if let Some(session) = &mut self.session {
            sooner(session.poll(now));
        }
        if let Some(connecting) = &mut self.connecting {
            sooner(connecting.poll(now));
        }
        for dialog in &mut self.dialogs {
            sooner(dialog.poll(now));
        }
        event_loop.set_control_flow(match wake {
            Some(wake) => ControlFlow::WaitUntil(wake),
            None => ControlFlow::Wait,
        });
    }
}

/// Runs the viewer's windows on this thread, the main one, and `network`,
/// the session, on another, until the session has ended and what it had to
/// say has been read.
pub fn run_application<F>(network: F) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()> + Send + 'static,
{
    let event_loop = EventLoop::<UiEvent>::with_user_event().build().context(
        "MeshRMM Remote could not reach the desktop; it needs an X11 or Wayland session",
    )?;
    PROXY
        .set(event_loop.create_proxy())
        .map_err(|_| anyhow::anyhow!("the viewer's window loop was started twice"))?;
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    RUNNING.store(true, Ordering::Release);
    std::thread::Builder::new()
        .name("meshrmm-network".into())
        .spawn(move || {
            let result = network();
            match &result {
                Ok(()) => tracing::info!("the viewer's network session finished cleanly"),
                Err(error) => {
                    tracing::error!(error = ?error, "the viewer's network session failed")
                }
            }
            let error = result.as_ref().err().map(crate::errors::user_message);
            let _ = result_tx.send(result);
            RUNNING.store(false, Ordering::Release);
            post(UiEvent::NetworkFinished(error));
        })
        .context("failed to start the viewer's network thread")?;
    let mut app = App {
        session: None,
        connecting: None,
        dialogs: Vec::new(),
        finished: false,
        exit_at: None,
        gpu: None,
    };
    event_loop
        .run_app(&mut app)
        .context("the viewer's window loop failed")?;
    drop(app);
    result_rx
        .recv()
        .context("the viewer's network thread ended without a result")?
}
