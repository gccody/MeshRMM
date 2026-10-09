//! Starting and restarting capture on a desktop.
use super::*;

impl DesktopCaptureStreamer {
    /// Shows the company's connection notification on the primary monitor of
    /// the console, or of the viewed RDP session. It runs in its own helper,
    /// so sessions on the background desktop can notify the user too, and a
    /// desktop switch that replaces the other helpers leaves it alone.
    pub(super) fn show_connection_notification(&mut self) {
        let background = self.background_active.load(Ordering::Acquire);
        // A background session the company does not announce still notifies
        // the user if the technician switches to the user's desktop.
        if !self
            .connection_notification
            .as_ref()
            .is_some_and(|notification| notification.allowed(background))
        {
            return;
        }
        let Some(notification) = self.connection_notification.take() else {
            return;
        };
        if !notification.pending() {
            return;
        }
        let target = match self.running.as_ref().map(|running| running.target) {
            Some(DesktopTarget::Background) | None => preferred_desktop(),
            Some(target) => target,
        };
        match self.start_helper(
            ParentCommand::ShowConnectionNotification {
                text: notification.text().to_owned(),
            },
            target,
            DisplayId(0),
        ) {
            Ok(helper) => {
                notification.mark_shown();
                self.stop_notification_helper();
                self.notification_helper = Some(helper);
            }
            Err(error) => {
                tracing::warn!(error = ?error, desktop = target.name(), "could not show the connection notification")
            }
        }
    }

    pub(super) fn start_capture(
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
            match self.start_or_add_display(target, config, display_id, &sink) {
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
            return self.start_capture(config, Some(display.id), sink);
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
                headless: self.virtual_display.as_ref().map(VirtualDisplay::target),
            },
        )?;
        let started = running
            .started
            .recv_timeout(self.start_timeout())
            .context("capture helper did not reconfigure promptly")??;
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

    /// Starts capture on `target`, first adding a virtual display when the
    /// console has no monitor.
    fn start_or_add_display(
        &mut self,
        target: DesktopTarget,
        config: StreamConfig,
        display_id: Option<DisplayId>,
        sink: &EncodedFrameSink,
    ) -> anyhow::Result<StartedDesktop> {
        let error = match self.start_on_desktop(target, config, display_id, Arc::clone(sink)) {
            Err(error) if error.is::<NoDisplays>() => error,
            result => return result,
        };
        if self.virtual_display.is_some() {
            return Err(error.context("the virtual display did not become active"));
        }
        if !matches!(target, DesktopTarget::Default | DesktopTarget::Winlogon) {
            return Err(error);
        }
        tracing::info!(
            width = self.headless_resolution.width,
            height = self.headless_resolution.height,
            "the console has no monitor; adding a virtual display"
        );
        let display = VirtualDisplay::add(self.headless_resolution)
            .context("the computer has no monitor, and a virtual display could not be added")?;
        self.virtual_display = Some(display);
        self.start_on_desktop(target, config, display_id, Arc::clone(sink))
    }

    fn start_timeout(&self) -> Duration {
        if self.virtual_display.is_some() {
            START_TIMEOUT + HEADLESS_ARRIVAL
        } else {
            START_TIMEOUT
        }
    }

    pub(super) fn start_on_desktop(
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
            headless: self.virtual_display.as_ref().map(VirtualDisplay::target),
        };
        if let Err(error) = send_command(&input, &start) {
            terminate_and_wait(&launched.process);
            let _ = reader.join();
            let _ = stderr.join();
            return Err(error).context("failed to start the desktop helper");
        }
        let started = match started_rx.recv_timeout(self.start_timeout()) {
            Ok(Ok(started)) => started,
            Ok(Err(failure)) => {
                terminate_and_wait(&launched.process);
                let _ = reader.join();
                let _ = stderr.join();
                return Err(match failure {
                    StartFailure::NoDisplays => NoDisplays.into(),
                    StartFailure::Failed(message) => {
                        anyhow::anyhow!("desktop helper failed: {message}")
                    }
                });
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
}
