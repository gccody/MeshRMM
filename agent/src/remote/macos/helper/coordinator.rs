//! The coordinator's end of the session helpers. launchd starts a helper in
//! every graphical session, the login window's included; each connects to
//! the coordinator's socket. A remote session uses the helper of the session
//! on the console and moves to another helper when the console changes.
use std::collections::{HashMap, VecDeque};
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use anyhow::{Context, bail};
use meshrmm_protocol::{
    Annotation, ClipboardContent, CursorShape, DisplayId, FileMessage, RemoteInput,
};
use tokio::sync::Notify;

use super::protocol::{
    self, Call, Event, Frame, Hello, InputState, Reply, Request, StreamSettings,
};
use crate::remote::macos::local::Started;
use crate::remote::platform::ScreenInput;

/// How long a helper may take to answer a request. Starting capture is the
/// slowest, when ScreenCaptureKit first lists the displays.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

/// Connected helpers, newest last.
#[derive(Default)]
pub(crate) struct Registry {
    helpers: Mutex<Vec<Arc<Connection>>>,
}

/// Starts accepting helpers on `path`. Only processes running this Agent's
/// executable are accepted.
pub(crate) fn listen(path: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .with_context(|| format!("could not listen for session helpers at {}", path.display()))?;
    // Helpers run as each signed-in user; the peer check below admits only
    // this Agent's executable.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    let executable = std::fs::canonicalize(std::env::current_exe()?)?;
    let registry = Arc::clone(REGISTRY.get_or_init(Arc::default));
    std::thread::Builder::new()
        .name("meshrmm-helper-listener".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "could not accept a session helper");
                        continue;
                    }
                };
                if let Err(error) = registry.admit(stream, &executable) {
                    tracing::warn!(error = ?error, "refused a session helper");
                }
            }
        })?;
    tracing::info!(socket = %path.display(), "listening for session helpers");
    Ok(())
}

pub(crate) fn registry() -> anyhow::Result<Arc<Registry>> {
    REGISTRY
        .get()
        .cloned()
        .context("the coordinator does not accept session helpers")
}

impl Registry {
    fn admit(self: &Arc<Self>, stream: UnixStream, executable: &Path) -> anyhow::Result<()> {
        let (uid, pid) = peer(&stream)?;
        let peer_executable = process_path(pid)?;
        if peer_executable != executable {
            bail!(
                "process {pid} runs {}, not the Agent",
                peer_executable.display()
            );
        }
        stream.set_read_timeout(Some(CALL_TIMEOUT))?;
        let hello: Hello = protocol::read(&mut &stream)?.context("the helper closed at once")?;
        stream.set_read_timeout(None)?;
        if hello.version != protocol::VERSION {
            bail!(
                "helper protocol {} does not match {}",
                hello.version,
                protocol::VERSION
            );
        }
        let connection = Arc::new(Connection {
            uid,
            login_window: hello.login_window,
            writer: Mutex::new(stream.try_clone()?),
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            closed: AtomicBool::new(false),
            session: Mutex::default(),
        });
        tracing::info!(
            uid,
            pid,
            login_window = hello.login_window,
            "session helper connected"
        );
        self.helpers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&connection));
        let registry = Arc::clone(self);
        std::thread::Builder::new()
            .name("meshrmm-helper-reader".into())
            .spawn(move || {
                connection.read_events(BufReader::new(stream));
                registry
                    .helpers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|helper| !Arc::ptr_eq(helper, &connection));
                tracing::info!(
                    uid,
                    login_window = connection.login_window,
                    "session helper disconnected"
                );
            })?;
        Ok(())
    }

    /// The helper of the session on the console.
    fn console_helper(&self) -> anyhow::Result<Arc<Connection>> {
        let console = console_user();
        self.helpers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|helper| match console {
                Some(uid) => helper.uid == uid && !helper.login_window,
                None => helper.login_window,
            })
            .cloned()
            .with_context(|| match console {
                Some(uid) => format!("no session helper is running for the console user {uid}"),
                None => "no session helper is running at the login window".to_owned(),
            })
    }
}

/// The signed-in user on the console, or `None` at the login window.
fn console_user() -> Option<u32> {
    #[link(name = "SystemConfiguration", kind = "framework")]
    unsafe extern "C" {
        fn SCDynamicStoreCopyConsoleUser(
            store: *const std::ffi::c_void,
            uid: *mut u32,
            gid: *mut u32,
        ) -> Option<std::ptr::NonNull<objc2_core_foundation::CFString>>;
    }
    let mut uid = 0;
    // SAFETY: a null store is allowed and both out pointers are valid.
    let name =
        unsafe { SCDynamicStoreCopyConsoleUser(std::ptr::null(), &mut uid, std::ptr::null_mut()) }?;
    // SAFETY: the function returns a +1 reference.
    let name = unsafe { objc2_core_foundation::CFRetained::from_raw(name) };
    (name.to_string() != "loginwindow" && uid != 0).then_some(uid)
}

/// The peer's user ID and process ID.
fn peer(stream: &UnixStream) -> anyhow::Result<(u32, i32)> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: the descriptor is a connected socket and the out pointers are valid.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error()).context("could not identify the helper");
    }
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SAFETY: as above; LOCAL_PEERPID writes a pid_t.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&raw mut pid).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("could not identify the helper");
    }
    Ok((uid, pid))
}

fn process_path(pid: i32) -> anyhow::Result<PathBuf> {
    let mut buffer = vec![0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is as large as the size passed.
    let length =
        unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return Err(std::io::Error::last_os_error())
            .context("could not find the helper's executable");
    }
    buffer.truncate(length as usize);
    Ok(PathBuf::from(String::from_utf8(buffer)?))
}

/// One helper process.
pub(crate) struct Connection {
    uid: u32,
    login_window: bool,
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::SyncSender<Result<Reply, String>>>>,
    closed: AtomicBool,
    /// The session using this helper, which receives its events.
    session: Mutex<Option<Arc<SessionEvents>>>,
}

impl Connection {
    fn read_events(&self, mut reader: BufReader<UnixStream>) {
        loop {
            let event = match protocol::read::<Event>(&mut reader) {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(error) => {
                    tracing::warn!(error = ?error, "lost a session helper");
                    break;
                }
            };
            match event {
                Event::Reply { id, result } => {
                    if let Some(reply) = self
                        .pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id)
                    {
                        let _ = reply.send(result);
                    }
                }
                event => {
                    let session = self
                        .session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if let Some(session) = session {
                        session.receive(event);
                    }
                }
            }
        }
        self.closed.store(true, Ordering::SeqCst);
        // Waiting calls fail at once.
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn call(&self, request: Request) -> anyhow::Result<Reply> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("the session helper disconnected");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sender);
        let written = protocol::write(
            &mut *self.writer.lock().unwrap_or_else(|e| e.into_inner()),
            &Call { id, request },
        );
        if let Err(error) = written {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(error.context("could not reach the session helper"));
        }
        match receiver.recv_timeout(CALL_TIMEOUT) {
            Ok(result) => result.map_err(anyhow::Error::msg),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                bail!("the session helper did not answer in time")
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the session helper disconnected"),
        }
    }
}

/// What a remote session receives from its helper.
#[derive(Default)]
struct SessionEvents {
    frames: Mutex<Option<FrameSink>>,
    capture_ended: Mutex<Option<String>>,
    input_state: Mutex<InputState>,
    clipboard: Mutex<VecDeque<ClipboardContent>>,
    clipboard_ready: Arc<Notify>,
    files: Mutex<VecDeque<FileMessage>>,
    files_ready: Arc<Notify>,
    chat: Mutex<VecDeque<String>>,
    chat_ready: Arc<Notify>,
}

/// Publishes a capture's frames, resynchronizing on a keyframe when the
/// helper had to drop some.
struct FrameSink {
    publish: Box<dyn Fn(Frame) + Send>,
    next_sequence: Option<u64>,
    resynchronizing: bool,
    request_keyframe: Box<dyn Fn() + Send>,
}

impl SessionEvents {
    fn receive(&self, event: Event) {
        match event {
            Event::Frame(sequence, frame) => {
                if let Some(sink) = self
                    .frames
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                {
                    if sink.next_sequence.is_some_and(|next| next != sequence) && !frame.keyframe {
                        if !sink.resynchronizing {
                            (sink.request_keyframe)();
                        }
                        sink.resynchronizing = true;
                    }
                    sink.next_sequence = Some(sequence + 1);
                    if frame.keyframe {
                        sink.resynchronizing = false;
                    }
                    if !sink.resynchronizing {
                        (sink.publish)(frame);
                    }
                }
            }
            Event::CaptureEnded(error) => {
                *self.capture_ended.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
            }
            Event::InputState(state) => {
                *self.input_state.lock().unwrap_or_else(|e| e.into_inner()) = state;
            }
            Event::Clipboard(content) => {
                self.clipboard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(content);
                self.clipboard_ready.notify_one();
            }
            Event::Files(message) => {
                self.files
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(message);
                self.files_ready.notify_one();
            }
            Event::Chat(text) => {
                self.chat
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(text);
                self.chat_ready.notify_one();
            }
            Event::Reply { .. } => {}
        }
    }
}

/// Session state a new helper must be given when the console changes.
#[derive(Default, Clone, Copy)]
struct Desired {
    wallpaper_hidden: bool,
    prevent_idle_lock: bool,
    chat: bool,
}

/// A remote session's view of the console's helper.
pub(crate) struct Remote {
    registry: Arc<Registry>,
    current: Mutex<Option<Arc<Connection>>>,
    events: Arc<SessionEvents>,
    desired: Mutex<Desired>,
}

impl Remote {
    pub(crate) fn new() -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self {
            registry: registry()?,
            current: Mutex::default(),
            events: Arc::default(),
            desired: Mutex::default(),
        }))
    }

    /// The console's helper, moving the session to it when the console changed.
    fn helper(&self) -> anyhow::Result<Arc<Connection>> {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(helper) = current
            .as_ref()
            .filter(|helper| !helper.closed.load(Ordering::SeqCst))
        {
            return Ok(Arc::clone(helper));
        }
        let helper = self.registry.console_helper()?;
        self.detach(current.take());
        *helper.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&self.events));
        helper.call(Request::BeginSession)?;
        let desired = *self.desired.lock().unwrap_or_else(|e| e.into_inner());
        for request in [
            desired
                .wallpaper_hidden
                .then_some(Request::SetWallpaperHidden(true)),
            desired
                .prevent_idle_lock
                .then_some(Request::SetPreventIdleLock(true)),
            desired.chat.then_some(Request::StartChat),
        ]
        .into_iter()
        .flatten()
        {
            if let Err(error) = helper.call(request) {
                tracing::warn!(error = ?error, "could not restore session state in a new helper");
            }
        }
        tracing::info!(
            uid = helper.uid,
            login_window = helper.login_window,
            "remote session uses a session helper"
        );
        *current = Some(Arc::clone(&helper));
        Ok(helper)
    }

    fn detach(&self, helper: Option<Arc<Connection>>) {
        if let Some(helper) = helper {
            let _ = helper
                .session
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if !helper.closed.load(Ordering::SeqCst) {
                let _ = helper.call(Request::EndSession);
            }
        }
    }

    fn call(&self, request: Request) -> anyhow::Result<Reply> {
        self.helper()?.call(request)
    }

    fn done(&self, request: Request) -> anyhow::Result<()> {
        self.call(request).map(|_| ())
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        display_id: Option<DisplayId>,
        settings: StreamSettings,
        publish: impl Fn(Frame) + Send + 'static,
    ) -> anyhow::Result<Started> {
        let helper = self.helper()?;
        let remote = Arc::downgrade(self);
        *self.events.frames.lock().unwrap_or_else(|e| e.into_inner()) = Some(FrameSink {
            publish: Box::new(publish),
            next_sequence: None,
            resynchronizing: false,
            request_keyframe: Box::new(move || {
                if let Some(remote) = remote.upgrade() {
                    let _ = remote.request_keyframe();
                }
            }),
        });
        *self
            .events
            .capture_ended
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        match helper.call(Request::Start {
            display_id,
            settings,
        })? {
            Reply::Started {
                displays,
                active_display,
                format,
            } => Ok(Started {
                displays,
                active_display,
                format,
            }),
            Reply::Done => bail!("the session helper did not start capture"),
        }
    }

    pub(crate) fn stop(&self) -> anyhow::Result<()> {
        *self.events.frames.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let current = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match current {
            Some(helper) if !helper.closed.load(Ordering::SeqCst) => {
                helper.call(Request::StopCapture).map(|_| ())
            }
            _ => Ok(()),
        }
    }

    /// Why capture must restart: the helper stopped capturing or left, or
    /// another session took the console.
    pub(crate) fn poll_ended(&self) -> Option<anyhow::Error> {
        if let Some(error) = self
            .events
            .capture_ended
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Some(anyhow::anyhow!(error));
        }
        let current = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        if current.closed.load(Ordering::SeqCst) {
            return Some(anyhow::anyhow!("the session helper disconnected"));
        }
        let console = self.registry.console_helper().ok();
        if !console.is_some_and(|console| Arc::ptr_eq(&console, &current)) {
            return Some(anyhow::anyhow!("another session took the console"));
        }
        None
    }

    pub(crate) fn request_keyframe(&self) -> anyhow::Result<()> {
        self.done(Request::Keyframe)
    }

    pub(crate) fn set_bitrate(&self, bits_per_second: u32) -> anyhow::Result<()> {
        self.done(Request::SetBitrate(bits_per_second))
    }

    pub(crate) fn set_cursor_capture(&self, enabled: bool) -> anyhow::Result<()> {
        self.done(Request::SetCursorCapture(enabled))
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let current = self
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        self.detach(current);
    }
}

/// The session's input, routed to whichever helper it uses.
pub(crate) struct HelperInput(pub(crate) Arc<Remote>);

impl ScreenInput for HelperInput {
    fn set_wallpaper_hidden(&self, hidden: bool) -> anyhow::Result<()> {
        self.0
            .desired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .wallpaper_hidden = hidden;
        self.0.done(Request::SetWallpaperHidden(hidden))
    }
    fn set_prevent_idle_lock(&self, enabled: bool) -> anyhow::Result<()> {
        self.0
            .desired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .prevent_idle_lock = enabled;
        self.0.done(Request::SetPreventIdleLock(enabled))
    }
    fn set_blackout(&self, enabled: bool) -> anyhow::Result<()> {
        self.0.done(Request::SetBlackout(enabled))
    }
    fn maintenance_state(&self) -> Option<meshrmm_protocol::SessionMessage> {
        None
    }
    fn set_agent_input_blocked(&self, blocked: bool) -> anyhow::Result<()> {
        self.0.done(Request::SetAgentInputBlocked(blocked))
    }
    fn apply_files(&self, message: FileMessage) -> anyhow::Result<()> {
        self.0.done(Request::Files(message))
    }
    fn poll_files(&self) -> Option<FileMessage> {
        self.0
            .events
            .files
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
    }
    fn files_ready(&self) -> Arc<Notify> {
        Arc::clone(&self.0.events.files_ready)
    }
    fn apply(&self, input: RemoteInput) -> anyhow::Result<()> {
        self.0.done(Request::Input(input))
    }
    fn annotate(&self, annotation: Annotation) -> anyhow::Result<()> {
        self.0.done(Request::Annotate(annotation))
    }
    fn release_all(&self) -> anyhow::Result<()> {
        self.0.done(Request::ReleaseInput)
    }
    fn cursor_shape(&self) -> CursorShape {
        self.0
            .events
            .input_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cursor
    }
    fn agent_pointer_display(&self) -> Option<DisplayId> {
        self.0
            .events
            .input_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .agent_pointer_display
    }
    fn viewer_controls_input(&self) -> bool {
        self.0
            .events
            .input_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .viewer_controls_input
    }
    fn apply_clipboard(&self, content: ClipboardContent) -> anyhow::Result<()> {
        self.0.done(Request::Clipboard(content))
    }
    fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>> {
        Ok(self
            .0
            .events
            .clipboard
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front())
    }
    fn clipboard_ready(&self) -> Option<Arc<Notify>> {
        Some(Arc::clone(&self.0.events.clipboard_ready))
    }
    fn start_chat(&self) -> anyhow::Result<()> {
        self.0
            .desired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chat = true;
        self.0.done(Request::StartChat)
    }
    fn stop_chat(&self) {
        self.0
            .desired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .chat = false;
        let _ = self.0.done(Request::StopChat);
    }
    fn apply_chat(&self, text: String) -> anyhow::Result<()> {
        self.0.done(Request::Chat(text))
    }
    fn poll_chat(&self) -> anyhow::Result<Option<String>> {
        Ok(self
            .0
            .events
            .chat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front())
    }
    fn chat_ready(&self) -> Arc<Notify> {
        Arc::clone(&self.0.events.chat_ready)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a helper in this process against a coordinator socket and streams
    /// the console through it. Needs the Screen Recording permission.
    #[test]
    #[ignore = "captures the screen through a session helper"]
    fn streams_the_console_through_a_session_helper() {
        let socket =
            std::env::temp_dir().join(format!("meshrmm-helper-{}.sock", std::process::id()));
        listen(&socket).unwrap();
        let helper_socket = socket.clone();
        std::thread::spawn(move || super::super::host::run(&helper_socket));
        let remote = Remote::new().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while remote.registry.console_helper().is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the helper never connected"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let (sender, frames) = mpsc::channel();
        let started = remote
            .start(
                None,
                StreamSettings {
                    frames_per_second: 30,
                    bitrate_bits_per_second: 4_000_000,
                    codec: meshrmm_protocol::Codec::H264,
                    capture_cursor: true,
                    grayscale: false,
                },
                move |frame| {
                    let _ = sender.send(frame);
                },
            )
            .unwrap();
        assert!(started.format.width > 0);
        let first = frames.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(first.keyframe);
        remote.request_keyframe().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut keyframes = 0;
        while std::time::Instant::now() < deadline {
            if let Ok(frame) = frames.recv_timeout(Duration::from_millis(100)) {
                keyframes += usize::from(frame.keyframe);
            }
        }
        assert!(keyframes >= 1, "a requested keyframe arrives");
        assert!(remote.poll_ended().is_none());
        HelperInput(Arc::clone(&remote))
            .set_prevent_idle_lock(true)
            .unwrap();
        remote.stop().unwrap();
        drop(remote);
        let _ = std::fs::remove_file(socket);
    }
}
