//! Restarting the encoder of a running capture helper in place, against
//! fake helpers on real pipes. Each fake has a suspended process that never
//! runs, so the Agent's waits and terminations behave as they do for a real
//! helper.
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::sync::Condvar;

use windows::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};

use super::*;

const DISPLAY: DisplayId = DisplayId(1);
/// How long a test waits for a fake helper before it fails. Nothing waits
/// this long when the test passes.
const PATIENCE: Duration = Duration::from_secs(20);
/// What an old stream's encoder still has on the pipe when a restart comes.
const LEFTOVER: u8 = 0xee;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Capture,
    Input,
    Files,
    Clipboard,
    Chat,
    Other,
}

impl Kind {
    fn of(first: &ParentCommand) -> Self {
        match first {
            ParentCommand::Start { .. } => Self::Capture,
            ParentCommand::StartInput { .. } => Self::Input,
            ParentCommand::StartFiles => Self::Files,
            ParentCommand::StartClipboard => Self::Clipboard,
            ParentCommand::StartChatHelper { .. } => Self::Chat,
            _ => Self::Other,
        }
    }
}

/// A command as the tests compare it.
#[derive(Debug, Clone, PartialEq)]
enum Sent {
    Start { bitrate: u32 },
    StartInput(DisplayId),
    Input(RemoteInput),
    ReleaseInput,
    Stop,
    Other,
}

impl Sent {
    fn of(command: &ParentCommand) -> Self {
        match command {
            ParentCommand::Start {
                bitrate_bits_per_second,
                ..
            } => Self::Start {
                bitrate: *bitrate_bits_per_second,
            },
            ParentCommand::StartInput { display_id, .. } => Self::StartInput(*display_id),
            ParentCommand::Input(input) => Self::Input(input.clone()),
            ParentCommand::ReleaseInput => Self::ReleaseInput,
            ParentCommand::Stop => Self::Stop,
            _ => Self::Other,
        }
    }
}

#[derive(Default)]
struct Log {
    /// The kind of each helper launched, in launch order; known once it
    /// has read its first command.
    launches: Vec<Option<Kind>>,
    /// Every command each helper read, by launch.
    commands: Vec<(usize, Sent)>,
    /// Launches whose process has ended.
    ended: Vec<usize>,
}

/// Fake helpers, and what the Agent did with them.
#[derive(Default)]
struct Helpers {
    log: Mutex<Log>,
    changed: Condvar,
    /// A capture helper reads a restart and does not answer it.
    stall_restarts: AtomicBool,
    /// A capture helper reports the desktop changed when asked for a keyframe.
    lose_desktop_on_keyframe: AtomicBool,
}

type Ends = Arc<Mutex<Option<(File, File)>>>;

impl Helpers {
    fn launcher(self: &Arc<Self>) -> HelperLauncher {
        let helpers = Arc::clone(self);
        Arc::new(move |_target, _as_user| helpers.launch())
    }

    fn launch(self: &Arc<Self>) -> anyhow::Result<LaunchedHelper> {
        let (child_input, parent_input) = create_pipe()?;
        let (parent_output, child_output) = create_pipe()?;
        let (parent_stderr, child_stderr) = create_pipe()?;
        // CREATE_SUSPENDED | CREATE_NO_WINDOW: the process never runs.
        let mut child = std::process::Command::new(std::env::current_exe()?)
            .creation_flags(0x0000_0004 | 0x0800_0000)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let process = duplicate(&child)?;
        let kill = duplicate(&child)?;
        let process_id = child.id();
        let index = {
            let mut log = self.log.lock().unwrap();
            log.launches.push(None);
            log.launches.len() - 1
        };
        let ends: Ends = Arc::new(Mutex::new(Some((
            child_output.into_file(),
            child_stderr.into_file(),
        ))));
        // The pipe ends close when the process ends, as a helper's do.
        let process_ends = Arc::clone(&ends);
        let helpers = Arc::clone(self);
        thread::spawn(move || {
            let _ = child.wait();
            process_ends.lock().unwrap().take();
            helpers.log.lock().unwrap().ended.push(index);
            helpers.changed.notify_all();
        });
        let helpers = Arc::clone(self);
        let input = child_input.into_file();
        thread::spawn(move || helpers.serve(index, input, &ends, &kill));
        Ok(LaunchedHelper {
            process,
            process_id,
            session_id: 1,
            input: parent_input.into_file(),
            output: parent_output.into_file(),
            stderr: parent_stderr.into_file(),
        })
    }

    /// Plays a helper: a capture helper answers each Start with its stream,
    /// the others start and take their commands.
    fn serve(&self, index: usize, mut input: File, ends: &Ends, process: &OwnedHandle) {
        let mut kind = None;
        let mut bitrate = None;
        while let Ok(command) = read_command(&mut input) {
            let kind = *kind.get_or_insert_with(|| Kind::of(&command));
            {
                let mut log = self.log.lock().unwrap();
                log.launches[index] = Some(kind);
                log.commands.push((index, Sent::of(&command)));
            }
            self.changed.notify_all();
            match (kind, command) {
                (_, ParentCommand::Stop) => {
                    emit(ends, &ChildEvent::Stopped);
                    let _ = unsafe { TerminateProcess(process.0, 0) };
                    return;
                }
                (
                    Kind::Capture,
                    ParentCommand::Start {
                        bitrate_bits_per_second,
                        frames_per_second,
                        codec,
                        pixel_format,
                        ..
                    },
                ) => {
                    if bitrate.is_some() {
                        emit(ends, &frame(false, vec![LEFTOVER]));
                        if self.stall_restarts.load(Ordering::Acquire) {
                            continue;
                        }
                    }
                    bitrate = Some(bitrate_bits_per_second);
                    let display = display();
                    emit(
                        ends,
                        &ChildEvent::Started(StartedDesktop {
                            format: ActiveFormat {
                                width: 1920,
                                height: 1080,
                                frames_per_second,
                                bitrate_bits_per_second,
                                codec,
                                pixel_format,
                            },
                            displays: vec![display.clone()],
                            active_display: display,
                        }),
                    );
                    let marker = bitrate_bits_per_second.to_le_bytes().to_vec();
                    emit(ends, &frame(true, marker.clone()));
                    emit(ends, &frame(false, marker));
                }
                (Kind::Capture, ParentCommand::RequestKeyframe)
                    if self.lose_desktop_on_keyframe.load(Ordering::Acquire) =>
                {
                    emit(ends, &ChildEvent::Error("the input desktop changed".into()));
                    let _ = unsafe { TerminateProcess(process.0, 1) };
                    return;
                }
                (Kind::Capture, _) => {}
                (
                    _,
                    ParentCommand::StartInput { .. }
                    | ParentCommand::StartFiles
                    | ParentCommand::StartClipboard
                    | ParentCommand::StartChatHelper { .. },
                ) => emit(ends, &ChildEvent::InputStarted),
                _ => {}
            }
        }
    }

    /// Waits until `ready` holds for the log.
    fn wait<T>(&self, mut ready: impl FnMut(&Log) -> Option<T>) -> T {
        let deadline = Instant::now() + PATIENCE;
        let mut log = self.log.lock().unwrap();
        loop {
            if let Some(value) = ready(&log) {
                return value;
            }
            let now = Instant::now();
            assert!(now < deadline, "the fake helpers never got there");
            log = self.changed.wait_timeout(log, deadline - now).unwrap().0;
        }
    }

    fn launched(&self) -> Vec<Kind> {
        self.log
            .lock()
            .unwrap()
            .launches
            .iter()
            .map(|kind| kind.expect("a helper read no command"))
            .collect()
    }

    /// The launch index of every helper of `kind`.
    fn of_kind(&self, kind: Kind) -> Vec<usize> {
        self.launched()
            .iter()
            .enumerate()
            .filter_map(|(index, launched)| (*launched == kind).then_some(index))
            .collect()
    }

    /// What the helper launched `index`th was sent.
    fn sent(&self, index: usize) -> Vec<Sent> {
        self.log
            .lock()
            .unwrap()
            .commands
            .iter()
            .filter(|(launch, _)| *launch == index)
            .map(|(_, sent)| sent.clone())
            .collect()
    }
}

fn duplicate(child: &std::process::Child) -> anyhow::Result<OwnedHandle> {
    let mut handle = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            HANDLE(child.as_raw_handle()),
            GetCurrentProcess(),
            &mut handle,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    }?;
    Ok(OwnedHandle(handle))
}

fn emit(ends: &Ends, event: &ChildEvent) {
    if let Some((output, _)) = ends.lock().unwrap().as_mut() {
        let _ = write_event(output, event);
    }
}

fn frame(keyframe: bool, data: Vec<u8>) -> ChildEvent {
    ChildEvent::Frame(EncodedAccessUnit {
        capture_timestamp_us: 1,
        encode_complete_timestamp_us: 2,
        keyframe,
        codec_config: None,
        data,
    })
}

fn display() -> Display {
    Display {
        session: meshrmm_protocol::DesktopSession::Console,
        id: DISPLAY,
        name: "Display".into(),
        x: 0,
        y: 0,
        width: 1920,
        height: 1080,
        primary: true,
    }
}

fn config(bitrate_bits_per_second: u32) -> StreamConfig {
    StreamConfig {
        frames_per_second: 30,
        bitrate_bits_per_second,
        codec: VideoCodec::H265,
        pixel_format: VideoPixelFormat::Yuv420,
        capture_cursor: true,
        grayscale: false,
    }
}

/// A frame as a stream's sink got it: whether it is a keyframe, and its data.
type Received = (bool, Vec<u8>);

fn sink() -> (EncodedFrameSink, mpsc::Receiver<Received>) {
    let (frames, received) = mpsc::channel();
    let frames = Mutex::new(frames);
    (
        Arc::new(move |frame: EncodedAccessUnit| {
            let _ = frames.lock().unwrap().send((frame.keyframe, frame.data));
        }),
        received,
    )
}

/// The first two frames of a stream at `bitrate`: its keyframe, then the
/// next frame.
fn first_frames(received: &mpsc::Receiver<Received>) -> Vec<Received> {
    (0..2)
        .map(|_| received.recv_timeout(PATIENCE).expect("no frame arrived"))
        .collect()
}

fn opening(bitrate: u32) -> Vec<Received> {
    let marker = bitrate.to_le_bytes().to_vec();
    vec![(true, marker.clone()), (false, marker)]
}

/// A streamer started on fake helpers at `bitrate`, with the frames of its
/// first stream.
fn started(helpers: &Arc<Helpers>, bitrate: u32) -> (DesktopCaptureStreamer, Vec<Received>) {
    let mut streamer = DesktopCaptureStreamer::default();
    streamer.launcher = helpers.launcher();
    let (first, received) = sink();
    let started = streamer
        .start(config(bitrate), Some(DISPLAY), first)
        .unwrap();
    assert_eq!(started.format.bitrate_bits_per_second, bitrate);
    (streamer, first_frames(&received))
}

/// Restarts the stream at `bitrate` as the session does after a congestion
/// step, returning the new stream's format and frames.
fn restart(streamer: &mut DesktopCaptureStreamer, bitrate: u32) -> (ActiveFormat, Vec<Received>) {
    streamer.end_stream();
    let (next, received) = sink();
    let started = streamer
        .start(config(bitrate), Some(DISPLAY), next)
        .unwrap();
    (started.format, first_frames(&received))
}

#[test]
fn an_encoder_restart_asks_the_running_helper_and_launches_nothing() {
    let helpers = Arc::new(Helpers::default());
    let (mut streamer, frames) = started(&helpers, 8_000_000);
    assert_eq!(frames, opening(8_000_000));
    let launched = helpers.launched();
    let [capture] = helpers.of_kind(Kind::Capture)[..] else {
        panic!("expected one capture helper, launched {launched:?}");
    };

    let (format, frames) = restart(&mut streamer, 2_000_000);
    assert_eq!(format.bitrate_bits_per_second, 2_000_000);
    assert_eq!(frames, opening(2_000_000));
    // The same helper got the new settings; nothing else was launched or
    // stopped, and no keyframe beyond the new encoder's own was asked for.
    assert_eq!(helpers.launched(), launched);
    assert_eq!(
        helpers.sent(capture),
        vec![
            Sent::Start { bitrate: 8_000_000 },
            Sent::Start { bitrate: 2_000_000 },
        ]
    );
    assert!(helpers.log.lock().unwrap().ended.is_empty());
}

#[test]
fn a_stream_restarted_in_place_begins_as_a_relaunched_one_does() {
    let helpers = Arc::new(Helpers::default());
    let (mut streamer, _) = started(&helpers, 8_000_000);

    // A relaunch, as every restart was before.
    streamer.stop().unwrap();
    let (relaunched_sink, relaunched) = sink();
    let after_relaunch = streamer
        .start(config(4_000_000), Some(DISPLAY), relaunched_sink)
        .unwrap();
    let relaunched_frames = first_frames(&relaunched);

    // The same step in place.
    let (format, frames) = restart(&mut streamer, 2_000_000);

    // Each new stream starts with its new encoder's keyframe, and nothing
    // of the old stream, which still had a frame on the pipe, reaches it.
    assert_eq!(relaunched_frames, opening(4_000_000));
    assert_eq!(frames, opening(2_000_000));
    assert_eq!(after_relaunch.format.bitrate_bits_per_second, 4_000_000);
    assert_eq!(format.bitrate_bits_per_second, 2_000_000);
    // The relaunched stream's sink was let go at the restart: the frame
    // the old encoder left on the pipe went nowhere.
    assert!(relaunched.try_recv().is_err());
    assert_eq!(helpers.of_kind(Kind::Capture).len(), 2);
}

#[test]
fn a_helper_that_does_not_restart_in_time_is_replaced_once_and_the_session_goes_on() {
    let helpers = Arc::new(Helpers::default());
    let (mut streamer, _) = started(&helpers, 8_000_000);
    let launched = helpers.launched();
    let [stalled] = helpers.of_kind(Kind::Capture)[..] else {
        panic!("expected one capture helper, launched {launched:?}");
    };

    // The helper takes the restart and never answers; the Agent does not
    // wait out a real limit in this test.
    helpers.stall_restarts.store(true, Ordering::Release);
    streamer.restart_timeout = Duration::ZERO;
    streamer.end_stream();
    let (next, received) = sink();
    let format = streamer
        .start(config(2_000_000), Some(DISPLAY), next)
        .unwrap()
        .format;
    helpers.stall_restarts.store(false, Ordering::Release);

    // The stalled helper was ended, not asked to stop, and one replacement
    // started the stream on the same desktop; the other helpers stayed.
    helpers.wait(|log| log.ended.contains(&stalled).then_some(()));
    assert_eq!(
        helpers.sent(stalled).last(),
        Some(&Sent::Start { bitrate: 2_000_000 })
    );
    let mut expected = launched;
    expected.push(Kind::Capture);
    assert_eq!(helpers.launched(), expected);
    assert_eq!(format.bitrate_bits_per_second, 2_000_000);
    assert_eq!(first_frames(&received), opening(2_000_000));

    // The replacement restarts in place again.
    streamer.restart_timeout = START_TIMEOUT;
    let (format, frames) = restart(&mut streamer, 1_000_000);
    assert_eq!(format.bitrate_bits_per_second, 1_000_000);
    assert_eq!(frames, opening(1_000_000));
    assert_eq!(helpers.launched(), expected);
}

#[test]
fn a_changed_desktop_still_replaces_the_helper() {
    let helpers = Arc::new(Helpers::default());
    let (mut streamer, _) = started(&helpers, 8_000_000);
    let launched = helpers.launched();

    // Desktop Duplication loses the desktop when Windows switches it, and
    // the helper ends.
    helpers
        .lose_desktop_on_keyframe
        .store(true, Ordering::Release);
    streamer.request_keyframe().unwrap();
    let reader = streamer
        .running
        .as_mut()
        .and_then(|running| running.reader.take())
        .unwrap();
    reader.join().unwrap();
    assert!(matches!(streamer.poll_ended(), Some(Err(_))));
    helpers
        .lose_desktop_on_keyframe
        .store(false, Ordering::Release);

    // The session starts capture again, which takes a new helper.
    let (next, received) = sink();
    streamer
        .start(config(8_000_000), Some(DISPLAY), next)
        .unwrap();
    assert_eq!(first_frames(&received), opening(8_000_000));
    let mut expected = launched;
    expected.push(Kind::Capture);
    assert_eq!(helpers.launched(), expected);

    // Its encoder restarts in place.
    let (_, frames) = restart(&mut streamer, 2_000_000);
    assert_eq!(frames, opening(2_000_000));
    assert_eq!(helpers.launched(), expected);
}

#[test]
fn an_encoder_restart_leaves_the_input_route_and_the_held_keys_alone() {
    let helpers = Arc::new(Helpers::default());
    let (mut streamer, _) = started(&helpers, 8_000_000);
    let [input] = helpers.of_kind(Kind::Input)[..] else {
        panic!("expected one input helper");
    };
    let routed = lock_route(&streamer.input_route).writer().unwrap();
    let controller = streamer.input_controller();
    let ctrl = |pressed| RemoteInput::Key {
        display_id: DISPLAY,
        scan_code: 0x1d,
        extended: false,
        pressed,
    };
    controller.apply(ctrl(true)).unwrap();
    helpers.wait(|log| {
        log.commands
            .contains(&(input, Sent::Input(ctrl(true))))
            .then_some(())
    });
    let before = helpers.sent(input);

    restart(&mut streamer, 2_000_000);

    // The input helper got nothing: no release, no new display. The route
    // still leads to it, and the record still has Ctrl down, so the
    // viewer's key-up reaches the helper that pressed it.
    assert_eq!(helpers.sent(input), before);
    let route = lock_route(&streamer.input_route);
    assert!(Arc::ptr_eq(&route.writer().unwrap(), &routed));
    assert!(route.held().admits(&ctrl(false)));
    drop(route);
    controller.apply(ctrl(false)).unwrap();
    helpers.wait(|log| {
        log.commands
            .contains(&(input, Sent::Input(ctrl(false))))
            .then_some(())
    });
    let mut expected = before;
    expected.push(Sent::Input(ctrl(false)));
    assert_eq!(helpers.sent(input), expected);
    assert_eq!(helpers.of_kind(Kind::Input), vec![input]);
}

#[test]
fn a_new_streams_frames_follow_its_started_event_on_the_pipe() {
    let pipe = Arc::new(Mutex::new(Vec::new()));
    let failed = Arc::new(AtomicBool::new(false));
    // The old stream's encoder had a frame out before the restart.
    write_event(&mut *pipe.lock().unwrap(), &frame(false, vec![LEFTOVER])).unwrap();
    let (new_stream, announcement) = stream_frames(Arc::clone(&pipe), Arc::clone(&failed));
    // The new encoder's keyframe comes before the helper has told the
    // Agent the stream started.
    new_stream(EncodedAccessUnit {
        capture_timestamp_us: 1,
        encode_complete_timestamp_us: 2,
        keyframe: true,
        codec_config: None,
        data: vec![1],
    });
    let display = display();
    announcement
        .announce(StartedDesktop {
            format: ActiveFormat {
                width: 1920,
                height: 1080,
                frames_per_second: 30,
                bitrate_bits_per_second: 2_000_000,
                codec: VideoCodec::H265,
                pixel_format: VideoPixelFormat::Yuv420,
            },
            displays: vec![display.clone()],
            active_display: display,
        })
        .unwrap();
    new_stream(EncodedAccessUnit {
        capture_timestamp_us: 3,
        encode_complete_timestamp_us: 4,
        keyframe: false,
        codec_config: None,
        data: vec![2],
    });
    write_event(&mut *pipe.lock().unwrap(), &ChildEvent::Stopped).unwrap();
    assert!(!failed.load(Ordering::Acquire));

    // The Agent's reader gives the new stream's sink everything after
    // Started, its keyframe first, and nothing before.
    let (sink, received) = sink();
    let (started_tx, started_rx) = mpsc::channel();
    dispatch_child_events(
        pipe.lock().unwrap().as_slice(),
        Arc::new(Mutex::new(FrameRoute::starting(sink))),
        started_tx,
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new((CursorShape::Default, false, None))),
        Arc::new(Mutex::new(None)),
        Arc::new(Mutex::new(Instant::now())),
    );
    assert!(started_rx.recv().unwrap().is_ok());
    assert_eq!(
        received.try_iter().collect::<Vec<_>>(),
        vec![(true, vec![1]), (false, vec![2])]
    );
}
