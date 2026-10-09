use webrtc::data_channel::RTCDataChannel;

use super::super::platform::ScreenInput;
use super::*;
#[test]
fn recording_keeps_cursor_for_both_input_owners_and_restores_preference() {
    for show_cursor in [false, true] {
        for viewer_controls_input in [false, true] {
            assert!(super::capture_cursor_for_session(
                show_cursor,
                viewer_controls_input,
                true
            ));
            assert_eq!(
                super::capture_cursor_for_session(show_cursor, viewer_controls_input, false),
                show_cursor && !viewer_controls_input
            );
        }
    }
}

use meshrmm_protocol::{ClipboardContent, CursorShape, FileMessage, RemoteInput};

struct TestInput {
    events: mpsc::UnboundedSender<&'static str>,
    file_gate: Mutex<std::sync::mpsc::Receiver<()>>,
}
impl ScreenInput for TestInput {
    fn set_prevent_idle_lock(&self, _: bool) -> anyhow::Result<()> {
        Ok(())
    }

    fn set_wallpaper_hidden(&self, _: bool) -> anyhow::Result<()> {
        Ok(())
    }
    fn apply_files(&self, _: FileMessage) -> anyhow::Result<()> {
        self.events.send("file blocked")?;
        self.file_gate
            .lock()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))?;
        Ok(())
    }
    fn apply(&self, _: RemoteInput) -> anyhow::Result<()> {
        self.events.send("input")?;
        Ok(())
    }
    fn annotate(&self, _: meshrmm_protocol::Annotation) -> anyhow::Result<()> {
        Ok(())
    }
    fn apply_chat(&self, _: String) -> anyhow::Result<()> {
        self.events.send("chat")?;
        Ok(())
    }
    fn apply_clipboard(&self, _: ClipboardContent) -> anyhow::Result<()> {
        self.events.send("clipboard")?;
        Ok(())
    }
    fn release_all(&self) -> anyhow::Result<()> {
        self.events.send("released")?;
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
    fn files_ready(&self) -> Arc<tokio::sync::Notify> {
        Arc::new(tokio::sync::Notify::new())
    }
    fn poll_files(&self) -> Option<FileMessage> {
        None
    }
    fn chat_ready(&self) -> Arc<tokio::sync::Notify> {
        Arc::new(tokio::sync::Notify::new())
    }
    fn poll_chat(&self) -> anyhow::Result<Option<String>> {
        Ok(None)
    }
    fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>> {
        Ok(None)
    }
    fn start_chat(&self) -> anyhow::Result<()> {
        Ok(())
    }
    fn stop_chat(&self) {}
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_file_operation_does_not_delay_input_chat_or_clipboard() {
    let (events, mut received) = mpsc::unbounded_channel();
    let (release, gate) = std::sync::mpsc::channel();
    let input: Arc<dyn ScreenInput> = Arc::new(TestInput {
        events,
        file_gate: Mutex::new(gate),
    });
    let channel = Arc::new(RTCDataChannel::default());
    let (errors, _) = mpsc::unbounded_channel();
    let (files, mut file_task) = spawn_file_worker(
        input.clone(),
        ServiceChannel::new(channel.clone()).await,
        None,
    )
    .unwrap();
    let (keys, mut input_task) =
        spawn_input_worker(input.clone(), channel.clone(), errors).unwrap();
    let (chat, mut chat_task) = spawn_chat_worker(
        input.clone(),
        ServiceChannel::new(channel.clone()).await,
        None,
    )
    .unwrap();
    let (clipboard, mut clipboard_task) =
        spawn_clipboard_worker(input, ServiceChannel::new(channel).await, None).unwrap();
    files.try_send(FileMessage::Pick).unwrap();
    assert_eq!(received.recv().await, Some("file blocked"));
    keys.try_send(RemoteInput::PointerMove {
        display_id: DisplayId(1),
        x: 0,
        y: 0,
    })
    .unwrap();
    chat.try_send(SessionMessage::Chat {
        text: "still responsive".into(),
    })
    .unwrap();
    for message in ClipboardContent::from("independent").messages().unwrap() {
        clipboard.try_send(message).unwrap();
    }
    let mut observed = Vec::new();
    for _ in 0..3 {
        observed.push(
            tokio::time::timeout(std::time::Duration::from_secs(1), received.recv())
                .await
                .unwrap()
                .unwrap(),
        );
    }
    observed.sort();
    assert_eq!(observed, ["chat", "clipboard", "input"]);
    release.send(()).unwrap();
    file_task.shutdown().await;
    input_task.shutdown().await;
    chat_task.shutdown().await;
    clipboard_task.shutdown().await;
    assert_eq!(received.recv().await, Some("released"));
}
