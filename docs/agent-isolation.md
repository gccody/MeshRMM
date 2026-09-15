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
