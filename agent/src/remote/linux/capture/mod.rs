//! Screen capture, with a backend for each kind of display server. The
//! session's environment says which one runs it: X11 sessions are captured
//! through the X server, and Wayland sessions through their compositor,
//! since Wayland has no common capture interface without a prompt.
mod kwin;
mod mutter;
mod pipewire;
mod wayland;
mod wlroots;
mod x11;

use std::sync::Arc;

use anyhow::bail;
use meshrmm_protocol::CursorShape;

use super::display::Output;
use super::encoder::RawFrame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CaptureSettings {
    pub frames_per_second: u32,
    pub capture_cursor: bool,
}

/// Receives each captured frame, borrowed only for the call.
pub(crate) type FrameSink = Arc<dyn Fn(&RawFrame<'_>) + Send + Sync>;

/// A running capture of one display, stopped when dropped.
pub(crate) trait Stream: Send {
    /// Why capture stopped by itself, once it has.
    fn poll_ended(&mut self) -> Option<anyhow::Error>;
    /// Shows or hides the pointer in later frames. Returns `false` when the
    /// capture has to start again for the change.
    fn set_cursor_capture(&mut self, enabled: bool) -> bool;
    /// The pointer's shape, when the display server tells it.
    fn cursor_shape(&self) -> CursorShape {
        CursorShape::Default
    }
}

/// A display server's capture interface.
pub(crate) trait Backend: Send {
    fn name(&self) -> &'static str;
    /// The displays it can capture.
    fn outputs(&mut self) -> anyhow::Result<Vec<Output>>;
    /// Starts capturing `output`, sending frames to `sink` as the screen
    /// changes, at most `settings.frames_per_second` times a second.
    fn start(
        &mut self,
        output: &Output,
        settings: CaptureSettings,
        sink: FrameSink,
    ) -> anyhow::Result<Box<dyn Stream>>;
}

/// The capture backend for this process's session.
pub(crate) fn detect() -> anyhow::Result<Box<dyn Backend>> {
    let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some() && session_type != "x11";
    if !wayland {
        if std::env::var_os("DISPLAY").is_some() {
            return Ok(Box::new(x11::X11::connect()?));
        }
        bail!("this session has no display server to capture");
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let globals = wayland::Globals::list()?;
    if desktop.split(':').any(|name| name == "GNOME") || mutter::available() {
        return Ok(Box::new(mutter::Mutter::connect()?));
    }
    if globals.has(kwin::SCREENCAST_GLOBAL) {
        return Ok(Box::new(kwin::KWin::new()));
    }
    if globals.has(wlroots::SCREENCOPY_GLOBAL) {
        return Ok(Box::new(wlroots::Wlroots::new()));
    }
    if desktop.split(':').any(|name| name == "KDE") {
        bail!(
            "KWin did not offer its screencast interface; reinstall the MeshRMM Agent so KDE authorizes it"
        );
    }
    bail!(
        "this Wayland desktop ({}) offers no screen capture interface the Agent supports",
        if desktop.is_empty() {
            "unknown"
        } else {
            desktop.as_str()
        }
    )
}
