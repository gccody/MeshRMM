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
use meshrmm_protocol::{CursorShape, Display, DisplayId, RemoteInput, SessionMessage};
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
const EVENT_STARTED: u8 = 1;
const EVENT_FRAME: u8 = 2;
const EVENT_ERROR: u8 = 3;
const EVENT_STOPPED: u8 = 4;
const EVENT_CURSOR: u8 = 5;
const EVENT_INPUT_STARTED: u8 = 6;
const MAX_CODEC_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const MAX_CONTROL_BYTES: usize = 64 * 1024;
const MAX_DISPLAY_NAME_BYTES: usize = 4 * 1024;
const MAX_DISPLAYS: usize = 64;
const START_TIMEOUT: Duration = Duration::from_secs(5);
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
    Start {
        display_id: Option<DisplayId>,
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
        codec: VideoCodec,
        pixel_format: VideoPixelFormat,
    },
    RequestKeyframe,
    SetBitrate(u32),
    StartInput {
        display_id: DisplayId,
    },
    Input(RemoteInput),
    ReleaseInput,
    Stop,
}

pub struct StartedDesktop {
    pub format: ActiveFormat,
    pub displays: Vec<Display>,
    pub active_display: Display,
}

enum ChildEvent {
    Started(StartedDesktop),
    InputStarted,
    Frame(EncodedAccessUnit),
    Cursor(CursorShape),
    Error(String),
    Stopped,
}

type HelperStatus = Arc<Mutex<Option<Result<(), String>>>>;
type HelperCursor = Arc<Mutex<CursorShape>>;
type InputWriter = Arc<Mutex<BufWriter<File>>>;
type InputRoute = Arc<Mutex<Option<InputWriter>>>;

/// Brokers a credential-free LocalSystem helper on the visible Windows
/// desktop. Only frames and remote-control events cross the inherited pipes;
/// the Agent token, configuration, and network stack remain in Session 0.
pub struct DesktopCaptureStreamer {
    running: Option<RunningHelper>,
    input: Option<RunningInputHelper>,
    input_route: InputRoute,
    preferred_desktop: Option<DesktopTarget>,
    cursor: HelperCursor,
}

impl DesktopCaptureStreamer {
    pub fn new() -> Self {
        Self {
            running: None,
            input: None,
            input_route: Arc::new(Mutex::new(None)),
            preferred_desktop: None,
            cursor: Arc::new(Mutex::new(CursorShape::Default)),
        }
    }

    pub fn start(
        &mut self,
        config: StreamConfig,
        display_id: Option<DisplayId>,
        sink: EncodedFrameSink,
    ) -> anyhow::Result<StartedDesktop> {
        if self.running.is_some() {
            anyhow::bail!("desktop helper is already running");
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

    fn start_on_desktop(
        &mut self,
        target: DesktopTarget,
        config: StreamConfig,
        display_id: Option<DisplayId>,
        sink: EncodedFrameSink,
    ) -> anyhow::Result<StartedDesktop> {
        let launched = launch_system_helper(target)?;
        let status: HelperStatus = Arc::new(Mutex::new(None));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let reader_status = Arc::clone(&status);
        let reader_cursor = Arc::clone(&self.cursor);
        let reader = thread::Builder::new()
            .name("meshrmm-desktop-ipc".into())
            .spawn(move || {
                dispatch_child_events(
                    launched.output,
                    sink,
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
        let input = Arc::new(Mutex::new(BufWriter::new(launched.input)));
        let start = ParentCommand::Start {
            display_id,
            frames_per_second: config.frames_per_second,
            bitrate_bits_per_second: config.bitrate_bits_per_second,
            codec: config.codec,
            pixel_format: config.pixel_format,
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
            route: Arc::clone(&self.input_route),
            cursor: Arc::clone(&self.cursor),
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
        if self.input.as_ref().is_some_and(|helper| {
            helper.target == target
                && helper.display_id == display_id
                && helper
                    .status
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_none()
        }) {
            return Ok(());
        }
        self.stop_input_helper();
        let helper = start_input_helper(target, display_id, Arc::clone(&self.cursor))?;
        *self
            .input_route
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(Arc::clone(&helper.input));
        self.input = Some(helper);
        Ok(())
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
        Self::new()
    }
}

impl Drop for DesktopCaptureStreamer {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::warn!(%error, "failed to stop desktop helper cleanly");
        }
        self.stop_input_helper();
    }
}

struct RunningHelper {
    process: OwnedHandle,
    process_id: u32,
    target: DesktopTarget,
    input: Arc<Mutex<BufWriter<File>>>,
    status: HelperStatus,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

struct RunningInputHelper {
    process: OwnedHandle,
    process_id: u32,
    target: DesktopTarget,
    display_id: DisplayId,
    input: Arc<Mutex<BufWriter<File>>>,
    status: HelperStatus,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
}

struct DesktopInputController {
    route: InputRoute,
    cursor: HelperCursor,
}

impl ScreenInput for DesktopInputController {
    fn apply(&self, input: RemoteInput) -> anyhow::Result<()> {
        let writer = self
            .route
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("desktop input helper is not running")?;
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
    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    let working_directory = executable
        .parent()
        .context("Agent executable has no parent directory")?;
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == NO_ACTIVE_SESSION {
        anyhow::bail!("Windows reported no active console session");
    }
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
    let mut process_info = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessAsUserW(
            Some(session_token.0),
            PCWSTR(executable_wide.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW,
            None,
            PCWSTR(working_directory_wide.as_ptr()),
            &startup,
            &mut process_info,
        )
    }
    .with_context(|| {
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

fn start_input_helper(
    target: DesktopTarget,
    display_id: DisplayId,
    cursor: HelperCursor,
) -> anyhow::Result<RunningInputHelper> {
    let launched = launch_system_helper(target)?;
    let status: HelperStatus = Arc::new(Mutex::new(None));
    let (started_tx, started_rx) = mpsc::sync_channel(1);
    let reader_status = Arc::clone(&status);
    let reader = thread::Builder::new()
        .name("meshrmm-desktop-input-ipc".into())
        .spawn(move || dispatch_input_events(launched.output, started_tx, reader_status, cursor))
        .context("failed to start desktop input-helper IPC reader")?;
    let stderr = thread::Builder::new()
        .name("meshrmm-desktop-input-stderr".into())
        .spawn(move || drain_child_stderr(launched.stderr))
        .context("failed to start desktop input-helper error reader")?;
    let input = Arc::new(Mutex::new(BufWriter::new(launched.input)));
    if let Err(error) = send_command(&input, &ParentCommand::StartInput { display_id }) {
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
        "independent LocalSystem desktop input helper started"
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
    output: File,
    sink: EncodedFrameSink,
    started_tx: mpsc::SyncSender<Result<StartedDesktop, String>>,
    status: HelperStatus,
    cursor: HelperCursor,
) {
    let mut output = BufReader::new(output);
    let mut started_tx = Some(started_tx);
    loop {
        match read_event(&mut output) {
            Ok(ChildEvent::Started(started)) => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Ok(started));
                } else {
                    set_status(
                        &status,
                        Err("desktop helper sent duplicate start event".into()),
                    );
                    break;
                }
            }
            Ok(ChildEvent::InputStarted) => {
                if let Some(sender) = started_tx.take() {
                    let message = "capture helper reported input-only startup".to_string();
                    let _ = sender.send(Err(message.clone()));
                    set_status(&status, Err(message));
                }
                break;
            }
            Ok(ChildEvent::Frame(frame)) => (sink)(frame),
            Ok(ChildEvent::Cursor(shape)) => {
                *cursor.lock().unwrap_or_else(|error| error.into_inner()) = shape;
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
                    let _ =
                        sender.send(Err("desktop helper stopped before capture started".into()));
                }
                set_status(&status, Ok(()));
                break;
            }
            Err(error) => {
                let message = format!("desktop-helper IPC failed: {error}");
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Err(message.clone()));
                }
                set_status(&status, Err(message));
                break;
            }
        }
    }
}

fn dispatch_input_events(
    output: File,
    started_tx: mpsc::SyncSender<Result<(), String>>,
    status: HelperStatus,
    cursor: HelperCursor,
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
            Ok(ChildEvent::Cursor(shape)) => {
                *cursor.lock().unwrap_or_else(|error| error.into_inner()) = shape;
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

fn send_command(
    input: &Arc<Mutex<BufWriter<File>>>,
    command: &ParentCommand,
) -> anyhow::Result<()> {
    let mut input = input.lock().unwrap_or_else(|error| error.into_inner());
    write_command(&mut *input, command)?;
    input.flush()?;
    Ok(())
}

/// Entry point for the isolated LocalSystem desktop helper. It loads no Agent
/// configuration, opens no network sockets, and receives no Agent credential.
pub fn run_child() -> anyhow::Result<()> {
    let (command_tx, command_rx) = mpsc::channel();
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
            display_id,
            frames_per_second,
            bitrate_bits_per_second,
            codec,
            pixel_format,
        } => run_capture_child(
            command_rx,
            display_id,
            frames_per_second,
            bitrate_bits_per_second,
            codec,
            pixel_format,
        ),
        ParentCommand::StartInput { display_id } => run_input_child(command_rx, display_id),
        _ => anyhow::bail!("desktop helper expected a capture or input start command"),
    }
}

fn run_capture_child(
    command_rx: mpsc::Receiver<io::Result<ParentCommand>>,
    display_id: Option<DisplayId>,
    frames_per_second: u32,
    bitrate_bits_per_second: u32,
    codec: VideoCodec,
    pixel_format: VideoPixelFormat,
) -> anyhow::Result<()> {
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
            Ok(Ok(
                ParentCommand::Start { .. }
                | ParentCommand::StartInput { .. }
                | ParentCommand::Input(_)
                | ParentCommand::ReleaseInput,
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
    Ok(())
}

fn run_input_child(
    command_rx: mpsc::Receiver<io::Result<ParentCommand>>,
    display_id: DisplayId,
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
            Ok(Ok(ParentCommand::Input(event))) => {
                if let Err(error) = input.apply(event) {
                    tracing::warn!(%error, "desktop input helper discarded invalid input");
                }
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
        ParentCommand::Start {
            display_id,
            frames_per_second,
            bitrate_bits_per_second,
            codec,
            pixel_format,
        } => {
            writer.write_all(&[COMMAND_START])?;
            write_u32(&mut writer, display_id.map_or(NO_DISPLAY, |id| id.0))?;
            write_u32(&mut writer, *frames_per_second)?;
            write_u32(&mut writer, *bitrate_bits_per_second)
                .and_then(|()| writer.write_all(&[codec_byte(*codec)]))
                .and_then(|()| writer.write_all(&[pixel_format_byte(*pixel_format)]))
        }
        ParentCommand::RequestKeyframe => writer.write_all(&[COMMAND_REQUEST_KEYFRAME]),
        ParentCommand::SetBitrate(bits_per_second) => {
            writer.write_all(&[COMMAND_SET_BITRATE])?;
            write_u32(&mut writer, *bits_per_second)
        }
        ParentCommand::StartInput { display_id } => {
            writer.write_all(&[COMMAND_START_INPUT])?;
            write_u32(&mut writer, display_id.0)
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
        ParentCommand::ReleaseInput => writer.write_all(&[COMMAND_RELEASE_INPUT]),
        ParentCommand::Stop => writer.write_all(&[COMMAND_STOP]),
    }
}

fn read_command(mut reader: impl Read) -> io::Result<ParentCommand> {
    match read_u8(&mut reader)? {
        COMMAND_START => {
            let display_id = read_u32(&mut reader)?;
            Ok(ParentCommand::Start {
                display_id: (display_id != NO_DISPLAY).then_some(DisplayId(display_id)),
                frames_per_second: read_u32(&mut reader)?,
                bitrate_bits_per_second: read_u32(&mut reader)?,
                codec: read_codec(&mut reader)?,
                pixel_format: read_pixel_format(&mut reader)?,
            })
        }
        COMMAND_REQUEST_KEYFRAME => Ok(ParentCommand::RequestKeyframe),
        COMMAND_SET_BITRATE => Ok(ParentCommand::SetBitrate(read_u32(&mut reader)?)),
        COMMAND_START_INPUT => Ok(ParentCommand::StartInput {
            display_id: DisplayId(read_u32(&mut reader)?),
        }),
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
        COMMAND_RELEASE_INPUT => Ok(ParentCommand::ReleaseInput),
        COMMAND_STOP => Ok(ParentCommand::Stop),
        opcode => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown desktop-helper command opcode {opcode}"),
        )),
    }
}

fn write_event(mut writer: impl Write, event: &ChildEvent) -> io::Result<()> {
    match event {
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
    fn command_protocol_round_trips_desktop_input() {
        let commands = [
            ParentCommand::Start {
                display_id: Some(DisplayId(3)),
                frames_per_second: 60,
                bitrate_bits_per_second: 12_000_000,
                codec: VideoCodec::H265,
                pixel_format: VideoPixelFormat::Yuv444,
            },
            ParentCommand::RequestKeyframe,
            ParentCommand::SetBitrate(4_000_000),
            ParentCommand::StartInput {
                display_id: DisplayId(3),
            },
            ParentCommand::Input(RemoteInput::PointerButton {
                display_id: DisplayId(3),
                button: PointerButton::Left,
                pressed: true,
            }),
            ParentCommand::ReleaseInput,
            ParentCommand::Stop,
        ];
        for command in commands {
            let mut bytes = Vec::new();
            write_command(&mut bytes, &command).unwrap();
            let decoded = read_command(bytes.as_slice()).unwrap();
            assert_eq!(command_name(&decoded), command_name(&command));
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
            ParentCommand::Stop => COMMAND_STOP,
        }
    }
}
