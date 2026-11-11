//! The coordinator's end of the session helpers. Every graphical session,
//! the login window's included, has a helper: on macOS launchd starts them
//! and each connects to the coordinator's socket, and on Linux the
//! coordinator starts them over a socket pair. A remote session uses the
//! helper of the session on the console and moves to another helper when the
//! console changes.
use std::collections::{HashMap, VecDeque};
use std::io::BufReader;
#[cfg(target_os = "macos")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "macos")]
use std::os::unix::io::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::net::UnixListener;
use std::os::unix::net::UnixStream;
#[cfg(target_os = "macos")]
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use meshrmm_protocol::{
    Annotation, ClipboardContent, CursorShape, DisplayId, FileMessage, RemoteInput,
};
use tokio::sync::Notify;

use super::platform::local::Started;
use super::protocol::{
    self, Call, Event, Frame, Hello, InputState, Reply, Request, SessionUi, StreamSettings,
};
use crate::remote::connection_approval::{ApprovalPrompt, Decision};
use crate::remote::platform::ScreenInput;
use crate::remote::thumbnail::{Capture, Signature};

/// How long a helper may take to answer a request. Starting capture is the
/// slowest, when ScreenCaptureKit first lists the displays.
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a capture that lost frames waits for the keyframe it asked for
/// before asking again: the helper drops frames when its queue is full, the
/// keyframe among them.
const KEYFRAME_RETRY: Duration = Duration::from_secs(1);

static REGISTRY: OnceLock<Arc<Registry>> = OnceLock::new();

/// Connected helpers, newest last.
pub(crate) struct Registry {
    helpers: Mutex<Vec<Arc<Connection>>>,
    /// Who holds the console: the platform's answer, or a test's.
    console: Box<dyn Fn() -> Console + Send + Sync>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            helpers: Mutex::default(),
            console: Box::new(Console::now),
        }
    }
}

/// Who holds the console, whose helper a remote session uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Console {
    /// The logind session active on seat0, a login screen's included.
    #[cfg(target_os = "linux")]
    Session(String),
    /// Seat0 shows no graphical session, such as a text console.
    #[cfg(target_os = "linux")]
    Nobody,
    /// The signed-in user whose session is on the console.
    #[cfg(target_os = "macos")]
    User(u32),
    /// The login window, as when fast user switching shows it.
    #[cfg(target_os = "macos")]
    LoginWindow,
}

impl Console {
    /// The session supervisor's reading of logind, taken every second.
    #[cfg(target_os = "linux")]
    fn now() -> Self {
        super::platform::sessions::console_session().map_or(Self::Nobody, Self::Session)
    }

    #[cfg(target_os = "macos")]
    fn now() -> Self {
        super::platform::session_close::console_user().map_or(Self::LoginWindow, Self::User)
    }

    /// Whether `helper` runs in the session on the console.
    fn served_by(&self, helper: &Connection) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Self::Session(id) => helper.login_session.as_deref() == Some(id.as_str()),
            #[cfg(target_os = "linux")]
            Self::Nobody => false,
            #[cfg(target_os = "macos")]
            Self::User(uid) => helper.uid == *uid && !helper.login_window,
            #[cfg(target_os = "macos")]
            Self::LoginWindow => helper.login_window,
        }
    }

    fn no_helper(&self) -> anyhow::Error {
        match self {
            #[cfg(target_os = "linux")]
            Self::Session(id) => anyhow::anyhow!("no session helper is running in session {id}"),
            #[cfg(target_os = "linux")]
            Self::Nobody => anyhow::anyhow!("no graphical session is on the console"),
            #[cfg(target_os = "macos")]
            Self::User(uid) => {
                anyhow::anyhow!("no session helper is running for the console user {uid}")
            }
            #[cfg(target_os = "macos")]
            Self::LoginWindow => {
                anyhow::anyhow!("no session helper is running at the login window")
            }
        }
    }

    /// What the technician is told while the console's session has no helper.
    fn waiting_notice(&self) -> String {
        #[cfg(target_os = "linux")]
        if *self == Self::Nobody {
            return "No graphical session is on the console. The remote session shows the \
                console again when one is."
                .to_owned();
        }
        "Another session took the console and the Agent cannot show it yet. The remote \
            session moves to it as soon as it can."
            .to_owned()
    }
}

/// Starts accepting helpers on `path`. Only processes running this Agent's
/// executable are accepted.
#[cfg(target_os = "macos")]
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

/// The registry, created for a coordinator that starts its own helpers.
#[cfg(target_os = "linux")]
pub(crate) fn create_registry() -> Arc<Registry> {
    Arc::clone(REGISTRY.get_or_init(Arc::default))
}

impl Registry {
    #[cfg(target_os = "macos")]
    fn admit(self: &Arc<Self>, stream: UnixStream, executable: &Path) -> anyhow::Result<()> {
        let (uid, pid) = peer(&stream)?;
        let peer_executable = process_path(pid)?;
        if peer_executable != executable {
            bail!(
                "process {pid} runs {}, not the Agent",
                peer_executable.display()
            );
        }
        self.attach(stream, uid, None).map(|_| ())
    }

    /// Adds the helper at the other end of `stream`, which runs as `uid` in
    /// `login_session` (logind's ID, on Linux), once it has said hello.
    pub(crate) fn attach(
        self: &Arc<Self>,
        stream: UnixStream,
        uid: u32,
        login_session: Option<String>,
    ) -> anyhow::Result<Arc<Connection>> {
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
            login_session,
            writer: Mutex::new(stream.try_clone()?),
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            closed: AtomicBool::new(false),
            reader: OnceLock::new(),
            session: Mutex::default(),
            approval: Mutex::default(),
        });
        tracing::info!(
            uid,
            login_window = hello.login_window,
            login_session = connection.login_session.as_deref(),
            "session helper connected"
        );
        self.helpers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&connection));
        let registry = Arc::clone(self);
        let reader = Arc::clone(&connection);
        std::thread::Builder::new()
            .name("meshrmm-helper-reader".into())
            .spawn(move || {
                reader.read_events(BufReader::new(stream));
                registry
                    .helpers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .retain(|helper| !Arc::ptr_eq(helper, &reader));
                tracing::info!(
                    uid,
                    login_window = reader.login_window,
                    login_session = reader.login_session.as_deref(),
                    "session helper disconnected"
                );
            })?;
        Ok(connection)
    }

    /// The helper of `uid`'s graphical session.
    pub(crate) fn user_helper(&self, uid: u32) -> anyhow::Result<Arc<Connection>> {
        self.helpers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|helper| helper.uid == uid && !helper.login_window)
            .cloned()
            .with_context(|| format!("no session helper is running for user {uid}"))
    }

    /// A JPEG of the console's main display, unless it looks like
    /// `previous`, from its session helper.
    pub(crate) fn thumbnail(&self, previous: Option<&Signature>) -> anyhow::Result<Capture> {
        match self
            .console_helper()?
            .call(Request::Thumbnail(previous.cloned()))?
        {
            Reply::Thumbnail(capture) => Ok(capture),
            reply => bail!("the session helper answered a thumbnail with {reply:?}"),
        }
    }

    /// The helper of the session on the console.
    fn console_helper(&self) -> anyhow::Result<Arc<Connection>> {
        let console = (self.console)();
        self.helper_of(&console).ok_or_else(|| console.no_helper())
    }

    /// The newest connected helper of the session `console` names.
    fn helper_of(&self, console: &Console) -> Option<Arc<Connection>> {
        self.helpers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .rev()
            .find(|helper| console.served_by(helper) && !helper.closed.load(Ordering::SeqCst))
            .cloned()
    }
}

/// The peer's user ID and process ID.
#[cfg(target_os = "macos")]
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

#[cfg(target_os = "macos")]
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
    /// The logind session the helper serves, on Linux.
    login_session: Option<String>,
    writer: Mutex<UnixStream>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, mpsc::SyncSender<Result<Reply, String>>>>,
    closed: AtomicBool,
    /// The thread reading the helper's events, the only one that delivers
    /// replies, so it must never wait for one.
    reader: OnceLock<ThreadId>,
    /// The session using this helper, which receives its events.
    session: Mutex<Option<Arc<SessionEvents>>>,
    /// Where an approval prompt's answer goes.
    approval: Mutex<Option<tokio::sync::oneshot::Sender<Decision>>>,
}

impl Connection {
    fn read_events(self: &Arc<Self>, mut reader: BufReader<UnixStream>) {
        let _ = self.reader.set(std::thread::current().id());
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
                Event::ApprovalDecision(byte) => {
                    if let (Some(answer), Some(decision)) = (
                        self.approval
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take(),
                        Decision::from_byte(byte),
                    ) {
                        let _ = answer.send(decision);
                    }
                }
                event => {
                    let session = self
                        .session
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    if session.is_some_and(|session| session.receive(event, Instant::now())) {
                        self.request_keyframe();
                    }
                }
            }
        }
        self.closed.store(true, Ordering::SeqCst);
        // A prompt nobody can answer any more falls back to the policy.
        self.approval
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // Waiting calls fail at once.
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Whether the helper went away.
    #[cfg(target_os = "linux")]
    pub(crate) fn closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Runs `request` and waits for the helper to finish it.
    pub(crate) fn run(&self, request: Request) -> anyhow::Result<()> {
        self.call(request).map(|_| ())
    }

    /// Sends `request` without waiting for the helper's reply, which the
    /// reader drops as nobody's.
    fn notify(&self, request: Request) {
        let _ = self.write(&Call {
            id: self.next_id.fetch_add(1, Ordering::Relaxed),
            request,
        });
    }

    /// Asks the helper for a keyframe on the reader's behalf, from a thread
    /// of its own: the reader must not wait even for the writer, which a
    /// caller's write the helper is not reading yet can hold while the helper
    /// waits to send its replies.
    fn request_keyframe(self: &Arc<Self>) {
        let helper = Arc::clone(self);
        if let Err(error) = std::thread::Builder::new()
            .name("meshrmm-helper-keyframe".into())
            .spawn(move || helper.notify(Request::Keyframe))
        {
            tracing::warn!(%error, "could not ask the session helper for a keyframe");
        }
    }

    /// Writes `call`. A failed write may have sent half of it, so it ends the
    /// connection: the reader sees the end and the session moves on as when
    /// the helper disconnects.
    fn write(&self, call: &Call) -> anyhow::Result<()> {
        let mut writer = self.writer.lock().unwrap_or_else(|e| e.into_inner());
        let written = protocol::write(&mut *writer, call);
        if written.is_err() {
            let _ = writer.shutdown(std::net::Shutdown::Both);
        }
        written
    }

    fn call(&self, request: Request) -> anyhow::Result<Reply> {
        if self.reader.get() == Some(&std::thread::current().id()) {
            #[cfg(not(test))]
            debug_assert!(false, "the session helper's reader waited for a reply");
            bail!("the session helper's reader cannot wait for a reply");
        }
        if self.closed.load(Ordering::SeqCst) {
            bail!("the session helper disconnected");
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::sync_channel(1);
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sender);
        if let Err(error) = self.write(&Call { id, request }) {
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
    audio: Mutex<Option<crate::remote::platform::AudioSink>>,
    audio_ended: AtomicBool,
}

/// Publishes a capture's frames, resynchronizing on a keyframe when the
/// helper had to drop some.
struct FrameSink {
    publish: Box<dyn Fn(Frame) + Send>,
    next_sequence: Option<u64>,
    resynchronizing: bool,
    /// When the sink last asked for a keyframe.
    keyframe_requested: Option<Instant>,
}

impl FrameSink {
    fn new(publish: impl Fn(Frame) + Send + 'static) -> Self {
        Self {
            publish: Box::new(publish),
            next_sequence: None,
            resynchronizing: false,
            keyframe_requested: None,
        }
    }

    /// Publishes frame `sequence` unless frames were lost before it, after
    /// which only a keyframe publishes again. Returns whether to ask the
    /// helper for one: when frames were lost, and again while the keyframe
    /// does not come, but at most once every `KEYFRAME_RETRY`.
    fn receive(&mut self, sequence: u64, frame: Frame, now: Instant) -> bool {
        if self.next_sequence.is_some_and(|next| next != sequence) {
            self.resynchronizing = true;
        }
        self.next_sequence = Some(sequence + 1);
        if frame.keyframe {
            self.resynchronizing = false;
        }
        if !self.resynchronizing {
            (self.publish)(frame);
            return false;
        }
        let request = self
            .keyframe_requested
            .is_none_or(|requested| now.duration_since(requested) >= KEYFRAME_RETRY);
        if request {
            self.keyframe_requested = Some(now);
        }
        request
    }
}

impl SessionEvents {
    /// Takes in `event`, received at `now`. Returns whether to ask the helper
    /// for a keyframe, which the caller does once the frames are unlocked.
    fn receive(&self, event: Event, now: Instant) -> bool {
        match event {
            Event::Frame(sequence, frame) => {
                return self
                    .frames
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_mut()
                    .is_some_and(|sink| sink.receive(sequence, frame, now));
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
            Event::Audio(packet) => {
                if let Some(send) = self
                    .audio
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    send(packet);
                }
            }
            Event::AudioEnded => self.audio_ended.store(true, Ordering::SeqCst),
            Event::Reply { .. } | Event::ApprovalDecision(_) => {}
        }
        false
    }
}

/// Session state a new helper must be given when the console changes.
#[derive(Default, Clone, Copy)]
struct Desired {
    wallpaper_hidden: bool,
    prevent_idle_lock: bool,
    chat: bool,
    display_border: bool,
}

/// A remote session's view of the console's helper. The session follows the
/// console: when another session takes it, the session leaves the helper it
/// used, undoing there what it changed, and moves to the console's helper, or
/// waits for one while the console's session has none.
pub(crate) struct Remote {
    registry: Arc<Registry>,
    current: Mutex<Option<Arc<Connection>>>,
    /// The helper capture last started on, until it stops.
    capturing: Mutex<Option<Arc<Connection>>>,
    events: Arc<SessionEvents>,
    desired: Mutex<Desired>,
    /// What every helper the session uses shows; the notification only once.
    ui: Mutex<SessionUi>,
    /// Set when the session left a session that lost the console, until
    /// capture starts in another: a display chosen there does not carry over.
    moved: AtomicBool,
    /// The console whose session the remote session waits to have a helper.
    waiting: Mutex<Option<Console>>,
    /// What the technician is still to be told.
    notice: Mutex<Option<String>>,
    /// The locked GNOME session the remote session unlocked, locked again
    /// when the remote session ends or leaves it.
    #[cfg(target_os = "linux")]
    unlocked: Mutex<Option<String>>,
    /// Locks a logind session, through logind outside tests.
    #[cfg(target_os = "linux")]
    lock_session: LockSession,
}

#[cfg(target_os = "linux")]
type LockSession = Box<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;

impl Remote {
    pub(crate) fn new(ui: SessionUi) -> anyhow::Result<Arc<Self>> {
        Ok(Arc::new(Self::with_registry(registry()?, ui)))
    }

    fn with_registry(registry: Arc<Registry>, ui: SessionUi) -> Self {
        Self {
            registry,
            current: Mutex::default(),
            capturing: Mutex::default(),
            events: Arc::default(),
            desired: Mutex::default(),
            ui: Mutex::new(ui),
            moved: AtomicBool::new(false),
            waiting: Mutex::default(),
            notice: Mutex::default(),
            #[cfg(target_os = "linux")]
            unlocked: Mutex::default(),
            #[cfg(target_os = "linux")]
            lock_session: Box::new(|session| {
                super::platform::sessions::Logind::connect()?.lock_session(session)
            }),
        }
    }

    /// The helper the session uses, or the console's when it has none or
    /// its helper left. Input stays with the helper the session uses until
    /// capture's next check, at most 100 ms, sees the console move.
    fn helper(&self) -> anyhow::Result<Arc<Connection>> {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(helper) = current
            .as_ref()
            .filter(|helper| !helper.closed.load(Ordering::SeqCst))
        {
            return Ok(Arc::clone(helper));
        }
        let console = (self.registry.console)();
        self.leave(current.take(), &console);
        self.attach(&mut current, console)
    }

    /// The console's helper, leaving the one the session uses when its
    /// session no longer has the console.
    fn follow_console(&self) -> anyhow::Result<Arc<Connection>> {
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        let console = (self.registry.console)();
        if let Some(helper) = current
            .as_ref()
            .filter(|helper| !helper.closed.load(Ordering::SeqCst) && console.served_by(helper))
        {
            return Ok(Arc::clone(helper));
        }
        self.leave(current.take(), &console);
        self.attach(&mut current, console)
    }

    /// Begins the session in the helper of `console`'s session, or, when it
    /// has none yet, tells the technician once that the session waits for it.
    fn attach(
        &self,
        current: &mut Option<Arc<Connection>>,
        console: Console,
    ) -> anyhow::Result<Arc<Connection>> {
        let Some(helper) = self.registry.helper_of(&console) else {
            let error = console.no_helper();
            let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
            if waiting.as_ref() != Some(&console) {
                tracing::info!(
                    ?console,
                    "the console's session has no session helper yet; waiting for it"
                );
                *self.notice.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(console.waiting_notice());
                *waiting = Some(console);
            }
            return Err(error);
        };
        self.waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        *helper.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::clone(&self.events));
        let ui = {
            let mut ui = self.ui.lock().unwrap_or_else(|e| e.into_inner());
            let shown = ui.clone();
            ui.notification = None;
            shown
        };
        helper.call(Request::BeginSession(ui))?;
        let desired = *self.desired.lock().unwrap_or_else(|e| e.into_inner());
        for request in [
            desired
                .wallpaper_hidden
                .then_some(Request::SetWallpaperHidden(true)),
            desired
                .prevent_idle_lock
                .then_some(Request::SetPreventIdleLock(true)),
            desired.chat.then_some(Request::StartChat),
            desired
                .display_border
                .then_some(Request::SetDisplayBorder(true)),
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

    /// Stops using `helper`, whose session's helper left or which is not on
    /// `console` any more. Ending the session there stops its capture and
    /// audio and releases what the session held or changed in it: keys and
    /// buttons, the idle-lock inhibition, the wallpaper, a Mac's blackout and
    /// blocked input, chat and file transfers. A Linux computer's keyboard
    /// and pointer are blocked by the coordinator, for the whole computer,
    /// and stay blocked. A session unlocked for the technician is locked
    /// again once it lost the console.
    fn leave(&self, helper: Option<Arc<Connection>>, console: &Console) {
        let Some(helper) = helper else {
            return;
        };
        let lost_console = !console.served_by(&helper);
        self.detach(&helper);
        *self
            .events
            .input_state
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = InputState::default();
        if !lost_console {
            return;
        }
        self.moved.store(true, Ordering::SeqCst);
        tracing::info!(
            uid = helper.uid,
            login_window = helper.login_window,
            login_session = helper.login_session.as_deref(),
            ?console,
            "remote session left a session helper whose session lost the console"
        );
        #[cfg(target_os = "linux")]
        {
            let mut unlocked = self.unlocked.lock().unwrap_or_else(|e| e.into_inner());
            if unlocked.is_some() && *unlocked == helper.login_session {
                self.lock_again(unlocked.take());
            }
        }
    }

    fn detach(&self, helper: &Connection) {
        let _ = helper
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if !helper.closed.load(Ordering::SeqCst) {
            let _ = helper.call(Request::EndSession);
        }
    }

    /// Locks `session` again, which the remote session unlocked.
    #[cfg(target_os = "linux")]
    fn lock_again(&self, session: Option<String>) {
        let Some(session) = session else {
            return;
        };
        match (self.lock_session)(&session) {
            Ok(()) => tracing::info!(session, "locked the GNOME session the technician unlocked"),
            Err(error) => {
                tracing::warn!(session, error = ?error, "could not lock the session again");
            }
        }
    }

    /// Runs `request` in the session's helper. While the console's session
    /// has no helper yet, the request has nowhere to go and is dropped:
    /// what the session must restore in the next helper is in `desired`.
    fn done(&self, request: Request) -> anyhow::Result<()> {
        match self.helper() {
            Ok(helper) => helper.run(request),
            Err(error)
                if self
                    .waiting
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_some() =>
            {
                tracing::debug!(error = ?error, "dropped a request while the console's session has no helper");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Starts capture in the console's helper. After the session moved to
    /// another session, capture shows that session's main display.
    pub(crate) fn start(
        self: &Arc<Self>,
        display_id: Option<DisplayId>,
        settings: StreamSettings,
        publish: impl Fn(Frame) + Send + 'static,
    ) -> anyhow::Result<Started> {
        let helper = self.follow_console()?;
        let display_id = display_id.filter(|_| !self.moved.load(Ordering::SeqCst));
        *self.events.frames.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(FrameSink::new(publish));
        *self
            .events
            .capture_ended
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        let started = helper.call(Request::Start {
            display_id,
            settings,
        });
        #[cfg(target_os = "linux")]
        let started = self.start_unlocked(&helper, started, display_id, settings);
        match started? {
            Reply::Started {
                displays,
                active_display,
                format,
            } => {
                self.moved.store(false, Ordering::SeqCst);
                *self.capturing.lock().unwrap_or_else(|e| e.into_inner()) = Some(helper);
                Ok(Started {
                    displays,
                    active_display,
                    format,
                })
            }
            _ => bail!("the session helper did not start capture"),
        }
    }

    /// GNOME on Wayland refuses to capture a locked session, which would
    /// leave the technician unable to see it. The coordinator unlocks the
    /// session through logind, as root may, and tries again; the session is
    /// locked again when the remote session ends. A login screen has no
    /// session to unlock, so its refusal stands.
    #[cfg(target_os = "linux")]
    fn start_unlocked(
        &self,
        helper: &Connection,
        started: anyhow::Result<Reply>,
        display_id: Option<DisplayId>,
        settings: StreamSettings,
    ) -> anyhow::Result<Reply> {
        let refused = started.as_ref().is_err_and(|error| {
            error
                .to_string()
                .starts_with(super::platform::GNOME_CAPTURE_REFUSED)
        });
        let Some(session) = helper
            .login_session
            .clone()
            .filter(|_| refused && !helper.login_window)
        else {
            return started;
        };
        super::platform::sessions::Logind::connect()?
            .unlock_session(&session)
            .context("could not unlock the session for the technician")?;
        tracing::info!(
            session,
            "unlocked the GNOME session so the technician can see it"
        );
        *self.unlocked.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
        // GNOME Shell lifts its inhibition once the lock screen is gone.
        let mut started = started;
        for _ in 0..10 {
            std::thread::sleep(Duration::from_millis(500));
            started = helper.call(Request::Start {
                display_id,
                settings,
            });
            if started.is_ok() {
                break;
            }
        }
        started
    }

    pub(crate) fn stop(&self) -> anyhow::Result<()> {
        *self.events.frames.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.capturing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
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

    /// Why capture must restart: the helper stopped capturing or left, the
    /// session moved to another helper, or another session took the console.
    /// The last leaves the session's helper at once, so that neither the
    /// picture nor input stays with a session that is off the console; the
    /// restart then moves to the console's helper. Capture control asks every
    /// 100 ms; on Linux the console is logind's as the session supervisor
    /// reads it every second, on macOS the console user at the time.
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
        let capturing = self
            .capturing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        if capturing.closed.load(Ordering::SeqCst) {
            return Some(anyhow::anyhow!("the session helper disconnected"));
        }
        let mut current = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if !current
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &capturing))
        {
            return Some(anyhow::anyhow!(
                "the session moved to another session helper"
            ));
        }
        let console = (self.registry.console)();
        if !console.served_by(&capturing) {
            self.leave(current.take(), &console);
            self.capturing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            return Some(anyhow::anyhow!("another session took the console"));
        }
        None
    }

    pub(crate) fn set_display_border(&self, enabled: bool) -> anyhow::Result<()> {
        self.desired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .display_border = enabled;
        self.done(Request::SetDisplayBorder(enabled))
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
        if let Some(helper) = current {
            self.detach(&helper);
        }
        #[cfg(target_os = "linux")]
        if let Err(error) = super::platform::input_block::set_blocked(false) {
            tracing::warn!(error = ?error, "could not unblock the local keyboard and pointer");
        }
        #[cfg(target_os = "linux")]
        self.lock_again(
            self.unlocked
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
    }
}

/// The session's input, routed to whichever helper it uses.
pub(crate) struct HelperInput(pub(crate) Arc<Remote>);

/// A connection approval prompt shown by the console's helper.
pub(crate) struct ApprovalRequest {
    helper: Arc<Connection>,
    answer: tokio::sync::oneshot::Receiver<Decision>,
}

impl ApprovalRequest {
    pub(crate) fn start(prompt: &ApprovalPrompt) -> anyhow::Result<Self> {
        let helper = registry()?.console_helper()?;
        let (sender, answer) = tokio::sync::oneshot::channel();
        *helper.approval.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender);
        helper.call(Request::PromptApproval {
            text: prompt.text.clone(),
            reason: prompt.reason.clone(),
            timeout_seconds: prompt.timeout.as_secs() as u32,
            lock_idle_seconds: prompt.lock_idle.as_secs() as u32,
        })?;
        Ok(Self { helper, answer })
    }

    pub(crate) async fn answer(&mut self) -> anyhow::Result<Decision> {
        (&mut self.answer)
            .await
            .map_err(|_| anyhow::anyhow!("the session helper left before the user answered"))
    }
}

impl Drop for ApprovalRequest {
    fn drop(&mut self) {
        if !self.helper.closed.load(Ordering::SeqCst) {
            self.helper.notify(Request::CancelApproval);
        }
    }
}

/// System audio a helper captures. It stops being healthy when the session
/// moves to another helper, which makes the transport start it there.
struct HelperAudio {
    remote: Arc<Remote>,
    helper: Arc<Connection>,
}

impl crate::remote::platform::AudioStream for HelperAudio {
    fn healthy(&self) -> bool {
        !self.helper.closed.load(Ordering::SeqCst)
            && !self.remote.events.audio_ended.load(Ordering::SeqCst)
            && self
                .remote
                .current
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.helper))
    }
}

impl Drop for HelperAudio {
    fn drop(&mut self) {
        self.remote
            .events
            .audio
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if !self.helper.closed.load(Ordering::SeqCst) {
            let _ = self.helper.call(Request::StopAudio);
        }
    }
}

impl ScreenInput for HelperInput {
    fn start_audio(
        &self,
        send: crate::remote::platform::AudioSink,
    ) -> anyhow::Result<Box<dyn crate::remote::platform::AudioStream>> {
        let helper = self.0.helper()?;
        *self
            .0
            .events
            .audio
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(send);
        self.0.events.audio_ended.store(false, Ordering::SeqCst);
        if let Err(error) = helper.call(Request::StartAudio) {
            self.0
                .events
                .audio
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            return Err(error);
        }
        Ok(Box::new(HelperAudio {
            remote: Arc::clone(&self.0),
            helper,
        }))
    }

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
        if let Some(reason) = self
            .0
            .notice
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Some(meshrmm_protocol::SessionMessage::MaintenanceError { reason });
        }
        let state = *self
            .0
            .events
            .input_state
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Only root can grab a Linux computer's keyboard and pointer, so the
        // coordinator blocks them itself.
        #[cfg(target_os = "linux")]
        let agent_input_blocked = super::platform::input_block::blocked();
        #[cfg(target_os = "macos")]
        let agent_input_blocked = state.agent_input_blocked;
        Some(meshrmm_protocol::SessionMessage::MaintenanceState {
            agent_input_blocked,
            blacked_out: state.blacked_out,
        })
    }
    fn set_agent_input_blocked(&self, blocked: bool) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            self.0.done(Request::ReleaseInput)?;
            super::platform::input_block::set_blocked(blocked)
        }
        #[cfg(target_os = "macos")]
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

    /// How long the tests wait for what should happen at once, well under
    /// `CALL_TIMEOUT`.
    const PROMPTLY: Duration = Duration::from_secs(2);

    fn frame(keyframe: bool) -> Frame {
        Frame {
            data: Vec::new(),
            keyframe,
            capture_timestamp_us: 0,
            encode_complete_timestamp_us: 0,
        }
    }

    /// A connection to a fake helper, and the helper's end of it.
    fn connect() -> (Arc<Connection>, UnixStream) {
        let (coordinator, helper) = UnixStream::pair().unwrap();
        send(
            &helper,
            &Hello {
                version: protocol::VERSION,
                login_window: false,
            },
        );
        let connection = Arc::new(Registry::default())
            .attach(coordinator, 501, None)
            .unwrap();
        helper.set_read_timeout(Some(PROMPTLY)).unwrap();
        (connection, helper)
    }

    /// Starts a session on `connection` whose frames go to `publish`.
    fn begin(connection: &Connection, publish: impl Fn(Frame) + Send + 'static) {
        let events = Arc::new(SessionEvents::default());
        *events.frames.lock().unwrap() = Some(FrameSink::new(publish));
        *connection.session.lock().unwrap() = Some(events);
    }

    fn send(mut helper: &UnixStream, message: &impl serde::Serialize) {
        protocol::write(&mut helper, message).unwrap();
    }

    #[test]
    fn a_frame_gap_asks_for_a_keyframe_and_the_reader_keeps_going() {
        let (connection, helper) = connect();
        let (published, frames) = mpsc::channel();
        begin(&connection, move |frame| {
            let _ = published.send(frame.keyframe);
        });
        send(&helper, &Event::Frame(0, frame(true)));
        send(&helper, &Event::Frame(2, frame(false)));
        assert_eq!(frames.recv_timeout(PROMPTLY), Ok(true));
        // Another caller waits for a reply only the reader can deliver.
        let (answered, answer) = mpsc::channel();
        let caller = Arc::clone(&connection);
        std::thread::spawn(move || {
            let started = Instant::now();
            let result = caller.call(Request::StopCapture);
            let _ = answered.send((result.is_ok(), started.elapsed()));
        });
        let mut requests = BufReader::new(helper.try_clone().unwrap());
        let mut keyframes = 0;
        for _ in 0..2 {
            let call: Call = protocol::read(&mut requests).unwrap().unwrap();
            keyframes += usize::from(matches!(call.request, Request::Keyframe));
            send(
                &helper,
                &Event::Reply {
                    id: call.id,
                    result: Ok(Reply::Done),
                },
            );
        }
        assert_eq!(keyframes, 1);
        let (ok, elapsed) = answer
            .recv_timeout(PROMPTLY)
            .expect("the reader did not deliver the reply");
        assert!(ok && elapsed < PROMPTLY);
        // Frame 3 waits for the keyframe; frames resume with it.
        send(&helper, &Event::Frame(3, frame(false)));
        send(&helper, &Event::Frame(4, frame(true)));
        send(&helper, &Event::Frame(5, frame(false)));
        assert_eq!(frames.recv_timeout(PROMPTLY), Ok(true));
        assert_eq!(frames.recv_timeout(PROMPTLY), Ok(false));
        assert!(connection.pending.lock().unwrap().is_empty());
    }

    #[test]
    fn a_sink_that_lost_frames_waits_for_a_keyframe_and_asks_again_while_none_comes() {
        let (published, frames) = mpsc::channel();
        let mut sink = FrameSink::new(move |frame| {
            let _ = published.send(frame.keyframe);
        });
        let start = Instant::now();
        let at = |elapsed: Duration| start + elapsed;
        let ms = Duration::from_millis;
        assert!(!sink.receive(0, frame(true), at(ms(0))));
        assert!(!sink.receive(1, frame(false), at(ms(10))));
        // Frames 2 and 3 were lost: one request for any number of gaps.
        assert!(sink.receive(4, frame(false), at(ms(20))));
        assert!(!sink.receive(6, frame(false), at(ms(30))));
        assert!(!sink.receive(7, frame(false), at(ms(19) + KEYFRAME_RETRY)));
        // The keyframe did not come, so the sink asks again.
        assert!(sink.receive(8, frame(false), at(ms(20) + KEYFRAME_RETRY)));
        assert!(!sink.receive(9, frame(false), at(ms(30) + KEYFRAME_RETRY)));
        assert!(!sink.receive(10, frame(true), at(ms(40) + KEYFRAME_RETRY)));
        assert!(!sink.receive(11, frame(false), at(ms(50) + KEYFRAME_RETRY)));
        assert_eq!(
            frames.try_iter().collect::<Vec<_>>(),
            [true, false, true, false]
        );
    }

    #[test]
    fn a_call_from_the_reader_fails_at_once() {
        let (connection, helper) = connect();
        let (sender, results) = mpsc::channel();
        let reader = Arc::downgrade(&connection);
        begin(&connection, move |_| {
            if let Some(connection) = reader.upgrade() {
                let started = Instant::now();
                let failed = connection.call(Request::StopCapture).is_err();
                let _ = sender.send((failed, started.elapsed()));
            }
        });
        send(&helper, &Event::Frame(0, frame(true)));
        let (failed, elapsed) = results
            .recv_timeout(PROMPTLY)
            .expect("the reader waited for a reply");
        assert!(failed && elapsed < PROMPTLY);
        assert!(connection.pending.lock().unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_failed_write_ends_the_connection() {
        let (connection, helper) = connect();
        // The helper stops taking requests but stays connected.
        helper.shutdown(std::net::Shutdown::Read).unwrap();
        connection.notify(Request::Keyframe);
        let deadline = Instant::now() + PROMPTLY;
        while !connection.closed() {
            assert!(
                Instant::now() < deadline,
                "the reader did not see the connection end"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The session a fake helper serves.
    #[derive(Clone, Copy)]
    struct Seat {
        uid: u32,
        session: &'static str,
    }

    const A: Seat = Seat {
        uid: 501,
        session: "c1",
    };
    const B: Seat = Seat {
        uid: 502,
        session: "c2",
    };
    const C: Seat = Seat {
        uid: 503,
        session: "c3",
    };

    impl Seat {
        /// The console while this seat's session holds it.
        fn console(self) -> Console {
            #[cfg(target_os = "linux")]
            let console = Console::Session(self.session.to_owned());
            #[cfg(target_os = "macos")]
            let console = Console::User(self.uid);
            console
        }
    }

    /// A registry whose console is whatever `console` holds.
    fn registry_on(console: &Arc<Mutex<Console>>) -> Arc<Registry> {
        let console = Arc::clone(console);
        Arc::new(Registry {
            helpers: Mutex::default(),
            console: Box::new(move || console.lock().unwrap().clone()),
        })
    }

    /// A fake helper that answers every call, starting capture on one
    /// display, and reports what it was asked.
    struct FakeHelper {
        connection: Arc<Connection>,
        requests: mpsc::Receiver<Request>,
    }

    impl FakeHelper {
        fn connect(registry: &Arc<Registry>, seat: Seat) -> Self {
            let (coordinator, helper) = UnixStream::pair().unwrap();
            send(
                &helper,
                &Hello {
                    version: protocol::VERSION,
                    login_window: false,
                },
            );
            let connection = registry
                .attach(coordinator, seat.uid, Some(seat.session.to_owned()))
                .unwrap();
            let (received, requests) = mpsc::channel();
            std::thread::spawn(move || {
                let mut calls = BufReader::new(helper.try_clone().unwrap());
                while let Ok(Some(call)) = protocol::read::<Call>(&mut calls) {
                    let reply = match call.request {
                        Request::Start { .. } => started(),
                        _ => Reply::Done,
                    };
                    // Reported before the reply, so whoever has the reply
                    // finds the request.
                    if received.send(call.request).is_err() {
                        break;
                    }
                    send(
                        &helper,
                        &Event::Reply {
                            id: call.id,
                            result: Ok(reply),
                        },
                    );
                }
            });
            Self {
                connection,
                requests,
            }
        }

        /// The requests received since the last look.
        fn received(&self) -> Vec<Request> {
            self.requests.try_iter().collect()
        }
    }

    fn started() -> Reply {
        let display = meshrmm_protocol::Display {
            session: meshrmm_protocol::DesktopSession::Console,
            id: DisplayId(1),
            name: "Display".into(),
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
            primary: true,
        };
        Reply::Started {
            displays: vec![display.clone()],
            active_display: display,
            format: meshrmm_protocol::VideoFormat {
                width: 1920,
                height: 1080,
                frames_per_second: 30,
                codec: meshrmm_protocol::Codec::H264,
                pixel_format: meshrmm_protocol::PixelFormat::Nv12,
                bitrate_bits_per_second: 4_000_000,
            },
        }
    }

    /// The requests' names.
    fn names(requests: &[Request]) -> Vec<String> {
        requests
            .iter()
            .map(|request| {
                format!("{request:?}")
                    .chars()
                    .take_while(char::is_ascii_alphanumeric)
                    .collect()
            })
            .collect()
    }

    fn settings() -> StreamSettings {
        StreamSettings {
            frames_per_second: 30,
            bitrate_bits_per_second: 4_000_000,
            codec: meshrmm_protocol::Codec::H264,
            capture_cursor: true,
            grayscale: false,
        }
    }

    fn click(pressed: bool) -> RemoteInput {
        RemoteInput::PointerButton {
            display_id: DisplayId(1),
            button: meshrmm_protocol::PointerButton::Left,
            pressed,
        }
    }

    fn start(remote: &Arc<Remote>, display_id: Option<DisplayId>) -> anyhow::Result<Started> {
        remote.start(display_id, settings(), |_| {})
    }

    fn is_notice(message: Option<meshrmm_protocol::SessionMessage>) -> bool {
        matches!(
            message,
            Some(meshrmm_protocol::SessionMessage::MaintenanceError { .. })
        )
    }

    #[test]
    fn the_session_follows_the_console_to_another_sessions_helper() {
        let console = Arc::new(Mutex::new(A.console()));
        let registry = registry_on(&console);
        let a = FakeHelper::connect(&registry, A);
        let b = FakeHelper::connect(&registry, B);
        let remote = Arc::new(Remote::with_registry(
            Arc::clone(&registry),
            SessionUi::default(),
        ));
        let input = HelperInput(Arc::clone(&remote));
        start(&remote, Some(DisplayId(7))).unwrap();
        let requests = a.received();
        assert_eq!(names(&requests), ["BeginSession", "Start"]);
        assert!(matches!(
            requests[1],
            Request::Start {
                display_id: Some(DisplayId(7)),
                ..
            }
        ));
        assert!(remote.poll_ended().is_none());

        *console.lock().unwrap() = B.console();
        // Capture restarts on a new stream, and A's session is left at
        // once: its capture stops and nothing it sends reaches the session.
        let ended = remote.poll_ended().expect("the console moved");
        assert!(format!("{ended}").contains("another session took the console"));
        assert_eq!(names(&a.received()), ["EndSession"]);
        assert!(a.connection.session.lock().unwrap().is_none());

        // The restart starts B's main display: display 7 was A's.
        start(&remote, Some(DisplayId(7))).unwrap();
        let requests = b.received();
        assert_eq!(names(&requests), ["BeginSession", "Start"]);
        assert!(matches!(
            requests[1],
            Request::Start {
                display_id: None,
                ..
            }
        ));
        input.apply(click(true)).unwrap();
        input.apply_chat("hello".into()).unwrap();
        assert_eq!(names(&b.received()), ["Input", "Chat"]);
        assert!(a.received().is_empty());
        assert!(remote.poll_ended().is_none());
    }

    #[test]
    fn leaving_a_session_off_the_console_releases_what_the_session_held_there() {
        let console = Arc::new(Mutex::new(A.console()));
        let registry = registry_on(&console);
        let a = FakeHelper::connect(&registry, A);
        let b = FakeHelper::connect(&registry, B);
        #[allow(unused_mut, reason = "only Linux locks sessions again")]
        let mut remote = Remote::with_registry(Arc::clone(&registry), SessionUi::default());
        #[cfg(target_os = "linux")]
        let locked = {
            let locked = Arc::new(Mutex::new(Vec::new()));
            let record = Arc::clone(&locked);
            remote.lock_session = Box::new(move |session| {
                record.lock().unwrap().push(session.to_owned());
                Ok(())
            });
            // As when GNOME refused to capture A's locked session.
            *remote.unlocked.lock().unwrap() = Some(A.session.to_owned());
            locked
        };
        let remote = Arc::new(remote);
        let input = HelperInput(Arc::clone(&remote));
        start(&remote, None).unwrap();
        input.set_prevent_idle_lock(true).unwrap();
        input.apply(click(true)).unwrap();
        assert_eq!(
            names(&a.received()),
            ["BeginSession", "Start", "SetPreventIdleLock", "Input"]
        );

        *console.lock().unwrap() = B.console();
        assert!(remote.poll_ended().is_some());
        // Ending the session in A's helper releases the held button and the
        // idle-lock inhibition there, as at the end of a remote session.
        assert_eq!(names(&a.received()), ["EndSession"]);
        #[cfg(target_os = "linux")]
        {
            assert_eq!(*locked.lock().unwrap(), [A.session]);
            assert!(remote.unlocked.lock().unwrap().is_none());
        }
        // The technician's choice carries over to B's session.
        start(&remote, None).unwrap();
        assert_eq!(
            names(&b.received()),
            ["BeginSession", "SetPreventIdleLock", "Start"]
        );
        drop(input);
        drop(remote);
        #[cfg(target_os = "linux")]
        assert_eq!(*locked.lock().unwrap(), [A.session]);
    }

    #[test]
    fn the_session_waits_for_the_helper_of_a_console_session_that_has_none() {
        let console = Arc::new(Mutex::new(A.console()));
        let registry = registry_on(&console);
        let a = FakeHelper::connect(&registry, A);
        let remote = Arc::new(Remote::with_registry(
            Arc::clone(&registry),
            SessionUi::default(),
        ));
        let input = HelperInput(Arc::clone(&remote));
        start(&remote, Some(DisplayId(7))).unwrap();
        a.received();
        assert!(!is_notice(input.maintenance_state()));

        *console.lock().unwrap() = C.console();
        assert!(remote.poll_ended().is_some());
        assert_eq!(names(&a.received()), ["EndSession"]);
        assert!(start(&remote, Some(DisplayId(7))).is_err());
        // The technician is told once, and the session goes on: input has
        // nowhere to go, but does not fail.
        assert!(is_notice(input.maintenance_state()));
        input.apply(click(true)).unwrap();
        input.apply(click(false)).unwrap();
        assert!(start(&remote, Some(DisplayId(7))).is_err());
        assert!(!is_notice(input.maintenance_state()));
        assert!(a.received().is_empty());

        let c = FakeHelper::connect(&registry, C);
        start(&remote, Some(DisplayId(7))).unwrap();
        let requests = c.received();
        assert_eq!(names(&requests), ["BeginSession", "Start"]);
        assert!(matches!(
            requests[1],
            Request::Start {
                display_id: None,
                ..
            }
        ));
        input.apply(click(true)).unwrap();
        assert_eq!(names(&c.received()), ["Input"]);
        assert!(remote.poll_ended().is_none());
        assert!(a.received().is_empty());
    }

    #[test]
    fn a_console_that_stays_put_changes_nothing() {
        let console = Arc::new(Mutex::new(A.console()));
        let registry = registry_on(&console);
        let a = FakeHelper::connect(&registry, A);
        let remote = Arc::new(Remote::with_registry(
            Arc::clone(&registry),
            SessionUi::default(),
        ));
        let input = HelperInput(Arc::clone(&remote));
        start(&remote, Some(DisplayId(7))).unwrap();
        a.received();
        for _ in 0..3 {
            assert!(remote.poll_ended().is_none());
            input.apply(click(true)).unwrap();
        }
        assert_eq!(names(&a.received()), ["Input", "Input", "Input"]);
        assert!(!is_notice(input.maintenance_state()));
    }

    #[test]
    fn a_helper_connecting_in_a_session_off_the_console_is_not_used() {
        let console = Arc::new(Mutex::new(A.console()));
        let registry = registry_on(&console);
        let a = FakeHelper::connect(&registry, A);
        let remote = Arc::new(Remote::with_registry(
            Arc::clone(&registry),
            SessionUi::default(),
        ));
        let input = HelperInput(Arc::clone(&remote));
        start(&remote, Some(DisplayId(7))).unwrap();
        a.received();

        let b = FakeHelper::connect(&registry, B);
        assert!(remote.poll_ended().is_none());
        input.apply(click(true)).unwrap();
        // A restart for another reason stays in A's session, on its display.
        start(&remote, Some(DisplayId(7))).unwrap();
        let requests = a.received();
        assert_eq!(names(&requests), ["Input", "Start"]);
        assert!(matches!(
            requests[1],
            Request::Start {
                display_id: Some(DisplayId(7)),
                ..
            }
        ));
        assert!(b.received().is_empty());
        assert!(b.connection.session.lock().unwrap().is_none());
    }

    /// Runs a helper in this process against a coordinator socket and streams
    /// the console through it. Needs the Screen Recording permission.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "captures the screen through a session helper"]
    fn streams_the_console_through_a_session_helper() {
        let socket =
            std::env::temp_dir().join(format!("meshrmm-helper-{}.sock", std::process::id()));
        listen(&socket).unwrap();
        let helper_socket = socket.clone();
        std::thread::spawn(move || super::super::host::run(&helper_socket));
        let remote = Remote::new(SessionUi::default()).unwrap();
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
        let (sender, packets) = mpsc::channel();
        let audio = HelperInput(Arc::clone(&remote))
            .start_audio(Box::new(move |packet| {
                let _ = sender.send(packet);
            }))
            .unwrap();
        std::process::Command::new("/usr/bin/afplay")
            .args(["-v", "0.3", "/System/Library/Sounds/Ping.aiff"])
            .status()
            .unwrap();
        let mut loudest = 0_i16;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while let Ok(packet) =
            packets.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            for sample in packet[6..].chunks_exact(2) {
                loudest = loudest.max(i16::from_le_bytes([sample[0], sample[1]]).saturating_abs());
            }
        }
        assert!(audio.healthy());
        assert!(loudest > 100, "the helper's system audio is silent");
        drop(audio);
        remote.stop().unwrap();
        drop(remote);
        let _ = std::fs::remove_file(socket);
    }
}
