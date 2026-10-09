//! The helper processes' side: a run loop for each kind of helper.
use std::ops::ControlFlow;

use super::*;

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
            headless,
        } => run_capture_child(
            command_rx,
            display_id,
            headless,
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
        ParentCommand::CaptureThumbnail => {
            crate::remote::thumbnail::follow_input_desktop();
            let thumbnail = crate::remote::thumbnail::capture_primary_display()
                .map_err(|error| format!("{error:#}"));
            let mut output = io::stdout().lock();
            write_thumbnail(&mut output, &thumbnail)?;
            output.flush()?;
            Ok(())
        }
        ParentCommand::StartFiles => run_file_child(command_rx),
        ParentCommand::StartClipboard => run_clipboard_child(command_rx),
        ParentCommand::StartChatHelper {
            viewer_name,
            show_banner,
        } => run_chat_child(command_rx, viewer_name, show_banner),
        ParentCommand::ShowConnectionNotification { text } => {
            run_notification_child(command_rx, text)
        }
        ParentCommand::PromptConnectionApproval {
            text,
            reason,
            timeout_seconds,
            lock_idle_seconds,
        } => run_approval_child(
            command_rx,
            ApprovalPrompt {
                text,
                reason,
                timeout: Duration::from_secs(timeout_seconds.into()),
                lock_idle: Duration::from_secs(lock_idle_seconds.into()),
            },
        ),
        ParentCommand::StartInput {
            display_id,
            viewer_name,
        } => run_input_child(command_rx, display_id, viewer_name),
        _ => anyhow::bail!("desktop helper expected a capture or input start command"),
    }
}

type ChildOutput = Arc<Mutex<BufWriter<io::Stdout>>>;

pub(super) fn run_capture_child(
    command_rx: mpsc::Receiver<io::Result<ParentCommand>>,
    mut display_id: Option<DisplayId>,
    mut headless: Option<HeadlessTarget>,
    mut config: StreamConfig,
) -> anyhow::Result<()> {
    let background = is_background_child();
    let mut border_enabled = false;
    loop {
        if config.frames_per_second == 0 || config.bitrate_bits_per_second == 0 {
            anyhow::bail!("desktop-helper frame rate and bitrate must be positive");
        }
        if let Some(target) = &headless
            && let Err(error) = crate::remote::virtual_display::show(target, HEADLESS_ARRIVAL)
        {
            tracing::warn!(error = format!("{error:#}"), "virtual display is not ready");
        }
        if !background && !crate::remote::virtual_display::console_has_display() {
            let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
            emit_child_event(&output, ChildEvent::NoDisplays)?;
            return Ok(());
        }
        let displays = enumerate_displays()?;
        let active_display = display_id
            .and_then(|id| displays.iter().find(|display| display.id == id))
            .or_else(|| displays.iter().find(|display| display.primary))
            .or_else(|| displays.first())
            .cloned()
            .context("Windows reported no displays on the active desktop")?;
        let border = if border_enabled && !background {
            Some(crate::remote::display_border::DisplayBorder::show(
                &active_display,
            )?)
        } else {
            None
        };
        let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
        let ipc_failed = Arc::new(AtomicBool::new(false));
        let sink = frame_sink(Arc::clone(&output), Arc::clone(&ipc_failed));
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
        let mut stream = CaptureStream {
            streamer,
            ipc_failed,
            output,
            border,
            active_display,
        };
        let terminal_error = match stream.serve(&command_rx, &mut border_enabled, background)? {
            CaptureEnd::Restart {
                display_id: next_display,
                headless: next_headless,
                config: next_config,
            } => {
                display_id = next_display;
                headless = next_headless;
                config = next_config;
                continue;
            }
            CaptureEnd::Finished(terminal_error) => terminal_error,
        };
        let _ = stream.streamer.stop();
        if stream.ipc_failed.load(Ordering::Acquire) {
            return Ok(());
        }
        match terminal_error {
            Some(message) => emit_child_event(&stream.output, ChildEvent::Error(message))?,
            None => emit_child_event(&stream.output, ChildEvent::Stopped)?,
        }
        return Ok(());
    }
}

/// Writes each encoded frame to the parent, and records a failed write in
/// `failed` so the capture loop can stop.
fn frame_sink(output: ChildOutput, failed: Arc<AtomicBool>) -> EncodedFrameSink {
    Arc::new(move |frame| {
        let mut output = output.lock().unwrap_or_else(|error| error.into_inner());
        if write_event(&mut *output, &ChildEvent::Frame(frame))
            .and_then(|()| output.flush())
            .is_err()
        {
            failed.store(true, Ordering::Release);
        }
    })
}

/// How a capture stream ended.
enum CaptureEnd {
    /// The parent sent new settings, so capture starts again with them.
    Restart {
        display_id: Option<DisplayId>,
        headless: Option<HeadlessTarget>,
        config: StreamConfig,
    },
    /// The stream is over, with the error to report if it failed.
    Finished(Option<String>),
}

/// A started capture stream. Fields drop in declaration order, so the
/// streamer stops before the display border closes.
struct CaptureStream {
    streamer: WindowsDesktopDuplicationStreamer,
    ipc_failed: Arc<AtomicBool>,
    output: ChildOutput,
    border: Option<crate::remote::display_border::DisplayBorder>,
    active_display: Display,
}

impl CaptureStream {
    /// Applies the parent's commands until the stream ends or the parent
    /// restarts it with new settings.
    fn serve(
        &mut self,
        command_rx: &mpsc::Receiver<io::Result<ParentCommand>>,
        border_enabled: &mut bool,
        background: bool,
    ) -> anyhow::Result<CaptureEnd> {
        loop {
            if self.ipc_failed.load(Ordering::Acquire) {
                return Ok(CaptureEnd::Finished(None));
            }
            if let Some(result) = self.streamer.poll_ended() {
                return Ok(CaptureEnd::Finished(
                    result.err().map(|error| error.to_string()),
                ));
            }
            match command_rx.recv_timeout(Duration::from_millis(16)) {
                Ok(Ok(ParentCommand::SetDisplayBorder(enabled))) => {
                    *border_enabled = enabled && !background;
                    self.set_border(*border_enabled)?;
                }
                Ok(Ok(ParentCommand::SetCursorCapture(enabled))) => {
                    self.streamer.set_cursor_capture(enabled);
                }
                Ok(Ok(ParentCommand::RequestKeyframe)) => {
                    if let Err(error) = self.streamer.request_keyframe() {
                        return Ok(CaptureEnd::Finished(Some(error.to_string())));
                    }
                }
                Ok(Ok(ParentCommand::SetBitrate(bits_per_second))) => {
                    if let Err(error) = self.streamer.set_bitrate(bits_per_second.max(1)) {
                        return Ok(CaptureEnd::Finished(Some(error.to_string())));
                    }
                }
                Ok(Ok(ParentCommand::Stop)) => return Ok(CaptureEnd::Finished(None)),
                Ok(Ok(ParentCommand::Start {
                    viewer_name: _,
                    display_id,
                    frames_per_second,
                    bitrate_bits_per_second,
                    codec,
                    pixel_format,
                    capture_cursor,
                    grayscale,
                    headless,
                })) => {
                    self.streamer.stop()?;
                    return Ok(CaptureEnd::Restart {
                        display_id,
                        headless,
                        config: StreamConfig {
                            frames_per_second,
                            bitrate_bits_per_second,
                            codec,
                            pixel_format,
                            capture_cursor,
                            grayscale,
                        },
                    });
                }
                Ok(Ok(
                    ParentCommand::EnumerateDisplays
                    | ParentCommand::CaptureThumbnail
                    | ParentCommand::StartFiles
                    | ParentCommand::StartClipboard
                    | ParentCommand::StartChatHelper { .. }
                    | ParentCommand::ShowConnectionNotification { .. }
                    | ParentCommand::PromptConnectionApproval { .. }
                    | ParentCommand::StartInput { .. }
                    | ParentCommand::Input(_)
                    | ParentCommand::Annotate(_)
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
                    return Ok(CaptureEnd::Finished(Some(
                        "capture helper received a command reserved for input".into(),
                    )));
                }
                Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Ok(CaptureEnd::Finished(None));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }

    fn set_border(&mut self, enabled: bool) -> io::Result<()> {
        self.border = None;
        if enabled {
            match crate::remote::display_border::DisplayBorder::show(&self.active_display) {
                Ok(value) => self.border = Some(value),
                Err(error) => emit_child_event(
                    &self.output,
                    ChildEvent::MaintenanceError(format!("Display border: {error:#}")),
                )?,
            }
        }
        Ok(())
    }
}

pub(super) fn run_input_child(
    command_rx: mpsc::Receiver<io::Result<ParentCommand>>,
    display_id: DisplayId,
    _viewer_name: String,
) -> anyhow::Result<()> {
    if is_background_child() {
        return run_background_input_child(command_rx);
    }
    let active_display = find_display(display_id)?;
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
    let credential_tx = spawn_credential_filler(output.clone())?;
    let mut sent_cursor = None;
    let terminal_error = loop {
        let cursor = (
            input.cursor_shape(),
            input.viewer_controls_input(),
            input.agent_pointer_display(),
        );
        if sent_cursor != Some(cursor) {
            if emit_child_event(&output, ChildEvent::Cursor(cursor.0, cursor.1, cursor.2)).is_err()
            {
                break None;
            }
            sent_cursor = Some(cursor);
        }
        match command_rx.recv_timeout(Duration::from_millis(16)) {
            Ok(Ok(command)) => {
                let flow = apply_input_command(
                    command,
                    &mut input,
                    &mut keep_awake,
                    &credential_tx,
                    &output,
                )?;
                if let ControlFlow::Break(terminal_error) = flow {
                    break terminal_error;
                }
            }
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    };
    let _ = input.release_all();
    match terminal_error {
        Some(message) => emit_child_event(&output, ChildEvent::Error(message))?,
        None => emit_child_event(&output, ChildEvent::Stopped)?,
    }
    Ok(())
}

fn find_display(display_id: DisplayId) -> anyhow::Result<Display> {
    enumerate_displays()?
        .into_iter()
        .find(|display| display.id == display_id)
        .context("input helper could not find the selected display")
}

/// Starts the thread that reports whether a credential prompt is showing
/// and fills it with the credentials sent through the returned channel.
fn spawn_credential_filler(output: ChildOutput) -> io::Result<mpsc::SyncSender<Vec<u8>>> {
    let (credential_tx, credential_rx) = mpsc::sync_channel::<Vec<u8>>(1);
    thread::Builder::new()
        .name("meshrmm-credential-fields".into())
        .spawn(move || {
            let detector = crate::remote::credentials::Detector::new().ok();
            let mut last_ready = None;
            loop {
                let ready = detector.as_ref().is_some_and(|d| d.ready());
                if last_ready != Some(ready) {
                    if emit_child_event(&output, ChildEvent::CredentialPrompt(ready)).is_err() {
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
                            &output,
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
    Ok(credential_tx)
}

/// Applies one parent command to the input helper. `Break` ends the helper,
/// with the error to report if it failed.
fn apply_input_command(
    command: ParentCommand,
    input: &mut WindowsInputController,
    keep_awake: &mut Option<crate::remote::keep_awake::KeepAwake>,
    credential_tx: &mpsc::SyncSender<Vec<u8>>,
    output: &ChildOutput,
) -> anyhow::Result<ControlFlow<Option<String>>> {
    match command {
        ParentCommand::AutofillCredentials(encrypted) => {
            if credential_tx.try_send(encrypted).is_err() {
                emit_child_event(
                    output,
                    ChildEvent::MaintenanceError("Credential autofill is busy; try again".into()),
                )?;
            }
        }
        ParentCommand::SetPreventIdleLock(enabled) => {
            if let Err(error) = crate::remote::keep_awake::set_enabled(keep_awake, enabled) {
                emit_child_event(
                    output,
                    ChildEvent::MaintenanceError(format!("Prevent idle lock: {error:#}")),
                )?;
            }
        }
        ParentCommand::StartInput { display_id, .. } => {
            let result =
                find_display(display_id).and_then(|display| input.set_active_display(display));
            if let Err(error) = result {
                return Ok(ControlFlow::Break(Some(error.to_string())));
            }
        }
        ParentCommand::Input(event) => {
            if let Err(error) = input.apply(event) {
                tracing::warn!(%error, "desktop input helper discarded invalid input");
            }
        }
        ParentCommand::Annotate(annotation) => {
            if let Err(error) = input.annotate(annotation) {
                emit_child_event(
                    output,
                    ChildEvent::MaintenanceError(format!("Annotate: {error:#}")),
                )?;
            }
        }
        ParentCommand::Blackout { enabled, text } => match input.set_blackout(enabled, &text) {
            Ok(()) => emit_maintenance_state(output, input)?,
            Err(error) => {
                emit_child_event(output, ChildEvent::MaintenanceError(error.to_string()))?
            }
        },
        ParentCommand::BlockInput(blocked) => match input.set_blocked(blocked) {
            Ok(()) => emit_maintenance_state(output, input)?,
            Err(error) => {
                emit_child_event(output, ChildEvent::MaintenanceError(error.to_string()))?
            }
        },
        ParentCommand::ReleaseInput => {
            if let Err(error) = input.release_all() {
                tracing::warn!(%error, "desktop input helper could not release input");
            }
        }
        ParentCommand::Stop => return Ok(ControlFlow::Break(None)),
        _ => {
            return Ok(ControlFlow::Break(Some(
                "input helper received a video command".into(),
            )));
        }
    }
    Ok(ControlFlow::Continue(()))
}

fn emit_maintenance_state(output: &ChildOutput, input: &WindowsInputController) -> io::Result<()> {
    emit_child_event(
        output,
        ChildEvent::MaintenanceState {
            agent_input_blocked: input.blocked(),
            blacked_out: input.blacked_out(),
        },
    )
}

pub(super) fn is_background_child() -> bool {
    std::env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--background-helper")
}

pub(super) fn background_display() -> Display {
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

pub(super) fn run_background_input_child(
    command_rx: mpsc::Receiver<io::Result<ParentCommand>>,
) -> anyhow::Result<()> {
    let mut workspace = crate::remote::background::Workspace::new()?;
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
            Ok(Ok(ParentCommand::Annotate(meshrmm_protocol::Annotation::Start { .. }))) => Err(
                anyhow::anyhow!("Annotations are unavailable in background mode"),
            ),
            Ok(Ok(
                ParentCommand::StartInput { .. }
                | ParentCommand::Annotate(_)
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

pub(super) fn enumerate_displays() -> anyhow::Result<Vec<Display>> {
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

pub(super) fn emit_child_event(output: &ChildOutput, event: ChildEvent) -> io::Result<()> {
    let mut output = output.lock().unwrap_or_else(|error| error.into_inner());
    write_event(&mut *output, &event)?;
    output.flush()
}

const WALLPAPER_RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub(super) fn run_file_child(
    commands: mpsc::Receiver<io::Result<ParentCommand>>,
) -> anyhow::Result<()> {
    // This helper runs as the interactive user and survives capture/desktop switches.
    // Keep the guard until Stop, pipe EOF, or an error unwinds this session.
    let _drag_windows = crate::remote::drag_windows::OutlineDragging::new()
        .inspect_err(|error| {
            tracing::warn!(%error, "could not disable window contents while dragging");
        })
        .ok();
    let mut wallpaper = crate::remote::wallpaper::Wallpaper::default();
    meshrmm_file_transfer::windows::set_displays(enumerate_displays()?);
    meshrmm_file_transfer::TransferSession::run_on_current_thread(move |files| {
        let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
        emit_child_event(&output, ChildEvent::InputStarted)?;
        let mut commands = async_helper_commands(commands)?;
        let ready = files.outgoing_ready();
        let wallpaper_error = |error: anyhow::Error| {
            tracing::warn!(%error, "wallpaper update failed");
            emit_child_event(
                &output,
                ChildEvent::MaintenanceError(format!("Wallpaper: {error:#}")),
            )
        };
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?
            .block_on(async {
                let mut wallpaper_retry = tokio::time::interval(WALLPAPER_RETRY_INTERVAL);
                wallpaper_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        command = commands.recv() => match command {
                            Some(Ok(ParentCommand::SetWallpaperHidden(hidden))) => {
                                if let Err(error) = wallpaper.set_hidden(hidden) {
                                    wallpaper_error(error)?;
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
                        // Explorer starts shortly after this helper at sign-in.
                        _ = wallpaper_retry.tick(), if wallpaper.pending() => {
                            if let Err(error) = wallpaper.retry() {
                                wallpaper_error(error)?;
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

pub(super) fn run_clipboard_child(
    commands: mpsc::Receiver<io::Result<ParentCommand>>,
) -> anyhow::Result<()> {
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let mut clipboard = crate::remote::clipboard::ClipboardSync::new(false)?;
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

pub(super) fn run_chat_child(
    commands: mpsc::Receiver<io::Result<ParentCommand>>,
    viewer_name: String,
    show_banner: bool,
) -> anyhow::Result<()> {
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let chat = meshrmm_chat::ChatSession::with_peer("Viewer");
    let _indicator =
        crate::remote::indicator::SessionIndicator::show(&viewer_name, chat.clone(), show_banner)?;
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
                                    let result = match crate::remote::credentials::prompt() {
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

/// Shows the connection notification, which closes itself, and stays until
/// the parent stops it so that the end of the connection closes it too.
pub(super) fn run_notification_child(
    commands: mpsc::Receiver<io::Result<ParentCommand>>,
    text: String,
) -> anyhow::Result<()> {
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let notification = crate::remote::connection_notification::NotificationWindow::show(&text)?;
    emit_child_event(&output, ChildEvent::InputStarted)?;
    match commands.recv() {
        // An error or a closed pipe means the parent is gone.
        Ok(Ok(ParentCommand::Stop)) | Ok(Err(_)) | Err(_) => {}
        Ok(Ok(_)) => anyhow::bail!("notification helper received an unexpected command"),
    }
    drop(notification);
    emit_child_event(&output, ChildEvent::Stopped)?;
    Ok(())
}

/// Asks the user to accept the connection, reports the answer, and stays
/// until the parent stops it. The parent sends nothing but Stop, so any
/// command, like a closed pipe, means it no longer needs the answer.
pub(super) fn run_approval_child(
    commands: mpsc::Receiver<io::Result<ParentCommand>>,
    prompt: ApprovalPrompt,
) -> anyhow::Result<()> {
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    emit_child_event(&output, ChildEvent::InputStarted)?;
    let decision = crate::remote::connection_approval::ask(&prompt, || {
        !matches!(commands.try_recv(), Err(mpsc::TryRecvError::Empty))
    });
    if let Some(decision) = decision {
        emit_child_event(&output, ChildEvent::ApprovalDecision(decision))?;
        let _ = commands.recv();
    }
    emit_child_event(&output, ChildEvent::Stopped)?;
    Ok(())
}

// Adapt the bounded native pipe reader without periodic wakeups. The bridge
// exits on Stop/EOF; blocking pipe work stays off the async service worker.

pub(super) fn async_helper_commands(
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
