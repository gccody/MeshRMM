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
