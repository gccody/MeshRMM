//! The keys and pointer buttons the session has told the device to hold
//! down, kept beside the route to the input helper that holds them.
//!
//! A helper is replaced when the input desktop changes, and the old one may
//! be gone, killed, or on a desktop that no longer takes input, so its own
//! release cannot be relied on. The replacement therefore releases what the
//! session holds before it takes any of the viewer's input, and a release for
//! a key the session does not hold is not forwarded: the viewer's key-up for
//! a key pressed before the replacement does nothing.
use super::*;

/// What the device has been told to hold down.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct HeldOnDevice {
    keys: Vec<(u16, bool)>,
    buttons: Vec<meshrmm_protocol::PointerButton>,
}

impl HeldOnDevice {
    /// Whether `input` goes to the helper: everything except the release of
    /// a key or button the device does not hold.
    pub(super) fn admits(&self, input: &RemoteInput) -> bool {
        match input {
            RemoteInput::Key {
                scan_code,
                extended,
                pressed: false,
                ..
            } => self.keys.contains(&(*scan_code, *extended)),
            RemoteInput::PointerButton {
                button,
                pressed: false,
                ..
            }
            | RemoteInput::PointerButtonAt {
                button,
                pressed: false,
                ..
            } => self.buttons.contains(button),
            _ => true,
        }
    }

    /// Notes `input`, which the helper was given.
    pub(super) fn note(&mut self, input: &RemoteInput) {
        match input {
            RemoteInput::Key {
                scan_code,
                extended,
                pressed,
                ..
            } => {
                let key = (*scan_code, *extended);
                self.keys.retain(|held| *held != key);
                if *pressed {
                    self.keys.push(key);
                }
            }
            RemoteInput::PointerButton {
                button, pressed, ..
            }
            | RemoteInput::PointerButtonAt {
                button, pressed, ..
            } => {
                self.buttons.retain(|held| held != button);
                if *pressed {
                    self.buttons.push(*button);
                }
            }
            // A helper releases everything before it types.
            RemoteInput::TypeText { .. } => self.clear(),
            _ => {}
        }
    }

    pub(super) fn clear(&mut self) {
        self.keys.clear();
        self.buttons.clear();
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }

    /// The releases of everything held, keys first, on `display_id`; the
    /// record is then empty.
    pub(super) fn take_releases(&mut self, display_id: DisplayId) -> Vec<RemoteInput> {
        let keys = self
            .keys
            .drain(..)
            .map(|(scan_code, extended)| RemoteInput::Key {
                display_id,
                scan_code,
                extended,
                pressed: false,
            });
        let buttons = self
            .buttons
            .drain(..)
            .map(|button| RemoteInput::PointerButton {
                display_id,
                button,
                pressed: false,
            });
        keys.chain(buttons).collect()
    }
}

/// The input helper's pipe, if one is routed, and what the device holds.
#[derive(Default)]
pub(super) struct InputRouteState {
    writer: Option<InputWriter>,
    held: HeldOnDevice,
}

pub(super) type DeviceInputRoute = Arc<Mutex<InputRouteState>>;

pub(super) fn lock_route(route: &DeviceInputRoute) -> std::sync::MutexGuard<'_, InputRouteState> {
    route.lock().unwrap_or_else(|error| error.into_inner())
}

impl InputRouteState {
    pub(super) fn writer(&self) -> Option<InputWriter> {
        self.writer.clone()
    }

    #[cfg(test)]
    pub(super) fn held(&self) -> &HeldOnDevice {
        &self.held
    }

    /// Stops routing input to the current helper. What it holds stays in the
    /// record for its replacement to release.
    pub(super) fn unroute(&mut self) -> Option<InputWriter> {
        self.writer.take()
    }

    /// Routes input to `writer`, a helper showing `display_id`, after
    /// releasing there what the session holds: the helper that pressed it
    /// may not have released it on its way out.
    pub(super) fn route_to(&mut self, writer: InputWriter, display_id: DisplayId) {
        for release in self.held.take_releases(display_id) {
            if let Err(error) = send_input(&writer, release) {
                tracing::warn!(%error, "could not release held input on the new desktop");
            }
        }
        self.writer = Some(writer);
    }

    /// Notes that the routed helper releases everything by itself.
    pub(super) fn released(&mut self) {
        self.held.clear();
    }
}

/// Sends `input` to the routed helper, if any, and notes what it holds.
pub(super) fn forward_input(route: &DeviceInputRoute, input: RemoteInput) -> anyhow::Result<()> {
    let writer = {
        let state = lock_route(route);
        if !state.held.admits(&input) {
            return Ok(());
        }
        // Desktop switches temporarily unroute input while the old helper
        // goes and its replacement starts. Discard events in this gap
        // instead of ending the remote session or replaying stale input
        // onto the new desktop.
        let Some(writer) = state.writer() else {
            return Ok(());
        };
        writer
    };
    // Sending may wait for room, so it happens without the route locked.
    send_input(&writer, input.clone()).context("failed to send input to the active desktop")?;
    let mut state = lock_route(route);
    // A helper replaced meanwhile released what it held, or never read this.
    if state
        .writer
        .as_ref()
        .is_some_and(|routed| Arc::ptr_eq(routed, &writer))
    {
        state.held.note(&input);
    }
    Ok(())
}

/// Releases everything on the routed helper. Without one, or when its pipe
/// is gone, the record stays for the next helper to release.
pub(super) fn release_routed(route: &DeviceInputRoute) -> anyhow::Result<()> {
    let Some(writer) = lock_route(route).writer() else {
        return Ok(());
    };
    send_after_input(&writer, &ParentCommand::ReleaseInput)
        .context("failed to release input on the active desktop")?;
    let mut state = lock_route(route);
    if state
        .writer
        .as_ref()
        .is_some_and(|routed| Arc::ptr_eq(routed, &writer))
    {
        state.released();
    }
    Ok(())
}
