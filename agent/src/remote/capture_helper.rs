use meshrmm_protocol::ClipboardContent;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use meshrmm_protocol::{
    CursorShape, Display, DisplayId, MAX_CLIPBOARD_WIRE_BYTES, RemoteInput, SessionMessage,
};
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_TIMEOUT,
};
use windows::Win32::Security::{
    DuplicateTokenEx, SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation,
    TOKEN_ALL_ACCESS, TokenPrimary, TokenSessionId,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken,
    PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use meshrmm_remote_screen::{
    ActiveFormat, EncodedAccessUnit, EncodedFrameSink, StreamConfig, VideoCodec, VideoPixelFormat,
    WindowsDesktopDuplicationStreamer,
};

use super::input::WindowsInputController;
use super::platform::ScreenInput;

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
const NO_DISPLAY: u32 = u32::MAX;
const NO_ACTIVE_SESSION: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DesktopTarget {
    Default,
    Winlogon,
}

impl DesktopTarget {
    fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Winlogon => "Winlogon",
        }
    }

    fn alternate(self) -> Self {
        match self {
            Self::Default => Self::Winlogon,
            Self::Winlogon => Self::Default,
        }
    }
}

enum ParentCommand {
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
    },
    RequestKeyframe,
    SetBitrate(u32),
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
    Files(meshrmm_protocol::FileMessage),
    Started(StartedDesktop),
    InputStarted,
    MaintenanceState {
        agent_input_blocked: bool,
        blacked_out: bool,
    },
    MaintenanceError(String),
    Frame(EncodedAccessUnit),
    Cursor(CursorShape),
    Clipboard(ClipboardContent),
    Chat(String),
    Error(String),
    Stopped,
}

type HelperStatus = Arc<Mutex<Option<Result<(), String>>>>;
type HelperCursor = Arc<Mutex<CursorShape>>;
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
    chat_enabled: Arc<AtomicBool>,
}

impl DesktopCaptureStreamer {
    pub fn new(viewer_name: String, blackout_message: String) -> Self {
        Self {
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
            cursor: Arc::new(Mutex::new(CursorShape::Default)),
            clipboard: Arc::new(ClipboardEvents::default()),
            files: Arc::new(FileEvents::default()),
            chat: Arc::new(ChatEvents::default()),
            maintenance: Arc::new(Mutex::new(None)),
            chat_enabled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn start(
        &mut self,
        config: StreamConfig,
        display_id: Option<DisplayId>,
        sink: EncodedFrameSink,
    ) -> anyhow::Result<StartedDesktop> {
        if self.running.is_some() {
            let result = self.reconfigure(config, display_id, Arc::clone(&sink));
            if result.is_ok() {
                return result;
            }
            tracing::warn!(error = ?result.err(), "could not reuse capture helper; starting a replacement");
            let _ = self.stop();
        }
        let preferred = self.preferred_desktop.unwrap_or_else(preferred_desktop);
        let mut last_error = None;
        for target in [preferred, preferred.alternate()] {
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
        Ok(started)
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
        let reader = thread::Builder::new()
            .name("meshrmm-desktop-ipc".into())
            .spawn(move || {
                dispatch_child_events(
                    launched.output,
                    reader_sink,
                    started_tx,
                    reader_status,
                    reader_cursor,
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
            process: launched.process,
            process_id: launched.process_id,
            target,
            input,
            status,
            reader: Some(reader),
            stderr: Some(stderr),
        });
        Ok(started)
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
        let running = self.running.as_ref()?;
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
        if self
            .file_helper
            .as_ref()
            .is_none_or(|h| h.status.lock().unwrap().is_some())
            || self.file_route.lock().unwrap().is_none()
        {
            self.stop_file_helper();
            match start_input_helper(
                &self.viewer_name,
                DesktopTarget::Default,
                display_id,
                Arc::clone(&self.cursor),
                Arc::clone(&self.clipboard),
                Arc::clone(&self.files),
                Arc::clone(&self.chat),
                Arc::clone(&self.maintenance),
                HelperKind::Files,
            ) {
                Ok(helper) => {
                    *self.file_route.lock().unwrap() = Some(Arc::clone(&helper.input));
                    self.file_helper = Some(helper);
                }
                Err(error) => {
                    tracing::warn!(%error, "file transfers require a signed-in interactive user")
                }
            }
        }
        if self
            .chat_helper
            .as_ref()
            .is_none_or(|helper| helper.target != target || helper.status.lock().unwrap().is_some())
        {
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
        if self
            .clipboard_helper
            .as_ref()
            .is_none_or(|helper| helper.target != target || helper.status.lock().unwrap().is_some())
        {
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
                HelperKind::Clipboard,
            ) {
                Ok(helper) => {
                    *self.clipboard_route.lock().unwrap() = Some(Arc::clone(&helper.input));
                    self.clipboard_helper = Some(helper);
                }
                Err(error) => tracing::warn!(%error, "independent clipboard helper unavailable"),
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
            HelperKind::Input,
        )?;
        *self
            .input_route
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&helper.input));
        self.input = Some(helper);
        Ok(())
    }

    pub fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop_chat_helper();
        self.stop_clipboard_helper();
        self.stop_file_helper();
        self.stop_input_helper();
        self.stop()
    }

    fn stop_chat_helper(&mut self) {
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
        )
    }
}

impl Drop for DesktopCaptureStreamer {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

struct RunningHelper {
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
    chat_enabled: Arc<AtomicBool>,
}

impl ScreenInput for DesktopInputController {
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
    fn apply_files(&self, message: meshrmm_protocol::FileMessage) -> anyhow::Result<()> {
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

    fn apply(&self, input: RemoteInput) -> anyhow::Result<()> {
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

    fn cursor_shape(&self) -> CursorShape {
        *self
            .cursor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
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

fn launch_system_helper(target: DesktopTarget) -> anyhow::Result<LaunchedHelper> {
    launch_helper(target, false)
}
fn launch_helper(target: DesktopTarget, as_user: bool) -> anyhow::Result<LaunchedHelper> {
    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    let working_directory = executable
        .parent()
        .context("Agent executable has no parent directory")?;
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
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
    let (child_input, parent_input) = create_inherited_pipe(false)?;
    let (parent_output, child_output) = create_inherited_pipe(true)?;
    let (parent_stderr, child_stderr) = create_inherited_pipe(true)?;
    let executable_wide = wide(executable.as_os_str());
    let working_directory_wide = wide(working_directory.as_os_str());
    let mut command_line = wide(OsStr::new(&format!(
        "\"{}\" --capture-helper",
        executable.display()
    )));
    let mut desktop = wide(OsStr::new(&format!("winsta0\\{}", target.name())));
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        dwFlags: STARTF_USESTDHANDLES,
        hStdInput: child_input.0,
        hStdOutput: child_output.0,
        hStdError: child_stderr.0,
        ..Default::default()
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
    let launched = unsafe {
        CreateProcessAsUserW(
            Some(session_token.0),
            PCWSTR(executable_wide.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW | windows::Win32::System::Threading::CREATE_UNICODE_ENVIRONMENT,
            (!environment.is_null()).then_some(environment.cast_const()),
            PCWSTR(working_directory_wide.as_ptr()),
            &startup,
            &mut process_info,
        )
    };
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
    drop(child_input);
    drop(child_output);
    drop(child_stderr);
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

fn helper_uses_user_token(kind: HelperKind, target: DesktopTarget) -> bool {
    kind == HelperKind::Files || (kind == HelperKind::Clipboard && target == DesktopTarget::Default)
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

fn create_inherited_pipe(parent_reads: bool) -> anyhow::Result<(OwnedHandle, OwnedHandle)> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        bInheritHandle: BOOL(1),
        ..Default::default()
    };
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe { CreatePipe(&mut read, &mut write, Some(&attributes), 0) }
        .context("failed to create desktop-helper IPC pipe")?;
    let read = OwnedHandle(read);
    let write = OwnedHandle(write);
    let parent = if parent_reads { &read } else { &write };
    unsafe { SetHandleInformation(parent.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        .context("failed to protect the parent desktop-helper pipe handle")?;
    Ok((read, write))
}

fn dispatch_child_events(
    output: impl Read,
    sink: Arc<Mutex<Option<EncodedFrameSink>>>,
    started_tx: mpsc::Sender<Result<StartedDesktop, String>>,
    status: HelperStatus,
    cursor: HelperCursor,
) {
    let mut output = BufReader::new(output);
    loop {
        match read_event(&mut output) {
            Ok(ChildEvent::Started(started)) => {
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
                if let Some(sink) = sink
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .as_ref()
                {
                    (sink)(frame);
                }
            }
            Ok(ChildEvent::Cursor(shape)) => {
                *cursor.lock().unwrap_or_else(|error| error.into_inner()) = shape;
            }
            Ok(
                ChildEvent::MaintenanceError(_)
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
) {
    let mut output = BufReader::new(output);
    let mut started_tx = Some(started_tx);
    loop {
        match read_event(&mut output) {
            Ok(ChildEvent::InputStarted) => {
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
            Ok(ChildEvent::MaintenanceError(reason)) => {
                *maintenance.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SessionMessage::MaintenanceError { reason });
            }
            Ok(ChildEvent::MaintenanceState {
                agent_input_blocked,
                blacked_out,
            }) => {
                *maintenance.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(SessionMessage::MaintenanceState {
                        agent_input_blocked,
                        blacked_out,
                    });
            }
            Ok(ChildEvent::Cursor(shape)) => {
                *cursor.lock().unwrap_or_else(|error| error.into_inner()) = shape;
            }
            Ok(ChildEvent::Files(message)) => {
                let mut queue = files.queue.lock().unwrap();
                if queue.len() < 32 {
                    queue.push_back(message);
                    files.ready.notify_one();
                }
            }
            Ok(ChildEvent::Chat(text)) => {
                let mut queue = chat.queue.lock().unwrap_or_else(|e| e.into_inner());
                if queue.len() < 32 {
                    queue.push_back(text);
                    chat.ready.notify_one();
                }
            }
            Ok(ChildEvent::Clipboard(text)) => {
                *clipboard
                    .latest
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = Some(text);
                clipboard.ready.notify_one();
            }
            Ok(ChildEvent::Error(message)) => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Err(message.clone()));
                }
                set_status(&status, Err(message));
                break;
            }
            Ok(ChildEvent::Stopped) => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Err("desktop input helper stopped before startup".into()));
                }
                set_status(&status, Ok(()));
                break;
            }
            Ok(ChildEvent::Started(_) | ChildEvent::Frame(_)) => {
                let message = "desktop input helper reported a video event".to_string();
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Err(message.clone()));
                }
                set_status(&status, Err(message));
                break;
            }
            Err(error) => {
                let message = format!("desktop input-helper IPC failed: {error}");
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Err(message.clone()));
                }
                set_status(&status, Err(message));
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

fn drain_child_stderr(stderr: File) {
    for line in BufReader::new(stderr).lines() {
        match line {
            Ok(line) => tracing::warn!(message = %line, "desktop helper wrote to stderr"),
            Err(error) => {
                tracing::warn!(%error, "failed to read desktop-helper stderr");
                break;
            }
        }
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
        } => run_capture_child(
            command_rx,
            display_id,
            frames_per_second,
            bitrate_bits_per_second,
            codec,
            pixel_format,
            capture_cursor,
        ),
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
    mut frames_per_second: u32,
    mut bitrate_bits_per_second: u32,
    mut codec: VideoCodec,
    mut pixel_format: VideoPixelFormat,
    mut capture_cursor: bool,
) -> anyhow::Result<()> {
    'capture: loop {
        if frames_per_second == 0 || bitrate_bits_per_second == 0 {
            anyhow::bail!("desktop-helper frame rate and bitrate must be positive");
        }
        let displays = enumerate_displays()?;
        let active_display = display_id
            .and_then(|id| displays.iter().find(|display| display.id == id))
            .or_else(|| displays.iter().find(|display| display.primary))
            .or_else(|| displays.first())
            .cloned()
            .context("Windows reported no displays on the active desktop")?;
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
        let active = match streamer.start(
            StreamConfig {
                frames_per_second,
                bitrate_bits_per_second,
                codec,
                pixel_format,
                capture_cursor,
            },
            active_display.id.0,
            sink,
        ) {
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
                active_display,
            }),
        )?;

        let mut terminal_error = None;
        loop {
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
                })) => {
                    streamer.stop()?;
                    display_id = next_display;
                    frames_per_second = next_fps;
                    bitrate_bits_per_second = next_bitrate;
                    codec = next_codec;
                    pixel_format = next_pixel_format;
                    capture_cursor = next_capture_cursor;
                    continue 'capture;
                }
                Ok(Ok(
                    ParentCommand::StartFiles
                    | ParentCommand::StartClipboard
                    | ParentCommand::StartChatHelper { .. }
                    | ParentCommand::StartInput { .. }
                    | ParentCommand::Input(_)
                    | ParentCommand::Blackout { .. }
                    | ParentCommand::BlockInput(_)
                    | ParentCommand::ReleaseInput
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
    let displays = enumerate_displays()?;
    let active_display = displays
        .into_iter()
        .find(|display| display.id == display_id)
        .context("input helper could not find the selected display")?;
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
    let mut sent_cursor = None;
    let mut terminal_error = None;
    loop {
        let cursor = input.cursor_shape();
        if sent_cursor != Some(cursor) {
            if emit_child_event(&output, ChildEvent::Cursor(cursor)).is_err() {
                break;
            }
            sent_cursor = Some(cursor);
        }
        match command_rx.recv_timeout(Duration::from_millis(16)) {
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

fn enumerate_displays() -> anyhow::Result<Vec<Display>> {
    meshrmm_remote_screen::enumerate_displays()
        .context("failed to enumerate displays on the active desktop")?
        .into_iter()
        .map(|display| {
            Ok(Display {
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
        }
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
            let bytes = SessionMessage::Input(*input)
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
            })
        }
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
        ChildEvent::Cursor(shape) => {
            let bytes = SessionMessage::CursorShape { shape: *shape }
                .encode()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            checked_len(bytes.len(), MAX_CONTROL_BYTES, "cursor shape")?;
            writer.write_all(&[EVENT_CURSOR])?;
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
            let length = bounded_len(read_u32(&mut reader)?, MAX_CONTROL_BYTES, "cursor shape")?;
            let mut bytes = vec![0; length];
            reader.read_exact(&mut bytes)?;
            match SessionMessage::decode(&bytes) {
                Ok(SessionMessage::CursorShape { shape }) => Ok(ChildEvent::Cursor(shape)),
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

struct OwnedHandle(HANDLE);
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    fn into_file(self) -> File {
        let raw = self.0.0;
        std::mem::forget(self);
        unsafe { File::from_raw_handle(raw) }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use meshrmm_protocol::PointerButton;

    #[test]
    fn capture_reader_accepts_reconfiguration_and_discards_frames_while_unrouted() {
        let display = Display {
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
                Arc::new(Mutex::new(CursorShape::Default)),
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
    fn cursor_capture_survives_helper_start_and_leaves_next_command_aligned() {
        for enabled in [false, true] {
            let mut bytes = Vec::new();
            write_command(
                &mut bytes,
                &ParentCommand::Start {
                    viewer_name: "Viewer".into(),
                    display_id: Some(DisplayId(1)),
                    frames_per_second: 60,
                    bitrate_bits_per_second: 6_000_000,
                    codec: VideoCodec::H264,
                    pixel_format: VideoPixelFormat::Yuv420,
                    capture_cursor: enabled,
                },
            )
            .unwrap();
            write_command(&mut bytes, &ParentCommand::RequestKeyframe).unwrap();
            let mut reader = bytes.as_slice();
            assert!(
                matches!(read_command(&mut reader).unwrap(), ParentCommand::Start { capture_cursor, .. } if capture_cursor == enabled)
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
            ParentCommand::Start { .. } => COMMAND_START,
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

        let (reader, writer) = create_inherited_pipe(false).unwrap();
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
    fn stalled_clipboard_pipe_does_not_block_input_pipe() {
        let (blocked_read, blocked_write) = create_inherited_pipe(false).unwrap();
        let clipboard = CommandWriter::new(blocked_write.into_file()).unwrap();
        // Far larger than the anonymous pipe buffer; its writer must wait until
        // the reader drains/closes it. The caller only enqueues these bytes.
        clipboard.send(vec![0; 1024 * 1024]).unwrap();
        let (input_read, input_write) = create_inherited_pipe(false).unwrap();
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
    tokio::runtime::Builder::new_current_thread()
        .build()?
        .block_on(async {
            loop {
                tokio::select! {
                    command = commands.recv() => match command {
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

    #[tokio::test]
    async fn helper_pipe_notifies_services_and_coalesces_clipboard_changes() {
        let (read, write) = create_inherited_pipe(true).unwrap();
        let (started, startup) = mpsc::sync_channel(1);
        let clipboard = Arc::new(ClipboardEvents::default());
        let files = Arc::new(FileEvents::default());
        let chat = Arc::new(ChatEvents::default());
        let reader_clipboard = clipboard.clone();
        let reader_files = files.clone();
        let reader_chat = chat.clone();
        let (finished, done) = tokio::sync::oneshot::channel();
        thread::spawn(move || {
            dispatch_input_events(
                read.into_file(),
                started,
                Arc::new(Mutex::new(None)),
                Arc::new(Mutex::new(CursorShape::Default)),
                reader_clipboard,
                reader_files,
                reader_chat,
                Arc::new(Mutex::new(None)),
            );
            let _ = finished.send(());
        });
        let mut writer = write.into_file();
        for event in [
            ChildEvent::InputStarted,
            ChildEvent::Clipboard(ClipboardContent::Text("first".into())),
            ChildEvent::Clipboard(ClipboardContent::Text("latest".into())),
            ChildEvent::Chat("hello".into()),
            ChildEvent::Files(meshrmm_protocol::FileMessage::Available),
            ChildEvent::Stopped,
        ] {
            write_event(&mut writer, &event).unwrap();
        }
        tokio::time::timeout(Duration::from_secs(2), done)
            .await
            .unwrap()
            .unwrap();
        startup.recv().unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            clipboard.ready.notified().await;
            files.ready.notified().await;
            chat.ready.notified().await;
        })
        .await
        .unwrap();
        assert_eq!(
            clipboard.latest.lock().unwrap().take(),
            Some(ClipboardContent::Text("latest".into()))
        );
        assert_eq!(
            chat.queue.lock().unwrap().pop_front().as_deref(),
            Some("hello")
        );
        assert!(matches!(
            files.queue.lock().unwrap().pop_front(),
            Some(meshrmm_protocol::FileMessage::Available)
        ));
        assert!(clipboard.latest.lock().unwrap().is_none());
    }
}
