# Event-driven service transport

## Chat

Chat UI producers and the Windows helper pipe reader notify the transport when
outgoing messages arrive. Consumers drain the existing bounded queues on wakeup;
there is no chat transport polling timer. The Windows chat helper awaits either
native commands or outgoing UI notifications. Native pipe reads remain on their
own thread and command queues remain bounded.

Validation: macOS chat 7, agent 10 and viewer 27 tests passed; Windows endpoint
192.168.1.152 chat 7, agent 44 and viewer 19 tests passed. Three existing invasive
Windows desktop tests remain ignored. Tests cover queued-before-wait messages,
notification coalescing, queue limits, helper command ordering/EOF and independent
workers when the file helper stalls. macOS Clippy passed with the existing
`too_many_arguments` allowance.

## Channel readiness and capacity

Each physical reliable channel has one shared observer for open, close and
buffer-low events. Legacy services share that observer, so installing one service
cannot replace another service's callback. Consumers register before checking
state/capacity to avoid missed wakeups. A five-second full-buffer deadline bounds
a stalled send. Channel readiness and viewer bulk backpressure no longer poll;
Agent clipboard chunks drain when capacity is available. Viewer native service
workers have explicit shutdown notifications, including during connection startup.

Validation: macOS agent/viewer suites and Windows agent/viewer suites passed.
The shared transport suite exercises real local WebRTC connections: waiting for
open, a queued file burst with an application receiver paused, continued input,
multiple capacity waiters, close handling and sticky legacy routes.

## Files

The native file producer notifies consumers after placing a message in its
bounded output queue. The Windows file helper and parent transport await these
notifications. Agent sends wait for network capacity; viewer consumers reserve
space in the bounded sender queue before removing output. The protocol's
per-message acknowledgements and existing queue limits remain in place. Native
clipboard discovery/progress handling retains its own maintenance timer.

Validation: macOS file-transfer 6, agent 10, viewer 27, chat 7 and shared transport
3 tests passed. Windows file-transfer 7, agent 44, viewer 19, chat 7 and shared
transport 3 passed (the same three invasive tests ignored). A new regression
verifies a full output queue blocks its producer, freeing capacity preserves
message order and the next message wakes its consumer. macOS Clippy passed.

## Clipboard forwarding

The Windows helper pipe reader wakes the clipboard transport immediately and
keeps only the latest clipboard value, preserving coalescing. Direct native
clipboard detection still runs every 250 ms. The viewer checks its native
clipboard at that interval and drains serialized chunks by reserving sender
capacity, without a separate 5 ms timer. Incoming clipboard content still clears
superseded outgoing chunks and uses the existing echo suppression.

Validation: macOS agent 10, viewer 27, chat 7, files 6 and shared transport 3 tests
passed. Windows agent 45, viewer 19, chat 7, clipboard 1, files 7 and transport 3
passed, with three existing invasive tests ignored. A real Windows pipe test
verifies helper notifications for all three services, clipboard latest-value
coalescing and consumption. macOS Clippy passed.

## Helper executable startup

The installed smoke test exposed a nested-runtime panic in the chat helper.
The Agent executable now dispatches native helpers before entering its main
Tokio runtime. A Windows integration test launches the real Agent executable,
starts the chat helper, sends chat commands, and requires a clean Stopped event
and successful process exit. It reproduced the panic before the fix and passed
afterward. All targeted Windows suites and the macOS Agent suite passed again.

## Installed validation (2026-09-15)

Installed the final local macOS viewer and Windows Agent on `192.168.1.152`.
The installer preserved enrollment configuration, retained an executable backup,
and verified the service reconnected. No release was published. Final Agent
SHA-256: `cc79b4aea726568430c5ab63c0af2230d0c9488a3cf08cbe1732dcfd4912ce0f`.

Verified through the installed UI:

- Bidirectional chat, including the endpoint banner and viewer unread state.
- Clipboard copying from macOS TextEdit into the Windows chat input and back
  into TextEdit with a distinct endpoint-created marker.
- Remote keyboard input and both monitor capture configurations.
- Chat and keyboard input remained responsive during a 60-second file-helper
  suspension. The helper resumed successfully through the harness's `finally`.
- A selected file transfer waited during a second 30-second file-helper
  suspension, then reached “Transfer complete” after resume. Its 417,792 bytes
  matched SHA-256 `24acbe16c375e8c74731123cbf6e762148dd298b14fb67b0f8410f92bdaf321b`
  on both machines. An earlier picker attempt selected a directory containing
  non-regular files and was correctly rejected; the retry selected the exact
  generated artifact.
- Closing the viewer removed all desktop helpers. Only the Windows service and
  its worker remained, and the dashboard continued to report the Agent online.

The final full Windows test run passed: Agent 45 plus the executable integration
regression 1, viewer 19, chat 7, clipboard 1, files 7 and shared transport 3.
Three pre-existing blackout/input-block tests remain ignored because they require
an invasive interactive harness. Clippy passed on macOS and Windows; the Windows
final pass included all targets. No CPU or battery savings were benchmarked.
