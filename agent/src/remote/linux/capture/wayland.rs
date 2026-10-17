//! The Wayland client the compositor backends share: the compositor's
//! globals, its outputs in the logical layout, and the events of the capture
//! objects KWin and wlroots compositors offer.
use std::os::fd::AsRawFd;
use std::time::Duration;

use anyhow::Context;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_plasma::screencast::v1::client::{
    zkde_screencast_stream_unstable_v1, zkde_screencast_unstable_v1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::remote::linux::display::Output;

/// One `wl_output` and what the compositor said about it.
#[derive(Debug, Default, Clone)]
pub(crate) struct OutputInfo {
    pub global: u32,
    pub proxy: Option<wl_output::WlOutput>,
    pub name: Option<String>,
    pub position: Option<(i32, i32)>,
    pub mode: Option<(i32, i32)>,
    pub scale: i32,
    pub transform: i32,
    pub logical_position: Option<(i32, i32)>,
    pub logical_size: Option<(i32, i32)>,
}

impl OutputInfo {
    /// The output in the logical layout: from xdg-output, else from its mode
    /// divided by its scale.
    fn output(&self) -> Option<Output> {
        let (x, y) = self.logical_position.or(self.position)?;
        let (width, height) = self.logical_size.or_else(|| {
            let (mut width, mut height) = self.mode?;
            if self.transform % 2 == 1 {
                std::mem::swap(&mut width, &mut height);
            }
            let scale = self.scale.max(1);
            Some((width / scale, height / scale))
        })?;
        Some(Output {
            name: self
                .name
                .clone()
                .unwrap_or_else(|| format!("Output {}", self.global)),
            x,
            y,
            width: u32::try_from(width).ok()?,
            height: u32::try_from(height).ok()?,
            primary: false,
        })
    }
}

/// A screencopy frame's progress.
#[derive(Debug, Default)]
pub(crate) struct FrameStatus {
    /// The shared-memory buffer the compositor wants: format, width, height, stride.
    pub buffer: Option<(u32, u32, u32, u32)>,
    pub buffer_done: bool,
    pub y_invert: bool,
    pub ready: bool,
    pub failed: bool,
}

/// A KWin screencast stream's progress.
#[derive(Debug, Default)]
pub(crate) struct KdeStreamStatus {
    pub node: Option<u32>,
    pub failed: Option<String>,
    pub closed: bool,
}

#[derive(Default)]
pub(crate) struct State {
    pub globals: Vec<(u32, String, u32)>,
    pub outputs: Vec<OutputInfo>,
    pub xdg_outputs: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    pub shm: Option<wl_shm::WlShm>,
    pub screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    pub kde_screencast: Option<zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1>,
    pub frame: FrameStatus,
    pub kde_stream: KdeStreamStatus,
}

pub(crate) struct Client {
    pub connection: Connection,
    pub queue: EventQueue<State>,
    pub state: State,
}

impl Client {
    /// Connects to `WAYLAND_DISPLAY`, binds what the backends use and learns
    /// the outputs.
    pub(crate) fn connect() -> anyhow::Result<Self> {
        let connection =
            Connection::connect_to_env().context("could not connect to the Wayland compositor")?;
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        connection.display().get_registry(&handle, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        if let Some(manager) = &state.xdg_outputs {
            for output in &state.outputs {
                if let Some(proxy) = &output.proxy {
                    manager.get_xdg_output(proxy, &handle, output.global);
                }
            }
        }
        queue.roundtrip(&mut state)?;
        Ok(Self {
            connection,
            queue,
            state,
        })
    }

    pub(crate) fn handle(&self) -> QueueHandle<State> {
        self.queue.handle()
    }

    /// The outputs in the logical layout. Wayland has no primary output; the
    /// one at the layout's origin counts as primary.
    pub(crate) fn outputs(&self) -> Vec<Output> {
        let mut outputs: Vec<Output> = self
            .state
            .outputs
            .iter()
            .filter_map(OutputInfo::output)
            .collect();
        let primary = outputs
            .iter()
            .position(|output| output.x == 0 && output.y == 0)
            .unwrap_or(0);
        if let Some(output) = outputs.get_mut(primary) {
            output.primary = true;
        }
        outputs
    }

    /// The `wl_output` named `name`.
    pub(crate) fn output_proxy(&self, name: &str) -> Option<wl_output::WlOutput> {
        self.state
            .outputs
            .iter()
            .find(|output| output.output().is_some_and(|output| output.name == name))
            .and_then(|output| output.proxy.clone())
    }

    /// Sends what is queued and handles the events that arrive within
    /// `timeout`.
    pub(crate) fn dispatch(&mut self, timeout: Duration) -> anyhow::Result<()> {
        self.queue.dispatch_pending(&mut self.state)?;
        self.queue.flush()?;
        if let Some(guard) = self.queue.prepare_read() {
            let mut poll = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd.
            let ready = unsafe { libc::poll(&mut poll, 1, timeout.as_millis() as libc::c_int) };
            if ready > 0 {
                guard.read().context("lost the Wayland compositor")?;
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        Ok(())
    }
}

/// The compositor's globals, for choosing a backend.
pub(crate) struct Globals(Vec<String>);

impl Globals {
    pub(crate) fn list() -> anyhow::Result<Self> {
        let client = Client::connect()?;
        Ok(Self(
            client
                .state
                .globals
                .into_iter()
                .map(|(_, name, _)| name)
                .collect(),
        ))
    }

    pub(crate) fn has(&self, interface: &str) -> bool {
        self.0.iter().any(|name| name == interface)
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                match interface.as_str() {
                    "wl_output" => {
                        let proxy = registry.bind::<wl_output::WlOutput, _, _>(
                            name,
                            version.min(4),
                            handle,
                            name,
                        );
                        state.outputs.push(OutputInfo {
                            global: name,
                            proxy: Some(proxy),
                            scale: 1,
                            ..OutputInfo::default()
                        });
                    }
                    "zxdg_output_manager_v1" => {
                        state.xdg_outputs = Some(registry.bind(name, version.min(3), handle, ()));
                    }
                    "wl_shm" => state.shm = Some(registry.bind(name, 1, handle, ())),
                    "zwlr_screencopy_manager_v1" => {
                        state.screencopy = Some(registry.bind(name, version.min(3), handle, ()));
                    }
                    "zkde_screencast_unstable_v1" => {
                        state.kde_screencast =
                            Some(registry.bind(name, version.min(4), handle, ()));
                    }
                    _ => {}
                }
                state.globals.push((name, interface, version));
            }
            wl_registry::Event::GlobalRemove { name } => {
                state.globals.retain(|(global, _, _)| *global != name);
                state.outputs.retain(|output| output.global != name);
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state
            .outputs
            .iter_mut()
            .find(|output| output.global == *global)
        else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                output.position = Some((x, y));
                output.transform = match transform {
                    WEnum::Value(transform) => transform as i32,
                    WEnum::Unknown(value) => value as i32,
                };
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if matches!(flags, WEnum::Value(flags) if flags.contains(wl_output::Mode::Current))
                {
                    output.mode = Some((width, height));
                }
            }
            wl_output::Event::Scale { factor } => output.scale = factor,
            wl_output::Event::Name { name } => output.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(output) = state
            .outputs
            .iter_mut()
            .find(|output| output.global == *global)
        else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                output.logical_position = Some((x, y))
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                output.logical_size = Some((width, height))
            }
            zxdg_output_v1::Event::Name { name } => {
                output.name.get_or_insert(name);
            }
            _ => {}
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let frame = &mut state.frame;
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                let format = match format {
                    WEnum::Value(format) => format as u32,
                    WEnum::Unknown(value) => value,
                };
                frame.buffer = Some((format, width, height, stride));
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                frame.y_invert = matches!(
                    flags,
                    WEnum::Value(flags) if flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert)
                );
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => frame.buffer_done = true,
            zwlr_screencopy_frame_v1::Event::Ready { .. } => frame.ready = true,
            zwlr_screencopy_frame_v1::Event::Failed => frame.failed = true,
            _ => {}
        }
    }
}

impl Dispatch<zkde_screencast_stream_unstable_v1::ZkdeScreencastStreamUnstableV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &zkde_screencast_stream_unstable_v1::ZkdeScreencastStreamUnstableV1,
        event: zkde_screencast_stream_unstable_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let stream = &mut state.kde_stream;
        match event {
            zkde_screencast_stream_unstable_v1::Event::Created { node } => stream.node = Some(node),
            zkde_screencast_stream_unstable_v1::Event::Failed { error } => {
                stream.failed = Some(error)
            }
            zkde_screencast_stream_unstable_v1::Event::Closed => stream.closed = true,
            _ => {}
        }
    }
}

/// Interfaces whose events the backends do not need.
macro_rules! ignore_events {
    ($($interface:ty),* $(,)?) => {
        $(
            impl Dispatch<$interface, ()> for State {
                fn event(
                    _: &mut Self,
                    _: &$interface,
                    _: <$interface as Proxy>::Event,
                    _: &(),
                    _: &Connection,
                    _: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

ignore_events!(
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    zkde_screencast_unstable_v1::ZkdeScreencastUnstableV1,
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_prefer_the_logical_layout() {
        let scaled = OutputInfo {
            name: Some("eDP-1".into()),
            position: Some((0, 0)),
            mode: Some((2880, 1800)),
            scale: 2,
            ..OutputInfo::default()
        };
        let output = scaled.output().unwrap();
        assert_eq!((output.width, output.height), (1440, 900));
        let described = OutputInfo {
            logical_position: Some((1440, 0)),
            logical_size: Some((1280, 720)),
            ..scaled
        };
        let output = described.output().unwrap();
        assert_eq!((output.x, output.width, output.height), (1440, 1280, 720));
        assert!(OutputInfo::default().output().is_none());
    }
}
