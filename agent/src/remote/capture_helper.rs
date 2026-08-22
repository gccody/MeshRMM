use std::ffi::{OsStr, c_void};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Context;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation, WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use pulsermm_remote_screen::{
    ActiveFormat, EncodedAccessUnit, EncodedFrameSink, StreamConfig, WindowsScreenStreamer,
};

const COMMAND_START: u8 = 1;
const COMMAND_REQUEST_KEYFRAME: u8 = 2;
const COMMAND_SET_BITRATE: u8 = 3;
const COMMAND_STOP: u8 = 4;
const EVENT_STARTED: u8 = 1;
const EVENT_FRAME: u8 = 2;
const EVENT_ERROR: u8 = 3;
const EVENT_STOPPED: u8 = 4;
const MAX_CODEC_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 64 * 1024;
const START_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT_MS: u32 = 5_000;

enum ParentCommand {
    Start {
        display_id: u32,
        frames_per_second: u32,
        bitrate_bits_per_second: u32,
    },
    RequestKeyframe,
    SetBitrate(u32),
    Stop,
}

enum ChildEvent {
    Started(ActiveFormat),
    Frame(EncodedAccessUnit),
    Error(String),
    Stopped,
}

type HelperStatus = Arc<Mutex<Option<Result<(), String>>>>;

/// Capture-only process launched with the active interactive user's token.
/// The inherited anonymous pipes carry no Agent credential or remote input.
pub struct UserCaptureStreamer {
    running: Option<RunningHelper>,
}

impl UserCaptureStreamer {
    pub fn new() -> Self {
        Self { running: None }
    }

    pub fn start(
        &mut self,
        config: StreamConfig,
        display_id: u32,
        sink: EncodedFrameSink,
    ) -> anyhow::Result<ActiveFormat> {
        if self.running.is_some() {
            anyhow::bail!("capture helper is already running");
        }

        let launched = launch_active_user_helper()?;
        let status: HelperStatus = Arc::new(Mutex::new(None));
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let reader_status = Arc::clone(&status);
        let reader = thread::Builder::new()
            .name("pulsermm-capture-ipc".into())
            .spawn(move || dispatch_child_events(launched.output, sink, started_tx, reader_status))
            .context("failed to start the capture-helper IPC reader")?;
        let stderr = thread::Builder::new()
            .name("pulsermm-capture-stderr".into())
            .spawn(move || drain_child_stderr(launched.stderr))
            .context("failed to start the capture-helper error reader")?;

        let input = Arc::new(Mutex::new(BufWriter::new(launched.input)));
        let start = ParentCommand::Start {
            display_id,
            frames_per_second: config.frames_per_second,
            bitrate_bits_per_second: config.bitrate_bits_per_second,
        };
        if let Err(error) = send_command(&input, &start) {
            terminate_and_wait(&launched.process);
            let _ = reader.join();
            let _ = stderr.join();
            return Err(error).context("failed to start the active-user capture helper");
        }

        let active = match started_rx.recv_timeout(START_TIMEOUT) {
            Ok(Ok(active)) => active,
            Ok(Err(message)) => {
                terminate_and_wait(&launched.process);
                let _ = reader.join();
                let _ = stderr.join();
                anyhow::bail!("active-user capture helper failed: {message}");
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                terminate_and_wait(&launched.process);
                let _ = reader.join();
                let _ = stderr.join();
                anyhow::bail!("active-user capture helper did not start within 20 seconds");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                terminate_and_wait(&launched.process);
                let _ = reader.join();
                let _ = stderr.join();
                anyhow::bail!("active-user capture helper exited before capture started");
            }
        };

        tracing::info!(
            process_id = launched.process_id,
            width = active.width,
            height = active.height,
            "active-user capture helper started"
        );
        self.running = Some(RunningHelper {
            process: launched.process,
            process_id: launched.process_id,
            input,
            status,
            reader: Some(reader),
            stderr: Some(stderr),
        });
        Ok(active)
    }

    pub fn request_keyframe(&self) -> anyhow::Result<()> {
        let running = self
            .running
            .as_ref()
            .context("capture helper is not running")?;
        send_command(&running.input, &ParentCommand::RequestKeyframe)
            .context("failed to request a capture-helper keyframe")
    }

    pub fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        let running = self
            .running
            .as_ref()
            .context("capture helper is not running")?;
        send_command(
            &running.input,
            &ParentCommand::SetBitrate(bits_per_second.max(1)),
        )
        .context("failed to change the capture-helper bitrate")
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
                "capture helper did not stop promptly; terminating it"
            );
            terminate_and_wait(&running.process);
        }
        running.finish();
        send_result.context("failed to stop the active-user capture helper")
    }
}

impl Default for UserCaptureStreamer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for UserCaptureStreamer {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::warn!(%error, "failed to stop active-user capture helper cleanly");
        }
    }
}

struct RunningHelper {
    process: OwnedHandle,
    process_id: u32,
    input: Arc<Mutex<BufWriter<File>>>,
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
            None => anyhow::bail!("capture helper exited without a final status"),
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
    input: File,
    output: File,
    stderr: File,
}

fn launch_active_user_helper() -> anyhow::Result<LaunchedHelper> {
    let executable = std::env::current_exe().context("could not locate the Agent executable")?;
    let working_directory = executable
        .parent()
        .context("Agent executable has no parent directory")?;
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    if session_id == u32::MAX {
        anyhow::bail!("Windows reported no active console user for screen capture");
    }

    let mut token = HANDLE::default();
    unsafe { WTSQueryUserToken(session_id, &mut token) }
        .context("failed to obtain the active console user's process token")?;
    let token = OwnedHandle(token);
    let environment = UserEnvironment::create(&token)?;

    let (child_input, parent_input) = create_inherited_pipe(false)?;
    let (parent_output, child_output) = create_inherited_pipe(true)?;
    let (parent_stderr, child_stderr) = create_inherited_pipe(true)?;

    let executable_wide = wide(executable.as_os_str());
    let working_directory_wide = wide(working_directory.as_os_str());
    let mut command_line = wide(OsStr::new(&format!(
        "\"{}\" --capture-helper",
        executable.display()
    )));
    let mut desktop = wide(OsStr::new("winsta0\\default"));
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
            Some(token.0),
            PCWSTR(executable_wide.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            true,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            Some(environment.0.cast_const()),
            PCWSTR(working_directory_wide.as_ptr()),
            &startup,
            &mut process_info,
        )
    }
    .context("failed to launch capture helper as the active console user")?;
    let _thread = OwnedHandle(process_info.hThread);

    // These endpoints belong only to the child. Closing the parent copies here
    // also guarantees EOF propagates in both directions on process shutdown.
    drop(child_input);
    drop(child_output);
    drop(child_stderr);
    Ok(LaunchedHelper {
        process: OwnedHandle(process_info.hProcess),
        process_id: process_info.dwProcessId,
        input: parent_input.into_file(),
        output: parent_output.into_file(),
        stderr: parent_stderr.into_file(),
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
        .context("failed to create capture-helper IPC pipe")?;
    let read = OwnedHandle(read);
    let write = OwnedHandle(write);
    let parent = if parent_reads { &read } else { &write };
    unsafe { SetHandleInformation(parent.0, HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        .context("failed to protect the parent capture-helper pipe handle")?;
    Ok((read, write))
}

fn dispatch_child_events(
    output: File,
    sink: EncodedFrameSink,
    started_tx: mpsc::SyncSender<Result<ActiveFormat, String>>,
    status: HelperStatus,
) {
    let mut output = BufReader::new(output);
    let mut started_tx = Some(started_tx);
    loop {
        match read_event(&mut output) {
            Ok(ChildEvent::Started(format)) => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Ok(format));
                } else {
                    set_status(
                        &status,
                        Err("capture helper sent duplicate start event".into()),
                    );
                    break;
                }
            }
            Ok(ChildEvent::Frame(frame)) => (sink)(frame),
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
                        sender.send(Err("capture helper stopped before capture started".into()));
                }
                set_status(&status, Ok(()));
                break;
            }
            Err(error) => {
                if let Some(sender) = started_tx.take() {
                    let _ = sender.send(Err(format!("capture-helper IPC failed: {error}")));
                }
                set_status(&status, Err(format!("capture-helper IPC failed: {error}")));
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
            Ok(line) => tracing::warn!(message = %line, "capture helper wrote to stderr"),
            Err(error) => {
                tracing::warn!(%error, "failed to read capture-helper stderr");
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

/// Entry point for the unprivileged child. It intentionally loads no config,
/// initializes no network stack, and receives no remote-control messages.
pub fn run_child() -> anyhow::Result<()> {
    let (command_tx, command_rx) = mpsc::channel();
    thread::Builder::new()
        .name("pulsermm-capture-commands".into())
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
        .context("failed to start capture-helper command reader")?;

    let first = command_rx
        .recv()
        .context("capture-helper command pipe closed before startup")??;
    let ParentCommand::Start {
        display_id,
        frames_per_second,
        bitrate_bits_per_second,
    } = first
    else {
        anyhow::bail!("capture helper expected a start command");
    };
    if frames_per_second == 0 || bitrate_bits_per_second == 0 {
        anyhow::bail!("capture-helper frame rate and bitrate must be positive");
    }

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

    let mut streamer = WindowsScreenStreamer::new();
    let active = match streamer.start(
        StreamConfig {
            frames_per_second,
            bitrate_bits_per_second,
        },
        display_id,
        sink,
    ) {
        Ok(active) => active,
        Err(error) => {
            emit_child_event(&output, ChildEvent::Error(error.to_string()))?;
            return Ok(());
        }
    };
    emit_child_event(&output, ChildEvent::Started(active))?;

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
        match command_rx.recv_timeout(Duration::from_millis(100)) {
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
            Ok(Ok(ParentCommand::Start { .. })) => {
                terminal_error = Some("capture helper received a duplicate start command".into());
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
        } => {
            writer.write_all(&[COMMAND_START])?;
            write_u32(&mut writer, *display_id)?;
            write_u32(&mut writer, *frames_per_second)?;
            write_u32(&mut writer, *bitrate_bits_per_second)
        }
        ParentCommand::RequestKeyframe => writer.write_all(&[COMMAND_REQUEST_KEYFRAME]),
        ParentCommand::SetBitrate(bits_per_second) => {
            writer.write_all(&[COMMAND_SET_BITRATE])?;
            write_u32(&mut writer, *bits_per_second)
        }
        ParentCommand::Stop => writer.write_all(&[COMMAND_STOP]),
    }
}

fn read_command(mut reader: impl Read) -> io::Result<ParentCommand> {
    match read_u8(&mut reader)? {
        COMMAND_START => Ok(ParentCommand::Start {
            display_id: read_u32(&mut reader)?,
            frames_per_second: read_u32(&mut reader)?,
            bitrate_bits_per_second: read_u32(&mut reader)?,
        }),
        COMMAND_REQUEST_KEYFRAME => Ok(ParentCommand::RequestKeyframe),
        COMMAND_SET_BITRATE => Ok(ParentCommand::SetBitrate(read_u32(&mut reader)?)),
        COMMAND_STOP => Ok(ParentCommand::Stop),
        opcode => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown capture-helper command opcode {opcode}"),
        )),
    }
}

fn write_event(mut writer: impl Write, event: &ChildEvent) -> io::Result<()> {
    match event {
        ChildEvent::Started(format) => {
            writer.write_all(&[EVENT_STARTED])?;
            write_u32(&mut writer, format.width)?;
            write_u32(&mut writer, format.height)?;
            write_u32(&mut writer, format.frames_per_second)?;
            write_u32(&mut writer, format.bitrate_bits_per_second)
        }
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
        ChildEvent::Error(message) => {
            let message = message.as_bytes();
            checked_len(message.len(), MAX_ERROR_BYTES, "capture-helper error")?;
            writer.write_all(&[EVENT_ERROR])?;
            write_u32(&mut writer, message.len() as u32)?;
            writer.write_all(message)
        }
        ChildEvent::Stopped => writer.write_all(&[EVENT_STOPPED]),
    }
}

fn read_event(mut reader: impl Read) -> io::Result<ChildEvent> {
    match read_u8(&mut reader)? {
        EVENT_STARTED => Ok(ChildEvent::Started(ActiveFormat {
            width: read_u32(&mut reader)?,
            height: read_u32(&mut reader)?,
            frames_per_second: read_u32(&mut reader)?,
            bitrate_bits_per_second: read_u32(&mut reader)?,
        })),
        EVENT_FRAME => {
            let capture_timestamp_us = read_u64(&mut reader)?;
            let encode_complete_timestamp_us = read_u64(&mut reader)?;
            let keyframe = match read_u8(&mut reader)? {
                0 => false,
                1 => true,
                value => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid capture-helper keyframe flag {value}"),
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
        EVENT_ERROR => {
            let length = bounded_len(
                read_u32(&mut reader)?,
                MAX_ERROR_BYTES,
                "capture-helper error",
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
            format!("unknown capture-helper event opcode {opcode}"),
        )),
    }
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

// Kernel handles may be waited on and closed from any thread. Ownership still
// remains unique because OwnedHandle is not Clone.
unsafe impl Send for OwnedHandle {}

impl OwnedHandle {
    fn into_file(self) -> File {
        let raw = self.0.0;
        std::mem::forget(self);
        // SAFETY: ownership of this valid parent pipe endpoint moves to File.
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

struct UserEnvironment(*mut c_void);

impl UserEnvironment {
    fn create(token: &OwnedHandle) -> anyhow::Result<Self> {
        let mut environment = std::ptr::null_mut();
        unsafe { CreateEnvironmentBlock(&mut environment, Some(token.0), false) }
            .context("failed to create the active console user's environment")?;
        Ok(Self(environment))
    }
}

impl Drop for UserEnvironment {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { DestroyEnvironmentBlock(self.0) };
        }
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_protocol_round_trips() {
        let commands = [
            ParentCommand::Start {
                display_id: 3,
                frames_per_second: 60,
                bitrate_bits_per_second: 12_000_000,
            },
            ParentCommand::RequestKeyframe,
            ParentCommand::SetBitrate(4_000_000),
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
        assert_eq!(decoded.encode_complete_timestamp_us, 22);
        assert!(decoded.keyframe);
        assert_eq!(decoded.codec_config, Some(vec![1, 2, 3]));
        assert_eq!(decoded.data, vec![4, 5, 6, 7]);
    }

    fn command_name(command: &ParentCommand) -> u8 {
        match command {
            ParentCommand::Start { .. } => COMMAND_START,
            ParentCommand::RequestKeyframe => COMMAND_REQUEST_KEYFRAME,
            ParentCommand::SetBitrate(_) => COMMAND_SET_BITRATE,
            ParentCommand::Stop => COMMAND_STOP,
        }
    }
}
