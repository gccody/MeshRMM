//! The independent helpers for input, files, clipboard, chat, and the
//! connection notification, which live beside the capture helper.
use super::*;

pub(super) struct RunningInputHelper {
    process: OwnedHandle,
    process_id: u32,
    target: DesktopTarget,
    display_id: DisplayId,
    input: InputWriter,
    status: HelperStatus,
    reader: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<()>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HelperKind {
    Input,
    Files,
    Clipboard,
    Chat,
    Notification,
}

impl HelperKind {
    /// The helper that `start` launches, if it launches one.
    pub(super) fn started_by(start: &ParentCommand) -> Option<Self> {
        match start {
            ParentCommand::StartInput { .. } => Some(Self::Input),
            ParentCommand::StartFiles => Some(Self::Files),
            ParentCommand::StartClipboard => Some(Self::Clipboard),
            ParentCommand::StartChatHelper { .. } => Some(Self::Chat),
            ParentCommand::ShowConnectionNotification { .. } => Some(Self::Notification),
            _ => None,
        }
    }
}

pub(super) fn helper_uses_user_token(kind: HelperKind, target: DesktopTarget) -> bool {
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
pub(super) fn start_input_helper(
    start: ParentCommand,
    target: DesktopTarget,
    display_id: DisplayId,
    cursor: HelperCursor,
    clipboard: HelperClipboard,
    files: HelperFiles,
    chat: HelperChat,
    maintenance: HelperMaintenance,
    credentials: HelperCredentials,
) -> anyhow::Result<RunningInputHelper> {
    let kind = HelperKind::started_by(&start).context("not a desktop helper start command")?;
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
    if let Err(error) = send_command(&input, &start) {
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

impl DesktopCaptureStreamer {
    pub(super) fn ensure_input_helper(
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
                    ParentCommand::StartFiles,
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
                    ParentCommand::StartChatHelper {
                        viewer_name: self.viewer_name.clone(),
                        show_banner: self.session_banner,
                    },
                    target,
                    display_id,
                    Arc::clone(&self.cursor),
                    Arc::clone(&self.clipboard),
                    Arc::clone(&self.files),
                    Arc::clone(&self.chat),
                    Arc::clone(&self.maintenance),
                    Arc::clone(&self.credentials),
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
                    ParentCommand::StartClipboard,
                    target,
                    display_id,
                    Arc::clone(&self.cursor),
                    Arc::clone(&self.clipboard),
                    Arc::clone(&self.files),
                    Arc::clone(&self.chat),
                    Arc::clone(&self.maintenance),
                    Arc::clone(&self.credentials),
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
            ParentCommand::StartInput {
                display_id,
                viewer_name: self.viewer_name.clone(),
            },
            target,
            display_id,
            Arc::clone(&self.cursor),
            Arc::clone(&self.clipboard),
            Arc::clone(&self.files),
            Arc::clone(&self.chat),
            Arc::clone(&self.maintenance),
            Arc::clone(&self.credentials),
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

    pub(super) fn stop_chat_helper(&mut self) {
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

    pub(super) fn stop_notification_helper(&mut self) {
        if let Some(mut helper) = self.notification_helper.take() {
            let _ = send_command(&helper.input, &ParentCommand::Stop);
            if unsafe { WaitForSingleObject(helper.process.0, STOP_TIMEOUT_MS) } == WAIT_TIMEOUT {
                terminate_and_wait(&helper.process);
            }
            helper.finish();
        }
    }

    pub(super) fn stop_clipboard_helper(&mut self) {
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

    pub(super) fn stop_file_helper(&mut self) {
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

    pub(super) fn stop_input_helper(&mut self) {
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
