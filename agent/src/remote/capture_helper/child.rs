//! The helper processes' side: a run loop for each kind of helper.
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

pub(super) fn run_capture_child(
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
            Some(crate::remote::display_border::DisplayBorder::show(
                &active_display,
            )?)
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
                        match crate::remote::display_border::DisplayBorder::show(&active_display) {
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

pub(super) fn run_input_child(
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
            let detector = crate::remote::credentials::Detector::new().ok();
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
                if let Err(error) = crate::remote::keep_awake::set_enabled(&mut keep_awake, enabled)
                {
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

pub(super) fn emit_child_event(
    output: &Arc<Mutex<BufWriter<io::Stdout>>>,
    event: ChildEvent,
) -> io::Result<()> {
    let mut output = output.lock().unwrap_or_else(|error| error.into_inner());
    write_event(&mut *output, &event)?;
    output.flush()
}

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
                                if let Err(error) = crate::remote::wallpaper::set_hidden(&mut wallpaper, hidden) {
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
) -> anyhow::Result<()> {
    let output = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let chat = meshrmm_chat::ChatSession::with_peer("Viewer");
    let _indicator = crate::remote::indicator::SessionIndicator::show(&viewer_name, chat.clone())?;
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
