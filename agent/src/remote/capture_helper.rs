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

/// Entry point for the isolated LocalSystem desktop helper. It loads no Agent
/// configuration, opens no network sockets, and receives no Agent credential.
pub fn run_child() -> anyhow::Result<()> {
    let _background_desktop = if is_background_child() {
        let owner = meshrmm_remote_screen::background::Desktop::create()?;
        let binding = meshrmm_remote_screen::background::Desktop::bind()?;
        Some((binding, owner))
    } else {
        None
    };
    // stdout is the binary frame/control protocol. Forward diagnostics through
    // stderr, which the parent already drains into the protected Agent log.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(io::stderr)
        .with_ansi(false)
        .with_thread_names(true)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize helper logging: {error}"))?;
    let (command_tx, command_rx) = mpsc::sync_channel(64);
    thread::Builder::new()
        .name("meshrmm-desktop-commands".into())
        .spawn(move || {
            let mut input = io::stdin().lock();
            loop {
                let command = read_command(&mut input);
                let disconnected = command.is_err();
                if command_tx.send(command).is_err() || disconnected {
                    break;
                }
            }
        })
        .context("failed to start desktop-helper command reader")?;

    match command_rx
        .recv()
        .context("desktop-helper command pipe closed before startup")??
    {
        ParentCommand::Start {
            viewer_name: _,
            display_id,
            frames_per_second,
            bitrate_bits_per_second,
            codec,
            pixel_format,
            capture_cursor,
            grayscale,
        } => run_capture_child(
            command_rx,
            display_id,
            StreamConfig {
                frames_per_second,
                bitrate_bits_per_second,
                codec,
                pixel_format,
                capture_cursor,
                grayscale,
            },
        ),
        ParentCommand::EnumerateDisplays => {
            let displays = enumerate_displays()?;
            checked_len(displays.len(), MAX_DISPLAYS, "display count")?;
            let mut output = io::stdout().lock();
            write_u32(&mut output, displays.len() as u32)?;
            for display in &displays {
                write_display(&mut output, display)?;
            }
            output.flush()?;
            Ok(())
        }
        ParentCommand::StartFiles => run_file_child(command_rx),
        ParentCommand::StartClipboard => run_clipboard_child(command_rx),
        ParentCommand::StartChatHelper { viewer_name } => run_chat_child(command_rx, viewer_name),
        ParentCommand::StartInput {
            display_id,
            viewer_name,
        } => run_input_child(command_rx, display_id, viewer_name),
        _ => anyhow::bail!("desktop helper expected a capture or input start command"),
    }
}

fn run_capture_child(
    command_rx: mpsc::Receiver<io::Result<ParentCommand>>,
    mut display_id: Option<DisplayId>,
    mut config: StreamConfig,
) -> anyhow::Result<()> {
    let background = is_background_child();
    let mut border_enabled = false;
    'capture: loop {
        if config.frames_per_second == 0 || config.bitrate_bits_per_second == 0 {
            anyhow::bail!("desktop-helper frame rate and bitrate must be positive");
        }
        let displays = enumerate_displays()?;
        let active_display = display_id
            .and_then(|id| displays.iter().find(|display| display.id == id))
            .or_else(|| displays.iter().find(|display| display.primary))
            .or_else(|| displays.first())
            .cloned()
            .context("Windows reported no displays on the active desktop")?;
        let mut border = if border_enabled && !background {
            Some(super::display_border::DisplayBorder::show(&active_display)?)
        } else {
            None
        };
        let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
        let ipc_failed = Arc::new(AtomicBool::new(false));
        let sink_output = Arc::clone(&output);
        let sink_failed = Arc::clone(&ipc_failed);
        let sink: EncodedFrameSink = Arc::new(move |frame| {
            let mut output = sink_output
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if write_event(&mut *output, &ChildEvent::Frame(frame))
                .and_then(|()| output.flush())
                .is_err()
            {
                sink_failed.store(true, Ordering::Release);
            }
        });
        let mut streamer = WindowsDesktopDuplicationStreamer::new();
        let active = match streamer.start(config, active_display.id.0, sink) {
            Ok(active) => active,
            Err(error) => {
                emit_child_event(&output, ChildEvent::Error(error.to_string()))?;
                return Ok(());
            }
        };
        emit_child_event(
            &output,
            ChildEvent::Started(StartedDesktop {
                format: active,
                displays,
                active_display: active_display.clone(),
            }),
        )?;

        let mut terminal_error = None;
        loop {
            let _keep_border_alive = &border;
            if ipc_failed.load(Ordering::Acquire) {
                break;
            }
            if let Some(result) = streamer.poll_ended() {
                if let Err(error) = result {
                    terminal_error = Some(error.to_string());
                }
                break;
            }
            match command_rx.recv_timeout(Duration::from_millis(16)) {
                Ok(Ok(ParentCommand::SetDisplayBorder(enabled))) => {
                    border = None;
                    border_enabled = enabled && !background;
                    if border_enabled {
                        match super::display_border::DisplayBorder::show(&active_display) {
                            Ok(value) => border = Some(value),
                            Err(error) => emit_child_event(
                                &output,
                                ChildEvent::MaintenanceError(format!("Display border: {error:#}")),
                            )?,
                        }
                    }
                }
                Ok(Ok(ParentCommand::SetCursorCapture(enabled))) => {
                    streamer.set_cursor_capture(enabled);
                }
                Ok(Ok(ParentCommand::RequestKeyframe)) => {
                    if let Err(error) = streamer.request_keyframe() {
                        terminal_error = Some(error.to_string());
                        break;
                    }
                }
                Ok(Ok(ParentCommand::SetBitrate(bits_per_second))) => {
                    if let Err(error) = streamer.set_bitrate(bits_per_second.max(1)) {
                        terminal_error = Some(error.to_string());
                        break;
                    }
                }
                Ok(Ok(ParentCommand::Stop)) => break,
                Ok(Ok(ParentCommand::Start {
                    viewer_name: _,
                    display_id: next_display,
                    frames_per_second: next_fps,
                    bitrate_bits_per_second: next_bitrate,
                    codec: next_codec,
                    pixel_format: next_pixel_format,
                    capture_cursor: next_capture_cursor,
                    grayscale: next_grayscale,
                })) => {
                    streamer.stop()?;
                    display_id = next_display;
                    config.frames_per_second = next_fps;
                    config.bitrate_bits_per_second = next_bitrate;
                    config.codec = next_codec;
                    config.pixel_format = next_pixel_format;
                    config.capture_cursor = next_capture_cursor;
                    config.grayscale = next_grayscale;
                    continue 'capture;
                }
                Ok(Ok(
                    ParentCommand::EnumerateDisplays
                    | ParentCommand::StartFiles
                    | ParentCommand::StartClipboard
                    | ParentCommand::StartChatHelper { .. }
                    | ParentCommand::StartInput { .. }
                    | ParentCommand::Input(_)
                    | ParentCommand::SetWallpaperHidden(_)
                    | ParentCommand::SetPreventIdleLock(_)
                    | ParentCommand::Blackout { .. }
                    | ParentCommand::BlockInput(_)
                    | ParentCommand::ReleaseInput
                    | ParentCommand::PromptCredentials
                    | ParentCommand::AutofillCredentials(_)
                    | ParentCommand::Clipboard(_)
                    | ParentCommand::Files(_)
                    | ParentCommand::Chat(_)
                    | ParentCommand::StartChat
                    | ParentCommand::StopChat,
                )) => {
                    terminal_error =
                        Some("capture helper received a command reserved for input".into());
                    break;
                }
                Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        let _ = streamer.stop();
        if ipc_failed.load(Ordering::Acquire) {
            return Ok(());
        }
        match terminal_error {
            Some(message) => emit_child_event(&output, ChildEvent::Error(message))?,
            None => emit_child_event(&output, ChildEvent::Stopped)?,
        }
        return Ok(());
    }
}

fn run_input_child(
    command_rx: mpsc::Receiver<io::Result<ParentCommand>>,
    display_id: DisplayId,
    _viewer_name: String,
) -> anyhow::Result<()> {
    if is_background_child() {
        return run_background_input_child(command_rx);
    }
    let displays = enumerate_displays()?;
    let active_display = displays
        .into_iter()
        .find(|display| display.id == display_id)
        .context("input helper could not find the selected display")?;
    let mut keep_awake = None;
    let mut input = WindowsInputController::new();
    input.set_active_display(active_display)?;
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    emit_child_event(&output, ChildEvent::InputStarted)?;
    emit_child_event(
        &output,
        ChildEvent::MaintenanceState {
            agent_input_blocked: false,
            blacked_out: false,
        },
    )?;
    // UI Automation providers may block; keep discovery and fills off the
    // desktop input loop so pointer/key release remains responsive.
    let (credential_tx, credential_rx) = mpsc::sync_channel::<Vec<u8>>(1);
    let credential_output = output.clone();
    thread::Builder::new()
        .name("meshrmm-credential-fields".into())
        .spawn(move || {
            let detector = super::credentials::Detector::new().ok();
            let mut last_ready = None;
            loop {
                let ready = detector.as_ref().is_some_and(|d| d.ready());
                if last_ready != Some(ready) {
                    if emit_child_event(&credential_output, ChildEvent::CredentialPrompt(ready))
                        .is_err()
                    {
                        break;
                    }
                    last_ready = Some(ready);
                }
                match credential_rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(mut encrypted) => {
                        let result = detector
                            .as_ref()
                            .context("Windows credential detection unavailable")
                            .and_then(|d| d.fill(&mut encrypted));
                        let message = match result {
                            Ok(()) => {
                                "Credentials filled. Review the account and submit when ready."
                                    .into()
                            }
                            Err(error) => format!("Autofill: {error:#}"),
                        };
                        if emit_child_event(
                            &credential_output,
                            ChildEvent::Credentials(CredentialResult {
                                encrypted: None,
                                message,
                            }),
                        )
                        .is_err()
                        {
                            break;
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        })?;
    let mut sent_cursor = None;
    let mut terminal_error = None;
    loop {
        let cursor = (
            input.cursor_shape(),
            input.viewer_controls_input(),
            input.agent_pointer_display(),
        );
        if sent_cursor != Some(cursor) {
            if emit_child_event(&output, ChildEvent::Cursor(cursor.0, cursor.1, cursor.2)).is_err()
            {
                break;
            }
            sent_cursor = Some(cursor);
        }
        match command_rx.recv_timeout(Duration::from_millis(16)) {
            Ok(Ok(ParentCommand::AutofillCredentials(encrypted))) => {
                if credential_tx.try_send(encrypted).is_err() {
                    emit_child_event(
                        &output,
                        ChildEvent::MaintenanceError(
                            "Credential autofill is busy; try again".into(),
                        ),
                    )?;
                }
            }
            Ok(Ok(ParentCommand::SetPreventIdleLock(enabled))) => {
                if let Err(error) = super::keep_awake::set_enabled(&mut keep_awake, enabled) {
                    emit_child_event(
                        &output,
                        ChildEvent::MaintenanceError(format!("Prevent idle lock: {error:#}")),
                    )?;
                }
            }
            Ok(Ok(ParentCommand::StartInput { display_id, .. })) => {
                let result = enumerate_displays().and_then(|displays| {
                    let display = displays
                        .into_iter()
                        .find(|display| display.id == display_id)
                        .context("input helper could not find the selected display")?;
                    input.set_active_display(display)
                });
                if let Err(error) = result {
                    terminal_error = Some(error.to_string());
                    break;
                }
            }
            Ok(Ok(ParentCommand::Input(event))) => {
                if let Err(error) = input.apply(event) {
                    tracing::warn!(%error, "desktop input helper discarded invalid input");
                }
            }
            Ok(Ok(ParentCommand::Blackout { enabled, text })) => {
                if let Err(error) = input.set_blackout(enabled, &text) {
                    emit_child_event(&output, ChildEvent::MaintenanceError(error.to_string()))?;
                    continue;
                }
                emit_child_event(
                    &output,
                    ChildEvent::MaintenanceState {
                        agent_input_blocked: input.blocked(),
                        blacked_out: input.blacked_out(),
                    },
                )?;
            }
            Ok(Ok(ParentCommand::BlockInput(blocked))) => {
                if let Err(error) = input.set_blocked(blocked) {
                    emit_child_event(&output, ChildEvent::MaintenanceError(error.to_string()))?;
                    continue;
                }
                emit_child_event(
                    &output,
                    ChildEvent::MaintenanceState {
                        agent_input_blocked: input.blocked(),
                        blacked_out: input.blacked_out(),
                    },
                )?;
            }
            Ok(Ok(ParentCommand::ReleaseInput)) => {
                if let Err(error) = input.release_all() {
                    tracing::warn!(%error, "desktop input helper could not release input");
                }
            }
            Ok(Ok(ParentCommand::Stop)) => break,
            Ok(Ok(_)) => {
                terminal_error = Some("input helper received a video command".into());
                break;
            }
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    let _ = input.release_all();
    match terminal_error {
        Some(message) => emit_child_event(&output, ChildEvent::Error(message))?,
        None => emit_child_event(&output, ChildEvent::Stopped)?,
    }
    Ok(())
}

fn is_background_child() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--background-helper")
}

fn background_display() -> Display {
    let info = meshrmm_remote_screen::background::display();
    Display {
        session: meshrmm_protocol::DesktopSession::Background,
        id: DisplayId(info.id),
        name: info.name,
        x: info.x,
        y: info.y,
        width: info.width,
        height: info.height,
        primary: false,
    }
}

fn run_background_input_child(
    command_rx: mpsc::Receiver<io::Result<ParentCommand>>,
) -> anyhow::Result<()> {
    let mut workspace = super::background::Workspace::new()?;
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    emit_child_event(&output, ChildEvent::InputStarted)?;
    emit_child_event(
        &output,
        ChildEvent::MaintenanceState {
            agent_input_blocked: false,
            blacked_out: false,
        },
    )?;
    loop {
        workspace.pump();
        let result = match command_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(Ok(ParentCommand::Input(input))) => workspace.apply(input),
            Ok(Ok(ParentCommand::ReleaseInput)) => {
                workspace.release();
                Ok(())
            }
            Ok(Ok(ParentCommand::Stop))
            | Ok(Err(_))
            | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Ok(Ok(
                ParentCommand::Blackout { enabled: true, .. } | ParentCommand::BlockInput(true),
            )) => Err(anyhow::anyhow!(
                "Console blackout and input blocking are unavailable in background mode"
            )),
            Ok(Ok(
                ParentCommand::StartInput { .. }
                | ParentCommand::SetPreventIdleLock(_)
                | ParentCommand::Blackout { enabled: false, .. }
                | ParentCommand::BlockInput(false),
            ))
            | Err(mpsc::RecvTimeoutError::Timeout) => Ok(()),
            Ok(Ok(_)) => Err(anyhow::anyhow!("Unsupported background input command")),
        };
        if let Err(error) = result {
            emit_child_event(&output, ChildEvent::MaintenanceError(error.to_string()))?;
        }
    }
    drop(workspace);
    emit_child_event(&output, ChildEvent::Stopped).map_err(Into::into)
}

fn enumerate_displays() -> anyhow::Result<Vec<Display>> {
    if is_background_child() {
        return Ok(vec![background_display()]);
    }
    meshrmm_remote_screen::enumerate_displays()
        .context("failed to enumerate displays on the active desktop")?
        .into_iter()
        .map(|display| {
            Ok(Display {
                session: meshrmm_protocol::DesktopSession::Console,
                id: DisplayId(display.id),
                name: display.name,
                x: display.x,
                y: display.y,
                width: display.width,
                height: display.height,
                primary: display.primary,
            })
        })
        .collect()
}

fn emit_child_event(
    output: &Arc<Mutex<BufWriter<io::Stdout>>>,
    event: ChildEvent,
) -> io::Result<()> {
    let mut output = output.lock().unwrap_or_else(|error| error.into_inner());
    write_event(&mut *output, &event)?;
    output.flush()
}

fn write_command(mut writer: impl Write, command: &ParentCommand) -> io::Result<()> {
    match command {
        ParentCommand::PromptCredentials => writer.write_all(&[23]),
        ParentCommand::AutofillCredentials(bytes) => {
            checked_len(bytes.len(), 8192, "protected credentials")?;
            writer.write_all(&[24])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(bytes)
        }
        ParentCommand::EnumerateDisplays => writer.write_all(&[COMMAND_ENUMERATE_DISPLAYS]),
        ParentCommand::StartFiles => writer.write_all(&[13]),
        ParentCommand::StartClipboard => writer.write_all(&[16]),
        ParentCommand::StartChatHelper { viewer_name } => {
            checked_len(viewer_name.len(), MAX_CONTROL_BYTES, "viewer name")?;
            writer.write_all(&[17])?;
            write_u32(&mut writer, viewer_name.len() as u32)?;
            writer.write_all(viewer_name.as_bytes())
        }
        ParentCommand::Files(message) => {
            writer.write_all(&[12])?;
            write_file_message(&mut writer, message)
        }
        ParentCommand::Start {
            viewer_name,
            display_id,
            frames_per_second,
            bitrate_bits_per_second,
            codec,
            pixel_format,
            capture_cursor,
            grayscale,
        } => {
            writer.write_all(&[COMMAND_START])?;
            checked_len(viewer_name.len(), MAX_DISPLAY_NAME_BYTES, "viewer name")?;
            write_u32(&mut writer, viewer_name.len() as u32)?;
            writer.write_all(viewer_name.as_bytes())?;
            write_u32(&mut writer, display_id.map_or(NO_DISPLAY, |id| id.0))?;
            write_u32(&mut writer, *frames_per_second)?;
            write_u32(&mut writer, *bitrate_bits_per_second)
                .and_then(|()| writer.write_all(&[codec_byte(*codec)]))
                .and_then(|()| writer.write_all(&[pixel_format_byte(*pixel_format)]))
                .and_then(|()| writer.write_all(&[u8::from(*capture_cursor)]))
                .and_then(|()| writer.write_all(&[u8::from(*grayscale)]))
        }
        ParentCommand::SetWallpaperHidden(hidden) => writer.write_all(&[19, u8::from(*hidden)]),
        ParentCommand::SetPreventIdleLock(enabled) => writer.write_all(&[21, u8::from(*enabled)]),
        ParentCommand::SetCursorCapture(enabled) => writer.write_all(&[18, u8::from(*enabled)]),
        ParentCommand::SetDisplayBorder(enabled) => writer.write_all(&[20, u8::from(*enabled)]),
        ParentCommand::RequestKeyframe => writer.write_all(&[COMMAND_REQUEST_KEYFRAME]),
        ParentCommand::SetBitrate(bits_per_second) => {
            writer.write_all(&[COMMAND_SET_BITRATE])?;
            write_u32(&mut writer, *bits_per_second)
        }
        ParentCommand::StartInput {
            display_id,
            viewer_name,
        } => {
            checked_len(viewer_name.len(), MAX_DISPLAY_NAME_BYTES, "viewer name")?;
            writer.write_all(&[COMMAND_START_INPUT])?;
            write_u32(&mut writer, display_id.0)?;
            write_u32(&mut writer, viewer_name.len() as u32)?;
            writer.write_all(viewer_name.as_bytes())
        }
        ParentCommand::Input(input) => {
            let bytes = SessionMessage::Input(input.clone())
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CONTROL_BYTES, "desktop input")?;
            writer.write_all(&[COMMAND_INPUT])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ParentCommand::Blackout { enabled, text } => {
            checked_len(text.len(), MAX_CONTROL_BYTES, "blackout message")?;
            writer.write_all(&[COMMAND_BLACKOUT, u8::from(*enabled)])?;
            write_u32(&mut writer, text.len() as u32)?;
            writer.write_all(text.as_bytes())
        }
        ParentCommand::BlockInput(blocked) => {
            writer.write_all(&[COMMAND_BLOCK_INPUT, u8::from(*blocked)])
        }
        ParentCommand::ReleaseInput => writer.write_all(&[COMMAND_RELEASE_INPUT]),
        ParentCommand::Clipboard(text) => {
            let bytes = text
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CLIPBOARD_WIRE_BYTES, "desktop clipboard")?;
            writer.write_all(&[COMMAND_CLIPBOARD])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ParentCommand::StopChat => writer.write_all(&[COMMAND_STOP_CHAT]),
        ParentCommand::StartChat => writer.write_all(&[COMMAND_START_CHAT]),
        ParentCommand::Chat(text) => {
            if !meshrmm_protocol::valid_chat_text(text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid chat text",
                ));
            }
            writer.write_all(&[COMMAND_CHAT])?;
            write_u32(&mut writer, text.len() as u32)?;
            writer.write_all(text.as_bytes())
        }
        ParentCommand::Stop => writer.write_all(&[COMMAND_STOP]),
    }
}

fn read_command(mut reader: impl Read) -> io::Result<ParentCommand> {
    match read_u8(&mut reader)? {
        23 => Ok(ParentCommand::PromptCredentials),
        24 => {
            let length = bounded_len(read_u32(&mut reader)?, 8192, "protected credentials")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            Ok(ParentCommand::AutofillCredentials(bytes))
        }
        COMMAND_ENUMERATE_DISPLAYS => Ok(ParentCommand::EnumerateDisplays),
        16 => Ok(ParentCommand::StartClipboard),
        17 => {
            let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "viewer name")?;
            let mut name = vec![0; length];
            reader.read_exact(&mut name)?;
            let viewer_name = String::from_utf8(name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(ParentCommand::StartChatHelper { viewer_name })
        }
        COMMAND_START => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_DISPLAY_NAME_BYTES,
                "viewer name",
            )?;
            let mut name = vec![0; length];
            reader.read_exact(&mut name)?;
            let viewer_name = String::from_utf8(name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let display_id = read_u32(&mut reader)?;
            Ok(ParentCommand::Start {
                viewer_name,
                display_id: (display_id != NO_DISPLAY).then_some(DisplayId(display_id)),
                frames_per_second: read_u32(&mut reader)?,
                bitrate_bits_per_second: read_u32(&mut reader)?,
                codec: read_codec(&mut reader)?,
                pixel_format: read_pixel_format(&mut reader)?,
                capture_cursor: read_bool(&mut reader)?,
                grayscale: read_bool(&mut reader)?,
            })
        }
        19 => Ok(ParentCommand::SetWallpaperHidden(read_bool(&mut reader)?)),
        21 => Ok(ParentCommand::SetPreventIdleLock(read_bool(&mut reader)?)),
        18 => Ok(ParentCommand::SetCursorCapture(read_bool(&mut reader)?)),
        20 => Ok(ParentCommand::SetDisplayBorder(read_bool(&mut reader)?)),
        COMMAND_REQUEST_KEYFRAME => Ok(ParentCommand::RequestKeyframe),
        COMMAND_SET_BITRATE => Ok(ParentCommand::SetBitrate(read_u32(&mut reader)?)),
        COMMAND_START_INPUT => {
            let display_id = DisplayId(read_u32(&mut reader)?);
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_DISPLAY_NAME_BYTES,
                "viewer name",
            )?;
            let mut name = vec![0; length];
            reader.read_exact(&mut name)?;
            let viewer_name = String::from_utf8(name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(ParentCommand::StartInput {
                display_id,
                viewer_name,
            })
        }
        COMMAND_INPUT => {
            let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "desktop input")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            match SessionMessage::decode(&bytes) {
                Ok(SessionMessage::Input(input)) => Ok(ParentCommand::Input(input)),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "desktop input contained a non-input message",
                )),
                Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
        }
        COMMAND_BLACKOUT => {
            let enabled = read_u8(&mut reader)?;
            if enabled > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid blackout flag",
                ));
            }
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_CONTROL_BYTES,
                "blackout message",
            )?;
            let mut text = vec![0; length];
            reader.read_exact(&mut text)?;
            let text = String::from_utf8(text)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(ParentCommand::Blackout {
                enabled: enabled == 1,
                text,
            })
        }
        COMMAND_BLOCK_INPUT => {
            let mut value = [0];
            reader.read_exact(&mut value)?;
            match value[0] {
                0 => Ok(ParentCommand::BlockInput(false)),
                1 => Ok(ParentCommand::BlockInput(true)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid input block flag",
                )),
            }
        }
        COMMAND_RELEASE_INPUT => Ok(ParentCommand::ReleaseInput),
        COMMAND_CLIPBOARD => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_CLIPBOARD_WIRE_BYTES,
                "desktop clipboard",
            )?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            ClipboardContent::decode(&bytes)
                .map(ParentCommand::Clipboard)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        13 => Ok(ParentCommand::StartFiles),
        12 => Ok(ParentCommand::Files(read_file_message(&mut reader)?)),
        COMMAND_STOP_CHAT => Ok(ParentCommand::StopChat),
        COMMAND_START_CHAT => Ok(ParentCommand::StartChat),
        COMMAND_CHAT => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                meshrmm_protocol::MAX_CHAT_TEXT_BYTES,
                "chat text",
            )?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            let text = String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if !meshrmm_protocol::valid_chat_text(&text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid chat text",
                ));
            }
            Ok(ParentCommand::Chat(text))
        }
        COMMAND_STOP => Ok(ParentCommand::Stop),
        opcode => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown desktop-helper command opcode {opcode}"),
        )),
    }
}

fn write_event(mut writer: impl Write, event: &ChildEvent) -> io::Result<()> {
    match event {
        ChildEvent::CredentialPrompt(ready) => writer.write_all(&[13, u8::from(*ready)]),
        ChildEvent::Credentials(result) => {
            let bytes = serde_json::to_vec(result).map_err(io::Error::other)?;
            checked_len(bytes.len(), 32768, "credential result")?;
            writer.write_all(&[12])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ChildEvent::Files(message) => {
            writer.write_all(&[9])?;
            write_file_message(&mut writer, message)
        }
        ChildEvent::Started(started) => {
            writer.write_all(&[EVENT_STARTED])?;
            write_u32(&mut writer, started.format.width)?;
            write_u32(&mut writer, started.format.height)?;
            write_u32(&mut writer, started.format.frames_per_second)?;
            write_u32(&mut writer, started.format.bitrate_bits_per_second)?;
            writer.write_all(&[codec_byte(started.format.codec)])?;
            writer.write_all(&[pixel_format_byte(started.format.pixel_format)])?;
            write_u32(&mut writer, started.active_display.id.0)?;
            checked_len(started.displays.len(), MAX_DISPLAYS, "display list")?;
            write_u32(&mut writer, started.displays.len() as u32)?;
            for display in &started.displays {
                write_display(&mut writer, display)?;
            }
            Ok(())
        }
        ChildEvent::MaintenanceError(reason) => {
            checked_len(reason.len(), MAX_ERROR_BYTES, "maintenance error")?;
            writer.write_all(&[11])?;
            write_u32(&mut writer, reason.len() as u32)?;
            writer.write_all(reason.as_bytes())
        }
        ChildEvent::MaintenanceState {
            agent_input_blocked,
            blacked_out,
        } => writer.write_all(&[10, u8::from(*agent_input_blocked), u8::from(*blacked_out)]),
        ChildEvent::InputStarted => writer.write_all(&[EVENT_INPUT_STARTED]),
        ChildEvent::Frame(frame) => {
            let codec_config = frame.codec_config.as_deref().unwrap_or_default();
            checked_len(
                codec_config.len(),
                MAX_CODEC_CONFIG_BYTES,
                "codec configuration",
            )?;
            checked_len(frame.data.len(), MAX_FRAME_BYTES, "encoded frame")?;
            writer.write_all(&[EVENT_FRAME])?;
            write_u64(&mut writer, frame.capture_timestamp_us)?;
            write_u64(&mut writer, frame.encode_complete_timestamp_us)?;
            writer.write_all(&[u8::from(frame.keyframe)])?;
            write_u32(&mut writer, codec_config.len() as u32)?;
            write_u32(&mut writer, frame.data.len() as u32)?;
            writer.write_all(codec_config)?;
            writer.write_all(&frame.data)
        }
        ChildEvent::Cursor(shape, viewer_controls_input, pointer_display) => {
            let bytes = SessionMessage::CursorShape { shape: *shape }
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CONTROL_BYTES, "cursor shape")?;
            writer.write_all(&[EVENT_CURSOR, u8::from(*viewer_controls_input)])?;
            write_u32(&mut writer, pointer_display.map_or(u32::MAX, |id| id.0))?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ChildEvent::Clipboard(text) => {
            let bytes = text
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CLIPBOARD_WIRE_BYTES, "clipboard content")?;
            writer.write_all(&[EVENT_CLIPBOARD])?;
            write_u32(&mut writer, bytes.len() as u32)?;
            writer.write_all(&bytes)
        }
        ChildEvent::Chat(text) => {
            if !meshrmm_protocol::valid_chat_text(text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid chat text",
                ));
            }
            writer.write_all(&[EVENT_CHAT])?;
            write_u32(&mut writer, text.len() as u32)?;
            writer.write_all(text.as_bytes())
        }
        ChildEvent::Error(message) => {
            let message = message.as_bytes();
            checked_len(message.len(), MAX_ERROR_BYTES, "desktop-helper error")?;
            writer.write_all(&[EVENT_ERROR])?;
            write_u32(&mut writer, message.len() as u32)?;
            writer.write_all(message)
        }
        ChildEvent::Stopped => writer.write_all(&[EVENT_STOPPED]),
    }
}

fn read_event(mut reader: impl Read) -> io::Result<ChildEvent> {
    match read_u8(&mut reader)? {
        12 => {
            let length = bounded_len(read_u32(&mut reader)?, 32768, "credential result")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            Ok(ChildEvent::Credentials(
                serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            ))
        }
        13 => match read_u8(&mut reader)? {
            0 => Ok(ChildEvent::CredentialPrompt(false)),
            1 => Ok(ChildEvent::CredentialPrompt(true)),
            _ => Err(io::Error::other("invalid credential prompt state")),
        },
        11 => {
            let length = bounded_len(read_u32(&mut reader)?, MAX_ERROR_BYTES, "maintenance error")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            Ok(ChildEvent::MaintenanceError(
                String::from_utf8(bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            ))
        }
        10 => {
            let a = read_u8(&mut reader)?;
            let b = read_u8(&mut reader)?;
            if a > 1 || b > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid maintenance state",
                ));
            }
            Ok(ChildEvent::MaintenanceState {
                agent_input_blocked: a == 1,
                blacked_out: b == 1,
            })
        }
        EVENT_STARTED => {
            let format = ActiveFormat {
                width: read_u32(&mut reader)?,
                height: read_u32(&mut reader)?,
                frames_per_second: read_u32(&mut reader)?,
                bitrate_bits_per_second: read_u32(&mut reader)?,
                codec: read_codec(&mut reader)?,
                pixel_format: read_pixel_format(&mut reader)?,
            };
            let active_display_id = DisplayId(read_u32(&mut reader)?);
            let count = bounded_len(read_u32(&mut reader)?, MAX_DISPLAYS, "display list")?;
            let mut displays = Vec::with_capacity(count);
            for _ in 0..count {
                displays.push(read_display(&mut reader)?);
            }
            let active_display = displays
                .iter()
                .find(|display| display.id == active_display_id)
                .cloned()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "desktop helper selected an unknown display",
                    )
                })?;
            Ok(ChildEvent::Started(StartedDesktop {
                format,
                displays,
                active_display,
            }))
        }
        EVENT_INPUT_STARTED => Ok(ChildEvent::InputStarted),
        EVENT_FRAME => {
            let capture_timestamp_us = read_u64(&mut reader)?;
            let encode_complete_timestamp_us = read_u64(&mut reader)?;
            let keyframe = match read_u8(&mut reader)? {
                0 => false,
                1 => true,
                value => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid keyframe flag {value}"),
                    ));
                }
            };
            let codec_config_len = bounded_len(
                read_u32(&mut reader)?,
                MAX_CODEC_CONFIG_BYTES,
                "codec configuration",
            )?;
            let frame_len = bounded_len(read_u32(&mut reader)?, MAX_FRAME_BYTES, "encoded frame")?;
            let mut codec_config = vec![0; codec_config_len];
            let mut data = vec![0; frame_len];
            reader.read_exact(&mut codec_config)?;
            reader.read_exact(&mut data)?;
            Ok(ChildEvent::Frame(EncodedAccessUnit {
                capture_timestamp_us,
                encode_complete_timestamp_us,
                keyframe,
                codec_config: (!codec_config.is_empty()).then_some(codec_config),
                data,
            }))
        }
        EVENT_CURSOR => {
            let viewer_controls_input = read_bool(&mut reader)?;
            let pointer_id = read_u32(&mut reader)?;
            let pointer_display = (pointer_id != u32::MAX).then_some(DisplayId(pointer_id));
            let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "cursor shape")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            match SessionMessage::decode(&bytes) {
                Ok(SessionMessage::CursorShape { shape }) => Ok(ChildEvent::Cursor(
                    shape,
                    viewer_controls_input,
                    pointer_display,
                )),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cursor event contained an unexpected message",
                )),
                Err(error) => Err(io::Error::new(io::ErrorKind::InvalidData, error)),
            }
        }
        EVENT_CLIPBOARD => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_CLIPBOARD_WIRE_BYTES,
                "clipboard content",
            )?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            ClipboardContent::decode(&bytes)
                .map(ChildEvent::Clipboard)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        9 => Ok(ChildEvent::Files(read_file_message(&mut reader)?)),
        EVENT_CHAT => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                meshrmm_protocol::MAX_CHAT_TEXT_BYTES,
                "chat text",
            )?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            let text = String::from_utf8(bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            if !meshrmm_protocol::valid_chat_text(&text) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid chat text",
                ));
            }
            Ok(ChildEvent::Chat(text))
        }
        EVENT_ERROR => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_ERROR_BYTES,
                "desktop-helper error",
            )?;
            let mut message = vec![0; length];
            reader.read_exact(&mut message)?;
            String::from_utf8(message)
                .map(ChildEvent::Error)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        }
        EVENT_STOPPED => Ok(ChildEvent::Stopped),
        opcode => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown desktop-helper event opcode {opcode}"),
        )),
    }
}

fn write_display(writer: &mut impl Write, display: &Display) -> io::Result<()> {
    let name = display.name.as_bytes();
    checked_len(name.len(), MAX_DISPLAY_NAME_BYTES, "display name")?;
    write_u32(writer, display.id.0)?;
    write_u32(writer, display.x as u32)?;
    write_u32(writer, display.y as u32)?;
    write_u32(writer, display.width)?;
    write_u32(writer, display.height)?;
    writer.write_all(&[u8::from(display.primary)])?;
    write_u32(writer, name.len() as u32)?;
    writer.write_all(name)
}

fn read_display(reader: &mut impl Read) -> io::Result<Display> {
    let id = DisplayId(read_u32(reader)?);
    let x = read_u32(reader)? as i32;
    let y = read_u32(reader)? as i32;
    let width = read_u32(reader)?;
    let height = read_u32(reader)?;
    let primary = match read_u8(reader)? {
        0 => false,
        1 => true,
        value => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid display primary flag {value}"),
            ));
        }
    };
    let name_len = bounded_len(read_u32(reader)?, MAX_DISPLAY_NAME_BYTES, "display name")?;
    let mut name = vec![0; name_len];
    reader.read_exact(&mut name)?;
    let name = String::from_utf8(name)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(Display {
        session: if id == meshrmm_protocol::BACKGROUND_DISPLAY_ID {
            meshrmm_protocol::DesktopSession::Background
        } else {
            meshrmm_protocol::DesktopSession::Console
        },
        id,
        name,
        x,
        y,
        width,
        height,
        primary,
    })
}

fn checked_len(length: usize, maximum: usize, label: &str) -> io::Result<()> {
    if length > maximum || length > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{label} exceeds the IPC size limit"),
        ));
    }
    Ok(())
}

fn bounded_len(length: u32, maximum: usize, label: &str) -> io::Result<usize> {
    let length = length as usize;
    checked_len(length, maximum, label)?;
    Ok(length)
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}
fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}
fn read_bool(reader: &mut impl Read) -> io::Result<bool> {
    match read_u8(reader)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid boolean",
        )),
    }
}

fn read_u8(reader: &mut impl Read) -> io::Result<u8> {
    let mut value = [0; 1];
    reader.read_exact(&mut value)?;
    Ok(value[0])
}
fn codec_byte(codec: VideoCodec) -> u8 {
    match codec {
        VideoCodec::H264 => 1,
        VideoCodec::H265 => 2,
    }
}
fn read_codec(reader: &mut impl Read) -> io::Result<VideoCodec> {
    match read_u8(reader)? {
        1 => Ok(VideoCodec::H264),
        2 => Ok(VideoCodec::H265),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid desktop-helper codec {value}"),
        )),
    }
}
fn pixel_format_byte(pixel_format: VideoPixelFormat) -> u8 {
    match pixel_format {
        VideoPixelFormat::Yuv420 => 1,
        VideoPixelFormat::Yuv444 => 2,
    }
}
fn read_pixel_format(reader: &mut impl Read) -> io::Result<VideoPixelFormat> {
    match read_u8(reader)? {
        1 => Ok(VideoPixelFormat::Yuv420),
        2 => Ok(VideoPixelFormat::Yuv444),
        value => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid desktop-helper pixel format {value}"),
        )),
    }
}
fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut value = [0; 4];
    reader.read_exact(&mut value)?;
    Ok(u32::from_le_bytes(value))
}
fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut value = [0; 8];
    reader.read_exact(&mut value)?;
    Ok(u64::from_le_bytes(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::PointerButton;

    #[test]
    fn rdp_monitor_ids_are_stable_and_do_not_alias_console_or_other_users() {
        assert_ne!(rdp_display_id(3, DisplayId(1)), DisplayId(1));
        assert_ne!(
            rdp_display_id(3, DisplayId(1)),
            rdp_display_id(4, DisplayId(1))
        );
        assert_ne!(
            rdp_display_id(3, DisplayId(1)),
            rdp_display_id(3, DisplayId(2))
        );
        assert_ne!(
            rdp_display_id(3, DisplayId(u32::MAX - 1)),
            meshrmm_protocol::BACKGROUND_DISPLAY_ID
        );
        assert_eq!(
            DesktopTarget::Rdp(3, false).alternate(),
            DesktopTarget::Rdp(3, true)
        );
        assert!(helper_uses_user_token(
            HelperKind::Files,
            DesktopTarget::Rdp(3, true)
        ));
        assert!(helper_uses_user_token(
            HelperKind::Clipboard,
            DesktopTarget::Rdp(3, false)
        ));
        assert!(!helper_uses_user_token(
            HelperKind::Clipboard,
            DesktopTarget::Rdp(3, true)
        ));
    }

    #[test]
    fn rdp_catalog_preserves_all_monitors_and_maps_the_active_monitor() {
        let session = meshrmm_protocol::DesktopSession::Rdp {
            id: 3,
            user: "Alice".into(),
        };
        let console = Display {
            session: meshrmm_protocol::DesktopSession::Console,
            id: DisplayId(1),
            name: "Console".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let mut streamer =
            DesktopCaptureStreamer::new(String::new(), String::new(), PathBuf::new());
        streamer.console_displays = vec![console.clone()];
        streamer.selected_session = Some(3);
        streamer.session_displays = vec![(
            Display {
                session: session.clone(),
                id: rdp_display_id(3, console.id),
                ..console.clone()
            },
            console.id,
        )];
        let monitors: Vec<_> = (1..=3)
            .map(|id| Display {
                id: DisplayId(id),
                x: (id as i32 - 1) * 1920,
                ..console.clone()
            })
            .collect();
        let started = streamer.with_background_display(StartedDesktop {
            active_display: monitors[1].clone(),
            displays: monitors,
            format: ActiveFormat {
                width: 1920,
                height: 1080,
                frames_per_second: 30,
                bitrate_bits_per_second: 8_000_000,
                codec: VideoCodec::H264,
                pixel_format: VideoPixelFormat::Yuv420,
            },
        });
        assert_eq!(started.active_display.id, rdp_display_id(3, DisplayId(2)));
        assert_eq!(started.active_display.session, session);
        assert_eq!(
            started
                .active_display
                .session_displays(&started.displays)
                .len(),
            3
        );
        assert_eq!(started.displays.len(), 5);
        let input = streamer.input_controller();
        assert!(!input.is_console_session());
        *streamer.cursor.lock().unwrap() = (CursorShape::Default, false, Some(DisplayId(3)));
        assert_eq!(
            input.agent_pointer_display(),
            Some(rdp_display_id(3, DisplayId(3)))
        );
        streamer.selected_session = None;
        let console_started = streamer.with_background_display(StartedDesktop {
            active_display: console.clone(),
            displays: vec![console],
            format: started.format,
        });
        assert_eq!(
            console_started.active_display.session,
            meshrmm_protocol::DesktopSession::Console
        );
        assert!(streamer.display_routes.lock().unwrap().is_empty());
        assert!(input.is_console_session());
    }

    #[test]
    fn background_start_preserves_all_console_displays_when_switching() {
        let console: Vec<_> = (1..=3)
            .map(|id| Display {
                session: meshrmm_protocol::DesktopSession::Console,
                id: DisplayId(id),
                name: format!("Display {id}"),
                x: (id as i32 - 1) * 1920,
                y: 0,
                width: 1920,
                height: 1080,
                primary: id == 1,
            })
            .collect();
        let mut streamer =
            DesktopCaptureStreamer::new(String::new(), String::new(), PathBuf::new());
        streamer.console_displays = console.clone();
        let format = ActiveFormat {
            width: 1920,
            height: 1080,
            frames_per_second: 20,
            bitrate_bits_per_second: 12_000_000,
            codec: VideoCodec::H264,
            pixel_format: VideoPixelFormat::Yuv420,
        };
        for background in [true, false, true] {
            streamer
                .background_active
                .store(background, Ordering::Release);
            let active = if background {
                background_display()
            } else {
                console[2].clone()
            };
            let started = streamer.with_background_display(StartedDesktop {
                format,
                displays: if background {
                    vec![active.clone()]
                } else {
                    console.clone()
                },
                active_display: active.clone(),
            });
            assert_eq!(started.displays.len(), 4);
            assert_eq!(started.active_display.id, active.id);
            for (actual, expected) in started.displays.iter().zip(&console) {
                assert_eq!(actual.id, expected.id);
                assert_eq!(actual.x, expected.x);
            }
            assert_eq!(started.displays[3].id, background_display().id);
        }
        let mut bytes = Vec::new();
        write_command(&mut bytes, &ParentCommand::EnumerateDisplays).unwrap();
        assert!(matches!(
            read_command(bytes.as_slice()).unwrap(),
            ParentCommand::EnumerateDisplays
        ));
    }

    #[test]
    fn capture_reader_accepts_reconfiguration_and_discards_frames_while_unrouted() {
        let display = Display {
            session: meshrmm_protocol::DesktopSession::Console,
            id: DisplayId(1),
            name: "Display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let mut bytes = Vec::new();
        for id in [1, 2] {
            let selected = Display {
                session: meshrmm_protocol::DesktopSession::Console,
                id: DisplayId(id),
                ..display.clone()
            };
            write_event(
                &mut bytes,
                &ChildEvent::Started(StartedDesktop {
                    format: ActiveFormat {
                        width: 1920,
                        height: 1080,
                        frames_per_second: 60,
                        bitrate_bits_per_second: 12_000_000,
                        codec: VideoCodec::H264,
                        pixel_format: VideoPixelFormat::Yuv420,
                    },
                    displays: vec![selected.clone()],
                    active_display: selected,
                }),
            )
            .unwrap();
            write_event(
                &mut bytes,
                &ChildEvent::Frame(EncodedAccessUnit {
                    capture_timestamp_us: id as u64,
                    encode_complete_timestamp_us: id as u64,
                    keyframe: true,
                    codec_config: None,
                    data: vec![id as u8],
                }),
            )
            .unwrap();
        }
        write_event(&mut bytes, &ChildEvent::Stopped).unwrap();
        for enabled in [false, true] {
            let frames = Arc::new(Mutex::new(Vec::new()));
            let received = Arc::clone(&frames);
            let sink: EncodedFrameSink = Arc::new(move |frame| {
                received.lock().unwrap().push(frame.data);
            });
            let (started_tx, started_rx) = mpsc::channel();
            let status = Arc::new(Mutex::new(None));
            dispatch_child_events(
                bytes.as_slice(),
                Arc::new(Mutex::new(enabled.then_some(sink))),
                started_tx,
                Arc::clone(&status),
                Arc::new(Mutex::new((CursorShape::Default, false, None))),
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(Instant::now())),
            );
            assert_eq!(
                started_rx.recv().unwrap().unwrap().active_display.id,
                DisplayId(1)
            );
            assert_eq!(
                started_rx.recv().unwrap().unwrap().active_display.id,
                DisplayId(2)
            );
            assert_eq!(*status.lock().unwrap(), Some(Ok(())));
            assert_eq!(
                *frames.lock().unwrap(),
                if enabled {
                    vec![vec![1], vec![2]]
                } else {
                    vec![]
                }
            );
        }
    }

    #[test]
    fn maintenance_ipc_preserves_flags_and_unicode_and_rejects_bad_flags() {
        for blocked in [false, true] {
            let mut bytes = Vec::new();
            write_command(&mut bytes, &ParentCommand::BlockInput(blocked)).unwrap();
            assert!(
                matches!(read_command(bytes.as_slice()).unwrap(), ParentCommand::BlockInput(value) if value == blocked)
            );
        }
        assert!(read_command([COMMAND_BLOCK_INPUT, 2].as_slice()).is_err());
        assert!(read_command([COMMAND_BLACKOUT, 2].as_slice()).is_err());
        let text = "Maintenance by Zoë 王\nPlease wait";
        let mut bytes = Vec::new();
        write_command(
            &mut bytes,
            &ParentCommand::Blackout {
                enabled: true,
                text: text.into(),
            },
        )
        .unwrap();
        assert!(
            matches!(read_command(bytes.as_slice()).unwrap(), ParentCommand::Blackout { enabled: true, text: value } if value == text)
        );
        let mut bytes = Vec::new();
        write_event(
            &mut bytes,
            &ChildEvent::MaintenanceState {
                agent_input_blocked: true,
                blacked_out: false,
            },
        )
        .unwrap();
        assert!(matches!(
            read_event(bytes.as_slice()).unwrap(),
            ChildEvent::MaintenanceState {
                agent_input_blocked: true,
                blacked_out: false
            }
        ));
        let mut bytes = Vec::new();
        write_event(
            &mut bytes,
            &ChildEvent::MaintenanceError("access denied".into()),
        )
        .unwrap();
        assert!(
            matches!(read_event(bytes.as_slice()).unwrap(), ChildEvent::MaintenanceError(reason) if reason == "access denied")
        );
    }

    #[test]
    fn wallpaper_commands_preserve_pipe_alignment_and_reject_invalid_flags() {
        for hidden in [false, true] {
            let mut bytes = Vec::new();
            write_command(&mut bytes, &ParentCommand::SetWallpaperHidden(hidden)).unwrap();
            write_command(&mut bytes, &ParentCommand::Stop).unwrap();
            let mut reader = bytes.as_slice();
            assert!(
                matches!(read_command(&mut reader).unwrap(), ParentCommand::SetWallpaperHidden(value) if value == hidden)
            );
            assert!(matches!(
                read_command(&mut reader).unwrap(),
                ParentCommand::Stop
            ));
            assert!(reader.is_empty());
        }
        assert!(read_command([19, 2].as_slice()).is_err());
    }

    #[test]
    fn cursor_ownership_and_visibility_round_trip_without_desynchronizing_ipc() {
        for viewer in [false, true] {
            let mut bytes = Vec::new();
            write_event(
                &mut bytes,
                &ChildEvent::Cursor(CursorShape::Text, viewer, Some(DisplayId(2))),
            )
            .unwrap();
            write_event(&mut bytes, &ChildEvent::Stopped).unwrap();
            let mut reader = bytes.as_slice();
            assert!(
                matches!(read_event(&mut reader).unwrap(), ChildEvent::Cursor(CursorShape::Text, owner, Some(DisplayId(2))) if owner == viewer)
            );
            assert!(matches!(
                read_event(&mut reader).unwrap(),
                ChildEvent::Stopped
            ));
            assert!(reader.is_empty());

            let mut bytes = Vec::new();
            write_command(&mut bytes, &ParentCommand::SetCursorCapture(viewer)).unwrap();
            write_command(&mut bytes, &ParentCommand::RequestKeyframe).unwrap();
            let mut reader = bytes.as_slice();
            assert!(
                matches!(read_command(&mut reader).unwrap(), ParentCommand::SetCursorCapture(enabled) if enabled == viewer)
            );
            assert!(matches!(
                read_command(&mut reader).unwrap(),
                ParentCommand::RequestKeyframe
            ));
            assert!(reader.is_empty());
        }
        assert!(read_command([18, 2].as_slice()).is_err());
        assert!(read_event([EVENT_CURSOR, 2].as_slice()).is_err());
    }

    #[test]
    fn capture_flags_survive_helper_start_and_leave_next_command_aligned() {
        for (cursor, monochrome) in [(false, false), (false, true), (true, false), (true, true)] {
            let mut bytes = Vec::new();
            write_command(
                &mut bytes,
                &ParentCommand::Start {
                    viewer_name: "Viewer".into(),
                    display_id: Some(DisplayId(1)),
                    frames_per_second: 24,
                    bitrate_bits_per_second: 1_000_000,
                    codec: VideoCodec::H264,
                    pixel_format: VideoPixelFormat::Yuv420,
                    capture_cursor: cursor,
                    grayscale: monochrome,
                },
            )
            .unwrap();
            write_command(&mut bytes, &ParentCommand::RequestKeyframe).unwrap();
            let mut reader = bytes.as_slice();
            assert!(
                matches!(read_command(&mut reader).unwrap(), ParentCommand::Start {
                    capture_cursor, grayscale, frames_per_second: 24, bitrate_bits_per_second: 1_000_000, ..
                } if capture_cursor == cursor && grayscale == monochrome)
            );
            assert!(matches!(
                read_command(&mut reader).unwrap(),
                ParentCommand::RequestKeyframe
            ));
            assert!(reader.is_empty());
        }
    }

    #[test]
    fn command_protocol_round_trips_desktop_input() {
        let commands = [
            ParentCommand::Start {
                viewer_name: "Zoë 王".into(),
                display_id: Some(DisplayId(3)),
                frames_per_second: 60,
                bitrate_bits_per_second: 12_000_000,
                codec: VideoCodec::H265,
                pixel_format: VideoPixelFormat::Yuv444,
                capture_cursor: true,
                grayscale: false,
            },
            ParentCommand::StartClipboard,
            ParentCommand::StartChatHelper {
                viewer_name: "Zoë 王".into(),
            },
            ParentCommand::RequestKeyframe,
            ParentCommand::SetBitrate(4_000_000),
            ParentCommand::StartInput {
                viewer_name: "Zoë 王".into(),
                display_id: DisplayId(3),
            },
            ParentCommand::Input(RemoteInput::PointerButton {
                display_id: DisplayId(3),
                button: PointerButton::Left,
                pressed: true,
            }),
            ParentCommand::ReleaseInput,
            ParentCommand::BlockInput(true),
            ParentCommand::BlockInput(false),
            ParentCommand::Clipboard("winget install Example.Package\n".into()),
            ParentCommand::Stop,
        ];
        for command in commands {
            let mut bytes = Vec::new();
            write_command(&mut bytes, &command).unwrap();
            let decoded = read_command(bytes.as_slice()).unwrap();
            assert_eq!(command_name(&decoded), command_name(&command));
            if let ParentCommand::Start { viewer_name, .. }
            | ParentCommand::StartInput { viewer_name, .. } = decoded
            {
                assert_eq!(viewer_name, "Zoë 王");
            }
        }
    }

    #[test]
    fn started_event_round_trips_display_metadata() {
        let display = Display {
            session: meshrmm_protocol::DesktopSession::Console,
            id: DisplayId(2),
            name: "Secure display".into(),
            x: -1920,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        let event = ChildEvent::Started(StartedDesktop {
            format: ActiveFormat {
                width: 1920,
                height: 1080,
                frames_per_second: 60,
                bitrate_bits_per_second: 12_000_000,
                codec: VideoCodec::H265,
                pixel_format: VideoPixelFormat::Yuv444,
            },
            displays: vec![display.clone()],
            active_display: display,
        });
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).unwrap();
        let ChildEvent::Started(decoded) = read_event(bytes.as_slice()).unwrap() else {
            panic!("expected started event");
        };
        assert_eq!(decoded.active_display.id, DisplayId(2));
        assert_eq!(decoded.displays[0].x, -1920);
        assert_eq!(decoded.format.codec, VideoCodec::H265);
        assert_eq!(decoded.format.pixel_format, VideoPixelFormat::Yuv444);
    }

    #[test]
    fn frame_event_round_trips() {
        let event = ChildEvent::Frame(EncodedAccessUnit {
            capture_timestamp_us: 11,
            encode_complete_timestamp_us: 22,
            keyframe: true,
            codec_config: Some(vec![1, 2, 3]),
            data: vec![4, 5, 6, 7],
        });
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).unwrap();
        let ChildEvent::Frame(decoded) = read_event(bytes.as_slice()).unwrap() else {
            panic!("expected frame event");
        };
        assert_eq!(decoded.capture_timestamp_us, 11);
        assert_eq!(decoded.codec_config, Some(vec![1, 2, 3]));
        assert_eq!(decoded.data, vec![4, 5, 6, 7]);
    }

    #[test]
    fn input_started_event_round_trips() {
        let mut bytes = Vec::new();
        write_event(&mut bytes, &ChildEvent::InputStarted).unwrap();
        assert!(matches!(
            read_event(bytes.as_slice()).unwrap(),
            ChildEvent::InputStarted
        ));
    }

    #[test]
    fn clipboard_commands_and_events_round_trip_all_formats() {
        for content in [
            ClipboardContent::from("winget install Example.Package\n"),
            ClipboardContent::Html {
                html: "<b>Zoë 王</b>".repeat(10000),
                text: "Zoë 王".into(),
            },
            ClipboardContent::Image {
                width: 512,
                height: 512,
                rgba: vec![255; 512 * 512 * 4],
            },
        ] {
            let mut bytes = Vec::new();
            write_event(&mut bytes, &ChildEvent::Clipboard(content.clone())).unwrap();
            let ChildEvent::Clipboard(decoded) = read_event(bytes.as_slice()).unwrap() else {
                panic!("expected clipboard event");
            };
            assert_eq!(decoded, content);
            let mut bytes = Vec::new();
            write_command(&mut bytes, &ParentCommand::Clipboard(content.clone())).unwrap();
            let ParentCommand::Clipboard(decoded) = read_command(bytes.as_slice()).unwrap() else {
                panic!("expected clipboard command");
            };
            assert_eq!(decoded, content);
        }
    }

    #[test]
    fn desktop_targets_alternate() {
        assert_eq!(DesktopTarget::Default.alternate(), DesktopTarget::Winlogon);
        assert_eq!(DesktopTarget::Winlogon.alternate(), DesktopTarget::Default);
    }

    fn command_name(command: &ParentCommand) -> u8 {
        match command {
            ParentCommand::EnumerateDisplays => COMMAND_ENUMERATE_DISPLAYS,
            ParentCommand::Start { .. } => COMMAND_START,
            ParentCommand::SetWallpaperHidden(_) => 19,
            ParentCommand::SetPreventIdleLock(_) => 21,
            ParentCommand::SetCursorCapture(_) => 18,
            ParentCommand::SetDisplayBorder(_) => 20,
            ParentCommand::RequestKeyframe => COMMAND_REQUEST_KEYFRAME,
            ParentCommand::SetBitrate(_) => COMMAND_SET_BITRATE,
            ParentCommand::StartInput { .. } => COMMAND_START_INPUT,
            ParentCommand::Input(_) => COMMAND_INPUT,
            ParentCommand::ReleaseInput => COMMAND_RELEASE_INPUT,
            ParentCommand::BlockInput(_) => COMMAND_BLOCK_INPUT,
            ParentCommand::Blackout { .. } => COMMAND_BLACKOUT,
            ParentCommand::Clipboard(_) => COMMAND_CLIPBOARD,
            ParentCommand::StartFiles => 13,
            ParentCommand::StartClipboard => 16,
            ParentCommand::StartChatHelper { .. } => 17,
            ParentCommand::PromptCredentials => 23,
            ParentCommand::AutofillCredentials(_) => 24,
            ParentCommand::Files(_) => 12,
            ParentCommand::Chat(_) => COMMAND_CHAT,
            ParentCommand::StartChat => COMMAND_START_CHAT,
            ParentCommand::StopChat => COMMAND_STOP_CHAT,
            ParentCommand::Stop => COMMAND_STOP,
        }
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;
    #[test]
    fn chat_commands_and_events_round_trip() {
        let text = "Hello 👋\nReply from the other computer";
        let mut bytes = Vec::new();
        write_command(&mut bytes, &ParentCommand::StartChat).unwrap();
        assert!(matches!(
            read_command(bytes.as_slice()).unwrap(),
            ParentCommand::StartChat
        ));
        bytes.clear();
        write_command(&mut bytes, &ParentCommand::Chat(text.into())).unwrap();
        assert!(
            matches!(read_command(bytes.as_slice()).unwrap(), ParentCommand::Chat(value) if value == text)
        );
        bytes.clear();
        write_event(&mut bytes, &ChildEvent::Chat(text.into())).unwrap();
        assert!(
            matches!(read_event(bytes.as_slice()).unwrap(), ChildEvent::Chat(value) if value == text)
        );
    }
    #[test]
    fn oversized_chat_is_rejected_before_reading_payload() {
        let mut command = vec![COMMAND_CHAT];
        command
            .extend_from_slice(&(meshrmm_protocol::MAX_CHAT_TEXT_BYTES as u32 + 1).to_le_bytes());
        assert!(
            matches!(read_command(command.as_slice()), Err(e) if e.kind() == io::ErrorKind::InvalidData)
        );
        command[0] = EVENT_CHAT;
        assert!(
            matches!(read_event(command.as_slice()), Err(e) if e.kind() == io::ErrorKind::InvalidData)
        );
    }
}

fn write_file_message(
    mut writer: impl Write,
    message: &meshrmm_protocol::FileMessage,
) -> io::Result<()> {
    let bytes = SessionMessage::FileTransfer(message.clone())
        .encode()
        .map_err(io::Error::other)?;
    checked_len(bytes.len(), MAX_CONTROL_BYTES, "file transfer")?;
    write_u32(&mut writer, bytes.len() as u32)?;
    writer.write_all(&bytes)
}
fn read_file_message(mut reader: impl Read) -> io::Result<meshrmm_protocol::FileMessage> {
    let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "file transfer")?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    match SessionMessage::decode(&bytes).map_err(io::Error::other)? {
        SessionMessage::FileTransfer(message) => Ok(message),
        _ => Err(io::Error::other("invalid file transfer packet")),
    }
}

fn run_file_child(commands: mpsc::Receiver<io::Result<ParentCommand>>) -> anyhow::Result<()> {
    // This helper runs as the interactive user and survives capture/desktop switches.
    // Keep the guard until Stop, pipe EOF, or an error unwinds this session.
    let _drag_windows = super::drag_windows::OutlineDragging::new()
        .inspect_err(|error| {
            tracing::warn!(%error, "could not disable window contents while dragging");
        })
        .ok();
    let mut wallpaper = None;
    meshrmm_file_transfer::windows::set_displays(enumerate_displays()?);
    meshrmm_file_transfer::TransferSession::run_on_current_thread(move |files| {
        let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
        emit_child_event(&output, ChildEvent::InputStarted)?;
        let mut commands = async_helper_commands(commands)?;
        let ready = files.outgoing_ready();
        tokio::runtime::Builder::new_current_thread()
            .build()?
            .block_on(async {
                loop {
                    tokio::select! {
                        command = commands.recv() => match command {
                            Some(Ok(ParentCommand::SetWallpaperHidden(hidden))) => {
                                if let Err(error) = super::wallpaper::set_hidden(&mut wallpaper, hidden) {
                                    tracing::warn!(%error, "wallpaper update failed");
                                    emit_child_event(&output, ChildEvent::MaintenanceError(format!("Wallpaper: {error:#}")))?;
                                }
                            }
                            Some(Ok(ParentCommand::Files(message))) => files.receive(message),
                            Some(Ok(ParentCommand::Stop)) | None => break,
                            _ => anyhow::bail!("file helper received an unexpected command"),
                        },
                        _ = ready.notified() => {
                            while let Some(message) = files.poll() {
                                emit_child_event(&output, ChildEvent::Files(message))?;
                            }
                        }
                    }
                }
                Ok::<(), anyhow::Error>(())
            })?;
        emit_child_event(&output, ChildEvent::Stopped)?;
        Ok(())
    })
}

fn run_clipboard_child(commands: mpsc::Receiver<io::Result<ParentCommand>>) -> anyhow::Result<()> {
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let mut clipboard = super::clipboard::ClipboardSync::new(false)?;
    emit_child_event(&output, ChildEvent::InputStarted)?;
    loop {
        match commands.recv_timeout(CLIPBOARD_POLL_INTERVAL) {
            Ok(Ok(ParentCommand::Clipboard(content))) => {
                if let Err(error) = clipboard.apply(content) {
                    tracing::warn!(%error, "clipboard apply failed");
                }
            }
            Ok(Ok(ParentCommand::Stop)) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            _ => anyhow::bail!("clipboard helper received an unexpected command"),
        }
        match clipboard.poll() {
            Ok(Some(content)) => emit_child_event(&output, ChildEvent::Clipboard(content))?,
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "clipboard poll failed"),
        }
    }
    emit_child_event(&output, ChildEvent::Stopped)?;
    Ok(())
}

#[cfg(test)]
mod isolation_tests {
    use super::*;
    #[test]
    fn desktop_switch_discards_unrouted_input_and_resumes_without_replay() {
        let streamer = DesktopCaptureStreamer::default();
        let controller = streamer.input_controller();
        let event = |pressed| RemoteInput::PointerButton {
            display_id: DisplayId(1),
            button: meshrmm_protocol::PointerButton::Left,
            pressed,
        };
        // A release arriving between helpers must not disconnect the viewer.
        controller.apply(event(false)).unwrap();
        controller.release_all().unwrap();

        let (reader, writer) = create_pipe().unwrap();
        *streamer.input_route.lock().unwrap() =
            Some(Arc::new(CommandWriter::new(writer.into_file()).unwrap()));
        controller.apply(event(true)).unwrap();
        controller.release_all().unwrap();
        let mut reader = reader.into_file();
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::Input(RemoteInput::PointerButton { pressed: true, .. })
        ));
        assert!(matches!(
            read_command(&mut reader).unwrap(),
            ParentCommand::ReleaseInput
        ));
    }

    #[test]
    fn interactive_clipboard_uses_user_token_but_secure_input_does_not() {
        assert!(helper_uses_user_token(
            HelperKind::Clipboard,
            DesktopTarget::Default
        ));
        assert!(!helper_uses_user_token(
            HelperKind::Clipboard,
            DesktopTarget::Winlogon
        ));
        assert!(!helper_uses_user_token(
            HelperKind::Input,
            DesktopTarget::Default
        ));
        assert!(!helper_uses_user_token(
            HelperKind::Chat,
            DesktopTarget::Default
        ));
        assert!(helper_uses_user_token(
            HelperKind::Files,
            DesktopTarget::Default
        ));
    }

    #[test]
    fn helper_pipes_start_non_inheritable() {
        let (read, write) = create_pipe().unwrap();
        for handle in [&read, &write] {
            let mut flags = 0;
            unsafe { windows::Win32::Foundation::GetHandleInformation(handle.0, &mut flags) }
                .unwrap();
            assert_eq!(flags & HANDLE_FLAG_INHERIT.0, 0);
        }
        let handles = [read.0, write.0];
        // The attribute list accepts the pipe ends once a launch marks them inheritable.
        for handle in handles {
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
                .unwrap();
        }
        HandleListAttribute::new(&handles).unwrap();
    }

    #[test]
    fn stalled_clipboard_pipe_does_not_block_input_pipe() {
        let (blocked_read, blocked_write) = create_pipe().unwrap();
        let clipboard = CommandWriter::new(blocked_write.into_file()).unwrap();
        // Far larger than the anonymous pipe buffer; its writer must wait until
        // the reader drains/closes it. The caller only enqueues these bytes.
        clipboard.send(vec![0; 1024 * 1024]).unwrap();
        let (input_read, input_write) = create_pipe().unwrap();
        let input = Arc::new(CommandWriter::new(input_write.into_file()).unwrap());
        let (received, result) = mpsc::channel();
        let reader = thread::spawn(move || {
            let command = read_command(input_read.into_file()).unwrap();
            received
                .send(matches!(command, ParentCommand::ReleaseInput))
                .unwrap();
        });
        send_command(&input, &ParentCommand::ReleaseInput).unwrap();
        assert!(result.recv_timeout(Duration::from_secs(2)).unwrap());
        drop(blocked_read); // Unblock and close the clipboard writer too.
        reader.join().unwrap();
    }

    #[test]
    fn pipe_byte_budget_rejects_oversized_work_without_waiting() {
        let writer = CommandWriter::new(io::sink()).unwrap();
        assert!(
            writer
                .send(vec![
                    0;
                    2 * MAX_CLIPBOARD_WIRE_BYTES + MAX_CONTROL_BYTES + 1
                ])
                .is_err()
        );
        writer.send(vec![COMMAND_RELEASE_INPUT]).unwrap();
    }
}

fn run_chat_child(
    commands: mpsc::Receiver<io::Result<ParentCommand>>,
    viewer_name: String,
) -> anyhow::Result<()> {
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let chat = meshrmm_chat::ChatSession::with_peer("Viewer");
    let _indicator = super::indicator::SessionIndicator::show(&viewer_name, chat.clone())?;
    emit_child_event(&output, ChildEvent::InputStarted)?;
    let mut commands = async_helper_commands(commands)?;
    let ready = chat.outgoing_ready();
    let prompt_open = Arc::new(AtomicBool::new(false));
    tokio::runtime::Builder::new_current_thread()
        .build()?
        .block_on(async {
            loop {
                tokio::select! {
                    command = commands.recv() => match command {
                        Some(Ok(ParentCommand::PromptCredentials)) => {
                            if !prompt_open.swap(true, Ordering::AcqRel) {
                                let output = output.clone();
                                let prompt_open = prompt_open.clone();
                                thread::Builder::new().name("meshrmm-credential-prompt".into()).spawn(move || {
                                    let result = match super::credentials::prompt() {
                                        Ok(Some((encrypted, username))) => CredentialResult { encrypted: Some(encrypted), message: format!("Validated {username}; saved on this computer until forgotten") },
                                        Ok(None) => CredentialResult { encrypted: None, message: "Credential request cancelled".into() },
                                        Err(error) => CredentialResult { encrypted: None, message: format!("{error:#}") },
                                    };
                                    let _ = emit_child_event(&output, ChildEvent::Credentials(result));
                                    prompt_open.store(false, Ordering::Release);
                                })?;
                            }
                        }
                        Some(Ok(ParentCommand::StartChat)) => chat.set_available(true),
                        Some(Ok(ParentCommand::StopChat)) => chat.set_available(false),
                        Some(Ok(ParentCommand::Chat(text))) => {
                            if chat.available() { chat.receive(text); }
                        }
                        Some(Ok(ParentCommand::Stop)) | None => break,
                        _ => anyhow::bail!("chat helper received an unexpected command"),
                    },
                    _ = ready.notified() => {
                        while let Some(text) = chat.poll() {
                            emit_child_event(&output, ChildEvent::Chat(text))?;
                        }
                    }
                }
            }
            Ok::<(), anyhow::Error>(())
        })?;
    emit_child_event(&output, ChildEvent::Stopped)?;
    Ok(())
}

// Adapt the bounded native pipe reader without periodic wakeups. The bridge
// exits on Stop/EOF; blocking pipe work stays off the async service worker.
fn async_helper_commands(
    commands: mpsc::Receiver<io::Result<ParentCommand>>,
) -> io::Result<tokio::sync::mpsc::Receiver<io::Result<ParentCommand>>> {
    let (sender, receiver) = tokio::sync::mpsc::channel(64);
    thread::Builder::new()
        .name("meshrmm-service-commands".into())
        .spawn(move || {
            while let Ok(command) = commands.recv() {
                let stop = matches!(&command, Ok(ParentCommand::Stop) | Err(_));
                if sender.blocking_send(command).is_err() || stop {
                    break;
                }
            }
        })?;
    Ok(receiver)
}

#[cfg(test)]
mod service_command_tests {
    use super::*;

    #[tokio::test]
    async fn command_bridge_preserves_order_and_closes_after_stop() {
        let (sender, commands) = mpsc::sync_channel(64);
        let mut receiver = async_helper_commands(commands).unwrap();
        sender.send(Ok(ParentCommand::StartChat)).unwrap();
        sender
            .send(Ok(ParentCommand::Chat("hello".into())))
            .unwrap();
        sender.send(Ok(ParentCommand::Stop)).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            assert!(matches!(receiver.recv().await, Some(Ok(ParentCommand::StartChat))));
            assert!(matches!(receiver.recv().await, Some(Ok(ParentCommand::Chat(text))) if text == "hello"));
            assert!(matches!(receiver.recv().await, Some(Ok(ParentCommand::Stop))));
            assert!(receiver.recv().await.is_none());
        }).await.unwrap();
    }

    #[tokio::test]
    async fn command_bridge_wakes_on_pipe_disconnect() {
        let (sender, commands) = mpsc::sync_channel(64);
        let mut receiver = async_helper_commands(commands).unwrap();
        drop(sender);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod service_event_tests {
    use super::*;

    struct Dispatched {
        startup: Result<(), String>,
        status: Option<Result<(), String>>,
        cursor: HelperCursor,
        clipboard: HelperClipboard,
        files: HelperFiles,
        chat: HelperChat,
        maintenance: HelperMaintenance,
        credentials: HelperCredentials,
    }

    fn dispatch(kind: HelperKind, events: &[ChildEvent]) -> Dispatched {
        let (read, write) = create_pipe().unwrap();
        let (started, startup) = mpsc::sync_channel(1);
        let status: HelperStatus = Arc::new(Mutex::new(None));
        let cursor: HelperCursor = Arc::new(Mutex::new((CursorShape::Default, false, None)));
        let clipboard = Arc::new(ClipboardEvents::default());
        let files = Arc::new(FileEvents::default());
        let chat = Arc::new(ChatEvents::default());
        let maintenance: HelperMaintenance = Arc::new(Mutex::new(None));
        let credentials = Arc::new(Mutex::new(Credentials::default()));
        let reader = {
            let (status, cursor, clipboard, files, chat, maintenance, credentials) = (
                status.clone(),
                cursor.clone(),
                clipboard.clone(),
                files.clone(),
                chat.clone(),
                maintenance.clone(),
                credentials.clone(),
            );
            thread::spawn(move || {
                dispatch_input_events(
                    read.into_file(),
                    started,
                    status,
                    cursor,
                    clipboard,
                    files,
                    chat,
                    maintenance,
                    credentials,
                    kind,
                )
            })
        };
        let mut writer = write.into_file();
        for event in events {
            // The reader may already have stopped after a rejected event.
            if write_event(&mut writer, event).is_err() {
                break;
            }
        }
        drop(writer);
        reader.join().unwrap();
        let status = status.lock().unwrap().take();
        Dispatched {
            startup: startup.recv().unwrap(),
            status,
            cursor,
            clipboard,
            files,
            chat,
            maintenance,
            credentials,
        }
    }

    #[tokio::test]
    async fn helper_pipe_notifies_services_and_coalesces_clipboard_changes() {
        let clipboard = dispatch(
            HelperKind::Clipboard,
            &[
                ChildEvent::InputStarted,
                ChildEvent::Clipboard(ClipboardContent::Text("first".into())),
                ChildEvent::Clipboard(ClipboardContent::Text("latest".into())),
                ChildEvent::Stopped,
            ],
        );
        let chat = dispatch(
            HelperKind::Chat,
            &[
                ChildEvent::InputStarted,
                ChildEvent::Chat("hello".into()),
                ChildEvent::Stopped,
            ],
        );
        let files = dispatch(
            HelperKind::Files,
            &[
                ChildEvent::InputStarted,
                ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
                ChildEvent::Stopped,
            ],
        );
        for helper in [&clipboard, &chat, &files] {
            assert_eq!(helper.startup, Ok(()));
            assert_eq!(helper.status, Some(Ok(())));
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            clipboard.clipboard.ready.notified().await;
            files.files.ready.notified().await;
            chat.chat.ready.notified().await;
        })
        .await
        .unwrap();
        assert_eq!(
            clipboard.clipboard.latest.lock().unwrap().take(),
            Some(ClipboardContent::Text("latest".into()))
        );
        assert_eq!(
            chat.chat.queue.lock().unwrap().pop_front().as_deref(),
            Some("hello")
        );
        assert!(matches!(
            files.files.queue.lock().unwrap().pop_front(),
            Some(meshrmm_protocol::FileMessage::Available)
        ));
        assert!(clipboard.clipboard.latest.lock().unwrap().is_none());
    }

    #[test]
    fn helpers_are_trusted_only_for_events_their_run_loops_send() {
        use HelperKind::*;
        let events = [
            ChildEvent::InputStarted,
            ChildEvent::Error("failed".into()),
            ChildEvent::Stopped,
            ChildEvent::Cursor(CursorShape::Default, true, None),
            ChildEvent::MaintenanceState {
                agent_input_blocked: false,
                blacked_out: false,
            },
            ChildEvent::CredentialPrompt(true),
            ChildEvent::MaintenanceError("wallpaper".into()),
            ChildEvent::Credentials(CredentialResult {
                encrypted: None,
                message: "done".into(),
            }),
            ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
            ChildEvent::Clipboard(ClipboardContent::Text("copied".into())),
            ChildEvent::Chat("hello".into()),
        ];
        let expected: [&[HelperKind]; 11] = [
            &[Input, Files, Clipboard, Chat],
            &[Input, Files, Clipboard, Chat],
            &[Input, Files, Clipboard, Chat],
            &[Input],
            &[Input],
            &[Input],
            &[Input, Files],
            &[Input, Chat],
            &[Files],
            &[Clipboard],
            &[Chat],
        ];
        for (event, senders) in events.iter().zip(expected) {
            for kind in [Input, Files, Clipboard, Chat] {
                assert_eq!(
                    helper_sends(kind, event),
                    senders.contains(&kind),
                    "{kind:?} helper, {} event",
                    child_event_name(event)
                );
            }
        }
    }

    #[test]
    fn user_token_helpers_cannot_forge_other_helpers_events() {
        let forged = [
            (
                HelperKind::Clipboard,
                ChildEvent::Chat("forged chat".into()),
            ),
            (
                HelperKind::Clipboard,
                ChildEvent::MaintenanceError("forged".into()),
            ),
            (
                HelperKind::Files,
                ChildEvent::Cursor(CursorShape::Text, true, None),
            ),
            (
                HelperKind::Files,
                ChildEvent::MaintenanceState {
                    agent_input_blocked: true,
                    blacked_out: true,
                },
            ),
            (
                HelperKind::Files,
                ChildEvent::Clipboard(ClipboardContent::Text("forged".into())),
            ),
            (HelperKind::Files, ChildEvent::CredentialPrompt(true)),
            (
                HelperKind::Clipboard,
                ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
            ),
            (
                HelperKind::Chat,
                ChildEvent::Clipboard(ClipboardContent::Text("forged".into())),
            ),
            (HelperKind::Input, ChildEvent::Chat("forged chat".into())),
        ];
        for (kind, event) in forged {
            let name = child_event_name(&event);
            let result = dispatch(
                kind,
                &[
                    ChildEvent::InputStarted,
                    event,
                    ChildEvent::Chat("after".into()),
                    ChildEvent::Stopped,
                ],
            );
            assert_eq!(result.startup, Ok(()));
            let Some(Err(message)) = result.status else {
                panic!("{kind:?} helper's {name} event was accepted");
            };
            assert!(message.contains("unexpected"), "{message}");
            assert_eq!(
                *result.cursor.lock().unwrap(),
                (CursorShape::Default, false, None)
            );
            assert!(result.clipboard.latest.lock().unwrap().is_none());
            assert!(result.files.queue.lock().unwrap().is_empty());
            assert!(result.chat.queue.lock().unwrap().is_empty());
            assert!(result.maintenance.lock().unwrap().is_none());
            assert!(!result.credentials.lock().unwrap().state.can_autofill);
        }
    }

    #[test]
    fn stderr_lines_are_bounded_without_losing_the_next_line() {
        let mut input = Vec::new();
        input.extend(std::iter::repeat_n(b'x', 10_000));
        input.extend_from_slice(b"\r\nnext\npartial");
        // A small buffer exercises lines that span several reads.
        let mut reader = BufReader::with_capacity(7, &input[..]);
        let mut line = Vec::new();
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 16).unwrap(),
            Some(true)
        );
        assert_eq!(line, vec![b'x'; 16]);
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 16).unwrap(),
            Some(false)
        );
        assert_eq!(line, b"next");
        assert_eq!(
            read_bounded_line(&mut reader, &mut line, 16).unwrap(),
            Some(false)
        );
        assert_eq!(line, b"partial");
        assert_eq!(read_bounded_line(&mut reader, &mut line, 16).unwrap(), None);
    }

    #[test]
    fn stderr_budget_suppresses_floods_and_reports_them_per_window() {
        let start = Instant::now();
        let window = Duration::from_secs(60);
        let mut budget = LineBudget::new(2, window, start);
        assert_eq!(budget.admit(start), (true, 0));
        assert_eq!(budget.admit(start), (true, 0));
        assert_eq!(budget.admit(start), (false, 0));
        assert_eq!(budget.admit(start + Duration::from_secs(59)), (false, 0));
        assert_eq!(budget.admit(start + window), (true, 2));
        assert_eq!(budget.admit(start + window), (true, 0));
        assert_eq!(budget.admit(start + window), (false, 0));
        assert_eq!(budget.take_suppressed(), 1);
        assert_eq!(budget.take_suppressed(), 0);
    }

    #[test]
    fn forged_event_before_start_fails_startup() {
        let result = dispatch(
            HelperKind::Clipboard,
            &[ChildEvent::Files(meshrmm_protocol::FileMessage::Available)],
        );
        assert!(result.startup.is_err());
        assert!(matches!(result.status, Some(Err(_))));
    }
}

#[cfg(test)]
mod credential_tests {
    use super::*;

    #[test]
    fn credential_ipc_round_trips_and_bounds_payloads() {
        let encrypted = vec![17, 42, 0, 255];
        let mut bytes = Vec::new();
        write_command(
            &mut bytes,
            &ParentCommand::AutofillCredentials(encrypted.clone()),
        )
        .unwrap();
        assert!(
            matches!(read_command(&bytes[..]).unwrap(), ParentCommand::AutofillCredentials(value) if value == encrypted)
        );
        bytes.clear();
        write_event(
            &mut bytes,
            &ChildEvent::Credentials(CredentialResult {
                encrypted: Some(encrypted.clone()),
                message: "Saved".into(),
            }),
        )
        .unwrap();
        assert!(
            matches!(read_event(&bytes[..]).unwrap(), ChildEvent::Credentials(value) if value.encrypted == Some(encrypted))
        );
        assert!(read_command(&[24, 1, 32, 0, 0][..]).is_err());
        assert!(read_event(&[12, 1, 128, 0, 0][..]).is_err());
        assert!(read_event(&[13, 2][..]).is_err());
    }
}
