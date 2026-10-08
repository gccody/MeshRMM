//! Live connections: Agents' control sockets, remote sessions' signaling
//! sockets, and the website's presence stream.
pub mod coordinator;
mod hub;
pub mod presence;
pub mod sessions;

use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};

pub use self::{
    hub::{AgentConnection, AgentHub, ToAgent},
    presence::Presence,
    sessions::Sessions,
};

/// Agents and viewers ping every 20 seconds and give up on a socket after a
/// minute without a frame; the server does the same.
pub const LIVENESS: Duration = Duration::from_secs(60);

/// A peer that takes longer than this to accept a message has stopped
/// reading, and is dropped.
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Sends `message`. Returns whether the peer took it in time.
pub async fn send(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(SEND_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

pub fn close_frame(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: reason.into(),
    }))
}
