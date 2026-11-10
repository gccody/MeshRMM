//! Commands on their way to a helper's pipe, written in order by a thread of
//! their own so that a helper reading slowly holds up only its own commands.
//!
//! A queue of 64 commands and a byte budget bound what a helper that stops
//! reading can make the Agent hold. What a full queue does depends on the
//! command: pointer motion takes the place of motion queued last, or waits
//! outside the queue for room, replaced by later motion; the rest of the
//! viewer's input waits for room, so keys and typed text are neither lost
//! nor reordered; other commands fail at once.
use std::collections::VecDeque;
use std::io::{self, BufWriter, Write};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use meshrmm_protocol::{MAX_CLIPBOARD_WIRE_BYTES, RemoteInput};

use super::{MAX_CONTROL_BYTES, ParentCommand, write_command};

/// Commands queued for the writer.
const COMMAND_SLOTS: usize = 64;
/// Bytes queued or being written: two clipboard copies and a control message.
const COMMAND_BYTES: usize = 2 * MAX_CLIPBOARD_WIRE_BYTES + MAX_CONTROL_BYTES;
/// How long the viewer's input waits for room before the helper counts as
/// stuck. A helper takes milliseconds to type a piece of text.
const INPUT_ROOM_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct CommandWriter {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|error| error.into_inner())
    }
}

#[derive(Default)]
struct Queue {
    commands: VecDeque<Queued>,
    /// Bytes queued or being written.
    bytes: usize,
    /// The latest pointer motion that found no room. It is queued when room
    /// appears, and dropped when another command takes that room first.
    held_motion: Option<Vec<u8>>,
    /// The pipe failed.
    broken: bool,
    /// The writer was dropped: write what is queued, then stop.
    finished: bool,
}

struct Queued {
    bytes: Vec<u8>,
    motion: bool,
}

impl Queue {
    fn has_room(&self, bytes: usize) -> bool {
        self.commands.len() < COMMAND_SLOTS && self.bytes + bytes <= COMMAND_BYTES
    }

    fn push(&mut self, bytes: Vec<u8>, motion: bool) {
        self.bytes += bytes.len();
        self.commands.push_back(Queued { bytes, motion });
    }

    fn queue_held_motion(&mut self) {
        if self
            .held_motion
            .as_ref()
            .is_some_and(|motion| self.has_room(motion.len()))
            && let Some(motion) = self.held_motion.take()
        {
            self.push(motion, true);
        }
    }
}

impl CommandWriter {
    pub(super) fn new(writer: impl Write + Send + 'static) -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let queue = Arc::clone(&shared);
        thread::Builder::new()
            .name("meshrmm-helper-writer".into())
            .spawn(move || write_commands(&queue, writer))?;
        Ok(Self { shared })
    }

    /// Queues `bytes`, or fails at once when there is no room.
    pub(super) fn send(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        self.enqueue(bytes, None)
    }

    /// Queues `bytes` after everything sent before, waiting up to `timeout`
    /// for room.
    pub(super) fn send_waiting(&self, bytes: Vec<u8>, timeout: Duration) -> anyhow::Result<()> {
        self.enqueue(bytes, Some(timeout))
    }

    fn enqueue(&self, bytes: Vec<u8>, timeout: Option<Duration>) -> anyhow::Result<()> {
        anyhow::ensure!(
            bytes.len() <= COMMAND_BYTES,
            "helper pipe byte budget exhausted"
        );
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        let mut queue = self.shared.lock();
        loop {
            anyhow::ensure!(!queue.broken, "the helper's command pipe is closed");
            // Motion held back from before this command goes ahead of it.
            let with_motion = match &queue.held_motion {
                Some(motion) => {
                    queue.commands.len() + 2 <= COMMAND_SLOTS
                        && queue.bytes + motion.len() + bytes.len() <= COMMAND_BYTES
                }
                None => queue.has_room(bytes.len()),
            };
            // A command that cannot wait drops it instead.
            if with_motion || (deadline.is_none() && queue.has_room(bytes.len())) {
                if let Some(motion) = queue.held_motion.take()
                    && with_motion
                {
                    queue.push(motion, true);
                }
                queue.push(bytes, false);
                self.shared.changed.notify_all();
                return Ok(());
            }
            let Some(deadline) = deadline else {
                anyhow::bail!("helper command queue full");
            };
            let now = Instant::now();
            anyhow::ensure!(
                now < deadline,
                "the helper did not take its commands in time"
            );
            queue = self
                .shared
                .changed
                .wait_timeout(queue, deadline - now)
                .unwrap_or_else(|error| error.into_inner())
                .0;
        }
    }

    /// Queues pointer motion in place of motion queued last, or holds it back
    /// while there is no room. Never waits.
    pub(super) fn send_motion(&self, bytes: Vec<u8>) -> anyhow::Result<()> {
        let mut queue = self.shared.lock();
        anyhow::ensure!(!queue.broken, "the helper's command pipe is closed");
        let queued = &mut *queue;
        if let Some(last) = queued.commands.back_mut().filter(|last| last.motion) {
            queued.bytes = queued.bytes - last.bytes.len() + bytes.len();
            last.bytes = bytes;
        } else if queued.held_motion.is_none() && queued.has_room(bytes.len()) {
            queued.push(bytes, true);
            self.shared.changed.notify_all();
        } else {
            queued.held_motion = Some(bytes);
        }
        Ok(())
    }
}

impl Drop for CommandWriter {
    fn drop(&mut self) {
        self.shared.lock().finished = true;
        self.shared.changed.notify_all();
    }
}

fn write_commands(shared: &Shared, writer: impl Write) {
    let mut writer = BufWriter::new(writer);
    loop {
        let bytes = {
            let mut queue = shared.lock();
            loop {
                queue.queue_held_motion();
                if let Some(command) = queue.commands.pop_front() {
                    break command.bytes;
                }
                if queue.finished {
                    return;
                }
                queue = shared
                    .changed
                    .wait(queue)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };
        let written = writer.write_all(&bytes).and_then(|()| writer.flush());
        let mut queue = shared.lock();
        queue.bytes -= bytes.len();
        queue.broken = written.is_err();
        shared.changed.notify_all();
        if queue.broken {
            return;
        }
    }
}

/// Sends the viewer's input to a helper, pointer motion by
/// [`CommandWriter::send_motion`] and the rest in order, waiting for room.
pub(super) fn send_input(writer: &CommandWriter, input: RemoteInput) -> anyhow::Result<()> {
    let motion = matches!(input, RemoteInput::PointerMove { .. });
    let mut bytes = Vec::new();
    write_command(&mut bytes, &ParentCommand::Input(input))?;
    if motion {
        writer.send_motion(bytes)
    } else {
        writer.send_waiting(bytes, INPUT_ROOM_TIMEOUT)
    }
}

/// Sends `command` in order after the viewer's input, waiting for room.
pub(super) fn send_after_input(
    writer: &CommandWriter,
    command: &ParentCommand,
) -> anyhow::Result<()> {
    let mut bytes = Vec::new();
    write_command(&mut bytes, command)?;
    writer.send_waiting(bytes, INPUT_ROOM_TIMEOUT)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use meshrmm_protocol::DisplayId;

    use super::super::read_command;
    use super::*;

    /// A pipe whose first write says it started and then waits until the
    /// pipe is opened, and that hands what it was written when dropped.
    struct GatedPipe {
        entered: mpsc::Sender<()>,
        gate: Option<mpsc::Receiver<()>>,
        written: Vec<u8>,
        done: mpsc::Sender<Vec<u8>>,
    }

    impl Write for GatedPipe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(gate) = self.gate.take() {
                let _ = self.entered.send(());
                let _ = gate.recv_timeout(Duration::from_secs(5));
            }
            self.written.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for GatedPipe {
        fn drop(&mut self) {
            let _ = self.done.send(std::mem::take(&mut self.written));
        }
    }

    /// A writer whose helper is busy with `first` until the returned sender
    /// lets it go on.
    fn busy_writer(
        first: RemoteInput,
    ) -> (CommandWriter, mpsc::Sender<()>, mpsc::Receiver<Vec<u8>>) {
        let (entered, started) = mpsc::channel();
        let (open, gate) = mpsc::channel();
        let (done, written) = mpsc::channel();
        let pipe = GatedPipe {
            entered,
            gate: Some(gate),
            written: Vec::new(),
            done,
        };
        let writer = CommandWriter::new(pipe).unwrap();
        send_input(&writer, first).unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        (writer, open, written)
    }

    fn motion(x: u16) -> RemoteInput {
        RemoteInput::PointerMove {
            display_id: DisplayId(1),
            x,
            y: 0,
        }
    }

    fn piece(index: usize) -> RemoteInput {
        RemoteInput::TypeText {
            display_id: DisplayId(1),
            text: format!("piece {index}"),
        }
    }

    fn key(scan_code: u16) -> RemoteInput {
        RemoteInput::Key {
            display_id: DisplayId(1),
            scan_code,
            extended: false,
            pressed: true,
        }
    }

    fn commands(bytes: &[u8]) -> Vec<RemoteInput> {
        let mut reader = bytes;
        let mut commands = Vec::new();
        while !reader.is_empty() {
            match read_command(&mut reader).unwrap() {
                ParentCommand::Input(input) => commands.push(input),
                _ => panic!("not input"),
            }
        }
        commands
    }

    #[test]
    fn pointer_motion_while_text_is_typed_neither_fails_nor_reorders_keys() {
        // The helper is busy with the first piece and reads nothing more.
        let (writer, open, written) = busy_writer(piece(0));
        let mut sent = vec![piece(0)];
        for index in 1..COMMAND_SLOTS - 1 {
            send_input(&writer, piece(index)).unwrap();
            sent.push(piece(index));
        }
        // Thousands of moves take one place.
        for x in 0..2_000 {
            send_input(&writer, motion(x)).unwrap();
        }
        sent.push(motion(1_999));
        send_input(&writer, key(0x1e)).unwrap();
        sent.push(key(0x1e));
        // The queue is full: these moves wait outside it, the latest only.
        for x in 2_000..4_000 {
            send_input(&writer, motion(x)).unwrap();
        }
        open.send(()).unwrap();
        // Waits for the room the helper makes; the held move goes first.
        send_input(&writer, key(0x30)).unwrap();
        sent.push(motion(3_999));
        sent.push(key(0x30));
        drop(writer);
        let written = written.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(commands(&written), sent);
    }

    #[test]
    fn input_waits_for_room_only_so_long() {
        let (writer, _open, _written) = busy_writer(piece(0));
        for index in 1..=COMMAND_SLOTS {
            writer
                .send_waiting(
                    {
                        let mut bytes = Vec::new();
                        write_command(&mut bytes, &ParentCommand::Input(piece(index))).unwrap();
                        bytes
                    },
                    Duration::ZERO,
                )
                .unwrap();
        }
        // The helper has stopped reading: input that must not be lost fails,
        // which ends the session as before; motion is held back.
        assert!(writer.send_waiting(vec![0], Duration::ZERO).is_err());
        assert!(writer.send(vec![0]).is_err());
        send_input(&writer, motion(1)).unwrap();
    }

    #[test]
    fn a_closed_pipe_fails_input_at_once() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let writer = CommandWriter::new(Closed).unwrap();
        writer.send(vec![0]).unwrap();
        // The writer marks the pipe broken once its write fails.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut queue = writer.shared.lock();
        while !queue.broken {
            let now = Instant::now();
            assert!(now < deadline, "the write never failed");
            queue = writer
                .shared
                .changed
                .wait_timeout(queue, deadline - now)
                .unwrap()
                .0;
        }
        drop(queue);
        assert!(send_input(&writer, key(0x1e)).is_err());
        assert!(send_input(&writer, motion(1)).is_err());
    }
}
