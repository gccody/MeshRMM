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

Clipboard assembly, helper notifications and capacity-driven sends have their own native worker.
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
their own command pipe. Chat transport notifications/sending have an independent native
worker. The input helper performs no chat/UI or clipboard work. Full helper
shutdown runs on the capture worker, so dropping session resources does not wait
for native helper processes on an async runtime thread.

Validation: all 39 enabled Windows Agent tests pass, including the new chat-helper
startup framing, Unicode chat commands, banner message behavior and independent
worker/pipe tests. Interactive installed-agent validation follows the remaining
file/network isolation steps.

## File transfer

File dispatch, output notifications and network writes have a separate worker and
bounded inbound queue. File helper stalls never run inside the signaling/session loop.

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

## Diagnostics and cancellation

Agent file logging uses a bounded writer queue; a stalled disk drops log records
instead of stalling service threads. Helper command-reader queues are bounded as
well as parent pipe-writer queues. Failed capture startup awaits native cleanup
before a reconnect can reuse the streamer.

Validation: Windows Agent suite and a logger test that stalls its writer, fills
the queue and confirms producers return with explicit dropped-record accounting.

## Clipboard issue found during live validation

The Windows clipboard library reports an error when HTML is absent, including
on ordinary plain-text clipboards. ClipboardSync now probes HTML availability
before reading that format. The normal-desktop clipboard helper uses the
signed-in user's token; secure-desktop input retains its LocalSystem token.
Helper startup logs identify each service role for fault-injection tests.

Validation: 42 enabled Windows Agent tests plus a native clipboard test covering
plain text, HTML, images and suppression of clipboard echo all pass.

## Installed endpoint validation (2026-09-15)

Tested on Windows endpoint `192.168.1.152` with the local macOS viewer. Each
isolation change was tested and committed before starting the next change.
Both final release builds were installed locally; no release was published.
The installer verified the agent binary hash, preserved enrollment configuration,
and confirmed signaling reconnected. Final agent SHA-256:
`06dc5f2df262e73d803139e10cbeee1084ba6d6fed733cc81b92c6c30442973c`.

Physical fault injection uses `scripts/test-helper-isolation.ps1`, which validates
the selected helper process and resumes it in a `finally` block.

| Paused helper | Duration | Observed independent behavior |
| --- | --- | --- |
| Clipboard | 90 seconds | Live keyboard/video, bidirectional chat, dashboard online |
| Files | 90 seconds | Keyboard/video continued with an upload pending; upload completed after resume |
| Chat | 60 seconds | Keyboard/video and Windows-to-Mac clipboard continued |
| Capture | 90 seconds | Input and clipboard continued, dashboard reported online, video recovered automatically after resume |
| Input | 30 seconds | Chat arrived on the endpoint and video displayed it during the pause |

The transferred test file was 1,081,344 bytes; source and destination SHA-256:
`82dc98f339452dd78912dfe6b873bc9c7034b539d707889abe564b8ed27f6e9b`.
The corrected clipboard helper was also tested in both directions interactively.
Switching between both physical monitors and returning to the original display
worked with the final installed builds. The browser event stream briefly
reconnected after being backgrounded; it returned to live/agent-online status
while the capture helper was still suspended.

Final automated suites: Windows agent 42 passed, protocol 25, clipboard 1,
file transfer 6, viewer 19, session transport 3, signaling 5. Three existing
Windows blackout/input-block tests remain explicitly ignored because they require
their own interactive harness. macOS agent 10, viewer 27, protocol 25, transport 3
and signaling 5 passed. Clippy passed for Windows agent/shared transport and
macOS viewer/shared transport with the existing `too_many_arguments` lint allowed.

All paused helpers resumed successfully. Closing the final viewer session removed
every desktop helper; only the Windows service and its worker remained, and the
dashboard showed the agent online. The temporary Notepad tab was discarded.
The generated transfer file remains available as a checksum validation artifact.

See [event-driven service transport](event-driven-services.md) for the subsequent
polling removal, process-level regression test and installed validation.
