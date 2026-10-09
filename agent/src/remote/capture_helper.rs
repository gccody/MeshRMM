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
    CursorShape, Display, DisplayId, HeadlessResolution, MAX_CLIPBOARD_WIRE_BYTES, RemoteInput,
    SessionMessage,
};
use windows::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation, WAIT_TIMEOUT};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
    TokenSessionId,
};
use windows::Win32::System::RemoteDesktop::*;
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess,
    OpenProcessToken, PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

use meshrmm_remote_screen::{
    ActiveFormat, EncodedAccessUnit, EncodedFrameSink, StreamConfig, VideoCodec, VideoPixelFormat,
    WindowsDesktopDuplicationStreamer,
};

use super::connection_approval::{ApprovalPrompt, Decision};
use super::connection_notification::ConnectionNotification;
use super::input::WindowsInputController;
use super::platform::ScreenInput;
use super::virtual_display::{HeadlessTarget, VirtualDisplay};
use crate::win32::{HandleListAttribute, OwnedHandle, create_pipe, wide};

mod approval;
mod capture;
mod child;
mod events;
mod input_controller;
mod launch;
mod services;
mod sessions;
mod stderr;
#[cfg(test)]
mod tests;
mod wire;

pub use approval::ApprovalHelper;
pub use child::run_child;
use child::*;
use events::*;
use input_controller::*;
use launch::*;
use services::*;
use sessions::*;
use stderr::*;
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
const EVENT_APPROVAL_DECISION: u8 = 14;
const EVENT_NO_DISPLAYS: u8 = 15;
const COMMAND_PROMPT_CONNECTION_APPROVAL: u8 = 26;
const COMMAND_CAPTURE_THUMBNAIL: u8 = 27;
const COMMAND_ANNOTATE: u8 = 28;
const MAX_CODEC_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_CONTROL_BYTES: usize = 64 * 1024;
const MAX_DISPLAY_NAME_BYTES: usize = 4 * 1024;
const MAX_DISPLAYS: usize = 64;
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a capture helper waits for a newly added virtual display to
/// become active, on top of `START_TIMEOUT`.
const HEADLESS_ARRIVAL: Duration = Duration::from_secs(5);
/// Covers the helper's start, the capture and the JPEG encoding.
const THUMBNAIL_TIMEOUT: Duration = Duration::from_secs(15);
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
    /// Replies with a JPEG of the primary display on the input desktop, then exits.
    CaptureThumbnail,
    StartFiles,
    StartClipboard,
    StartChatHelper {
        viewer_name: String,
        show_banner: bool,
    },
    ShowConnectionNotification {
        text: String,
    },
    PromptConnectionApproval {
        text: String,
        reason: String,
        timeout_seconds: u32,
        lock_idle_seconds: u32,
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
        /// The virtual monitor the service added to a console without one.
        headless: Option<HeadlessTarget>,
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
    Annotate(meshrmm_protocol::Annotation),
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
    ApprovalDecision(Decision),
    /// The console has no active display, so there is nothing to capture
    /// until the service adds a virtual one.
    NoDisplays,
    Error(String),
    Stopped,
}

/// Why a capture helper did not start.
#[derive(Debug)]
enum StartFailure {
    NoDisplays,
    Failed(String),
}

impl From<StartFailure> for anyhow::Error {
    fn from(failure: StartFailure) -> Self {
        match failure {
            StartFailure::NoDisplays => NoDisplays.into(),
            StartFailure::Failed(message) => anyhow::Error::msg(message),
        }
    }
}

#[derive(Debug)]
struct NoDisplays;

impl std::fmt::Display for NoDisplays {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the console has no active display")
    }
}

impl std::error::Error for NoDisplays {}

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
    session_banner: bool,
    /// Taken by the first start on a desktop whose policy allows it, so later
    /// starts do not retry it.
    connection_notification: Option<ConnectionNotification>,
    notification_helper: Option<RunningInputHelper>,
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
    headless_resolution: HeadlessResolution,
    /// Added while the console has no monitor, and kept until the remote
    /// session ends. Declared last so it is removed after the helpers stop.
    virtual_display: Option<VirtualDisplay>,
}

impl DesktopCaptureStreamer {
    pub fn new(
        viewer_name: String,
        blackout_message: String,
        session_banner: bool,
        connection_notification: Option<ConnectionNotification>,
        credential_store: PathBuf,
    ) -> Self {
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
            session_banner,
            connection_notification,
            notification_helper: None,
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
            headless_resolution: super::virtual_display::last_resolution(),
            virtual_display: None,
        }
    }

    /// Sets the size of the virtual display for a console without a monitor.
    /// Replaces a virtual display of another size and returns true; capture
    /// must then restart.
    pub fn set_headless_resolution(&mut self, resolution: HeadlessResolution) -> bool {
        if !resolution.valid() {
            tracing::warn!(
                width = resolution.width,
                height = resolution.height,
                "ignoring an unsupported virtual display size"
            );
            return false;
        }
        self.headless_resolution = resolution;
        super::virtual_display::remember_resolution(resolution);
        if self
            .virtual_display
            .as_ref()
            .is_none_or(|display| display.target().resolution == resolution)
        {
            return false;
        }
        // The old monitor goes first so that Windows never shows both.
        self.virtual_display = None;
        match VirtualDisplay::add(resolution) {
            Ok(display) => self.virtual_display = Some(display),
            // The next start adds one again if the console is still headless.
            Err(error) => tracing::warn!(
                error = format!("{error:#}"),
                "could not resize the virtual display"
            ),
        }
        true
    }

    pub fn start(
        &mut self,
        config: StreamConfig,
        display_id: Option<DisplayId>,
        sink: EncodedFrameSink,
    ) -> anyhow::Result<StartedDesktop> {
        let started = self.start_capture(config, display_id, sink)?;
        self.show_connection_notification();
        Ok(started)
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
}

impl Default for DesktopCaptureStreamer {
    fn default() -> Self {
        Self::new(
            String::new(),
            meshrmm_protocol::render_blackout_message("", ""),
            true,
            None,
            PathBuf::new(),
        )
    }
}

impl Drop for DesktopCaptureStreamer {
    fn drop(&mut self) {
        // Internal shutdowns replace helpers when the viewer changes desktops;
        // only the end of the connection closes the notification.
        self.stop_notification_helper();
        let _ = self.shutdown();
    }
}

struct RunningHelper {
    last_frame: Arc<Mutex<Instant>>,
    sink: Arc<Mutex<Option<EncodedFrameSink>>>,
    started: mpsc::Receiver<Result<StartedDesktop, StartFailure>>,
    process: OwnedHandle,
    process_id: u32,
    target: DesktopTarget,
    input: InputWriter,
    status: HelperStatus,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
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

/// A JPEG of the console's primary display, captured by a one-shot LocalSystem
/// helper that follows the input desktop, such as the lock screen.
pub fn capture_thumbnail() -> anyhow::Result<Vec<u8>> {
    ask_system_helper(
        preferred_desktop(),
        ParentCommand::CaptureThumbnail,
        THUMBNAIL_TIMEOUT,
        "screen thumbnail capture",
        read_thumbnail,
    )?
    .map_err(|message| anyhow::anyhow!(message))
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
