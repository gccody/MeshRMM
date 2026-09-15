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
