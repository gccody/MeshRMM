//! Between a terminal's socket and its shell.
//!
//! Binary messages are the shell's input one way and its output the other.
//! Output is flow controlled: the next chunk is taken from the shell only
//! while less than [`TERMINAL_WINDOW_BYTES`] sent is unacknowledged, so a
//! flood waits in the pseudo-terminal, and the shell blocks writing, instead
//! of piling up in memory here, in the server or in the browser. The socket
//! is always read, never held up by input the shell isn't taking, because
//! acknowledgements arrive behind that input: waiting for the shell there
//! while the shell waits for an acknowledgement would stop both. Input waits
//! in a queue of at most [`MAX_PENDING_INPUT_BYTES`]; more than that is
//! dropped. The website limits a paste to that much.
use std::collections::VecDeque;
use std::time::Duration;

use anyhow::Context;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use meshrmm_protocol::{
    MAX_TERMINAL_MESSAGE_BYTES, TERMINAL_WINDOW_BYTES, TerminalControl, TerminalEvent,
    valid_terminal_size,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::{self, Message};

/// A peer that takes longer than this to accept a message has stopped
/// reading.
pub(super) const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// The Agent pings the server this often, and gives up on a socket that has
/// said nothing for [`LIVENESS`].
const PING_INTERVAL: Duration = Duration::from_secs(20);
const LIVENESS: Duration = Duration::from_secs(60);
/// Input the shell hasn't read yet. Input beyond it is dropped.
const MAX_PENDING_INPUT_BYTES: usize = 1024 * 1024;
/// How often dropped input is logged.
const DROPPED_INPUT_LOG_INTERVAL: Duration = Duration::from_secs(10);
/// After the shell exits, output still arriving is passed on until none has
/// come for this long, and for at most [`MAX_EXIT_DRAIN`].
const EXIT_DRAIN: Duration = Duration::from_millis(300);
const MAX_EXIT_DRAIN: Duration = Duration::from_secs(5);

/// A shell's ends of the relay.
pub(in crate::remote) struct Io {
    /// The shell's output, in chunks of at most
    /// [`meshrmm_protocol::MAX_TERMINAL_OUTPUT_BYTES`]. Ends when the
    /// pseudo-terminal closes.
    pub output: mpsc::Receiver<Vec<u8>>,
    pub input: mpsc::Sender<Vec<u8>>,
    /// The shell's exit code, or `None` when a signal ended it.
    pub exited: oneshot::Receiver<Option<i32>>,
}

/// Changes the pseudo-terminal's size.
pub(in crate::remote) trait Resize: Sync {
    fn resize(&self, columns: u16, rows: u16) -> anyhow::Result<()>;
}

/// How a relay ended.
#[derive(Debug)]
pub(super) enum Ending {
    /// The shell exited, and the browser was told.
    Exited(Option<i32>),
    /// The server closed the socket, or it dropped.
    Closed,
    Failed(anyhow::Error),
}

/// How much output is in flight.
#[derive(Debug, Default)]
struct Window {
    sent: u64,
    acknowledged: u64,
}

impl Window {
    /// Whether more output may be sent.
    fn open(&self) -> bool {
        self.sent - self.acknowledged < TERMINAL_WINDOW_BYTES
    }

    fn sent(&mut self, bytes: usize) {
        self.sent += bytes as u64;
    }

    /// Records that the browser has drawn `bytes` in total. An
    /// acknowledgement that goes backwards or past what was sent is ignored.
    fn acknowledge(&mut self, bytes: u64) -> bool {
        if bytes < self.acknowledged || bytes > self.sent {
            return false;
        }
        self.acknowledged = bytes;
        true
    }
}

pub(super) async fn send_event<S>(socket: &mut S, event: &TerminalEvent) -> anyhow::Result<()>
where
    S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    let text = serde_json::to_string(event).expect("terminal events serialize");
    send(socket, Message::Text(text.into())).await
}

async fn send<S>(socket: &mut S, message: Message) -> anyhow::Result<()>
where
    S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    tokio::time::timeout(SEND_TIMEOUT, socket.send(message))
        .await
        .context("the terminal's socket stopped taking messages")?
        .context("could not write to the terminal's socket")
}

/// Relays until the shell exits or the socket ends. The caller ends the
/// shell afterwards.
pub(super) async fn relay<S>(socket: &mut S, io: Io, process: &dyn Resize) -> Ending
where
    S: Stream<Item = Result<Message, tungstenite::Error>>
        + Sink<Message, Error = tungstenite::Error>
        + Unpin,
{
    let Io {
        mut output,
        input,
        mut exited,
    } = io;
    let mut window = Window::default();
    let mut pending = VecDeque::<Vec<u8>>::new();
    let mut pending_bytes = 0;
    let mut output_open = true;
    // The exit code, and when the shell exited.
    let mut exit: Option<(Option<i32>, Instant)> = None;
    let mut drain_until = Instant::now();
    let mut ping = tokio::time::interval_at(Instant::now() + PING_INTERVAL, PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_heard = Instant::now();
    let mut dropped_input_logged: Option<Instant> = None;
    loop {
        if let Some((code, _)) = exit
            && !output_open
        {
            return finish(socket, code).await;
        }
        tokio::select! {
            message = socket.next() => {
                last_heard = Instant::now();
                let message = match message {
                    Some(Ok(message)) => message,
                    Some(Err(_)) | None => return Ending::Closed,
                };
                match message {
                    Message::Binary(bytes) => {
                        if bytes.len() > MAX_TERMINAL_MESSAGE_BYTES {
                            return Ending::Failed(anyhow::anyhow!("the server sent too much input at once"));
                        }
                        // Input after the shell exited goes nowhere.
                        if bytes.is_empty() || exit.is_some() {
                        } else if pending_bytes + bytes.len() > MAX_PENDING_INPUT_BYTES {
                            if dropped_input_logged.is_none_or(|at| at.elapsed() >= DROPPED_INPUT_LOG_INTERVAL) {
                                dropped_input_logged = Some(Instant::now());
                                tracing::warn!(pending_bytes, dropped_bytes = bytes.len(), "dropped terminal input the shell isn't reading");
                            }
                        } else {
                            pending_bytes += bytes.len();
                            pending.push_back(bytes.to_vec());
                        }
                    }
                    Message::Text(text) => match serde_json::from_str::<TerminalControl>(&text) {
                        Ok(TerminalControl::Resize { columns, rows }) => {
                            if !valid_terminal_size(columns, rows) {
                                tracing::debug!(columns, rows, "ignored a terminal size out of range");
                            } else if let Err(error) = process.resize(columns, rows) {
                                tracing::warn!(error = %format!("{error:#}"), "could not resize a terminal");
                            }
                        }
                        Ok(TerminalControl::Ack { bytes }) => {
                            if !window.acknowledge(bytes) {
                                tracing::debug!(bytes, sent = window.sent, "ignored an impossible terminal acknowledgement");
                            }
                        }
                        Err(_) => return Ending::Failed(anyhow::anyhow!("the server sent an unsupported terminal message")),
                    },
                    Message::Ping(payload) => {
                        if let Err(error) = send(socket, Message::Pong(payload)).await {
                            return Ending::Failed(error);
                        }
                    }
                    Message::Close(_) => return Ending::Closed,
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
            permit = input.reserve(), if !pending.is_empty() => {
                match permit {
                    Ok(permit) => {
                        let bytes = pending.pop_front().expect("pending input is not empty");
                        pending_bytes -= bytes.len();
                        permit.send(bytes);
                    }
                    // The shell stopped reading input for good.
                    Err(_) => {
                        pending.clear();
                        pending_bytes = 0;
                    }
                }
            }
            chunk = output.recv(), if output_open && window.open() => match chunk {
                Some(chunk) => {
                    window.sent(chunk.len());
                    if let Err(error) = send(socket, Message::Binary(chunk.into())).await {
                        return Ending::Failed(error);
                    }
                    if exit.is_some() {
                        drain_until = Instant::now() + EXIT_DRAIN;
                    }
                }
                None => output_open = false,
            },
            code = &mut exited, if exit.is_none() => {
                exit = Some((code.unwrap_or(None), Instant::now()));
                drain_until = Instant::now() + EXIT_DRAIN;
                pending.clear();
                pending_bytes = 0;
            }
            () = tokio::time::sleep_until(drain_until), if exit.is_some() => {
                let (code, exited_at) = exit.expect("the shell exited");
                // Output held back for the browser gets a little longer.
                if !window.open() && exited_at.elapsed() < MAX_EXIT_DRAIN {
                    drain_until = Instant::now() + EXIT_DRAIN;
                } else {
                    return finish(socket, code).await;
                }
            }
            _ = ping.tick() => {
                if last_heard.elapsed() > LIVENESS {
                    return Ending::Failed(anyhow::anyhow!("the server stopped answering"));
                }
                if let Err(error) = send(socket, Message::Ping(Default::default())).await {
                    return Ending::Failed(error);
                }
            }
        }
    }
}

/// Tells the browser the shell exited, and closes.
async fn finish<S>(socket: &mut S, code: Option<i32>) -> Ending
where
    S: Sink<Message, Error = tungstenite::Error> + Unpin,
{
    if let Err(error) = send_event(socket, &TerminalEvent::Exited { code }).await {
        return Ending::Failed(error);
    }
    let _ = tokio::time::timeout(SEND_TIMEOUT, socket.close()).await;
    Ending::Exited(code)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use meshrmm_protocol::MAX_TERMINAL_OUTPUT_BYTES;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::tungstenite::protocol::Role;

    use super::*;

    #[derive(Default)]
    struct Sizes(Mutex<Vec<(u16, u16)>>);

    impl Resize for Sizes {
        fn resize(&self, columns: u16, rows: u16) -> anyhow::Result<()> {
            self.0.lock().unwrap().push((columns, rows));
            Ok(())
        }
    }

    #[test]
    fn the_window_closes_until_output_is_acknowledged() {
        let mut window = Window::default();
        assert!(window.open());
        window.sent(TERMINAL_WINDOW_BYTES as usize - 1);
        assert!(window.open());
        window.sent(1);
        assert!(!window.open());
        assert!(
            !window.acknowledge(TERMINAL_WINDOW_BYTES + 1),
            "past what was sent"
        );
        assert!(window.acknowledge(1000));
        assert!(window.open());
        assert!(!window.acknowledge(999), "backwards");
        assert!(window.acknowledge(1000));
        assert!(window.acknowledge(TERMINAL_WINDOW_BYTES));
        assert_eq!(window.acknowledged, TERMINAL_WINDOW_BYTES);
    }

    type Socket = WebSocketStream<tokio::io::DuplexStream>;

    async fn pair() -> (Socket, Socket) {
        let (agent, server) = tokio::io::duplex(1 << 20);
        (
            WebSocketStream::from_raw_socket(agent, Role::Client, None).await,
            WebSocketStream::from_raw_socket(server, Role::Server, None).await,
        )
    }

    /// What the server side reads next, skipping pings.
    async fn next(server: &mut Socket) -> Message {
        loop {
            let message = tokio::time::timeout(Duration::from_secs(5), server.next())
                .await
                .expect("the relay said nothing")
                .expect("the relay's socket ended")
                .unwrap();
            if !matches!(message, Message::Ping(_)) {
                return message;
            }
        }
    }

    struct Fake {
        output: mpsc::Sender<Vec<u8>>,
        input: mpsc::Receiver<Vec<u8>>,
        exited: oneshot::Sender<Option<i32>>,
    }

    fn fake() -> (Io, Fake) {
        let (io, ends) = super::super::channels();
        (
            io,
            Fake {
                output: ends.output,
                input: ends.input,
                exited: ends.exited,
            },
        )
    }

    #[tokio::test]
    async fn input_resizes_and_output_pass_through_until_the_shell_exits() {
        let (mut agent, mut server) = pair().await;
        let (io, mut shell) = fake();
        let sizes = Sizes::default();
        let relay = async { relay(&mut agent, io, &sizes).await };
        let peer = async {
            server
                .send(Message::Binary(b"ls\r".to_vec().into()))
                .await
                .unwrap();
            assert_eq!(shell.input.recv().await.unwrap(), b"ls\r");
            server
                .send(Message::Text(
                    r#"{"type":"resize","columns":120,"rows":40}"#.into(),
                ))
                .await
                .unwrap();
            // Out of range: ignored.
            server
                .send(Message::Text(
                    r#"{"type":"resize","columns":0,"rows":40}"#.into(),
                ))
                .await
                .unwrap();
            shell.output.send(b"file\r\n".to_vec()).await.unwrap();
            assert_eq!(
                next(&mut server).await,
                Message::Binary(b"file\r\n".to_vec().into())
            );
            shell.exited.send(Some(3)).unwrap();
            drop(shell.output);
            assert_eq!(
                next(&mut server).await,
                Message::Text(r#"{"type":"exited","code":3}"#.into())
            );
            assert!(matches!(next(&mut server).await, Message::Close(_)));
        };
        let (ending, ()) = tokio::join!(relay, peer);
        assert!(matches!(ending, Ending::Exited(Some(3))), "{ending:?}");
        assert_eq!(*sizes.0.lock().unwrap(), vec![(120, 40)]);
    }

    #[tokio::test]
    async fn output_waits_for_acknowledgements() {
        let (mut agent, mut server) = pair().await;
        let (io, shell) = fake();
        let sizes = Sizes::default();
        let chunks = TERMINAL_WINDOW_BYTES as usize / MAX_TERMINAL_OUTPUT_BYTES;
        let producer = tokio::spawn(async move {
            // Twice the window, plus the channel's room.
            for _ in 0..chunks * 2 {
                if shell
                    .output
                    .send(vec![b'y'; MAX_TERMINAL_OUTPUT_BYTES])
                    .await
                    .is_err()
                {
                    break;
                }
            }
            shell
        });
        let relay = async { relay(&mut agent, io, &sizes).await };
        let peer = async {
            let mut received = 0;
            while received < TERMINAL_WINDOW_BYTES as usize {
                match next(&mut server).await {
                    Message::Binary(bytes) => received += bytes.len(),
                    other => panic!("unexpected {other:?}"),
                }
            }
            assert_eq!(received, TERMINAL_WINDOW_BYTES as usize);
            // Nothing more until the browser acknowledges.
            assert!(
                tokio::time::timeout(Duration::from_millis(300), server.next())
                    .await
                    .is_err()
            );
            server
                .send(Message::Text(
                    format!(r#"{{"type":"ack","bytes":{received}}}"#).into(),
                ))
                .await
                .unwrap();
            match next(&mut server).await {
                Message::Binary(bytes) => assert_eq!(bytes.len(), MAX_TERMINAL_OUTPUT_BYTES),
                other => panic!("unexpected {other:?}"),
            }
            server.close(None).await.unwrap();
        };
        let (ending, ()) = tokio::join!(relay, peer);
        assert!(matches!(ending, Ending::Closed), "{ending:?}");
        drop(producer);
    }

    #[tokio::test]
    async fn acknowledgements_pass_input_the_shell_isnt_reading() {
        let (mut agent, mut server) = pair().await;
        // The shell neither reads input nor gets to write more output.
        let (io, shell) = fake();
        let sizes = Sizes::default();
        let relay = async { relay(&mut agent, io, &sizes).await };
        let peer = async {
            shell.output.send(b"prompt".to_vec()).await.unwrap();
            assert!(matches!(next(&mut server).await, Message::Binary(_)));
            // Twice the input queue: the rest is dropped, not waited on.
            for _ in 0..2 * MAX_PENDING_INPUT_BYTES / MAX_TERMINAL_MESSAGE_BYTES {
                server
                    .send(Message::Binary(
                        vec![b'x'; MAX_TERMINAL_MESSAGE_BYTES].into(),
                    ))
                    .await
                    .unwrap();
            }
            server
                .send(Message::Text(
                    r#"{"type":"resize","columns":100,"rows":30}"#.into(),
                ))
                .await
                .unwrap();
            shell.exited.send(Some(0)).unwrap();
            assert_eq!(
                next(&mut server).await,
                Message::Text(r#"{"type":"exited","code":0}"#.into())
            );
            shell.input
        };
        let (ending, mut input) = tokio::join!(relay, peer);
        assert!(matches!(ending, Ending::Exited(Some(0))), "{ending:?}");
        assert_eq!(*sizes.0.lock().unwrap(), vec![(100, 30)]);
        let mut queued = 0;
        while let Ok(bytes) = input.try_recv() {
            queued += bytes.len();
        }
        assert!(queued <= MAX_PENDING_INPUT_BYTES, "{queued}");
    }

    #[tokio::test]
    async fn unsupported_messages_end_the_terminal() {
        let (mut agent, mut server) = pair().await;
        let (io, _shell) = fake();
        let sizes = Sizes::default();
        server
            .send(Message::Text(r#"{"type":"input"}"#.into()))
            .await
            .unwrap();
        let ending = relay(&mut agent, io, &sizes).await;
        assert!(matches!(ending, Ending::Failed(_)), "{ending:?}");
    }
}
