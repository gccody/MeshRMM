//! GNOME Wayland capture through Mutter's own screencast D-Bus API, the one
//! GNOME's remote desktop uses. It needs no permission prompt; GNOME shows
//! its screen-sharing indicator while a capture runs. Mutter sends the frames
//! as a PipeWire stream, only when the screen changes.
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, bail};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::pipewire;
use super::{Backend, CaptureSettings, FrameSink, Stream};
use crate::remote::linux::display::Output;

const SCREEN_CAST: &str = "org.gnome.Mutter.ScreenCast";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Whether Mutter's screencast service runs on the session bus.
pub(crate) fn available() -> bool {
    zbus::blocking::Connection::session()
        .ok()
        .and_then(|bus| {
            zbus::blocking::fdo::DBusProxy::new(&bus)
                .ok()?
                .name_has_owner(SCREEN_CAST.try_into().ok()?)
                .ok()
        })
        .unwrap_or(false)
}

pub(crate) struct Mutter {
    bus: zbus::blocking::Connection,
}

impl Mutter {
    pub(crate) fn connect() -> anyhow::Result<Self> {
        let bus = zbus::blocking::Connection::session()
            .context("could not connect to the session's D-Bus")?;
        Ok(Self { bus })
    }
}

type MonitorId = (String, String, String, String);
type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type Monitor = (MonitorId, Vec<Mode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorId>,
    HashMap<String, OwnedValue>,
);
type State = (
    u32,
    Vec<Monitor>,
    Vec<LogicalMonitor>,
    HashMap<String, OwnedValue>,
);

/// The displays in Mutter's layout. With the logical layout, which Wayland
/// sessions use, a monitor's size in the layout is its mode's size over its
/// scale.
fn outputs_from(state: State) -> Vec<Output> {
    let (_, monitors, logical_monitors, properties) = state;
    let logical_layout = properties
        .get("layout-mode")
        .and_then(|mode| u32::try_from(mode).ok())
        .is_none_or(|mode| mode == 1);
    let mut outputs = Vec::new();
    for (x, y, scale, transform, primary, members, _) in logical_monitors {
        for member in members {
            let Some(mode) =
                monitors
                    .iter()
                    .find(|(id, _, _)| *id == member)
                    .and_then(|(_, modes, _)| {
                        modes.iter().find(|mode| {
                            mode.6
                                .get("is-current")
                                .and_then(|current| bool::try_from(current).ok())
                                .unwrap_or(false)
                        })
                    })
            else {
                continue;
            };
            let (mut width, mut height) = (mode.1 as f64, mode.2 as f64);
            // Rotated a quarter turn either way.
            if transform % 2 == 1 {
                std::mem::swap(&mut width, &mut height);
            }
            if logical_layout && scale > 0.0 {
                width /= scale;
                height /= scale;
            }
            outputs.push(Output {
                name: member.0.clone(),
                x,
                y,
                width: width.round() as u32,
                height: height.round() as u32,
                primary,
            });
        }
    }
    outputs
}

impl Backend for Mutter {
    fn name(&self) -> &'static str {
        "GNOME (Mutter)"
    }

    fn outputs(&mut self) -> anyhow::Result<Vec<Output>> {
        let state: State = self
            .bus
            .call_method(
                Some(DISPLAY_CONFIG),
                "/org/gnome/Mutter/DisplayConfig",
                Some(DISPLAY_CONFIG),
                "GetCurrentState",
                &(),
            )
            .context("Mutter did not describe its displays")?
            .body()
            .deserialize()?;
        let outputs = outputs_from(state);
        anyhow::ensure!(!outputs.is_empty(), "Mutter reported no displays");
        Ok(outputs)
    }

    fn start(
        &mut self,
        output: &Output,
        settings: CaptureSettings,
        sink: FrameSink,
    ) -> anyhow::Result<Box<dyn Stream>> {
        let session: OwnedObjectPath = self
            .bus
            .call_method(
                Some(SCREEN_CAST),
                "/org/gnome/Mutter/ScreenCast",
                Some(SCREEN_CAST),
                "CreateSession",
                &(HashMap::<&str, Value<'_>>::new(),),
            )
            .map_err(|error| {
                // GNOME Shell inhibits remote access at the login screen and
                // while the screen is locked.
                if error.to_string().contains("inhibited") {
                    anyhow::anyhow!(
                        "{} at its login screen or while the screen is locked; sign in or unlock the computer first",
                        crate::remote::linux::GNOME_CAPTURE_REFUSED
                    )
                } else {
                    anyhow::Error::from(error).context("Mutter refused to start a screencast")
                }
            })?
            .body()
            .deserialize()?;
        let session_proxy = zbus::blocking::Proxy::new(
            &self.bus,
            SCREEN_CAST,
            session.clone(),
            "org.gnome.Mutter.ScreenCast.Session",
        )?;
        let mut properties = HashMap::new();
        // 0 leaves the pointer out of the frames; 1 draws it in.
        properties.insert(
            "cursor-mode",
            Value::from(u32::from(settings.capture_cursor)),
        );
        let stream_path: OwnedObjectPath = session_proxy
            .call("RecordMonitor", &(output.name.as_str(), properties))
            .with_context(|| format!("Mutter could not record {}", output.name))?;
        let stream_proxy = zbus::blocking::Proxy::new(
            &self.bus,
            SCREEN_CAST,
            stream_path,
            "org.gnome.Mutter.ScreenCast.Stream",
        )?;
        let mut added = stream_proxy.receive_signal("PipeWireStreamAdded")?;
        let (node_sender, node) = mpsc::channel();
        std::thread::Builder::new()
            .name("meshrmm-mutter-stream".into())
            .spawn(move || {
                if let Some(message) = added.next() {
                    let _ = node_sender.send(message.body().deserialize::<u32>());
                }
            })?;
        session_proxy
            .call_method("Start", &())
            .context("Mutter could not start the screencast")?;
        let node = match node.recv_timeout(STREAM_TIMEOUT) {
            Ok(Ok(node)) => node,
            Ok(Err(error)) => return Err(error).context("Mutter sent an invalid PipeWire node"),
            Err(_) => {
                let _ = session_proxy.call_method("Stop", &());
                bail!("Mutter did not start the screencast's PipeWire stream");
            }
        };
        tracing::info!(node, output = %output.name, "Mutter started a screencast");
        let stream = pipewire::Stream::connect(None, node, sink)?;
        Ok(Box::new(MutterStream {
            session: session_proxy,
            stream: Some(stream),
        }))
    }
}

/// A Mutter screencast, stopped when dropped. Mutter also stops it when
/// this process's D-Bus connection closes.
struct MutterStream {
    session: zbus::blocking::Proxy<'static>,
    stream: Option<pipewire::Stream>,
}

impl Stream for MutterStream {
    fn poll_ended(&mut self) -> Option<anyhow::Error> {
        self.stream
            .as_ref()?
            .ended()
            .map(|reason| anyhow::anyhow!(reason))
    }

    /// Mutter fixes the pointer mode when the screencast starts.
    fn set_cursor_capture(&mut self, _enabled: bool) -> bool {
        false
    }
}

impl Drop for MutterStream {
    fn drop(&mut self) {
        self.stream.take();
        let _ = self.session.call_method("Stop", &());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: i32, height: i32, current: bool) -> Mode {
        let mut properties = HashMap::new();
        if current {
            properties.insert(
                "is-current".to_owned(),
                OwnedValue::try_from(Value::from(true)).unwrap(),
            );
        }
        (
            format!("{width}x{height}"),
            width,
            height,
            60.0,
            1.0,
            vec![1.0],
            properties,
        )
    }

    fn id(connector: &str) -> MonitorId {
        (
            connector.into(),
            String::new(),
            String::new(),
            String::new(),
        )
    }

    #[test]
    fn scaled_and_rotated_monitors_use_their_logical_size() {
        let monitors = vec![
            (
                id("eDP-1"),
                vec![mode(1920, 1080, false), mode(2880, 1800, true)],
                HashMap::new(),
            ),
            (id("DP-1"), vec![mode(1920, 1080, true)], HashMap::new()),
        ];
        let logical = vec![
            (0, 0, 2.0, 0, true, vec![id("eDP-1")], HashMap::new()),
            (1440, 0, 1.0, 1, false, vec![id("DP-1")], HashMap::new()),
        ];
        let outputs = outputs_from((1, monitors, logical, HashMap::new()));
        assert_eq!(
            outputs
                .iter()
                .map(|output| (
                    output.name.as_str(),
                    output.x,
                    output.width,
                    output.height,
                    output.primary
                ))
                .collect::<Vec<_>>(),
            [
                ("eDP-1", 0, 1440, 900, true),
                ("DP-1", 1440, 1080, 1920, false)
            ]
        );
    }
}
