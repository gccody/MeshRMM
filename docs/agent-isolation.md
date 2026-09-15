# Agent service isolation

## Signaling

The Agent coordinator and remote-session WebSockets each own a separate network
task. Ping/pong and liveness detection continue when application command handling
is waiting for session startup or cleanup. Application messages use bounded
queues; queue exhaustion closes the connection instead of blocking the network
pump. Socket writes have a five-second deadline. Send acknowledgements mean the
message was flushed to the socket. Dropping the connection cancels its task.

Validation: `cargo test --locked -p meshrmm-signaling-client -p meshrmm-agent`
on the Windows test endpoint, plus signaling-client tests on macOS. The loopback
WebSocket tests hold application consumption, exchange ten pings, check dead-peer
detection without application polling, and verify cancellation closes the socket.

## Capture control

Capture start, display/profile changes, desktop recovery and stop run on a
cancellable native thread with a bounded command queue. They never execute on a
Tokio runtime worker or inside the session event loop. Cancellation does not join
a native thread in `Drop`; normal shutdown awaits a completion notification.
Existing encoder negotiation and frame ordering remain serialized within capture.

Validation: Windows Agent tests, including a deliberately blocked native worker
on a single-thread Tokio runtime. The test verifies timers still run, dropping
the worker does not join, and cleanup completes after releasing the operation.

## Input and maintenance

WebRTC input callbacks only enqueue keyboard/mouse commands. A dedicated input
worker preserves their order and releases pressed keys when cancelled. Maintenance
commands have another native worker. Cursor/state updates have a separate bounded
outbound queue; native input never waits for a network write. Control writes have
a five-second deadline. Input queue overflow ends the session to release keys
rather than silently discarding a key-up event.

Validation: Windows Agent tests and macOS native-worker tests. A blocked service
with a full queue does not prevent a second worker from processing ordered
commands; overflow is explicit and cancellation discards pending commands before
cleanup.

## Clipboard and helper pipes

Clipboard assembly, polling and outbound pacing have their own native worker.
Service mode uses a separate clipboard helper process and separate pipes from
keyboard/mouse input. Each parent-to-helper pipe has a dedicated writer thread,
a bounded command queue and a byte budget. A full or stalled pipe cannot block
its caller. Clipboard output is paced with a 64 KiB transport backlog limit.

Validation: 39 enabled Windows Agent tests pass. A real Windows anonymous pipe
is filled without draining it; an independent input pipe still receives a release
command within the test deadline. Tests also cover pipe byte-budget rejection,
clipboard helper startup framing, and rich clipboard format/chunk round trips.

## Chat

Chat and the session banner now live in a separate desktop helper process with
their own command pipe. Chat transport polling/sending has an independent native
worker. The input helper performs no chat/UI or clipboard work. Full helper
shutdown runs on the capture worker, so dropping session resources does not wait
for native helper processes on an async runtime thread.

Validation: all 39 enabled Windows Agent tests pass, including the new chat-helper
startup framing, Unicode chat commands, banner message behavior and independent
worker/pipe tests. Interactive installed-agent validation follows the remaining
file/network isolation steps.

## File transfer

File dispatch, polling and network writes now have a separate worker and bounded
inbound queue. File helper stalls never run inside the signaling/session loop.

Validation: Windows Agent and file-transfer suites. A regression test blocks the
actual file-worker dispatch path and confirms the input, chat and clipboard
workers still process messages on a single-thread async runtime, then verifies
input cleanup. File tests cover checksums, nested/Unicode/empty files, cancellation
cleanup and preserving existing destination files.

## Independent network streams

New agents/viewers negotiate separate reliable channels for chat, clipboard and
files. Input/control and unordered video retain their existing channels. Each
service chooses its outbound route once, after capability negotiation; older
peers use the legacy control channel after a two-second negotiation window.
Routes never switch mid-transfer. Full network isolation therefore requires both
updated peers; legacy peers still benefit from independent native workers.
The streams necessarily share the connection's available bandwidth.

The viewer also has independent service workers and send queues, so local
clipboard work cannot hold up keyboard/mouse sends or incoming chat.

Validation: 40 Windows Agent tests, 19 Windows viewer tests, 27 macOS viewer tests,
and three shared transport tests pass. The transport test establishes a real
local WebRTC connection, blocks the file-stream receive callback, and confirms
that input still arrives on the control stream. Route tests cover negotiation
and a late capability announcement after choosing the legacy route.
