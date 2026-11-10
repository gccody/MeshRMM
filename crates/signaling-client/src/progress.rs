//! What the socket under a signaling WebSocket has written, so the
//! connection can tell a slow write from a stalled one, and a busy link from
//! a dead one.
//!
//! A WebSocket message is one write to the library however large it is, so
//! only the stream underneath sees bytes go out. On a slow uplink a large
//! report takes minutes to write while the operating system takes a little
//! of it at a time; on a dead one it stops taking anything once its send
//! buffer is full.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

/// A stream that records its writes in a [`Progress`] it shares.
#[derive(Debug)]
pub struct Tracked<S> {
    stream: S,
    progress: Arc<Progress>,
}

impl<S> Tracked<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            progress: Arc::default(),
        }
    }

    pub(crate) fn progress(&self) -> Arc<Progress> {
        Arc::clone(&self.progress)
    }
}

#[derive(Debug)]
pub(crate) struct Progress(Mutex<State>);

#[derive(Debug)]
struct State {
    written_at: Instant,
    blocked: bool,
    drained_at: Option<Instant>,
}

impl Default for Progress {
    fn default() -> Self {
        Self(Mutex::new(State {
            written_at: Instant::now(),
            blocked: false,
            drained_at: None,
        }))
    }
}

impl Progress {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // Nothing panics while holding the lock, so a poisoned one is whole.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// When the socket last took bytes.
    pub(crate) fn written_at(&self) -> Instant {
        self.state().written_at
    }

    /// When the socket last took bytes after its send buffer had been full.
    /// Room in a full buffer only comes from the peer acknowledging what it
    /// received, so this shows the peer is there even while nothing it sends
    /// can be read, because its pongs wait behind a large message.
    pub(crate) fn drained_at(&self) -> Option<Instant> {
        self.state().drained_at
    }

    fn wrote(&self, result: &Poll<io::Result<usize>>) {
        let mut state = self.state();
        match result {
            Poll::Ready(Ok(written)) if *written > 0 => {
                let now = Instant::now();
                state.written_at = now;
                if std::mem::take(&mut state.blocked) {
                    state.drained_at = Some(now);
                }
            }
            Poll::Pending => state.blocked = true,
            Poll::Ready(_) => {}
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Tracked<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tracked<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(cx, buf);
        self.progress.wrote(&result);
        result
    }

    // rustls writes its records with one vectored write.
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write_vectored(cx, bufs);
        self.progress.wrote(&result);
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}
