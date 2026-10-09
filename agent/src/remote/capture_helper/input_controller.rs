//! The input controller that routes a session's input and services to the
//! helpers.
use super::*;

pub(super) struct DesktopInputController {
    pub(super) display_routes: Arc<Mutex<Vec<(DisplayId, DisplayId)>>>,
    pub(super) background_active: Arc<AtomicBool>,
    pub(super) chat_route: InputRoute,
    pub(super) clipboard_route: InputRoute,
    pub(super) blackout_message: String,
    pub(super) file_route: InputRoute,
    pub(super) route: InputRoute,
    pub(super) cursor: HelperCursor,
    pub(super) clipboard: HelperClipboard,
    pub(super) files: HelperFiles,
    pub(super) chat: HelperChat,
    pub(super) maintenance: HelperMaintenance,
    pub(super) credentials: HelperCredentials,
    pub(super) wallpaper_hidden: Arc<AtomicBool>,
    pub(super) prevent_idle_lock: Arc<AtomicBool>,
    pub(super) chat_enabled: Arc<AtomicBool>,
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
                let Some(encrypted) = crate::remote::credentials::load(&credentials.store)? else {
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
                crate::remote::credentials::forget(&credentials.store)?;
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

    fn annotate(&self, mut annotation: meshrmm_protocol::Annotation) -> anyhow::Result<()> {
        if let Some(display_id) = annotation.display_id() {
            let routes = self.display_routes.lock().unwrap();
            if !routes.is_empty() {
                let Some((_, local)) = routes.iter().find(|(wire, _)| *wire == display_id) else {
                    return Ok(());
                };
                annotation.set_display_id(*local);
            }
        }
        // A desktop switch replaces the helper, and its drawing with it.
        let Some(writer) = self
            .route
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
        else {
            return Ok(());
        };
        send_command(&writer, &ParentCommand::Annotate(annotation))
            .context("failed to send the annotation to the active desktop")
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
