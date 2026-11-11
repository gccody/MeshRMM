//! Which desktops and RDP sessions exist, and the displays each one has.
use super::*;

/// The most RDP sessions offered to the viewer. A terminal server's other
/// users are left out of the list, which keeps the session poll and the
/// display catalog bounded however many users are signed in.
pub(super) const MAX_RDP_SESSIONS: usize = 64;
/// How often the RDP session list is checked while capture runs.
pub(super) const SESSION_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// WTS session IDs above this do not fit in an RDP display ID.
const MAX_RDP_SESSION_ID: u32 = 0x7fff;
/// The helper-side ID of an RDP session's placeholder display. Windows
/// numbers monitors from 1 (`\\.\DISPLAY1`), so no monitor has it, and a
/// capture helper started on it chooses the session's primary monitor.
const PLACEHOLDER_LOCAL_ID: u32 = 0;
/// The helper-side ID of a session's "All monitors" display.
const ALL_MONITORS_LOCAL_ID: u32 = u32::MAX - 1;
/// The low byte of an RDP display ID that stands for "All monitors".
const ALL_MONITORS_RDP_BYTE: u32 = 255;

/// An active RDP session, as Windows reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RdpSession {
    pub(super) id: u32,
    pub(super) user: String,
    /// When a client last connected to it, so that a reconnect, which may
    /// bring other monitors, counts as a change.
    pub(super) connected: i64,
}

/// Which RDP sessions exist. Tests replace it.
pub(super) trait RdpSessions: Send + Sync {
    /// The active RDP sessions other than the console's: at most
    /// [`MAX_RDP_SESSIONS`], `keep` among them while it is active.
    fn active(&self, keep: Option<u32>) -> anyhow::Result<Vec<RdpSession>>;
    /// The size of the session's client display, if Windows knows it. It
    /// launches nothing into the session.
    fn client_size(&self, id: u32) -> Option<(u32, u32)>;
}

/// The RDP sessions Windows Terminal Services reports.
pub(super) struct WtsSessions;

impl RdpSessions for WtsSessions {
    fn active(&self, keep: Option<u32>) -> anyhow::Result<Vec<RdpSession>> {
        active_rdp_sessions(keep)
    }

    fn client_size(&self, id: u32) -> Option<(u32, u32)> {
        client_display_size(id)
    }
}

/// An offered RDP session and its displays.
struct KnownSession {
    session: RdpSession,
    /// The monitors a capture helper in the session reported, with their
    /// helper-side IDs, or one placeholder until one has run in it during
    /// this connection of the session's client.
    displays: Vec<(Display, DisplayId)>,
}

/// The RDP sessions offered to the viewer and their displays. Listing them
/// launches nothing: a session is offered with one placeholder display, sized
/// as its client, until the technician views it and its capture helper
/// reports its monitors, which are kept until the session's client
/// reconnects.
pub(super) struct RdpCatalog {
    source: Arc<dyn RdpSessions>,
    /// By session ID.
    known: Vec<KnownSession>,
    checked: Instant,
    pub(super) poll_interval: Duration,
}

impl RdpCatalog {
    pub(super) fn new(source: Arc<dyn RdpSessions>) -> Self {
        Self {
            source,
            known: Vec::new(),
            checked: Instant::now(),
            poll_interval: SESSION_POLL_INTERVAL,
        }
    }

    #[cfg(test)]
    pub(super) fn set_source(&mut self, source: Arc<dyn RdpSessions>) {
        self.source = source;
    }

    /// Whether the session list is due to be checked again.
    pub(super) fn poll_due(&self) -> bool {
        self.checked.elapsed() >= self.poll_interval
    }

    /// Lists the sessions again, keeping `keep` if it is still active, and
    /// returns whether the list changed. The displays of a session that is
    /// still connected as it was stay as they were, with their IDs.
    pub(super) fn refresh(&mut self, keep: Option<u32>) -> anyhow::Result<bool> {
        self.checked = Instant::now();
        let mut sessions = self.source.active(keep)?;
        sessions.retain(|session| session.id < MAX_RDP_SESSION_ID && !session.user.is_empty());
        sessions.sort_by_key(|session| session.id);
        sessions.dedup_by_key(|session| session.id);
        if sessions.len() > MAX_RDP_SESSIONS {
            let kept = keep.and_then(|id| sessions.iter().position(|session| session.id == id));
            if let Some(index) = kept.filter(|index| *index >= MAX_RDP_SESSIONS) {
                sessions.swap(index, MAX_RDP_SESSIONS - 1);
            }
            sessions.truncate(MAX_RDP_SESSIONS);
            sessions.sort_by_key(|session| session.id);
        }
        let changed = sessions.len() != self.known.len()
            || sessions
                .iter()
                .zip(&self.known)
                .any(|(session, known)| *session != known.session);
        if !changed {
            return Ok(false);
        }
        let mut previous = std::mem::take(&mut self.known);
        for session in sessions {
            let displays = match previous.iter().position(|known| known.session == session) {
                Some(index) => previous.swap_remove(index).displays,
                None => vec![self.placeholder(&session)],
            };
            self.known.push(KnownSession { session, displays });
        }
        Ok(true)
    }

    /// The display a session is offered with until its monitors are known.
    fn placeholder(&self, session: &RdpSession) -> (Display, DisplayId) {
        let (width, height) = self.source.client_size(session.id).unwrap_or((1920, 1080));
        let local = DisplayId(PLACEHOLDER_LOCAL_ID);
        (
            Display {
                session: session_label(session),
                id: rdp_display_id(session.id, local),
                name: "Remote Desktop".into(),
                x: 0,
                y: 0,
                width,
                height,
                primary: true,
            },
            local,
        )
    }

    pub(super) fn session(&self, id: u32) -> Option<&RdpSession> {
        self.known
            .iter()
            .map(|known| &known.session)
            .find(|session| session.id == id)
    }

    /// The session an RDP display ID belongs to, and the helper-side ID of
    /// the monitor to capture there: `None` for the session's primary.
    /// `None` for an ID outside the RDP range.
    pub(super) fn route(&self, id: DisplayId) -> Option<(u32, Option<DisplayId>)> {
        if id.0 < 0x8000_0000 || id.0 >= u32::MAX - 2 {
            return None;
        }
        let session = (id.0 >> 8) & MAX_RDP_SESSION_ID;
        let known = self
            .known
            .iter()
            .flat_map(|known| &known.displays)
            .find(|(display, _)| display.id == id)
            .map(|(_, local)| *local);
        // A monitor of an earlier connection of the session, which may have
        // other monitors now, is asked for by its number.
        let local = known.unwrap_or(match id.0 & 0xff {
            ALL_MONITORS_RDP_BYTE => DisplayId(ALL_MONITORS_LOCAL_ID),
            local => DisplayId(local),
        });
        Some((session, (local.0 != PLACEHOLDER_LOCAL_ID).then_some(local)))
    }

    /// Takes the monitors a capture helper in session `id` reported, and
    /// returns them with RDP display IDs, each with its helper-side ID.
    /// Empty when the session is not offered.
    pub(super) fn learn(&mut self, id: u32, displays: &[Display]) -> Vec<(Display, DisplayId)> {
        let Some(known) = self.known.iter_mut().find(|known| known.session.id == id) else {
            return Vec::new();
        };
        let session = session_label(&known.session);
        known.displays = displays
            .iter()
            .filter(|display| display.id.0 < 255 || display.id.0 == ALL_MONITORS_LOCAL_ID)
            .take(MAX_DISPLAYS)
            .map(|display| {
                let local = display.id;
                (
                    Display {
                        session: session.clone(),
                        id: rdp_display_id(id, local),
                        ..display.clone()
                    },
                    local,
                )
            })
            .collect();
        known.displays.clone()
    }

    /// Every offered display, by session.
    pub(super) fn displays(&self) -> impl Iterator<Item = &Display> {
        self.known
            .iter()
            .flat_map(|known| known.displays.iter().map(|(display, _)| display))
    }

    /// The first offered display, the primary of a session if one is.
    pub(super) fn first_display(&self) -> Option<&Display> {
        self.displays()
            .find(|display| display.primary)
            .or_else(|| self.displays().next())
    }
}

fn session_label(session: &RdpSession) -> meshrmm_protocol::DesktopSession {
    meshrmm_protocol::DesktopSession::Rdp {
        id: session.id,
        user: session.user.clone(),
    }
}

impl DesktopCaptureStreamer {
    /// Lists the RDP sessions again. It launches nothing.
    pub(super) fn refresh_sessions(&mut self) {
        match self.rdp.refresh(self.selected_session) {
            Ok(true) => self.catalog_changed = true,
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "could not enumerate RDP sessions"),
        }
    }

    /// Checks the RDP sessions when due. A change that leaves the viewed
    /// session as it was is published with [`Self::take_display_catalog`],
    /// and capture goes on untouched. The viewed session ending, or its
    /// client disconnecting or connecting again, takes its desktop away:
    /// capture stops, and the session starts it again once the session is
    /// back.
    pub(super) fn poll_sessions(&mut self) -> Option<anyhow::Result<()>> {
        if !self.rdp.poll_due() {
            return None;
        }
        let viewed = self
            .selected_session
            .and_then(|id| self.rdp.session(id).cloned());
        match self.rdp.refresh(self.selected_session) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(error) => {
                tracing::debug!(%error, "could not enumerate RDP sessions");
                return None;
            }
        }
        if let Some(id) = self.selected_session
            && self.rdp.session(id) != viewed.as_ref()
        {
            tracing::info!(
                session_id = id,
                "the viewed RDP session ended or its client reconnected"
            );
            return Some(self.stop());
        }
        self.catalog_changed = true;
        None
    }

    /// The displays to offer the viewer, if they changed since the last
    /// start while capture runs.
    pub fn take_display_catalog(&mut self) -> Option<Vec<Display>> {
        if !self.catalog_changed || self.running.is_none() {
            return None;
        }
        self.catalog_changed = false;
        Some(self.catalog())
    }

    /// Every display the viewer can choose: the console's, the background
    /// desktop and the RDP sessions'.
    fn catalog(&self) -> Vec<Display> {
        let mut displays = self.console_displays.clone();
        displays.push(background_display());
        displays.extend(self.rdp.displays().cloned());
        displays
    }

    pub(super) fn with_background_display(
        &mut self,
        mut started: StartedDesktop,
    ) -> StartedDesktop {
        let mut routes = Vec::new();
        if let Some(session_id) = self.selected_session {
            // The monitors the capture helper reported for the viewed session.
            for (display, local) in self.rdp.learn(session_id, &started.displays) {
                routes.push((display.id, local));
                if local == started.active_display.id {
                    started.active_display = display;
                }
            }
        } else if !self.background_active.load(Ordering::Acquire) {
            self.console_displays = started.displays.clone();
        }
        if self.console_displays.is_empty() {
            self.console_displays = enumerate_console_displays().unwrap_or_default();
        }
        *self.display_routes.lock().unwrap() = routes;
        started.displays = self.catalog();
        // The configuration that starts the stream carries the catalog.
        self.catalog_changed = false;
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
            | if local.0 == ALL_MONITORS_LOCAL_ID {
                ALL_MONITORS_RDP_BYTE
            } else {
                local.0
            },
    )
}

/// The active RDP sessions, with one WTS query for each of at most
/// [`MAX_RDP_SESSIONS`] of them.
fn active_rdp_sessions(keep: Option<u32>) -> anyhow::Result<Vec<RdpSession>> {
    let mut buffer = std::ptr::null_mut();
    let mut count = 0;
    unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut buffer, &mut count) }?;
    let mut ids = Vec::new();
    if !buffer.is_null() {
        let console = unsafe { WTSGetActiveConsoleSessionId() };
        for session in unsafe { std::slice::from_raw_parts(buffer, count as usize) } {
            if session.State == WTSActive
                && session.SessionId != console
                && session.SessionId < MAX_RDP_SESSION_ID
            {
                ids.push(session.SessionId);
            }
        }
        unsafe { WTSFreeMemory(buffer.cast()) };
    }
    if ids.len() > MAX_RDP_SESSIONS {
        if let Some(index) = keep.and_then(|id| ids.iter().position(|candidate| *candidate == id)) {
            ids.swap(index, 0);
        }
        ids.truncate(MAX_RDP_SESSIONS);
    }
    Ok(ids.into_iter().filter_map(rdp_session).collect())
}

/// The user and connection time of an active session, if it has a user.
fn rdp_session(id: u32) -> Option<RdpSession> {
    let mut info = PWSTR::null();
    let mut bytes = 0;
    unsafe { WTSQuerySessionInformationW(None, id, WTSSessionInfo, &mut info, &mut bytes) }.ok()?;
    if info.is_null() {
        return None;
    }
    let session = (bytes as usize >= std::mem::size_of::<WTSINFOW>()).then(|| {
        // SAFETY: WTS returned a WTSINFOW of `bytes` bytes.
        let info = unsafe { &*info.0.cast::<WTSINFOW>() };
        let length = info
            .UserName
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(info.UserName.len());
        RdpSession {
            id,
            user: String::from_utf16_lossy(&info.UserName[..length]),
            connected: info.ConnectTime,
        }
    });
    unsafe { WTSFreeMemory(info.0.cast()) };
    session.filter(|session| !session.user.is_empty())
}

/// The size of a session's client display, as its RDP client reported it.
/// Windows reports one size for the client, not its monitors.
fn client_display_size(id: u32) -> Option<(u32, u32)> {
    let mut info = PWSTR::null();
    let mut bytes = 0;
    unsafe { WTSQuerySessionInformationW(None, id, WTSClientDisplay, &mut info, &mut bytes) }
        .ok()?;
    if info.is_null() {
        return None;
    }
    let size = (bytes as usize >= std::mem::size_of::<WTS_CLIENT_DISPLAY>()).then(|| {
        // SAFETY: WTS returned a WTS_CLIENT_DISPLAY of `bytes` bytes.
        let display = unsafe { &*info.0.cast::<WTS_CLIENT_DISPLAY>() };
        (display.HorizontalResolution, display.VerticalResolution)
    });
    unsafe { WTSFreeMemory(info.0.cast()) };
    size.filter(|(width, height)| *width > 0 && *height > 0)
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
