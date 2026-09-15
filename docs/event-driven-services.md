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
