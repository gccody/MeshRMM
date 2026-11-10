//! The viewer's input, applied by the session's input worker, and the text of
//! "Type clipboard", which the worker types a piece at a time.
//!
//! A piece is one call to the platform's input, so no call comes near a
//! session helper's timeout, and the worker takes the viewer's other input
//! and its stop between pieces. Each piece is typed whole, Shift released at
//! its end, so stopping between pieces leaves no key down.
//!
//! While text is being typed, pointer input carries on between pieces. A key
//! the technician presses stops the typing first: their own typing and the
//! clipboard's would garble each other. Key releases stop nothing, for the
//! keys held when "Type clipboard" was clicked.
use std::sync::Arc;

use meshrmm_protocol::{DisplayId, RemoteInput};
use tokio::sync::mpsc;

use super::ControlCommand;
use crate::remote::platform::ScreenInput;

/// The most characters in a piece. Linux types a character in at most three
/// key events 4 ms apart, so a piece takes at most 0.77 s; Windows and macOS
/// do not pace their keys.
pub(crate) const TYPING_PIECE_CHARS: usize = 64;

/// What the control channel hands the input worker, in order.
#[derive(Debug)]
pub(super) enum InputCommand {
    Input(RemoteInput),
    /// Stop typing: the technician asked, or the viewer chose another display.
    StopTyping,
}

/// Text still to type.
struct Typing {
    display_id: DisplayId,
    text: String,
    /// The bytes of `text` typed so far.
    typed: usize,
}

impl Typing {
    /// The next piece, or `None` when everything is typed.
    fn next_piece(&mut self) -> Option<RemoteInput> {
        let rest = &self.text[self.typed..];
        if rest.is_empty() {
            return None;
        }
        let mut end = rest
            .char_indices()
            .nth(TYPING_PIECE_CHARS)
            .map_or(rest.len(), |(index, _)| index);
        // Windows and macOS type CR LF as one Enter, so keep the pair together.
        if end < rest.len() && rest[..end].ends_with('\r') && rest[end..].starts_with('\n') {
            end -= 1;
        }
        self.typed += end;
        Some(RemoteInput::TypeText {
            display_id: self.display_id,
            text: rest[..end].to_owned(),
        })
    }
}

/// Applies the viewer's input, and types its text between the rest.
pub(super) struct InputWorker {
    input: Arc<dyn ScreenInput>,
    errors: mpsc::UnboundedSender<ControlCommand>,
    typing: Option<Typing>,
}

impl InputWorker {
    pub(super) fn new(
        input: Arc<dyn ScreenInput>,
        errors: mpsc::UnboundedSender<ControlCommand>,
    ) -> Self {
        Self {
            input,
            errors,
            typing: None,
        }
    }

    /// Whether text is left to type.
    pub(super) fn typing(&self) -> bool {
        self.typing.is_some()
    }

    pub(super) fn command(&mut self, command: InputCommand) {
        match command {
            InputCommand::StopTyping => self.typing = None,
            InputCommand::Input(RemoteInput::TypeText { display_id, text }) => {
                // A new text replaces the one being typed.
                self.typing = None;
                if let Some(problem) = meshrmm_protocol::typed_text_problem(&text) {
                    let _ = self.errors.send(ControlCommand::MaintenanceError(format!(
                        "Type clipboard: {problem}"
                    )));
                } else if !text.is_empty() {
                    self.typing = Some(Typing {
                        display_id,
                        text,
                        typed: 0,
                    });
                }
            }
            InputCommand::Input(input) => {
                if matches!(input, RemoteInput::Key { pressed: true, .. }) {
                    self.typing = None;
                }
                self.apply(input);
            }
        }
    }

    /// Types the next piece of the text.
    pub(super) fn type_piece(&mut self) {
        let Some(typing) = self.typing.as_mut() else {
            return;
        };
        let piece = typing.next_piece();
        if typing.typed == typing.text.len() {
            self.typing = None;
        }
        if let Some(piece) = piece {
            self.apply(piece);
        }
    }

    /// Applies `input`. A failure means the input device or its helper is
    /// gone, which ends the session so that its input is released.
    fn apply(&mut self, input: RemoteInput) {
        if let Err(error) = self.input.apply(input) {
            tracing::warn!(%error, "remote input failed; releasing session input");
            self.typing = None;
            let _ = self.errors.send(ControlCommand::Stop);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use meshrmm_protocol::{
        ClipboardContent, CursorShape, FileMessage, MAX_TYPED_TEXT_CHARS, SessionMessage,
    };

    use super::*;

    type OnApply = Box<dyn Fn(&RemoteInput) + Send + Sync>;

    /// Input that records what it is asked to apply, and fails when told to.
    #[derive(Default)]
    struct RecordedInput {
        applied: Mutex<Vec<RemoteInput>>,
        fail: std::sync::atomic::AtomicBool,
        /// Called with each input before it is recorded.
        on_apply: Option<OnApply>,
    }

    impl RecordedInput {
        fn applied(&self) -> Vec<RemoteInput> {
            self.applied.lock().unwrap().clone()
        }
    }

    impl ScreenInput for RecordedInput {
        fn set_wallpaper_hidden(&self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn set_prevent_idle_lock(&self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn set_blackout(&self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn maintenance_state(&self) -> Option<SessionMessage> {
            None
        }
        fn set_agent_input_blocked(&self, _: bool) -> anyhow::Result<()> {
            Ok(())
        }
        fn apply_files(&self, _: FileMessage) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll_files(&self) -> Option<FileMessage> {
            None
        }
        fn files_ready(&self) -> Arc<tokio::sync::Notify> {
            Arc::default()
        }
        fn apply(&self, input: RemoteInput) -> anyhow::Result<()> {
            if let Some(on_apply) = &self.on_apply {
                on_apply(&input);
            }
            self.applied.lock().unwrap().push(input);
            anyhow::ensure!(
                !self.fail.load(std::sync::atomic::Ordering::SeqCst),
                "the input device is gone"
            );
            Ok(())
        }
        fn annotate(&self, _: meshrmm_protocol::Annotation) -> anyhow::Result<()> {
            Ok(())
        }
        fn release_all(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn cursor_shape(&self) -> CursorShape {
            CursorShape::Default
        }
        fn agent_pointer_display(&self) -> Option<DisplayId> {
            None
        }
        fn viewer_controls_input(&self) -> bool {
            true
        }
        fn apply_clipboard(&self, _: ClipboardContent) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll_clipboard(&self) -> anyhow::Result<Option<ClipboardContent>> {
            Ok(None)
        }
        fn start_chat(&self) -> anyhow::Result<()> {
            Ok(())
        }
        fn stop_chat(&self) {}
        fn apply_chat(&self, _: String) -> anyhow::Result<()> {
            Ok(())
        }
        fn poll_chat(&self) -> anyhow::Result<Option<String>> {
            Ok(None)
        }
        fn chat_ready(&self) -> Arc<tokio::sync::Notify> {
            Arc::default()
        }
    }

    fn type_text(text: &str) -> InputCommand {
        InputCommand::Input(RemoteInput::TypeText {
            display_id: DisplayId(1),
            text: text.into(),
        })
    }

    fn pointer(x: u16) -> InputCommand {
        InputCommand::Input(RemoteInput::PointerMove {
            display_id: DisplayId(1),
            x,
            y: 0,
        })
    }

    fn key(pressed: bool) -> InputCommand {
        InputCommand::Input(RemoteInput::Key {
            display_id: DisplayId(1),
            scan_code: 0x1e,
            extended: false,
            pressed,
        })
    }

    fn worker() -> (
        InputWorker,
        Arc<RecordedInput>,
        mpsc::UnboundedReceiver<ControlCommand>,
    ) {
        let input = Arc::new(RecordedInput::default());
        let (errors, received) = mpsc::unbounded_channel();
        (InputWorker::new(input.clone(), errors), input, received)
    }

    fn typed(applied: &[RemoteInput]) -> Vec<&str> {
        applied
            .iter()
            .filter_map(|input| match input {
                RemoteInput::TypeText { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The longest text: lowercase and capitals, keys without a US key, CR
    /// LF pairs and a character of four UTF-8 bytes.
    fn longest_text() -> String {
        // A CR LF across where the first piece would end.
        let mut text = "a".repeat(TYPING_PIECE_CHARS - 1);
        text.push_str("\r\n");
        text.extend(
            "Ab\r\nç🔑-"
                .chars()
                .cycle()
                .take(MAX_TYPED_TEXT_CHARS - TYPING_PIECE_CHARS - 1),
        );
        text
    }

    #[test]
    fn long_text_is_typed_completely_in_order_in_bounded_pieces() {
        let (mut worker, input, mut errors) = worker();
        let text = longest_text();
        assert_eq!(text.chars().count(), MAX_TYPED_TEXT_CHARS);
        worker.command(type_text(&text));
        let mut pieces = 0;
        while worker.typing() {
            worker.type_piece();
            pieces += 1;
            assert!(pieces <= MAX_TYPED_TEXT_CHARS, "typing never finished");
        }
        let applied = input.applied();
        let typed = typed(&applied);
        assert_eq!(typed.len(), pieces);
        assert!(pieces >= MAX_TYPED_TEXT_CHARS / TYPING_PIECE_CHARS);
        assert!(
            typed
                .iter()
                .all(|piece| piece.chars().count() <= TYPING_PIECE_CHARS)
        );
        assert!(
            typed
                .windows(2)
                .all(|pair| !(pair[0].ends_with('\r') && pair[1].starts_with('\n'))),
            "a CR LF pair was split"
        );
        assert_eq!(typed.concat(), text);
        assert!(errors.try_recv().is_err());
    }

    #[test]
    fn stopping_types_nothing_more() {
        for stop in [InputCommand::StopTyping, type_text(""), key(true)] {
            let (mut worker, input, _errors) = worker();
            worker.command(type_text(&"x".repeat(10 * TYPING_PIECE_CHARS)));
            worker.type_piece();
            worker.type_piece();
            worker.command(stop);
            assert!(!worker.typing());
            worker.type_piece();
            assert_eq!(
                typed(&input.applied()).concat().len(),
                2 * TYPING_PIECE_CHARS
            );
        }
    }

    #[test]
    fn a_new_text_replaces_the_one_being_typed() {
        let (mut worker, input, _errors) = worker();
        worker.command(type_text(&"x".repeat(10 * TYPING_PIECE_CHARS)));
        worker.type_piece();
        worker.command(type_text("new"));
        while worker.typing() {
            worker.type_piece();
        }
        assert_eq!(
            typed(&input.applied()),
            ["x".repeat(TYPING_PIECE_CHARS).as_str(), "new"]
        );
    }

    #[test]
    fn pointer_input_carries_on_and_key_releases_do_not_stop_typing() {
        let (mut worker, input, _errors) = worker();
        worker.command(type_text(&"x".repeat(3 * TYPING_PIECE_CHARS)));
        worker.type_piece();
        worker.command(pointer(7));
        worker.command(key(false));
        assert!(worker.typing());
        worker.type_piece();
        worker.command(key(true));
        worker.type_piece();
        let applied = input.applied();
        assert!(matches!(applied[0], RemoteInput::TypeText { .. }));
        assert!(matches!(applied[1], RemoteInput::PointerMove { x: 7, .. }));
        assert!(matches!(
            applied[2],
            RemoteInput::Key { pressed: false, .. }
        ));
        assert!(matches!(applied[3], RemoteInput::TypeText { .. }));
        // The press stops the typing, then reaches the device itself.
        assert!(matches!(applied[4], RemoteInput::Key { pressed: true, .. }));
        assert_eq!(applied.len(), 5);
    }

    #[test]
    fn text_over_the_maximum_is_refused_with_a_message() {
        let (mut worker, input, mut errors) = worker();
        worker.command(type_text(&"x".repeat(MAX_TYPED_TEXT_CHARS + 1)));
        assert!(!worker.typing());
        assert!(input.applied().is_empty());
        assert!(matches!(
            errors.try_recv(),
            Ok(ControlCommand::MaintenanceError(message))
                if message.contains("at most 5000 can be typed")
        ));
    }

    /// Whether the next input applied was a piece of text.
    async fn next(applied: &mut mpsc::UnboundedReceiver<bool>) -> bool {
        tokio::time::timeout(std::time::Duration::from_secs(5), applied.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_sent_while_a_piece_is_typed_goes_before_the_next_piece() {
        let (seen, mut applied) = mpsc::unbounded_channel();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let gate = Mutex::new(gate);
        let first = std::sync::atomic::AtomicBool::new(true);
        let input = Arc::new(RecordedInput {
            on_apply: Some(Box::new(move |input| {
                let _ = seen.send(matches!(input, RemoteInput::TypeText { .. }));
                // The first piece is typed until the test lets it finish.
                if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    let _ = gate
                        .lock()
                        .unwrap()
                        .recv_timeout(std::time::Duration::from_secs(5));
                }
            })),
            ..Default::default()
        });
        let (errors, _errors) = mpsc::unbounded_channel();
        let (commands, mut task) = super::super::service_workers::spawn_input_worker(
            input.clone(),
            Arc::new(webrtc::data_channel::RTCDataChannel::default()),
            errors,
        )
        .unwrap();
        commands
            .try_send(type_text(&"x".repeat(3 * TYPING_PIECE_CHARS)))
            .unwrap();
        assert!(next(&mut applied).await);
        commands.try_send(pointer(9)).unwrap();
        release.send(()).unwrap();
        for _ in 0..3 {
            next(&mut applied).await;
        }
        task.shutdown().await;
        let applied = input.applied();
        assert!(matches!(applied[0], RemoteInput::TypeText { .. }));
        assert!(matches!(applied[1], RemoteInput::PointerMove { x: 9, .. }));
        assert_eq!(typed(&applied).concat(), "x".repeat(3 * TYPING_PIECE_CHARS));
        assert_eq!(applied.len(), 4);
    }

    #[test]
    fn a_failed_piece_ends_the_session_and_the_typing() {
        let (mut worker, input, mut errors) = worker();
        worker.command(type_text(&"x".repeat(3 * TYPING_PIECE_CHARS)));
        input.fail.store(true, std::sync::atomic::Ordering::SeqCst);
        worker.type_piece();
        assert!(!worker.typing());
        assert!(matches!(errors.try_recv(), Ok(ControlCommand::Stop)));
    }
}
