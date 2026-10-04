//! The session helper process: launchd runs one in each graphical session,
//! including the login window's, and it serves that session's screen and
//! input to the coordinator.
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use super::protocol::{self, Call, Event, Frame, Hello, Reply, Request};
use crate::remote::macos::local::{LocalInput, LocalScreen};
use crate::remote::platform::ScreenInput;

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// How often input state, files and chat are checked during a session.
const PUMP_INTERVAL: Duration = Duration::from_millis(16);
const CLIPBOARD_INTERVAL: Duration = Duration::from_millis(250);
/// Frames waiting to be written; more are dropped, and the coordinator asks
/// for a keyframe.
const QUEUED_EVENTS: usize = 64;

/// Serves the coordinator at `socket` until the process is stopped.
pub fn run(socket: &Path) -> anyhow::Result<()> {
    // The login window's helper runs as root; users' helpers run as them.
    // SAFETY: geteuid has no preconditions.
    let login_window = unsafe { libc::geteuid() } == 0;
    if !login_window {
        request_permissions();
    }
    let input = Arc::new(LocalInput::new()?);
    tracing::info!(login_window, socket = %socket.display(), "session helper started");
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => {
                if let Err(error) = serve(stream, &input, login_window) {
                    tracing::warn!(error = ?error, "the coordinator connection ended");
                }
                input.end_session();
            }
            Err(error) => tracing::debug!(%error, "the coordinator is not reachable yet"),
        }
        std::thread::sleep(RECONNECT_DELAY);
    }
}

/// Asks the user for the permissions the helper needs, once: macOS shows each
/// prompt only until the user answers it.
fn request_permissions() {
    use objc2_core_graphics::{
        CGPreflightListenEventAccess, CGPreflightPostEventAccess, CGPreflightScreenCaptureAccess,
        CGRequestListenEventAccess, CGRequestPostEventAccess, CGRequestScreenCaptureAccess,
    };

    let screen = CGPreflightScreenCaptureAccess() || CGRequestScreenCaptureAccess();
    let control = CGPreflightPostEventAccess() || CGRequestPostEventAccess();
    let monitor = CGPreflightListenEventAccess() || CGRequestListenEventAccess();
    if !(screen && control && monitor) {
        tracing::warn!(
            screen_recording = screen,
            accessibility = control,
            input_monitoring = monitor,
            "the MeshRMM Agent is missing permissions; allow it in System Settings > Privacy & Security"
        );
    }
}

fn serve(stream: UnixStream, input: &Arc<LocalInput>, login_window: bool) -> anyhow::Result<()> {
    let mut uid = 0;
    let mut gid = 0;
    // SAFETY: the descriptor is a connected socket and the out pointers are valid.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        return Err(std::io::Error::last_os_error()).context("could not identify the coordinator");
    }
    // SAFETY: geteuid has no preconditions.
    if uid != 0 && uid != unsafe { libc::geteuid() } {
        bail!("the coordinator socket belongs to user {uid}");
    }
    let mut writer = stream.try_clone()?;
    protocol::write(
        &mut writer,
        &Hello {
            version: protocol::VERSION,
            login_window,
        },
    )?;
    let (events, outgoing) = mpsc::sync_channel::<Event>(QUEUED_EVENTS);
    let writer_thread = std::thread::Builder::new()
        .name("meshrmm-helper-writer".into())
        .spawn(move || {
            for event in outgoing {
                if protocol::write(&mut writer, &event).is_err() {
                    break;
                }
            }
        })?;
    let screen = Arc::new(Mutex::new(LocalScreen::new(Arc::clone(input))));
    let audio = Arc::new(Mutex::new(None::<meshrmm_audio::Capture>));
    let active = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let pump = spawn_pump(
        Arc::clone(input),
        Arc::clone(&screen),
        Arc::clone(&audio),
        Arc::clone(&active),
        Arc::clone(&stopped),
        events.clone(),
    )?;
    let sequence = Arc::new(AtomicU64::new(0));
    let mut reader = std::io::BufReader::new(stream);
    let result = loop {
        let call: Call = match protocol::read(&mut reader) {
            Ok(Some(call)) => call,
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        };
        let result = handle(
            call.request,
            input,
            &screen,
            &audio,
            &active,
            &sequence,
            &events,
        )
        .map_err(|error| format!("{error:#}"));
        if events
            .send(Event::Reply {
                id: call.id,
                result,
            })
            .is_err()
        {
            break Ok(());
        }
    };
    stopped.store(true, Ordering::SeqCst);
    let _ = pump.join();
    audio.lock().unwrap_or_else(|e| e.into_inner()).take();
    screen.lock().unwrap_or_else(|e| e.into_inner()).stop();
    drop(events);
    let _ = writer_thread.join();
    result
}

fn handle(
    request: Request,
    input: &Arc<LocalInput>,
    screen: &Mutex<LocalScreen>,
    audio: &Mutex<Option<meshrmm_audio::Capture>>,
    active: &AtomicBool,
    sequence: &Arc<AtomicU64>,
    events: &mpsc::SyncSender<Event>,
) -> anyhow::Result<Reply> {
    match request {
        Request::BeginSession => active.store(true, Ordering::SeqCst),
        Request::EndSession => {
            active.store(false, Ordering::SeqCst);
            audio.lock().unwrap_or_else(|e| e.into_inner()).take();
            screen.lock().unwrap_or_else(|e| e.into_inner()).stop();
            input.end_session();
        }
        Request::Start {
            display_id,
            settings,
        } => {
            let events = events.clone();
            let sequence = Arc::clone(sequence);
            let started = screen.lock().unwrap_or_else(|e| e.into_inner()).start(
                display_id,
                settings,
                move |unit| {
                    let mut data = unit.codec_config.unwrap_or_default();
                    data.extend_from_slice(&unit.data);
                    let frame = Frame {
                        data,
                        keyframe: unit.keyframe,
                        capture_timestamp_us: unit.capture_timestamp_us,
                        encode_complete_timestamp_us: unit.encode_complete_timestamp_us,
                    };
                    // A full queue drops the frame; the gap in sequence
                    // numbers makes the coordinator ask for a keyframe.
                    let _ = events.try_send(Event::Frame(
                        sequence.fetch_add(1, Ordering::Relaxed),
                        frame,
                    ));
                },
            )?;
            return Ok(Reply::Started {
                displays: started.displays,
                active_display: started.active_display,
                format: started.format,
            });
        }
        Request::StopCapture => screen.lock().unwrap_or_else(|e| e.into_inner()).stop(),
        Request::Keyframe => screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .request_keyframe(),
        Request::SetBitrate(bits_per_second) => screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_bitrate(bits_per_second)?,
        Request::SetCursorCapture(enabled) => screen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_cursor_capture(enabled),
        Request::Input(remote) => input.apply(remote)?,
        Request::ReleaseInput => input.release_all()?,
        Request::Annotate(annotation) => input.annotate(annotation)?,
        Request::SetWallpaperHidden(hidden) => input.set_wallpaper_hidden(hidden)?,
        Request::SetPreventIdleLock(enabled) => input.set_prevent_idle_lock(enabled)?,
        Request::SetBlackout(enabled) => input.set_blackout(enabled)?,
        Request::SetAgentInputBlocked(blocked) => input.set_agent_input_blocked(blocked)?,
        Request::Clipboard(content) => input.apply_clipboard(content)?,
        Request::Files(message) => input.apply_files(message)?,
        Request::StartChat => input.start_chat()?,
        Request::StopChat => input.stop_chat(),
        Request::Chat(text) => input.apply_chat(text)?,
        Request::StartAudio => {
            let events = events.clone();
            let capture = meshrmm_audio::capture(move |packet| {
                // Late audio is useless; a full queue drops it.
                let _ = events.try_send(Event::Audio(packet));
            })?;
            *audio.lock().unwrap_or_else(|e| e.into_inner()) = Some(capture);
        }
        Request::StopAudio => {
            audio.lock().unwrap_or_else(|e| e.into_inner()).take();
        }
    }
    Ok(Reply::Done)
}

/// Sends what the session produces on its own: input state, clipboard
/// changes, outgoing files and chat, and the end of capture.
fn spawn_pump(
    input: Arc<LocalInput>,
    screen: Arc<Mutex<LocalScreen>>,
    audio: Arc<Mutex<Option<meshrmm_audio::Capture>>>,
    active: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    events: mpsc::SyncSender<Event>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    Ok(std::thread::Builder::new()
        .name("meshrmm-helper-pump".into())
        .spawn(move || {
            let mut state = None;
            let mut clipboard_checked = Instant::now();
            while !stopped.load(Ordering::SeqCst) {
                std::thread::sleep(PUMP_INTERVAL);
                if !active.load(Ordering::SeqCst) {
                    state = None;
                    continue;
                }
                let mut outgoing = Vec::new();
                let current = input.input_state();
                if state != Some(current) {
                    state = Some(current);
                    outgoing.push(Event::InputState(current));
                }
                outgoing.extend(std::iter::from_fn(|| input.poll_files()).map(Event::Files));
                while let Ok(Some(text)) = input.poll_chat() {
                    outgoing.push(Event::Chat(text));
                }
                if clipboard_checked.elapsed() >= CLIPBOARD_INTERVAL {
                    clipboard_checked = Instant::now();
                    if let Ok(Some(content)) = input.poll_clipboard() {
                        outgoing.push(Event::Clipboard(content));
                    }
                }
                if let Some(error) = screen
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .poll_ended()
                {
                    outgoing.push(Event::CaptureEnded(format!("{error:#}")));
                }
                let mut capturing = audio.lock().unwrap_or_else(|e| e.into_inner());
                if capturing.as_ref().is_some_and(|capture| !capture.healthy()) {
                    capturing.take();
                    outgoing.push(Event::AudioEnded);
                }
                drop(capturing);
                for event in outgoing {
                    if events.send(event).is_err() {
                        return;
                    }
                }
            }
        })?)
}
