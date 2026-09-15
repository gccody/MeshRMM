//! Ephemeral native chat UI shared by the viewer and interactive endpoint helper.
use meshrmm_protocol::valid_chat_text;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "macos")]
use macos as native;
#[cfg(windows)]
use windows as native;

#[derive(Default)]
struct State {
    transcript: VecDeque<String>,
    outgoing: VecDeque<String>,
    outgoing_ready: Arc<tokio::sync::Notify>,
    revision: u64,
    available: bool,
    visible: bool,
    unread: usize,
    draft: String,
}
impl State {
    fn append(&mut self, who: &str, text: &str) {
        if self.transcript.len() == 200 {
            self.transcript.pop_front();
        }
        self.transcript.push_back(format!("{who}: {text}"));
        self.revision += 1;
    }
    fn send(&mut self, text: String) -> bool {
        if !valid_chat_text(&text) || self.outgoing.len() >= 32 {
            return false;
        }
        self.append("You", &text);
        self.outgoing.push_back(text);
        self.outgoing_ready.notify_one();
        true
    }
    fn text(&self) -> String {
        self.transcript
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Session state outlives popup views, including display/decoder changes.
#[derive(Clone, Default)]
pub struct ChatSession {
    state: Arc<Mutex<State>>,
    peer: String,
}
impl ChatSession {
    pub fn with_peer(peer: &str) -> Self {
        Self {
            peer: peer.to_owned(),
            ..Self::default()
        }
    }
    pub fn set_available(&self, available: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.available = available;
        if !available {
            state.visible = false;
        }
    }
    pub fn available(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .available
    }
    pub fn visible(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).visible
    }
    pub fn set_visible(&self, visible: bool) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.visible = visible && state.available;
        if state.visible {
            state.unread = 0;
        }
    }
    pub fn unread(&self) -> usize {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).unread
    }
    pub fn receive(&self, text: String) {
        if !valid_chat_text(&text) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.append(
            if self.peer.is_empty() {
                "Agent user"
            } else {
                &self.peer
            },
            &text,
        );
        if !state.visible {
            state.unread = state.unread.saturating_add(1);
        }
    }
    /// One transport consumer drains the queue after each notification.
    pub fn outgoing_ready(&self) -> Arc<tokio::sync::Notify> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outgoing_ready
            .clone()
    }
    pub fn poll(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outgoing
            .pop_front()
    }
}

#[cfg(target_os = "macos")]
pub use macos::Popup as ChatPopup;
#[cfg(windows)]
pub use windows::Popup as ChatPopup;

pub struct ChatWindow {
    state: Arc<Mutex<State>>,
    #[cfg(any(windows, target_os = "macos"))]
    window: native::Window,
    peer: String,
}
impl ChatWindow {
    pub fn open(peer: &str) -> anyhow::Result<Self> {
        let state = Arc::new(Mutex::new(State::default()));
        #[cfg(any(windows, target_os = "macos"))]
        let window = native::Window::open(Arc::clone(&state))?;
        Ok(Self {
            state,
            #[cfg(any(windows, target_os = "macos"))]
            window,
            peer: peer.to_owned(),
        })
    }
    pub fn receive(&self, text: String) {
        if valid_chat_text(&text) {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append(&self.peer, &text);
            self.refresh();
        }
    }
    pub fn poll(&self) -> Option<String> {
        self.refresh();
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .outgoing
            .pop_front()
    }
    fn refresh(&self) {
        #[cfg(any(windows, target_os = "macos"))]
        self.window.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_messages_and_backpressure_preserves_draft() {
        let mut state = State::default();
        assert!(!state.send(" \n".into()));
        assert!(!state.send("x".repeat(4097)));
        for _ in 0..32 {
            assert!(state.send("Hello 👋".into()));
        }
        assert!(!state.send("keep this draft".into()));
        assert_eq!(state.outgoing.len(), 32);
    }
    #[test]
    fn transcript_is_bounded_and_incoming_does_not_echo() {
        let mut state = State::default();
        for i in 0..250 {
            state.append("Peer", &i.to_string());
        }
        assert_eq!(state.transcript.len(), 200);
        assert_eq!(state.transcript.front().unwrap(), "Peer: 50");
        assert!(state.outgoing.is_empty());
    }
}

#[cfg(test)]
mod popup_tests {
    use super::*;
    #[test]
    fn unread_messages_accumulate_only_while_closed_and_opening_clears_them() {
        let chat = ChatSession::default();
        chat.set_available(true);
        chat.receive("first".into());
        chat.receive("second".into());
        assert_eq!(chat.unread(), 2);
        assert!(
            !chat.visible(),
            "viewer messages stay unread until the popup is opened"
        );
        chat.set_visible(true);
        assert_eq!(chat.unread(), 0);
        chat.receive("visible".into());
        assert_eq!(chat.unread(), 0);
        chat.set_visible(false);
        chat.receive("later".into());
        assert_eq!(chat.unread(), 1);
        assert_eq!(chat.state.lock().unwrap().transcript.len(), 4);
    }
    #[test]
    fn agent_session_labels_incoming_messages_as_viewer() {
        let chat = ChatSession::with_peer("Viewer");
        chat.receive("Hello 👋".into());
        assert_eq!(chat.state.lock().unwrap().text(), "Viewer: Hello 👋");
        assert_eq!(chat.unread(), 1);
    }
    #[test]
    fn invalid_and_outgoing_messages_do_not_mark_unread() {
        let chat = ChatSession::default();
        chat.receive(" \n".into());
        chat.receive("x".repeat(4097));
        assert_eq!(chat.unread(), 0);
        assert!(chat.state.lock().unwrap().send("my message".into()));
        assert_eq!(chat.unread(), 0);
        assert_eq!(chat.poll().as_deref(), Some("my message"));
        assert_eq!(chat.poll(), None);
    }
    #[test]
    fn view_replacement_preserves_history_draft_and_unread() {
        let chat = ChatSession::default();
        chat.set_available(true);
        chat.state.lock().unwrap().draft = "unfinished".into();
        chat.receive("new".into());
        let replacement = chat.clone();
        drop(chat);
        assert_eq!(replacement.unread(), 1);
        assert_eq!(replacement.state.lock().unwrap().draft, "unfinished");
        assert_eq!(replacement.state.lock().unwrap().transcript.len(), 1);
        replacement.set_available(false);
        replacement.set_visible(true);
        assert!(!replacement.visible());
        assert_eq!(replacement.unread(), 1);
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    #[tokio::test]
    async fn queued_and_later_messages_wake_the_consumer() {
        let chat = ChatSession::default();
        let ready = chat.outgoing_ready();
        chat.state.lock().unwrap().send("before wait".into());
        tokio::time::timeout(std::time::Duration::from_secs(1), ready.notified())
            .await
            .unwrap();
        assert_eq!(chat.poll().as_deref(), Some("before wait"));
        let waiting = ready.notified();
        tokio::pin!(waiting);
        waiting.as_mut().enable();
        for text in ["first", "second"] {
            chat.state.lock().unwrap().send(text.into());
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .unwrap();
        assert_eq!(chat.poll().as_deref(), Some("first"));
        assert_eq!(chat.poll().as_deref(), Some("second"));
        assert!(chat.poll().is_none());
        // Multiple sends may leave one coalesced permit after the queue drains.
        ready.notified().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), ready.notified())
                .await
                .is_err()
        );
    }
}
