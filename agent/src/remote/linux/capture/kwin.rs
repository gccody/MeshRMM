//! KDE Plasma Wayland capture through KWin's screencast protocol, the one
//! KDE's own remote desktop server uses. KWin offers it only to programs a
//! desktop file authorizes, which the Agent's installer adds; it then sends
//! the output's frames as a PipeWire stream, only when the screen changes.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use wayland_protocols_plasma::screencast::v1::client::zkde_screencast_unstable_v1::Pointer;

use super::wayland::Client;
use super::{Backend, CaptureSettings, FrameSink, Stream, pipewire};
use crate::remote::linux::display::Output;

pub(crate) const SCREENCAST_GLOBAL: &str = "zkde_screencast_unstable_v1";
const START_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(200);

pub(crate) struct KWin;

impl KWin {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl Backend for KWin {
    fn name(&self) -> &'static str {
        "KDE Plasma (KWin)"
    }

    fn outputs(&mut self) -> anyhow::Result<Vec<Output>> {
        let outputs = Client::connect()?.outputs();
        anyhow::ensure!(!outputs.is_empty(), "KWin reported no displays");
        Ok(outputs)
    }

    fn start(
        &mut self,
        output: &Output,
        settings: CaptureSettings,
        sink: FrameSink,
    ) -> anyhow::Result<Box<dyn Stream>> {
        let shared = Arc::new(Shared::default());
        let (started_sender, started) = mpsc::channel();
        let (worker, name) = (Arc::clone(&shared), output.name.clone());
        let thread = std::thread::Builder::new()
            .name("meshrmm-kwin-capture".into())
            .spawn(move || {
                let result = capture(&worker, &name, settings, sink, &started_sender);
                if let Err(error) = result {
                    let reason = format!("{error:#}");
                    if started_sender.send(Err(reason.clone())).is_err() {
                        *worker.ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
                    }
                }
            })
            .context("could not start KWin capture")?;
        let stream = KWinStream {
            shared,
            thread: Some(thread),
        };
        match started.recv_timeout(START_TIMEOUT) {
            Ok(Ok(())) => Ok(Box::new(stream)),
            Ok(Err(error)) => bail!(error),
            Err(_) => bail!("KWin did not start the screencast"),
        }
    }
}

#[derive(Default)]
struct Shared {
    stopped: AtomicBool,
    ended: Mutex<Option<String>>,
}

/// Runs the screencast until it is stopped or ends.
fn capture(
    shared: &Shared,
    name: &str,
    settings: CaptureSettings,
    sink: FrameSink,
    started: &mpsc::Sender<Result<(), String>>,
) -> anyhow::Result<()> {
    let mut client = Client::connect()?;
    let output = client
        .output_proxy(name)
        .with_context(|| format!("KWin has no display named {name}"))?;
    let screencast = client.state.kde_screencast.clone().context(
        "KWin did not offer its screencast interface; reinstall the MeshRMM Agent so KDE authorizes it",
    )?;
    let pointer = if settings.capture_cursor {
        Pointer::Embedded
    } else {
        Pointer::Hidden
    };
    let handle = client.handle();
    let stream = screencast.stream_output(&output, pointer as u32, &handle, ());
    let deadline = Instant::now() + START_TIMEOUT;
    let node = loop {
        client.dispatch(POLL)?;
        if let Some(node) = client.state.kde_stream.node {
            break node;
        }
        if let Some(error) = client.state.kde_stream.failed.take() {
            bail!("KWin could not start the screencast: {error}");
        }
        if Instant::now() >= deadline {
            bail!("KWin did not start the screencast");
        }
    };
    tracing::info!(node, output = name, "KWin started a screencast");
    let pipewire = pipewire::Stream::connect(None, node, sink)?;
    let _ = started.send(Ok(()));
    let ended = loop {
        if shared.stopped.load(Ordering::SeqCst) {
            break None;
        }
        if let Err(error) = client.dispatch(POLL) {
            break Some(format!("{error:#}"));
        }
        if client.state.kde_stream.closed {
            break Some("KWin ended the screencast".to_owned());
        }
        if let Some(reason) = pipewire.ended() {
            break Some(reason);
        }
    };
    drop(pipewire);
    stream.close();
    let _ = client.connection.flush();
    if let Some(reason) = ended {
        *shared.ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
    }
    Ok(())
}

struct KWinStream {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Stream for KWinStream {
    fn poll_ended(&mut self) -> Option<anyhow::Error> {
        self.shared
            .ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map(|reason| anyhow::anyhow!(reason))
    }

    /// KWin fixes the pointer mode when the screencast starts.
    fn set_cursor_capture(&mut self, _enabled: bool) -> bool {
        false
    }
}

impl Drop for KWinStream {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
