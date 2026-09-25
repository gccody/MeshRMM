use meshrmm_protocol::ClipboardContent;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use meshrmm_protocol::{
    CursorShape, Display, DisplayId, MAX_CLIPBOARD_WIRE_BYTES, RemoteInput, SessionMessage,
};
use windows::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_TIMEOUT};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::*;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use meshrmm_remote_screen::{
    ActiveFormat, EncodedAccessUnit, EncodedFrameSink, StreamConfig, VideoCodec, VideoPixelFormat,
    WindowsDesktopDuplicationStreamer,
};

use super::input::WindowsInputController;
use super::platform::ScreenInput;
use crate::win32::{OwnedHandle, wide};

mod child;
#[cfg(test)]
mod tests;
mod wire;

pub use child::run_child;
use child::*;
use wire::*;

const COMMAND_START: u8 = 1;
const COMMAND_REQUEST_KEYFRAME: u8 = 2;
const COMMAND_SET_BITRATE: u8 = 3;
const COMMAND_STOP: u8 = 4;
const COMMAND_INPUT: u8 = 5;
const COMMAND_RELEASE_INPUT: u8 = 6;
const COMMAND_START_INPUT: u8 = 7;
const COMMAND_CLIPBOARD: u8 = 8;
const COMMAND_CHAT: u8 = 9;
const COMMAND_START_CHAT: u8 = 10;
const COMMAND_STOP_CHAT: u8 = 11;
const COMMAND_BLOCK_INPUT: u8 = 14;
const COMMAND_BLACKOUT: u8 = 15;
const COMMAND_ENUMERATE_DISPLAYS: u8 = 22;
const EVENT_STARTED: u8 = 1;
const EVENT_FRAME: u8 = 2;
const EVENT_ERROR: u8 = 3;
const EVENT_STOPPED: u8 = 4;
const EVENT_CURSOR: u8 = 5;
const EVENT_INPUT_STARTED: u8 = 6;
const EVENT_CLIPBOARD: u8 = 7;
const EVENT_CHAT: u8 = 8;
const MAX_CODEC_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_CONTROL_BYTES: usize = 64 * 1024;
const MAX_DISPLAY_NAME_BYTES: usize = 4 * 1024;
const MAX_DISPLAYS: usize = 64;
const START_TIMEOUT: Duration = Duration::from_secs(5);
const CLIPBOARD_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STOP_TIMEOUT_MS: u32 = 5_000;
const MAX_STDERR_LINE_BYTES: usize = 4 * 1024;
const STDERR_LINES_PER_WINDOW: u32 = 120;
const STDERR_WINDOW: Duration = Duration::from_secs(60);
const NO_DISPLAY: u32 = u32::MAX;
const NO_ACTIVE_SESSION: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopTarget {
    Default,
    Winlogon,
    Background,
    Rdp(u32, bool),
}

impl DesktopTarget {
    fn name(self) -> &'static str {
        match self {
            Self::Default | Self::Rdp(_, false) => "default",
            Self::Winlogon | Self::Rdp(_, true) => "Winlogon",
            Self::Background => "MeshRMMBackground",
        }
    }

    fn alternate(self) -> Self {
        match self {
            Self::Default => Self::Winlogon,
            Self::Winlogon => Self::Default,
            Self::Background => Self::Background,
            Self::Rdp(id, secure) => Self::Rdp(id, !secure),
        }
    }
}

enum ParentCommand {
    PromptCredentials,
    AutofillCredentials(Vec<u8>),
    EnumerateDisplays,
    StartFiles,
    StartClipboard,
    StartChatHelper {
        viewer_name: String,
    },
    Files(meshrmm_protocol::FileMessage),
    Start {
        viewer_name: String,
        display_id: Option<DisplayId>,
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
        codec: VideoCodec,
        pixel_format: VideoPixelFormat,
        capture_cursor: bool,
        grayscale: bool,
    },
    RequestKeyframe,
    SetBitrate(u32),
    SetCursorCapture(bool),
    SetDisplayBorder(bool),
    SetWallpaperHidden(bool),
    SetPreventIdleLock(bool),
    StartInput {
        viewer_name: String,
        display_id: DisplayId,
    },
    Input(RemoteInput),
    ReleaseInput,
    BlockInput(bool),
    Blackout {
        enabled: bool,
        text: String,
    },
    Clipboard(ClipboardContent),
    Chat(String),
    StartChat,
    StopChat,
    Stop,
}

pub struct StartedDesktop {
    pub format: ActiveFormat,
    pub displays: Vec<Display>,
    pub active_display: Display,
}

enum ChildEvent {
    Credentials(CredentialResult),
    CredentialPrompt(bool),
    Files(meshrmm_protocol::FileMessage),
    Started(StartedDesktop),
    InputStarted,
    MaintenanceState {
        agent_input_blocked: bool,
        blacked_out: bool,
    },
    MaintenanceError(String),
    Frame(EncodedAccessUnit),
    Cursor(CursorShape, bool, Option<DisplayId>),
    Clipboard(ClipboardContent),
    Chat(String),
    Error(String),
    Stopped,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CredentialResult {
    encrypted: Option<Vec<u8>>,
    message: String,
}
#[derive(Default)]
struct Credentials {
    /// DPAPI ciphertext file; persists until ForgetCredentials.
    store: PathBuf,
    state: meshrmm_protocol::CredentialState,
}
impl Credentials {
    fn new(store: PathBuf) -> Self {
        Self {
            state: meshrmm_protocol::CredentialState {
                saved: super::credentials::saved(&store),
                ..Default::default()
            },
            store,
        }
    }
}
type HelperCredentials = Arc<Mutex<Credentials>>;

type HelperStatus = Arc<Mutex<Option<Result<(), String>>>>;
type HelperCursor = Arc<Mutex<(CursorShape, bool, Option<DisplayId>)>>;
#[derive(Default)]
struct FileEvents {
    queue: Mutex<std::collections::VecDeque<meshrmm_protocol::FileMessage>>,
    ready: Arc<tokio::sync::Notify>,
}
type HelperFiles = Arc<FileEvents>;
type HelperMaintenance = Arc<Mutex<Option<SessionMessage>>>;
#[derive(Default)]
struct ChatEvents {
    queue: Mutex<std::collections::VecDeque<String>>,
    ready: Arc<tokio::sync::Notify>,
}
type HelperChat = Arc<ChatEvents>;
#[derive(Default)]
struct ClipboardEvents {
    latest: Mutex<Option<ClipboardContent>>,
    ready: Arc<tokio::sync::Notify>,
}
type HelperClipboard = Arc<ClipboardEvents>;
type InputWriter = Arc<CommandWriter>;
type InputRoute = Arc<Mutex<Option<InputWriter>>>;

/// Brokers a credential-free LocalSystem helper on the visible Windows
/// desktop. Only frames and remote-control events cross the inherited pipes;
/// the Agent token, configuration, and network stack remain in Session 0.
pub struct DesktopCaptureStreamer {
    background_active: Arc<AtomicBool>,
    console_displays: Vec<Display>,
    session_displays: Vec<(Display, DisplayId)>,
    selected_session: Option<u32>,
    known_sessions: Vec<(u32, String)>,
    sessions_checked: Instant,
    display_routes: Arc<Mutex<Vec<(DisplayId, DisplayId)>>>,
    blackout_message: String,
    viewer_name: String,
    running: Option<RunningHelper>,
    input: Option<RunningInputHelper>,
    file_helper: Option<RunningInputHelper>,
    clipboard_helper: Option<RunningInputHelper>,
    chat_helper: Option<RunningInputHelper>,
    chat_route: InputRoute,
    clipboard_route: InputRoute,
    file_route: InputRoute,
    input_route: InputRoute,
    preferred_desktop: Option<DesktopTarget>,
    cursor: HelperCursor,
    clipboard: HelperClipboard,
    files: HelperFiles,
    chat: HelperChat,
    maintenance: HelperMaintenance,
    credentials: HelperCredentials,
    wallpaper_hidden: Arc<AtomicBool>,
    prevent_idle_lock: Arc<AtomicBool>,
    chat_enabled: Arc<AtomicBool>,
}

impl DesktopCaptureStreamer {
    pub fn new(viewer_name: String, blackout_message: String, credential_store: PathBuf) -> Self {
        Self {
            background_active: Arc::new(AtomicBool::new(false)),
            console_displays: Vec::new(),
            session_displays: Vec::new(),
            selected_session: None,
            known_sessions: Vec::new(),
            sessions_checked: Instant::now(),
            display_routes: Arc::new(Mutex::new(Vec::new())),
            viewer_name,
            blackout_message,
            running: None,
            input: None,
            file_helper: None,
            clipboard_helper: None,
            chat_helper: None,
            chat_route: Arc::new(Mutex::new(None)),
            clipboard_route: Arc::new(Mutex::new(None)),
            file_route: Arc::new(Mutex::new(None)),
            input_route: Arc::new(Mutex::new(None)),
            preferred_desktop: None,
            cursor: Arc::new(Mutex::new((CursorShape::Default, false, None))),
            clipboard: Arc::new(ClipboardEvents::default()),
            files: Arc::new(FileEvents::default()),
            chat: Arc::new(ChatEvents::default()),
            maintenance: Arc::new(Mutex::new(None)),
            credentials: Arc::new(Mutex::new(Credentials::new(credential_store))),
            wallpaper_hidden: Arc::new(AtomicBool::new(false)),
            prevent_idle_lock: Arc::new(AtomicBool::new(false)),
            chat_enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start(
        &mut self,
        config: StreamConfig,
        display_id: Option<DisplayId>,
        sink: EncodedFrameSink,
    ) -> anyhow::Result<StartedDesktop> {
        self.refresh_sessions();
        let selected =
            display_id.and_then(|id| self.session_displays.iter().find(|(d, _)| d.id == id));
        anyhow::ensure!(
            display_id
                .is_none_or(|id| id.0 < 0x8000_0000 || id.0 >= u32::MAX - 2 || selected.is_some()),
            "selected RDP display is no longer available"
        );
        let session = selected.and_then(|(d, _)| match d.session {
            meshrmm_protocol::DesktopSession::Rdp { id, .. } => Some(id),
            _ => None,
        });
        let display_id = selected.map(|(_, local)| *local).or(display_id);
        if session != self.selected_session {
            self.shutdown()?;
            self.selected_session = session;
            self.preferred_desktop = None;
        }
        let background =
            display_id.is_some_and(|id| id.0 == meshrmm_remote_screen::background::DISPLAY_ID);
        let config = if background {
            StreamConfig {
                frames_per_second: config.frames_per_second.min(20),
                ..config
            }
        } else {
            config
        };
        if background != self.background_active.load(Ordering::Acquire) {
            self.shutdown()?;
            self.background_active.store(background, Ordering::Release);
            self.preferred_desktop = None;
        }
        if background {
            meshrmm_remote_screen::background::require_session_zero()?;
            if self.console_displays.is_empty() {
                match enumerate_console_displays() {
                    Ok(displays) => self.console_displays = displays,
                    Err(error) => {
                        tracing::warn!(%error, "could not list console monitors before background launch")
                    }
                }
            }
        }
        if self.running.is_some() {
            let result = self.reconfigure(config, display_id, Arc::clone(&sink));
            if result.is_ok() {
                return result;
            }
            tracing::warn!(error = ?result.err(), "could not reuse capture helper; starting a replacement");
            let _ = self.stop();
        }
        let preferred = if background {
            DesktopTarget::Background
        } else {
            self.preferred_desktop.unwrap_or_else(|| {
                self.selected_session
                    .map_or_else(preferred_desktop, |id| DesktopTarget::Rdp(id, false))
            })
        };
        let mut last_error = None;
        for target in [preferred, preferred.alternate()]
            .into_iter()
            .take(if background { 1 } else { 2 })
        {
            let attempt_started = Instant::now();
            match self.start_on_desktop(target, config, display_id, Arc::clone(&sink)) {
                Ok(started) => {
                    tracing::info!(
                        desktop = target.name(),
                        startup_ms = attempt_started.elapsed().as_millis(),
                        "desktop helper capture became ready"
                    );
                    return Ok(started);
                }
                Err(error) => {
                    tracing::warn!(
                        desktop = target.name(),
                        startup_ms = attempt_started.elapsed().as_millis(),
                        error = ?error,
                        "desktop helper could not start"
                    );
                    last_error = Some(error);
                }
            }
        }
        if display_id.is_none()
            && !background
            && self.selected_session.is_none()
            && let Some((display, _)) = self
                .session_displays
                .iter()
                .find(|(d, _)| d.primary)
                .or(self.session_displays.first())
        {
            return self.start(config, Some(display.id), sink);
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no interactive desktop is available")))
    }

    fn reconfigure(
        &mut self,
        config: StreamConfig,
        display_id: Option<DisplayId>,
        sink: EncodedFrameSink,
    ) -> anyhow::Result<StartedDesktop> {
        let running = self
            .running
            .as_ref()
            .context("capture helper is not running")?;
        // Drop old-stream output before sending the command. The Started event
        // is a pipe-order barrier: all old encoder output precedes its reply.
        *running
            .sink
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        send_command(
            &running.input,
            &ParentCommand::Start {
                viewer_name: self.viewer_name.clone(),
                display_id,
                frames_per_second: config.frames_per_second,
                bitrate_bits_per_second: config.bitrate_bits_per_second,
                codec: config.codec,
                pixel_format: config.pixel_format,
                capture_cursor: config.capture_cursor,
                grayscale: config.grayscale,
            },
        )?;
        let started = running
            .started
            .recv_timeout(START_TIMEOUT)
            .context("capture helper did not reconfigure promptly")?
            .map_err(anyhow::Error::msg)?;
        let target = running.target;
        self.ensure_input_helper(target, started.active_display.id)?;
        let running = self
            .running
            .as_ref()
            .context("capture helper disappeared")?;
        *running
            .sink
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(sink);
        self.request_keyframe()?;
        Ok(self.with_background_display(started))
    }

    fn refresh_sessions(&mut self) {
        // Keep IDs stable for the life of this remote connection, including reconnects
        // of a previously discovered RDP session. Only advertise currently active users.
        let sessions = match active_rdp_sessions() {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::warn!(%error, "could not enumerate RDP sessions");
                return;
            }
        };
        self.known_sessions = sessions.clone();
        self.sessions_checked = Instant::now();
        self.session_displays.retain(|(d, _)| matches!(&d.session,
            meshrmm_protocol::DesktopSession::Rdp { id, .. } if sessions.iter().any(|(candidate, _)| candidate == id)));
        for (id, user) in sessions {
            match enumerate_desktop_displays(DesktopTarget::Rdp(id, false)) {
                Ok(displays) => {
                    self.session_displays.retain(|(d, _)| !matches!(d.session, meshrmm_protocol::DesktopSession::Rdp { id: candidate, .. } if candidate == id));
                    for mut display in displays {
                        // WTS session IDs and monitor counts are bounded before encoding.
                        if id >= 0x7fff || (display.id.0 >= 255 && display.id.0 != u32::MAX - 1) {
                            continue;
                        }
                        let local = display.id;
                        display.id = rdp_display_id(id, local);
                        display.session = meshrmm_protocol::DesktopSession::Rdp {
                            id,
                            user: user.clone(),
                        };
                        self.session_displays.push((display, local));
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, session_id = id, "could not enumerate RDP monitors")
                }
            }
        }
    }

    fn with_background_display(&mut self, mut started: StartedDesktop) -> StartedDesktop {
        let mut routes = Vec::new();
        if let Some(session_id) = self.selected_session {
            // Use the topology returned by the capture helper for the selected session.
            let session = self
                .session_displays
                .iter()
                .find_map(|(d, _)| match &d.session {
                    meshrmm_protocol::DesktopSession::Rdp { id, .. } if *id == session_id => {
                        Some(d.session.clone())
                    }
                    _ => None,
                });
            if let Some(session) = session {
                self.session_displays.retain(|(d, _)| d.session != session);
                for mut display in started.displays.iter().cloned() {
                    let local = display.id;
                    display.id = rdp_display_id(session_id, local);
                    display.session = session.clone();
                    routes.push((display.id, local));
                    if local == started.active_display.id {
                        started.active_display = display.clone();
                    }
                    self.session_displays.push((display, local));
                }
            }
        } else if !self.background_active.load(Ordering::Acquire) {
            self.console_displays = started.displays.clone();
        }
        if self.console_displays.is_empty() {
            self.console_displays = enumerate_console_displays().unwrap_or_default();
        }
        *self.display_routes.lock().unwrap() = routes;
        started.displays = self.console_displays.clone();
        started.displays.push(background_display());
        started
            .displays
            .extend(self.session_displays.iter().map(|(d, _)| d.clone()));
        started
    }

    fn start_on_desktop(
        &mut self,
        target: DesktopTarget,
        config: StreamConfig,
        display_id: Option<DisplayId>,
        sink: EncodedFrameSink,
    ) -> anyhow::Result<StartedDesktop> {
        let launched = launch_system_helper(target)?;
        let status: HelperStatus = Arc::new(Mutex::new(None));
        let (started_tx, started_rx) = mpsc::channel();
        let sink = Arc::new(Mutex::new(Some(sink)));
        let reader_sink = Arc::clone(&sink);
        let reader_status = Arc::clone(&status);
        let reader_cursor = Arc::clone(&self.cursor);
        let reader_maintenance = Arc::clone(&self.maintenance);
        let last_frame = Arc::new(Mutex::new(Instant::now()));
        let reader_progress = Arc::clone(&last_frame);
        let reader = thread::Builder::new()
            .name("meshrmm-desktop-ipc".into())
            .spawn(move || {
                dispatch_child_events(
                    launched.output,
                    reader_sink,
                    started_tx,
                    reader_status,
                    reader_cursor,
                    reader_maintenance,
                    reader_progress,
                )
            })
            .context("failed to start desktop-helper IPC reader")?;
        let stderr = thread::Builder::new()
            .name("meshrmm-desktop-stderr".into())
            .spawn(move || drain_child_stderr(launched.stderr))
            .context("failed to start desktop-helper error reader")?;
        let input = Arc::new(CommandWriter::new(launched.input)?);
        let start = ParentCommand::Start {
            viewer_name: self.viewer_name.clone(),
            display_id,
            frames_per_second: config.frames_per_second,
            bitrate_bits_per_second: config.bitrate_bits_per_second,
            codec: config.codec,
            pixel_format: config.pixel_format,
            capture_cursor: config.capture_cursor,
            grayscale: config.grayscale,
        };
        if let Err(error) = send_command(&input, &start) {
            terminate_and_wait(&launched.process);
            let _ = reader.join();
            let _ = stderr.join();
            return Err(error).context("failed to start the desktop helper");
        }
        let started = match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(started)) => started,
            Ok(Err(message)) => {
                terminate_and_wait(&launched.process);
                let _ = reader.join();
                let _ = stderr.join();
                anyhow::bail!("desktop helper failed: {message}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                terminate_and_wait(&launched.process);
                let _ = reader.join();
                let _ = stderr.join();
                anyhow::bail!("desktop helper did not start within 5 seconds");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminate_and_wait(&launched.process);
                let _ = reader.join();
                let _ = stderr.join();
                anyhow::bail!("desktop helper exited before capture started");
            }
        };
        tracing::info!(
            process_id = launched.process_id,
            session_id = launched.session_id,
            desktop = target.name(),
            width = started.format.width,
            height = started.format.height,
            "LocalSystem desktop helper started"
        );
        if let Err(error) = self.ensure_input_helper(target, started.active_display.id) {
            terminate_and_wait(&launched.process);
            let _ = reader.join();
            let _ = stderr.join();
            return Err(error).context("failed to start the independent desktop input helper");
        }
        self.preferred_desktop = Some(target);
        self.running = Some(RunningHelper {
            sink,
            started: started_rx,
            last_frame,
            process: launched.process,
            process_id: launched.process_id,
            target,
            input,
            status,
            reader: Some(reader),
            stderr: Some(stderr),
        });
        Ok(self.with_background_display(started))
    }

    pub fn set_display_border(&self, enabled: bool) -> anyhow::Result<()> {
        if self.running.is_some() {
            self.send(ParentCommand::SetDisplayBorder(enabled))?;
        }
        Ok(())
    }

    pub fn set_cursor_capture(&self, enabled: bool) -> anyhow::Result<()> {
        if self.running.is_some() {
            self.send(ParentCommand::SetCursorCapture(enabled))?;
        }
        Ok(())
    }

    pub fn request_keyframe(&self) -> anyhow::Result<()> {
        self.send(ParentCommand::RequestKeyframe)
            .context("failed to request a desktop-helper keyframe")
    }

    pub fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        self.send(ParentCommand::SetBitrate(bits_per_second.max(1)))
            .context("failed to change the desktop-helper bitrate")
    }

    pub fn input_controller(&self) -> Arc<dyn ScreenInput> {
        Arc::new(DesktopInputController {
            background_active: Arc::clone(&self.background_active),
            display_routes: Arc::clone(&self.display_routes),
            blackout_message: self.blackout_message.clone(),
            route: Arc::clone(&self.input_route),
            file_route: Arc::clone(&self.file_route),
            clipboard_route: Arc::clone(&self.clipboard_route),
            chat_route: Arc::clone(&self.chat_route),
            cursor: Arc::clone(&self.cursor),
            clipboard: Arc::clone(&self.clipboard),
            files: Arc::clone(&self.files),
            chat: Arc::clone(&self.chat),
            maintenance: Arc::clone(&self.maintenance),
            credentials: Arc::clone(&self.credentials),
            wallpaper_hidden: Arc::clone(&self.wallpaper_hidden),
            prevent_idle_lock: Arc::clone(&self.prevent_idle_lock),
            chat_enabled: Arc::clone(&self.chat_enabled),
        })
    }

    fn send(&self, command: ParentCommand) -> anyhow::Result<()> {
        let running = self
            .running
            .as_ref()
            .context("desktop helper is not running")?;
        send_command(&running.input, &command)
    }

    pub fn poll_ended(&mut self) -> Option<anyhow::Result<()>> {
        if self.sessions_checked.elapsed() >= Duration::from_secs(5) {
            self.sessions_checked = Instant::now();
            if let Ok(sessions) = active_rdp_sessions()
                && sessions != self.known_sessions
            {
                self.known_sessions = sessions;
                // Restart capture to publish the changed session catalog using the
                // existing ordered DisplayConfiguration/stream boundary.
                let result = self.stop();
                return Some(result);
            }
        }
        let running = self.running.as_ref()?;
        if running.target == DesktopTarget::Background
            && running
                .last_frame
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .elapsed()
                > Duration::from_secs(10)
        {
            terminate_and_wait(&running.process);
            set_status(
                &running.status,
                Err("Background rendering stalled; capture helper was terminated".into()),
            );
        }
        let status = running
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        status.as_ref()?;
        drop(status);
        let mut running = self.running.take()?;
        // Retry the same desktop first. Encoder/capture failures do not imply
        // that the interactive desktop changed, and switching desktops would
        // unnecessarily replace the independent input helper.
        self.preferred_desktop = Some(running.target);
        let result = running.take_status();
        running.finish();
        Some(result)
    }

    pub fn stop(&mut self) -> anyhow::Result<()> {
        let Some(mut running) = self.running.take() else {
            return Ok(());
        };
        let send_result = send_command(&running.input, &ParentCommand::Stop);
        if unsafe { WaitForSingleObject(running.process.0, STOP_TIMEOUT_MS) } == WAIT_TIMEOUT {
            tracing::warn!(
                process_id = running.process_id,
                "desktop helper did not stop promptly; terminating it"
            );
            terminate_and_wait(&running.process);
        }
        running.finish();
        send_result.context("failed to stop the desktop helper")
    }

    fn ensure_input_helper(
        &mut self,
        target: DesktopTarget,
        display_id: DisplayId,
    ) -> anyhow::Result<()> {
        if target != DesktopTarget::Background {
            if self
                .file_helper
                .as_ref()
                .is_none_or(|h| h.status.lock().unwrap().is_some())
                || self.file_route.lock().unwrap().is_none()
            {
                self.stop_file_helper();
                match start_input_helper(
                    &self.viewer_name,
                    match target {
                        DesktopTarget::Rdp(id, _) => DesktopTarget::Rdp(id, false),
                        _ => DesktopTarget::Default,
                    },
                    display_id,
                    Arc::clone(&self.cursor),
                    Arc::clone(&self.clipboard),
                    Arc::clone(&self.files),
                    Arc::clone(&self.chat),
                    Arc::clone(&self.maintenance),
                    Arc::clone(&self.credentials),
                    HelperKind::Files,
                ) {
                    Ok(helper) => {
                        let mut route = self.file_route.lock().unwrap();
                        send_command(
                            &helper.input,
                            &ParentCommand::SetWallpaperHidden(
                                self.wallpaper_hidden.load(Ordering::Acquire),
                            ),
                        )?;
                        *route = Some(Arc::clone(&helper.input));
                        self.file_helper = Some(helper);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "file transfers require a signed-in interactive user")
                    }
                }
            }
            if self.chat_helper.as_ref().is_none_or(|helper| {
                helper.target != target || helper.status.lock().unwrap().is_some()
            }) {
                self.stop_chat_helper();
                match start_input_helper(
                    &self.viewer_name,
                    target,
                    display_id,
                    Arc::clone(&self.cursor),
                    Arc::clone(&self.clipboard),
                    Arc::clone(&self.files),
                    Arc::clone(&self.chat),
                    Arc::clone(&self.maintenance),
                    Arc::clone(&self.credentials),
                    HelperKind::Chat,
                ) {
                    Ok(helper) => {
                        if self.chat_enabled.load(Ordering::Acquire) {
                            send_command(&helper.input, &ParentCommand::StartChat)?;
                        }
                        *self.chat_route.lock().unwrap() = Some(Arc::clone(&helper.input));
                        self.chat_helper = Some(helper);
                    }
                    Err(error) => tracing::warn!(%error, "independent chat helper unavailable"),
                }
            }
            if self.clipboard_helper.as_ref().is_none_or(|helper| {
                helper.target != target || helper.status.lock().unwrap().is_some()
            }) {
                self.stop_clipboard_helper();
                match start_input_helper(
                    &self.viewer_name,
                    target,
                    display_id,
                    Arc::clone(&self.cursor),
                    Arc::clone(&self.clipboard),
                    Arc::clone(&self.files),
                    Arc::clone(&self.chat),
                    Arc::clone(&self.maintenance),
                    Arc::clone(&self.credentials),
                    HelperKind::Clipboard,
                ) {
                    Ok(helper) => {
                        *self.clipboard_route.lock().unwrap() = Some(Arc::clone(&helper.input));
                        self.clipboard_helper = Some(helper);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "independent clipboard helper unavailable")
                    }
                }
            }
        }
        if let Some(helper) = self.input.as_mut()
            && helper.target == target
            && helper
                .status
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_none()
        {
            if helper.display_id != display_id {
                // Commands share one ordered pipe, so the new display mapping
                // is applied before any subsequent pointer/keyboard events.
                send_command(
                    &helper.input,
                    &ParentCommand::StartInput {
                        display_id,
                        viewer_name: self.viewer_name.clone(),
                    },
                )?;
                helper.display_id = display_id;
            }
            return Ok(());
        }
        self.stop_input_helper();
        *self
            .clipboard
            .latest
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let helper = start_input_helper(
            &self.viewer_name,
            target,
            display_id,
            Arc::clone(&self.cursor),
            Arc::clone(&self.clipboard),
            Arc::clone(&self.files),
            Arc::clone(&self.chat),
            Arc::clone(&self.maintenance),
            Arc::clone(&self.credentials),
            HelperKind::Input,
        )?;
        let mut route = self
            .input_route
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        send_command(
            &helper.input,
            &ParentCommand::SetPreventIdleLock(self.prevent_idle_lock.load(Ordering::Acquire)),
        )?;
        *route = Some(Arc::clone(&helper.input));
        self.input = Some(helper);
        Ok(())
    }

    pub fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop_chat_helper();
        self.stop_clipboard_helper();
        self.stop_file_helper();
        self.stop_input_helper();
        let mut credentials = self.credentials.lock().unwrap();
        *credentials = Credentials::new(std::mem::take(&mut credentials.store));
        drop(credentials);
        self.stop()
    }

    fn stop_chat_helper(&mut self) {
        self.credentials.lock().unwrap().state.prompt_active = false;
        *self.chat_route.lock().unwrap() = None;
        if let Some(mut helper) = self.chat_helper.take() {
            let _ = send_command(&helper.input, &ParentCommand::Stop);
            if unsafe { WaitForSingleObject(helper.process.0, STOP_TIMEOUT_MS) } == WAIT_TIMEOUT {
                terminate_and_wait(&helper.process);
            }
            helper.finish();
        }
        self.chat.queue.lock().unwrap().clear();
    }

    fn stop_clipboard_helper(&mut self) {
        *self.clipboard_route.lock().unwrap() = None;
        if let Some(mut helper) = self.clipboard_helper.take() {
            let _ = send_command(&helper.input, &ParentCommand::Stop);
            if unsafe { WaitForSingleObject(helper.process.0, STOP_TIMEOUT_MS) } == WAIT_TIMEOUT {
                terminate_and_wait(&helper.process);
            }
            helper.finish();
        }
        *self.clipboard.latest.lock().unwrap() = None;
    }

    fn stop_file_helper(&mut self) {
        *self.file_route.lock().unwrap() = None;
        if let Some(mut helper) = self.file_helper.take() {
            let _ = send_command(&helper.input, &ParentCommand::Stop);
            if unsafe { WaitForSingleObject(helper.process.0, STOP_TIMEOUT_MS) } == WAIT_TIMEOUT {
                terminate_and_wait(&helper.process);
            }
            helper.finish();
        }
        self.files.queue.lock().unwrap().clear();
    }

    fn stop_input_helper(&mut self) {
        self.credentials.lock().unwrap().state.can_autofill = false;
        let Some(mut helper) = self.input.take() else {
            return;
        };
        *self
            .input_route
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let _ = send_command(&helper.input, &ParentCommand::ReleaseInput);
        let _ = send_command(&helper.input, &ParentCommand::Stop);
        if unsafe { WaitForSingleObject(helper.process.0, STOP_TIMEOUT_MS) } == WAIT_TIMEOUT {
            tracing::warn!(
                process_id = helper.process_id,
                "desktop input helper did not stop promptly; terminating it"
            );
            terminate_and_wait(&helper.process);
        }
        helper.finish();
    }
}

impl Default for DesktopCaptureStreamer {
    fn default() -> Self {
        Self::new(
            String::new(),
            meshrmm_protocol::render_blackout_message("", ""),
            PathBuf::new(),
        )
    }
}

impl Drop for DesktopCaptureStreamer {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct RunningHelper {
    last_frame: Arc<Mutex<Instant>>,
    sink: Arc<Mutex<Option<EncodedFrameSink>>>,
    started: mpsc::Receiver<Result<StartedDesktop, String>>,
    process: OwnedHandle,
    process_id: u32,
    target: DesktopTarget,
    input: InputWriter,
    status: HelperStatus,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

struct RunningInputHelper {
    process: OwnedHandle,
    process_id: u32,
    target: DesktopTarget,
    display_id: DisplayId,
    input: InputWriter,
    status: HelperStatus,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

struct DesktopInputController {
    display_routes: Arc<Mutex<Vec<(DisplayId, DisplayId)>>>,
    background_active: Arc<AtomicBool>,
    chat_route: InputRoute,
    clipboard_route: InputRoute,
    blackout_message: String,
    file_route: InputRoute,
    route: InputRoute,
    cursor: HelperCursor,
    clipboard: HelperClipboard,
    files: HelperFiles,
    chat: HelperChat,
    maintenance: HelperMaintenance,
    credentials: HelperCredentials,
    wallpaper_hidden: Arc<AtomicBool>,
    prevent_idle_lock: Arc<AtomicBool>,
    chat_enabled: Arc<AtomicBool>,
}

impl ScreenInput for DesktopInputController {
    fn credential_state(&self) -> Option<meshrmm_protocol::CredentialState> {
        let mut state = self.credentials.lock().ok()?.state.clone();
        state.available = !self.is_background();
        state.can_autofill &= state.available && state.saved && !state.prompt_active;
        Some(state)
    }
    fn credential_command(&self, message: SessionMessage) -> anyhow::Result<()> {
        // Forgetting only deletes the saved file, so it works in any mode.
        anyhow::ensure!(
            matches!(message, SessionMessage::ForgetCredentials) || !self.is_background(),
            "Credentials are unavailable in background sessions"
        );
        match message {
            SessionMessage::PromptForCredentials => {
                let writer = self
                    .chat_route
                    .lock()
                    .unwrap()
                    .clone()
                    .context("Unlock Windows before requesting credentials")?;
                let mut credentials = self.credentials.lock().unwrap();
                anyhow::ensure!(
                    !credentials.state.prompt_active,
                    "A credential request is already open"
                );
                credentials.state.prompt_active = true;
                credentials.state.message = "Waiting for the remote user…".into();
                if let Err(error) = send_command(&writer, &ParentCommand::PromptCredentials) {
                    credentials.state.prompt_active = false;
                    return Err(error);
                }
                Ok(())
            }
            SessionMessage::AutofillCredentials => {
                let mut credentials = self.credentials.lock().unwrap();
                anyhow::ensure!(
                    credentials.state.saved
                        && credentials.state.can_autofill
                        && !credentials.state.prompt_active,
                    "No saved credentials or Windows password prompt"
                );
                let writer = self
                    .route
                    .lock()
                    .unwrap()
                    .clone()
                    .context("Windows desktop is switching; try again")?;
                // Another session may have forgotten or replaced the saved credential.
                let Some(encrypted) = super::credentials::load(&credentials.store)? else {
                    credentials.state.saved = false;
                    anyhow::bail!("No saved credentials");
                };
                send_command(&writer, &ParentCommand::AutofillCredentials(encrypted))
            }
            SessionMessage::ForgetCredentials => {
                let mut credentials = self.credentials.lock().unwrap();
                anyhow::ensure!(
                    !credentials.state.prompt_active,
                    "Close the credential dialog before forgetting credentials"
                );
                super::credentials::forget(&credentials.store)?;
                credentials.state.saved = false;
                credentials.state.message = "Saved credentials cleared".into();
                Ok(())
            }
            _ => anyhow::bail!("Invalid credential command"),
        }
    }

    fn is_console_session(&self) -> bool {
        !self.is_background() && self.display_routes.lock().unwrap().is_empty()
    }

    fn is_background(&self) -> bool {
        self.background_active.load(Ordering::Acquire)
    }
    fn set_prevent_idle_lock(&self, enabled: bool) -> anyhow::Result<()> {
        let route = self.route.lock().unwrap_or_else(|e| e.into_inner());
        self.prevent_idle_lock.store(enabled, Ordering::Release);
        if let Some(writer) = route.as_ref() {
            send_command(writer, &ParentCommand::SetPreventIdleLock(enabled))?;
        }
        Ok(())
    }
    fn set_wallpaper_hidden(&self, hidden: bool) -> anyhow::Result<()> {
        let route = self.file_route.lock().unwrap_or_else(|e| e.into_inner());
        self.wallpaper_hidden.store(hidden, Ordering::Release);
        if let Some(writer) = route.as_ref() {
            send_command(writer, &ParentCommand::SetWallpaperHidden(hidden))?;
        }
        Ok(())
    }
    fn set_blackout(&self, enabled: bool) -> anyhow::Result<()> {
        let writer = self
            .route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("desktop input helper is not running")?;
        send_command(
            &writer,
            &ParentCommand::Blackout {
                enabled,
                text: self.blackout_message.clone(),
            },
        )
    }
    fn maintenance_state(&self) -> Option<SessionMessage> {
        self.maintenance.lock().ok().and_then(|mut value| {
            if matches!(&*value, Some(SessionMessage::MaintenanceError { .. })) {
                value.take()
            } else {
                value.clone()
            }
        })
    }
    fn set_agent_input_blocked(&self, blocked: bool) -> anyhow::Result<()> {
        let writer = self
            .route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("desktop input helper is not running")?;
        send_command(&writer, &ParentCommand::BlockInput(blocked))
    }
    fn apply_files(&self, mut message: meshrmm_protocol::FileMessage) -> anyhow::Result<()> {
        if let meshrmm_protocol::FileMessage::Begin {
            destination:
                meshrmm_protocol::FileDestination::Drop { display_id, .. }
                | meshrmm_protocol::FileDestination::ClipboardPaste { display_id },
            ..
        } = &mut message
        {
            let routes = self.display_routes.lock().unwrap();
            if let Some((_, local)) = routes.iter().find(|(wire, _)| wire == display_id) {
                *display_id = *local;
            }
        }
        let writer = self
            .file_route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("interactive file helper unavailable")?;
        send_command(&writer, &ParentCommand::Files(message))?;
        Ok(())
    }
    fn files_ready(&self) -> Arc<tokio::sync::Notify> {
        self.files.ready.clone()
    }
    fn poll_files(&self) -> Option<meshrmm_protocol::FileMessage> {
        self.files.queue.lock().ok()?.pop_front()
    }

    fn stop_chat(&self) {
        self.chat_enabled.store(false, Ordering::Release);
        self.chat
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        if let Some(writer) = self
            .chat_route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let _ = send_command(&writer, &ParentCommand::StopChat);
        }
    }

    fn start_chat(&self) -> anyhow::Result<()> {
        let writer = self
            .chat_route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("interactive chat helper is unavailable")?;
        send_command(&writer, &ParentCommand::StartChat)?;
        self.chat_enabled.store(true, Ordering::Release);
        Ok(())
    }

    fn apply_chat(&self, text: String) -> anyhow::Result<()> {
        let writer = self
            .chat_route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("interactive chat helper is unavailable")?;
        send_command(&writer, &ParentCommand::Chat(text))?;
        Ok(())
    }
    fn chat_ready(&self) -> Arc<tokio::sync::Notify> {
        self.chat.ready.clone()
    }
    fn poll_chat(&self) -> anyhow::Result<Option<String>> {
        Ok(self
            .chat
            .queue
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front())
    }

    fn apply(&self, mut input: RemoteInput) -> anyhow::Result<()> {
        let routes = self.display_routes.lock().unwrap();
        if !routes.is_empty() {
            let Some((_, local)) = routes.iter().find(|(wire, _)| *wire == input.display_id())
            else {
                return Ok(());
            };
            input.set_display_id(*local);
        }
        drop(routes);
        let Some(writer) = self
            .route
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        else {
            // Desktop switches temporarily unroute input while the old helper
            // releases its pressed keys/buttons and its replacement starts.
            // Discard events in this gap instead of ending the remote session
            // or replaying stale input onto the new desktop.
            return Ok(());
        };
        send_command(&writer, &ParentCommand::Input(input))
            .context("failed to send input to the active desktop")
    }

    fn release_all(&self) -> anyhow::Result<()> {
        let Some(writer) = self
            .route
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        else {
            return Ok(());
        };
        send_command(&writer, &ParentCommand::ReleaseInput)
            .context("failed to release input on the active desktop")
    }

    fn agent_pointer_display(&self) -> Option<DisplayId> {
        let local = self.cursor.lock().unwrap_or_else(|e| e.into_inner()).2?;
        let routes = self.display_routes.lock().unwrap();
        if routes.is_empty() {
            Some(local)
        } else {
            routes
                .iter()
                .find(|(_, id)| *id == local)
                .map(|(wire, _)| *wire)
        }
    }

    fn cursor_shape(&self) -> CursorShape {
        self.cursor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .0
    }

    fn viewer_controls_input(&self) -> bool {
        self.cursor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .1
    }

    fn apply_clipboard(&self, text: ClipboardContent) -> anyhow::Result<()> {
        let writer = self
            .clipboard_route
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("desktop input helper is not running")?;
        send_command(&writer, &ParentCommand::Clipboard(text))
            .context("failed to send clipboard content to the active desktop")
    }

    fn clipboard_ready(&self) -> Option<Arc<tokio::sync::Notify>> {
        Some(self.clipboard.ready.clone())
    }
    fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>> {
        Ok(self
            .clipboard
            .latest
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take())
    }
}

impl RunningInputHelper {
    fn finish(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

impl RunningHelper {
    fn take_status(&self) -> anyhow::Result<()> {
        match self
            .status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            Some(Ok(())) => Ok(()),
            Some(Err(message)) => Err(anyhow::anyhow!(message)),
            None => anyhow::bail!("desktop helper exited without a final status"),
        }
    }

    fn finish(&mut self) {
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(stderr) = self.stderr.take() {
            let _ = stderr.join();
        }
    }
}

struct LaunchedHelper {
    process: OwnedHandle,
    process_id: u32,
    session_id: u32,
    input: File,
    output: File,
    stderr: File,
}

fn preferred_desktop() -> DesktopTarget {
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == NO_ACTIVE_SESSION {
        return DesktopTarget::Winlogon;
    }
    let mut token = HANDLE::default();
    if unsafe { WTSQueryUserToken(session_id, &mut token) }.is_ok() {
        drop(OwnedHandle(token));
        DesktopTarget::Default
    } else {
        DesktopTarget::Winlogon
    }
}

// Session 0 cannot see the console's complete monitor topology. Query the same
// desktop helper used by regular connections, without starting capture or input.
fn rdp_display_id(session: u32, local: DisplayId) -> DisplayId {
    DisplayId(
        0x8000_0000
            | (session << 8)
            | if local.0 == u32::MAX - 1 {
                255
            } else {
                local.0
            },
    )
}

fn active_rdp_sessions() -> anyhow::Result<Vec<(u32, String)>> {
    let mut buffer = std::ptr::null_mut();
    let mut count = 0;
    unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut buffer, &mut count) }?;
    let mut result = Vec::new();
    if !buffer.is_null() {
        for session in unsafe { std::slice::from_raw_parts(buffer, count as usize) } {
            if session.State != WTSActive
                || session.SessionId == unsafe { WTSGetActiveConsoleSessionId() }
            {
                continue;
            }
            let mut name = PWSTR::null();
            let mut bytes = 0;
            if unsafe {
                WTSQuerySessionInformationW(
                    None,
                    session.SessionId,
                    WTSUserName,
                    &mut name,
                    &mut bytes,
                )
            }
            .is_ok()
                && !name.is_null()
            {
                let user = unsafe { name.to_string() }.unwrap_or_default();
                unsafe { WTSFreeMemory(name.0.cast()) };
                if !user.is_empty() {
                    result.push((session.SessionId, user));
                }
            }
        }
        unsafe { WTSFreeMemory(buffer.cast()) };
    }
    Ok(result)
}

fn enumerate_console_displays() -> anyhow::Result<Vec<Display>> {
    let preferred = preferred_desktop();
    let mut last_error = None;
    for target in [preferred, preferred.alternate()] {
        match enumerate_desktop_displays(target) {
            Ok(displays) => return Ok(displays),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("no console desktop is available")))
}

fn enumerate_desktop_displays(target: DesktopTarget) -> anyhow::Result<Vec<Display>> {
    let mut launched = launch_system_helper(target)?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        let mut output = launched.output;
        let result = (|| {
            let count = bounded_len(read_u32(&mut output)?, MAX_DISPLAYS, "display count")?;
            (0..count)
                .map(|_| read_display(&mut output))
                .collect::<io::Result<Vec<_>>>()
        })();
        let _ = sender.send(result);
    });
    let stderr = thread::spawn(move || drain_child_stderr(launched.stderr));
    let result = (|| {
        write_command(&mut launched.input, &ParentCommand::EnumerateDisplays)?;
        let displays = receiver
            .recv_timeout(START_TIMEOUT)
            .context("console display enumeration timed out")??;
        anyhow::ensure!(!displays.is_empty(), "console desktop reported no displays");
        Ok(displays)
    })();
    terminate_and_wait(&launched.process);
    let _ = reader.join();
    let _ = stderr.join();
    result
}

fn launch_system_helper(target: DesktopTarget) -> anyhow::Result<LaunchedHelper> {
    launch_helper(target, false)
}
fn launch_helper(target: DesktopTarget, as_user: bool) -> anyhow::Result<LaunchedHelper> {
    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    let working_directory = executable
        .parent()
        .context("Agent executable has no parent directory")?;
    let session_id = if target == DesktopTarget::Background {
        meshrmm_remote_screen::background::require_session_zero()?;
        0
    } else if let DesktopTarget::Rdp(id, _) = target {
        id
    } else {
        unsafe { WTSGetActiveConsoleSessionId() }
    };
    if session_id == NO_ACTIVE_SESSION {
        anyhow::bail!("Windows reported no active console session");
    }
    let session_token = if as_user {
        let mut token = HANDLE::default();
        unsafe { WTSQueryUserToken(session_id, &mut token) }
            .context("no signed-in user for file transfers")?;
        OwnedHandle(token)
    } else {
        let mut process_token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut process_token) }
            .context("failed to open the LocalSystem coordinator token")?;
        let process_token = OwnedHandle(process_token);
        let mut session_token = HANDLE::default();
        unsafe {
            DuplicateTokenEx(
                process_token.0,
                TOKEN_ALL_ACCESS,
                None,
                SecurityImpersonation,
                TokenPrimary,
                &mut session_token,
            )
        }
        .context("failed to duplicate the LocalSystem coordinator token")?;
        let session_token = OwnedHandle(session_token);
        unsafe {
            SetTokenInformation(
                session_token.0,
                TokenSessionId,
                (&session_id as *const u32).cast(),
                std::mem::size_of::<u32>() as u32,
            )
        }
        .context("failed to move the desktop helper token into the console session")?;

        session_token
    };
    // Pipes start non-inheritable. Only this child's ends become inheritable,
    // just before the launch, and the explicit handle list keeps a concurrent
    // launch from passing them to another helper, which may run as the user.
    let (child_input, parent_input) = create_pipe()?;
    let (parent_output, child_output) = create_pipe()?;
    let (parent_stderr, child_stderr) = create_pipe()?;
    let child_handles = [child_input.0, child_output.0, child_stderr.0];
    let handle_list = HandleListAttribute::new(&child_handles)?;
    let executable_wide = wide(executable.as_os_str());
    let working_directory_wide = wide(working_directory.as_os_str());
    let mut command_line = wide(OsStr::new(&format!(
        "\"{}\" {}",
        executable.display(),
        if target == DesktopTarget::Background {
            "--background-helper"
        } else {
            "--capture-helper"
        }
    )));
    let desktop_path = if target == DesktopTarget::Background {
        // Launch in Session 0's window station, then bind the helper to its
        // private desktop before starting any GUI or capture threads.
        "winsta0\\default".to_owned()
    } else {
        format!("winsta0\\{}", target.name())
    };
    let mut desktop = wide(OsStr::new(&desktop_path));
    let startup = STARTUPINFOEXW {
        StartupInfo: STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOEXW>() as u32,
            lpDesktop: PWSTR(desktop.as_mut_ptr()),
            dwFlags: STARTF_USESTDHANDLES,
            hStdInput: child_input.0,
            hStdOutput: child_output.0,
            hStdError: child_stderr.0,
            ..Default::default()
        },
        lpAttributeList: handle_list.list(),
    };
    let mut environment = std::ptr::null_mut();
    if as_user {
        unsafe {
            windows::Win32::System::Environment::CreateEnvironmentBlock(
                &mut environment,
                Some(session_token.0),
                false,
            )
        }?;
    }
    let mut process_info = PROCESS_INFORMATION::default();
    let launched = child_handles
        .iter()
        .try_for_each(|&handle| unsafe {
            SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)
        })
        .context("failed to prepare the desktop-helper pipe handles")
        .and_then(|()| {
            unsafe {
                CreateProcessAsUserW(
                    Some(session_token.0),
                    PCWSTR(executable_wide.as_ptr()),
                    Some(PWSTR(command_line.as_mut_ptr())),
                    None,
                    None,
                    true,
                    CREATE_NO_WINDOW
                        | EXTENDED_STARTUPINFO_PRESENT
                        | windows::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT,
                    (!environment.is_null()).then_some(environment.cast_const()),
                    PCWSTR(working_directory_wide.as_ptr()),
                    &startup.StartupInfo,
                    &mut process_info,
                )
            }
            .map_err(anyhow::Error::from)
        });
    // The child holds its own copies now; close ours whether or not it started.
    drop(child_input);
    drop(child_output);
    drop(child_stderr);
    drop(handle_list);
    if !environment.is_null() {
        let _ =
            unsafe { windows::Win32::System::Environment::DestroyEnvironmentBlock(environment) };
    }
    launched.with_context(|| {
        format!(
            "failed to launch LocalSystem helper on winsta0\\{}",
            target.name()
        )
    })?;
    let _thread = OwnedHandle(process_info.hThread);
    Ok(LaunchedHelper {
        process: OwnedHandle(process_info.hProcess),
        process_id: process_info.dwProcessId,
        session_id,
        input: parent_input.into_file(),
        output: parent_output.into_file(),
        stderr: parent_stderr.into_file(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelperKind {
    Input,
    Files,
    Clipboard,
    Chat,
}

/// Whether a helper of `kind` sends `event`, matching the child run loops. The
/// Files and Clipboard helpers run with the user's token, so the parent treats
/// any other event as a protocol error instead of trusting it.
fn helper_sends(kind: HelperKind, event: &ChildEvent) -> bool {
    match event {
        ChildEvent::InputStarted | ChildEvent::Error(_) | ChildEvent::Stopped => true,
        ChildEvent::Cursor(..)
        | ChildEvent::MaintenanceState { .. }
        | ChildEvent::CredentialPrompt(_) => kind == HelperKind::Input,
        // The file helper reports wallpaper failures.
        ChildEvent::MaintenanceError(_) => matches!(kind, HelperKind::Input | HelperKind::Files),
        ChildEvent::Credentials(_) => matches!(kind, HelperKind::Input | HelperKind::Chat),
        ChildEvent::Files(_) => kind == HelperKind::Files,
        ChildEvent::Clipboard(_) => kind == HelperKind::Clipboard,
        ChildEvent::Chat(_) => kind == HelperKind::Chat,
        ChildEvent::Started(_) | ChildEvent::Frame(_) => false,
    }
}

fn child_event_name(event: &ChildEvent) -> &'static str {
    match event {
        ChildEvent::Credentials(_) => "credential result",
        ChildEvent::CredentialPrompt(_) => "credential detection",
        ChildEvent::Files(_) => "file transfer",
        ChildEvent::Started(_) => "video start",
        ChildEvent::InputStarted => "start",
        ChildEvent::MaintenanceState { .. } => "maintenance state",
        ChildEvent::MaintenanceError(_) => "maintenance error",
        ChildEvent::Frame(_) => "video frame",
        ChildEvent::Cursor(..) => "cursor",
        ChildEvent::Clipboard(_) => "clipboard",
        ChildEvent::Chat(_) => "chat",
        ChildEvent::Error(_) => "error",
        ChildEvent::Stopped => "stop",
    }
}

fn helper_uses_user_token(kind: HelperKind, target: DesktopTarget) -> bool {
    target != DesktopTarget::Background
        && (kind == HelperKind::Files
            || (kind == HelperKind::Clipboard
                && matches!(
                    target,
                    DesktopTarget::Default | DesktopTarget::Rdp(_, false)
                )))
}

// Keep the helper's startup options and independently shared event destinations explicit.
#[allow(clippy::too_many_arguments)]
fn start_input_helper(
    viewer_name: &str,
    target: DesktopTarget,
    display_id: DisplayId,
    cursor: HelperCursor,
    clipboard: HelperClipboard,
    files: HelperFiles,
    chat: HelperChat,
    maintenance: HelperMaintenance,
    credentials: HelperCredentials,
    kind: HelperKind,
) -> anyhow::Result<RunningInputHelper> {
    // Clipboard data can be owned/delayed-rendered by an interactive user app.
    // Use that user's token on the normal desktop, as the file helper does.
    let launched = launch_helper(target, helper_uses_user_token(kind, target))?;
    let status: HelperStatus = Arc::new(Mutex::new(None));
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let reader_status = Arc::clone(&status);
    let reader = thread::Builder::new()
        .name("meshrmm-desktop-input-ipc".into())
        .spawn(move || {
            dispatch_input_events(
                launched.output,
                started_tx,
                reader_status,
                cursor,
                clipboard,
                files,
                chat,
                maintenance,
                credentials,
                kind,
            )
        })
        .context("failed to start desktop input-helper IPC reader")?;
    let stderr = thread::Builder::new()
        .name("meshrmm-desktop-input-stderr".into())
        .spawn(move || drain_child_stderr(launched.stderr))
        .context("failed to start desktop input-helper error reader")?;
    let input = Arc::new(CommandWriter::new(launched.input)?);
    if let Err(error) = send_command(
        &input,
        &match kind {
            HelperKind::Files => ParentCommand::StartFiles,
            HelperKind::Clipboard => ParentCommand::StartClipboard,
            HelperKind::Chat => ParentCommand::StartChatHelper {
                viewer_name: viewer_name.to_owned(),
            },
            HelperKind::Input => ParentCommand::StartInput {
                display_id,
                viewer_name: viewer_name.to_owned(),
            },
        },
    ) {
        terminate_and_wait(&launched.process);
        let _ = reader.join();
        let _ = stderr.join();
        return Err(error).context("failed to initialize the desktop input helper");
    }
    match started_rx.recv_timeout(START_TIMEOUT) {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            terminate_and_wait(&launched.process);
            let _ = reader.join();
            let _ = stderr.join();
            anyhow::bail!("desktop input helper failed: {message}");
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            terminate_and_wait(&launched.process);
            let _ = reader.join();
            let _ = stderr.join();
            anyhow::bail!("desktop input helper did not start within 5 seconds");
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            terminate_and_wait(&launched.process);
            let _ = reader.join();
            let _ = stderr.join();
            anyhow::bail!("desktop input helper exited before startup");
        }
    }
    tracing::info!(
        process_id = launched.process_id,
        session_id = launched.session_id,
        desktop = target.name(),
        display_id = display_id.0,
        helper_kind = ?kind,
        "independent desktop service helper started"
    );
    Ok(RunningInputHelper {
        process: launched.process,
        process_id: launched.process_id,
        target,
        display_id,
        input,
        status,
        reader: Some(reader),
        stderr: Some(stderr),
    })
}

/// Creates a non-inheritable pipe and returns its (read, write) ends.
fn create_pipe() -> anyhow::Result<(OwnedHandle, OwnedHandle)> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe { CreatePipe(&mut read, &mut write, None, 0) }
        .context("failed to create desktop-helper IPC pipe")?;
    Ok((OwnedHandle(read), OwnedHandle(write)))
}

/// A PROC_THREAD_ATTRIBUTE_HANDLE_LIST that limits what a child inherits to
/// the listed handles. The handle array must outlive the process launch.
struct HandleListAttribute<'a> {
    // u64 storage keeps the opaque attribute list pointer-aligned.
    buffer: Vec<u64>,
    _handles: std::marker::PhantomData<&'a [HANDLE]>,
}

impl<'a> HandleListAttribute<'a> {
    fn new(handles: &'a [HANDLE]) -> anyhow::Result<Self> {
        let mut size = 0;
        // The sizing call reports ERROR_INSUFFICIENT_BUFFER by design.
        let _ = unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) };
        anyhow::ensure!(size > 0, "Windows reported no process attribute list size");
        let mut buffer = vec![0u64; size.div_ceil(std::mem::size_of::<u64>())];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }
            .context("failed to create the desktop-helper process attribute list")?;
        let attribute = Self {
            buffer,
            _handles: std::marker::PhantomData,
        };
        unsafe {
            UpdateProcThreadAttribute(
                attribute.list(),
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                std::mem::size_of_val(handles),
                None,
                None,
            )
        }
        .context("failed to limit the handles a desktop helper inherits")?;
        Ok(attribute)
    }

    fn list(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_ptr().cast_mut().cast())
    }
}

impl Drop for HandleListAttribute<'_> {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.list()) };
    }
}

fn dispatch_child_events(
    output: impl Read,
    sink: Arc<Mutex<Option<EncodedFrameSink>>>,
    started_tx: mpsc::Sender<Result<StartedDesktop, String>>,
    status: HelperStatus,
    cursor: HelperCursor,
    maintenance: HelperMaintenance,
    last_frame: Arc<Mutex<Instant>>,
) {
    let mut output = BufReader::new(output);
    loop {
        match read_event(&mut output) {
            Ok(ChildEvent::Started(started)) => {
                *last_frame.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                if started_tx.send(Ok(started)).is_err() {
                    break;
                }
            }
            Ok(ChildEvent::InputStarted) => {
                let message = "capture helper reported input-only startup".to_string();
                let _ = started_tx.send(Err(message.clone()));
                set_status(&status, Err(message));
                break;
            }
            Ok(ChildEvent::Frame(frame)) => {
                *last_frame.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                if let Some(sink) = sink
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    (sink)(frame);
                }
            }
            Ok(ChildEvent::Cursor(shape, viewer_controls_input, pointer_display)) => {
                *cursor.lock().unwrap_or_else(|error| error.into_inner()) =
                    (shape, viewer_controls_input, pointer_display);
            }
            Ok(ChildEvent::MaintenanceError(reason)) => {
                *maintenance.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SessionMessage::MaintenanceError { reason });
            }
            Ok(
                ChildEvent::Credentials(_)
                | ChildEvent::CredentialPrompt(_)
                | ChildEvent::MaintenanceState { .. }
                | ChildEvent::Files(_)
                | ChildEvent::Clipboard(_)
                | ChildEvent::Chat(_),
            ) => {
                set_status(
                    &status,
                    Err("capture helper reported an input-only clipboard event".into()),
                );
                break;
            }
            Ok(ChildEvent::Error(message)) => {
                let _ = started_tx.send(Err(message.clone()));
                set_status(&status, Err(message));
                break;
            }
            Ok(ChildEvent::Stopped) => {
                let _ = started_tx.send(Err("desktop helper stopped".into()));
                set_status(&status, Ok(()));
                break;
            }
            Err(error) => {
                let message = format!("desktop-helper IPC failed: {error}");
                let _ = started_tx.send(Err(message.clone()));
                set_status(&status, Err(message));
                break;
            }
        }
    }
}

// Each event destination is shared independently with the parent session.
#[allow(clippy::too_many_arguments)]
fn dispatch_input_events(
    output: File,
    started_tx: mpsc::SyncSender<Result<(), String>>,
    status: HelperStatus,
    cursor: HelperCursor,
    clipboard: HelperClipboard,
    files: HelperFiles,
    chat: HelperChat,
    maintenance: HelperMaintenance,
    credentials: HelperCredentials,
    kind: HelperKind,
) {
    let mut output = BufReader::new(output);
    let mut started_tx = Some(started_tx);
    let fail = |started_tx: &mut Option<mpsc::SyncSender<Result<(), String>>>, message: String| {
        if let Some(sender) = started_tx.take() {
            let _ = sender.send(Err(message.clone()));
        }
        set_status(&status, Err(message));
    };
    loop {
        let event = match read_event(&mut output) {
            Ok(event) if helper_sends(kind, &event) => event,
            Ok(event) => {
                tracing::warn!(
                    helper_kind = ?kind,
                    event = child_event_name(&event),
                    "desktop helper sent an event it never sends; stopping it"
                );
                fail(
                    &mut started_tx,
                    format!(
                        "desktop {kind:?} helper sent an unexpected {} event",
                        child_event_name(&event)
                    ),
                );
                break;
            }
            Err(error) => {
                fail(
                    &mut started_tx,
                    format!("desktop input-helper IPC failed: {error}"),
                );
                break;
            }
        };
        match event {
            ChildEvent::InputStarted => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Ok(()));
                } else {
                    set_status(
                        &status,
                        Err("desktop input helper sent duplicate start event".into()),
                    );
                    break;
                }
            }
            ChildEvent::Credentials(result) => {
                let mut current = credentials.lock().unwrap();
                if result.encrypted.is_some()
                    && (kind != HelperKind::Chat || !current.state.prompt_active)
                {
                    set_status(&status, Err("unexpected credential result".into()));
                    break;
                }
                current.state.message = result.message;
                if let Some(encrypted) = result.encrypted {
                    match super::credentials::save(&current.store, &encrypted) {
                        Ok(()) => current.state.saved = true,
                        Err(error) => {
                            current.state.message =
                                format!("Validated, but could not save credentials: {error:#}")
                        }
                    }
                }
                if kind == HelperKind::Chat {
                    current.state.prompt_active = false;
                }
            }
            ChildEvent::CredentialPrompt(ready) => {
                credentials.lock().unwrap().state.can_autofill = ready;
            }
            ChildEvent::MaintenanceError(reason) => {
                *maintenance.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SessionMessage::MaintenanceError { reason });
            }
            ChildEvent::MaintenanceState {
                agent_input_blocked,
                blacked_out,
            } => {
                *maintenance.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SessionMessage::MaintenanceState {
                        agent_input_blocked,
                        blacked_out,
                    });
            }
            ChildEvent::Cursor(shape, viewer_controls_input, pointer_display) => {
                *cursor.lock().unwrap_or_else(|error| error.into_inner()) =
                    (shape, viewer_controls_input, pointer_display);
            }
            ChildEvent::Files(message) => {
                // A window of the helper's transfer plus its acknowledgements
                // of the viewer's fits; the windows keep it from growing further.
                let mut queue = files.queue.lock().unwrap();
                if queue.len() < meshrmm_file_transfer::COMMAND_QUEUE {
                    queue.push_back(message);
                    files.ready.notify_one();
                } else {
                    tracing::warn!("dropped a file-transfer message from the desktop helper");
                }
            }
            ChildEvent::Chat(text) => {
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                if queue.len() < 32 {
                    queue.push_back(text);
                    chat.ready.notify_one();
                }
            }
            ChildEvent::Clipboard(text) => {
                *clipboard
                    .latest
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(text);
                clipboard.ready.notify_one();
            }
            ChildEvent::Error(message) => {
                fail(&mut started_tx, message);
                break;
            }
            ChildEvent::Stopped => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Err("desktop input helper stopped before startup".into()));
                }
                set_status(&status, Ok(()));
                break;
            }
            // helper_sends rejects video events before this match.
            ChildEvent::Started(_) | ChildEvent::Frame(_) => {
                fail(
                    &mut started_tx,
                    "desktop input helper reported a video event".into(),
                );
                break;
            }
        }
    }
}

fn set_status(status: &HelperStatus, value: Result<(), String>) {
    let mut status = status.lock().unwrap_or_else(|error| error.into_inner());
    if status.is_none() {
        *status = Some(value);
    }
}

/// Forwards a helper's stderr to the Agent log. Some helpers run with the
/// user's token, so lines are capped in length and rate and the rest is
/// drained without being kept, to keep the pipe from blocking the helper.
fn drain_child_stderr(stderr: File) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    let mut budget = LineBudget::new(STDERR_LINES_PER_WINDOW, STDERR_WINDOW, Instant::now());
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_STDERR_LINE_BYTES) {
            Ok(Some(truncated)) => {
                let (admitted, suppressed) = budget.admit(Instant::now());
                if suppressed > 0 {
                    tracing::warn!(suppressed, "suppressed desktop-helper stderr lines");
                }
                if admitted {
                    let message = String::from_utf8_lossy(&line);
                    tracing::warn!(%message, truncated, "desktop helper wrote to stderr");
                }
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "failed to read desktop-helper stderr");
                break;
            }
        }
    }
    let suppressed = budget.take_suppressed();
    if suppressed > 0 {
        tracing::warn!(suppressed, "suppressed desktop-helper stderr lines");
    }
}

/// Reads one line into `line`, keeping at most `limit` bytes and discarding
/// the rest up to the newline. Returns whether the line was cut short, or
/// `None` at end of input.
fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    limit: usize,
) -> io::Result<Option<bool>> {
    line.clear();
    let mut truncated = false;
    let mut read_any = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(read_any.then_some(truncated));
        }
        read_any = true;
        let newline = available.iter().position(|&byte| byte == b'\n');
        let content = &available[..newline.unwrap_or(available.len())];
        let room = limit.saturating_sub(line.len());
        truncated |= content.len() > room;
        line.extend_from_slice(&content[..content.len().min(room)]);
        let consumed = newline.map_or(available.len(), |index| index + 1);
        reader.consume(consumed);
        if newline.is_some() {
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(truncated));
        }
    }
}

/// Allows `limit` lines per window and counts the lines it refuses.
struct LineBudget {
    limit: u32,
    window: Duration,
    window_start: Instant,
    used: u32,
    suppressed: u64,
}

impl LineBudget {
    fn new(limit: u32, window: Duration, now: Instant) -> Self {
        Self {
            limit,
            window,
            window_start: now,
            used: 0,
            suppressed: 0,
        }
    }

    /// Returns whether to log this line, and how many lines the window that
    /// just ended suppressed.
    fn admit(&mut self, now: Instant) -> (bool, u64) {
        let mut ended = 0;
        if now.duration_since(self.window_start) >= self.window {
            ended = self.take_suppressed();
            self.window_start = now;
            self.used = 0;
        }
        if self.used < self.limit {
            self.used += 1;
            (true, ended)
        } else {
            self.suppressed += 1;
            (false, ended)
        }
    }

    fn take_suppressed(&mut self) -> u64 {
        std::mem::take(&mut self.suppressed)
    }
}

fn terminate_and_wait(process: &OwnedHandle) {
    let _ = unsafe { TerminateProcess(process.0, 1) };
    let _ = unsafe { WaitForSingleObject(process.0, STOP_TIMEOUT_MS) };
}

struct CommandWriter {
    sender: mpsc::SyncSender<Vec<u8>>,
    queued: Arc<std::sync::atomic::AtomicUsize>,
}

impl CommandWriter {
    fn new(writer: impl Write + Send + 'static) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(64);
        let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pending = Arc::clone(&queued);
        thread::Builder::new()
            .name("meshrmm-helper-writer".into())
            .spawn(move || {
                let mut writer = BufWriter::new(writer);
                while let Ok(bytes) = receiver.recv() {
                    let result = writer.write_all(&bytes).and_then(|()| writer.flush());
                    pending.fetch_sub(bytes.len(), Ordering::AcqRel);
                    if result.is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self { sender, queued })
    }

    fn send(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        let length = bytes.len();
        let limit = 2 * MAX_CLIPBOARD_WIRE_BYTES + MAX_CONTROL_BYTES;
        self.queued
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |queued| {
                queued.checked_add(length).filter(|next| *next <= limit)
            })
            .map_err(|_| anyhow::anyhow!("helper pipe byte budget exhausted"))?;
        if self.sender.try_send(bytes).is_err() {
            self.queued.fetch_sub(length, Ordering::AcqRel);
            anyhow::bail!("helper command queue full or closed");
        }
        Ok(())
    }
}

fn send_command(input: &InputWriter, command: &ParentCommand) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    write_command(&mut bytes, command)?;
    input.send(bytes)
}
