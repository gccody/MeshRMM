//! The service workers wait out a channel that has no room, for longer than the five seconds
//! that used to end them, and still end on the stop signal or the channel closing.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use meshrmm_protocol::{
    ClipboardContent, ClipboardReceiver, CursorShape, DisplayId, FileMessage, RemoteInput,
    SessionMessage,
};
use meshrmm_session_transport::test_link::TestLink;
use meshrmm_session_transport::{CHAT_CHANNEL, CLIPBOARD_CHANNEL, FILE_CHANNEL};
use tokio::sync::{Notify, mpsc};

use super::super::platform::ScreenInput;
use super::service_workers::{spawn_chat_worker, spawn_clipboard_worker, spawn_file_worker};

/// How long the link carries nothing: past the old five-second limit, and short of the 25
/// seconds after which ICE gives the connection up.
const STALL: Duration = Duration::from_secs(10);

/// Bounds a wait so a broken worker fails its test instead of hanging it. The clock is paused,
/// so this costs nothing while things work. It is long because the clock runs on while a
/// worker's own thread is busy.
const PATIENCE: Duration = Duration::from_secs(24 * 60 * 60);

async fn within<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(PATIENCE, future)
        .await
        .expect("the worker never got there")
}

/// What the device has to send, released to the workers as each test says.
struct Device {
    events: mpsc::UnboundedSender<&'static str>,
    files: Mutex<VecDeque<FileMessage>>,
    files_ready: Arc<Notify>,
    clipboard: Mutex<Option<ClipboardContent>>,
    clipboard_ready: Arc<Notify>,
    chat: Mutex<VecDeque<String>>,
    chat_ready: Arc<Notify>,
}

impl Device {
    fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<&'static str>) {
        let (events, received) = mpsc::unbounded_channel();
        let device = Self {
            events,
            files: Mutex::default(),
            files_ready: Arc::default(),
            clipboard: Mutex::default(),
            clipboard_ready: Arc::default(),
            chat: Mutex::default(),
            chat_ready: Arc::default(),
        };
        (Arc::new(device), received)
    }

    fn send_files(&self, messages: impl IntoIterator<Item = FileMessage>) {
        self.files.lock().unwrap().extend(messages);
        self.files_ready.notify_one();
    }

    fn copy(&self, content: ClipboardContent) {
        *self.clipboard.lock().unwrap() = Some(content);
        self.clipboard_ready.notify_one();
    }

    fn say(&self, texts: impl IntoIterator<Item = String>) {
        self.chat.lock().unwrap().extend(texts);
        self.chat_ready.notify_one();
    }
}

impl ScreenInput for Device {
    fn set_prevent_idle_lock(&self, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_wallpaper_hidden(&self, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn apply_files(&self, _: FileMessage) -> anyhow::Result<()> {
        self.events.send("file applied")?;
        Ok(())
    }
    fn apply(&self, _: RemoteInput) -> anyhow::Result<()> {
        Ok(())
    }
    fn annotate(&self, _: meshrmm_protocol::Annotation) -> anyhow::Result<()> {
        Ok(())
    }
    fn apply_chat(&self, _: String) -> anyhow::Result<()> {
        Ok(())
    }
    fn apply_clipboard(&self, _: ClipboardContent) -> anyhow::Result<()> {
        Ok(())
    }
    fn release_all(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_blackout(&self, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn set_agent_input_blocked(&self, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn maintenance_state(&self) -> Option<SessionMessage> {
        None
    }
    fn viewer_controls_input(&self) -> bool {
        false
    }
    fn agent_pointer_display(&self) -> Option<DisplayId> {
        None
    }
    fn cursor_shape(&self) -> CursorShape {
        CursorShape::Default
    }
    fn files_ready(&self) -> Arc<Notify> {
        self.files_ready.clone()
    }
    fn poll_files(&self) -> Option<FileMessage> {
        self.files.lock().unwrap().pop_front()
    }
    fn chat_ready(&self) -> Arc<Notify> {
        self.chat_ready.clone()
    }
    fn poll_chat(&self) -> anyhow::Result<Option<String>> {
        Ok(self.chat.lock().unwrap().pop_front())
    }
    fn clipboard_ready(&self) -> Option<Arc<Notify>> {
        Some(self.clipboard_ready.clone())
    }
    fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>> {
        let content = self.clipboard.lock().unwrap().take();
        if content.is_some() {
            self.events.send("clipboard polled")?;
        }
        Ok(content)
    }
    fn start_chat(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop_chat(&self) {}
}

/// The `index`th chunk of transfer `id`; a few fill a channel's queue limit.
fn chunk(id: u64, index: u8) -> FileMessage {
    FileMessage::Chunk {
        id,
        data: vec![index; 16 * 1024],
    }
}

const CHUNKS: u8 = 32;

async fn assert_files_arrive(link: &mut TestLink, messages: impl IntoIterator<Item = FileMessage>) {
    for message in messages {
        assert_eq!(
            within(link.next()).await,
            (
                FILE_CHANNEL.to_owned(),
                SessionMessage::FileTransfer(message)
            )
        );
    }
}

#[tokio::test(start_paused = true)]
async fn file_transfers_carry_on_after_their_channel_has_no_room_for_ten_seconds() {
    let (mut link, channels) = TestLink::connect(&[FILE_CHANNEL]).await;
    let (device, _events) = Device::new();
    link.cut();
    device.send_files((0..CHUNKS).map(|index| chunk(1, index)));
    let (_files, mut task) = spawn_file_worker(device.clone(), channels[0].clone(), None).unwrap();
    tokio::time::sleep(STALL).await;
    link.restore();
    assert_files_arrive(&mut link, (0..CHUNKS).map(|index| chunk(1, index))).await;
    // A transfer the file helper gave up on is reported, and the next one goes too.
    let later = [
        FileMessage::Error {
            id: 1,
            reason: "transfer acknowledgement timed out".into(),
        },
        chunk(2, 0),
        FileMessage::Finish { id: 2 },
    ];
    device.send_files(later.clone());
    assert_files_arrive(&mut link, later).await;
    task.shutdown().await;
    link.close().await;
}

/// Starts a file worker with more to send than its channel takes while the link is cut, and
/// returns once the stall has outlasted five seconds and the worker still serves its peer.
async fn file_worker_waiting_for_room(
    link: &TestLink,
    channels: &[meshrmm_session_transport::ServiceChannel],
) -> (
    mpsc::Sender<FileMessage>,
    crate::remote::native_task::NativeTask,
) {
    let (device, mut events) = Device::new();
    link.cut();
    device.send_files((0..CHUNKS).map(|index| chunk(1, index)));
    let (files, task) = spawn_file_worker(device, channels[0].clone(), None).unwrap();
    tokio::time::sleep(STALL).await;
    files.try_send(FileMessage::Ack { id: 1 }).unwrap();
    assert_eq!(within(events.recv()).await, Some("file applied"));
    (files, task)
}

#[tokio::test(start_paused = true)]
async fn the_stop_signal_ends_a_worker_waiting_for_room() {
    let (link, channels) = TestLink::connect(&[FILE_CHANNEL]).await;
    let (files, mut task) = file_worker_waiting_for_room(&link, &channels).await;
    within(task.shutdown()).await;
    assert!(files.is_closed());
    link.close().await;
}

#[tokio::test(start_paused = true)]
async fn closing_the_connection_ends_a_worker_waiting_for_room() {
    let (link, channels) = TestLink::connect(&[FILE_CHANNEL]).await;
    let (files, mut task) = file_worker_waiting_for_room(&link, &channels).await;
    link.sender.close().await.unwrap();
    within(files.closed()).await;
    task.shutdown().await;
    link.receiver.close().await.unwrap();
}

/// A copy made while the channel has no room replaces the rest of the one before it, which
/// the receiving side never completes.
#[tokio::test(start_paused = true)]
async fn the_latest_copy_is_sent_once_the_clipboard_channel_has_room_again() {
    let (mut link, channels) = TestLink::connect(&[CLIPBOARD_CHANNEL]).await;
    let (device, mut events) = Device::new();
    let (_clipboard, mut task) =
        spawn_clipboard_worker(device.clone(), channels[0].clone(), None).unwrap();
    link.cut();
    let earlier = ClipboardContent::Text("e".repeat(600 * 1024));
    let earlier_chunks = earlier.messages().unwrap().len();
    device.copy(earlier);
    assert_eq!(within(events.recv()).await, Some("clipboard polled"));
    tokio::time::sleep(STALL).await;
    let latest = ClipboardContent::from("latest");
    device.copy(latest.clone());
    assert_eq!(within(events.recv()).await, Some("clipboard polled"));
    link.restore();
    let mut receiver = ClipboardReceiver::default();
    let mut received = 0;
    let content = loop {
        let (label, message) = within(link.next()).await;
        assert_eq!(label, CLIPBOARD_CHANNEL);
        if let Some(content) = receiver.receive(message).unwrap() {
            break content;
        }
        received += 1;
    };
    assert_eq!(content, latest);
    assert!(
        received < earlier_chunks,
        "all {received} chunks of the earlier copy were sent"
    );
    task.shutdown().await;
    link.close().await;
}

#[tokio::test(start_paused = true)]
async fn chat_carries_on_after_its_channel_has_no_room_for_ten_seconds() {
    let (mut link, channels) = TestLink::connect(&[CHAT_CHANNEL]).await;
    let (device, _events) = Device::new();
    let (chat, mut task) = spawn_chat_worker(device.clone(), channels[0].clone(), None).unwrap();
    link.cut();
    chat.try_send(SessionMessage::ChatAvailable).unwrap();
    // More than webrtc-sctp's pending queue takes, in texts as long as a chat message may be.
    let texts: Vec<_> = (0..64)
        .map(|index| {
            format!(
                "{index:04}{}",
                "x".repeat(meshrmm_protocol::MAX_CHAT_TEXT_BYTES - 4)
            )
        })
        .collect();
    device.say(texts.clone());
    tokio::time::sleep(STALL).await;
    link.restore();
    assert_eq!(
        within(link.next()).await,
        (CHAT_CHANNEL.to_owned(), SessionMessage::ChatAvailable)
    );
    for text in texts {
        assert_eq!(
            within(link.next()).await,
            (CHAT_CHANNEL.to_owned(), SessionMessage::Chat { text })
        );
    }
    task.shutdown().await;
    link.close().await;
}
