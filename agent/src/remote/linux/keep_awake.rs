//! Keeps the session from going idle, and locking, while a technician works,
//! for the session only. Desktops take idle inhibitions on the session bus:
//! GNOME through its session manager, and KDE and others through the
//! freedesktop screensaver interface. The inhibition ends when it is
//! released or when this connection closes. Where neither interface exists,
//! as on Xfce, the X server's screen saver is reset every half minute, which
//! the X screen lockers watch, and power management is inhibited.
use std::sync::mpsc;
use std::time::Duration;

use anyhow::bail;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ScreenSaver};

const RESET_INTERVAL: Duration = Duration::from_secs(30);

pub(crate) struct KeepAwake {
    bus: Option<zbus::blocking::Connection>,
    release: Release,
}

enum Release {
    Gnome(u32),
    ScreenSaver(u32),
    /// The X11 reset thread, stopped when the sender drops, and the power
    /// management inhibition, if any.
    X11 {
        _stop: mpsc::Sender<()>,
        power: Option<u32>,
    },
}

pub(crate) fn set_enabled(current: &mut Option<KeepAwake>, enabled: bool) -> anyhow::Result<()> {
    if enabled && current.is_none() {
        *current = Some(KeepAwake::new()?);
    } else if !enabled {
        *current = None;
    }
    Ok(())
}

const REASON: &str = "A technician is working on this computer";

impl KeepAwake {
    fn new() -> anyhow::Result<Self> {
        let bus = zbus::blocking::Connection::session().ok();
        if let Some(bus) = &bus
            && let Some(release) = inhibit(bus)
        {
            return Ok(Self {
                bus: Some(bus.clone()),
                release,
            });
        }
        if std::env::var_os("DISPLAY").is_none() {
            bail!("this desktop does not let the Agent keep the session awake");
        }
        let (connection, _) = x11rb::rust_connection::RustConnection::connect(None)?;
        let (stop, stopped) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("meshrmm-keep-awake".into())
            .spawn(move || {
                // Until the sender drops.
                while connection
                    .force_screen_saver(ScreenSaver::RESET)
                    .is_ok_and(|_| connection.flush().is_ok())
                    && stopped.recv_timeout(RESET_INTERVAL) == Err(mpsc::RecvTimeoutError::Timeout)
                {
                }
            })?;
        let power = bus.as_ref().and_then(|bus| {
            bus.call_method(
                Some("org.freedesktop.PowerManagement"),
                "/org/freedesktop/PowerManagement/Inhibit",
                Some("org.freedesktop.PowerManagement.Inhibit"),
                "Inhibit",
                &("MeshRMM", REASON),
            )
            .ok()?
            .body()
            .deserialize::<u32>()
            .ok()
        });
        Ok(Self {
            bus,
            release: Release::X11 { _stop: stop, power },
        })
    }
}

/// An inhibition from the desktop's own idle interfaces.
fn inhibit(bus: &zbus::blocking::Connection) -> Option<Release> {
    // GSM_INHIBITOR_FLAG_IDLE: keeps the session from being marked idle.
    const IDLE: u32 = 8;
    if let Ok(reply) = bus.call_method(
        Some("org.gnome.SessionManager"),
        "/org/gnome/SessionManager",
        Some("org.gnome.SessionManager"),
        "Inhibit",
        &("meshrmm-agent", 0_u32, REASON, IDLE),
    ) && let Ok(cookie) = reply.body().deserialize::<u32>()
    {
        return Some(Release::Gnome(cookie));
    }
    if let Ok(reply) = bus.call_method(
        Some("org.freedesktop.ScreenSaver"),
        "/org/freedesktop/ScreenSaver",
        Some("org.freedesktop.ScreenSaver"),
        "Inhibit",
        &("MeshRMM", REASON),
    ) && let Ok(cookie) = reply.body().deserialize::<u32>()
    {
        return Some(Release::ScreenSaver(cookie));
    }
    None
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        let Some(bus) = &self.bus else { return };
        let _ = match self.release {
            Release::Gnome(cookie) => bus.call_method(
                Some("org.gnome.SessionManager"),
                "/org/gnome/SessionManager",
                Some("org.gnome.SessionManager"),
                "Uninhibit",
                &(cookie,),
            ),
            Release::ScreenSaver(cookie) => bus.call_method(
                Some("org.freedesktop.ScreenSaver"),
                "/org/freedesktop/ScreenSaver",
                Some("org.freedesktop.ScreenSaver"),
                "UnInhibit",
                &(cookie,),
            ),
            Release::X11 {
                power: Some(cookie),
                ..
            } => bus.call_method(
                Some("org.freedesktop.PowerManagement"),
                "/org/freedesktop/PowerManagement/Inhibit",
                Some("org.freedesktop.PowerManagement.Inhibit"),
                "UnInhibit",
                &(cookie,),
            ),
            Release::X11 { power: None, .. } => return,
        };
    }
}
