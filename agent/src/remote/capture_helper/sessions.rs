//! Which desktops and RDP sessions exist, and the displays each one has.
use super::*;

impl DesktopCaptureStreamer {
    pub(super) fn refresh_sessions(&mut self) {
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

    pub(super) fn with_background_display(
        &mut self,
        mut started: StartedDesktop,
    ) -> StartedDesktop {
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
}

pub(super) fn preferred_desktop() -> DesktopTarget {
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

pub(super) fn rdp_display_id(session: u32, local: DisplayId) -> DisplayId {
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

pub(super) fn active_rdp_sessions() -> anyhow::Result<Vec<(u32, String)>> {
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

// Session 0 cannot see the console's complete monitor topology. Query the same
// desktop helper used by regular connections, without starting capture or input.
pub(super) fn enumerate_console_displays() -> anyhow::Result<Vec<Display>> {
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
    let displays = ask_system_helper(
        target,
        ParentCommand::EnumerateDisplays,
        START_TIMEOUT,
        "console display enumeration",
        |output| {
            let count = bounded_len(read_u32(output)?, MAX_DISPLAYS, "display count")?;
            (0..count)
                .map(|_| read_display(output))
                .collect::<io::Result<Vec<_>>>()
        },
    )?;
    anyhow::ensure!(!displays.is_empty(), "console desktop reported no displays");
    Ok(displays)
}
