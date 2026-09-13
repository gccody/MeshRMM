**Audit fixes and rollout**

This change addresses the actionable findings in the September 12 source audit. The original audit describes the pre-change commit; this document describes the implementation and remaining validation limits.

| Audit finding | Implemented change |
| --- | --- |
| 1: cancellation leaks | Agent transport owns abort handles for both child tasks. A cleanup guard covers cancellation and initialization errors, releases input, stops capture, and schedules peer closure. Normal teardown joins the tasks. |
| 2: suspension | Device/company state is checked on legacy agent authentication, handoff/enrollment redemption, session admission, resume, and activity. Suspension revokes coordinator, remote-session, and inventory connections. |
| 3: competing viewers | Coordinator rejects a second live session; resume and activity renewal must match the active unexpired lease. Expired requests are not replayed on reconnect. |
| 4: macOS launches | The launch receiver is dropped before running the session. Subsequent links trigger replacement; replacement processes discard inherited update-session variables. |
| 5: endless expired retries | Missing sessions return 410. Revocation/expiry WebSocket close codes are classified as terminal. Resume and renewal recheck stored state after external awaits. |
| 6: inventory access | Inventory sockets expire after five minutes and reconnect through authenticated token issuance. Redemption checks company status and hostname. Company suspension closes sockets, including an alarm fallback. This is bounded reauthorization, not an instantaneous WorkOS user/session-revocation webhook. |
| 7: update/handoff expiry | The viewer redeems the handoff before updating. Helpers pass the authorized session through their process environment, without writing the bootstrap to disk. Initial session startup has a 15-minute window. |
| 8: viewer rollback | macOS extracts on the target filesystem. Relaunch waits for an initialization acknowledgement instead of assuming spawn/open succeeded. Failed update paths relaunch the restored viewer; download limits apply while streaming the response. |
| 9: partial enrollment | Claim and agent creation run in one D1 batch. A private endpoint recovery key allows retries of the same claim. The installer persists pending configuration before modifying the service. Old installers retain one-shot enrollment. |
| 10: reinstall duplicates | Repair preserves existing identity. Windows file replacement is atomic and flushed; failed installation attempts restore prior binary/configuration and restart previously running services. |
| 11: rotation | The server stages a hash and sends the credential over the authenticated coordinator. The agent atomically persists it and reconnects; successful authentication promotes it. The previous credential stays valid until that acknowledgement. |
| 12: stale presence | Coordinator alarms retry presence publication. Authoritative inventory snapshots query coordinator status, so Refresh repairs missed disconnect events. |
| 13: wrong initial idle policy | Idle enforcement waits for the company's policy response. |
| 14: setup hangs | Initial HTTP redemption and WebSocket setup have application deadlines. Pending ICE candidates are bounded. |
| 15: missing recovery headers | Encoder sequence headers accompany every keyframe, rather than only the first output. |

Dependency remediation updates the Cloudflare build tooling and affected transitive packages. npm reported zero vulnerabilities after installation. CI now runs SQL failure-path regressions; native release deployment waits for reusable CI and fails closed when the previous release version cannot be read.

**Rollout order**

1. Apply `server/migrations/0007_recoverable_enrollment.sql` to D1. The server requires its enrollment and pending-credential columns.
2. Deploy the server and publish the updated agent/viewer assets through the normal release procedure. Existing installed binaries need the native update to gain cancellation cleanup, recovery, and rotation support.
3. Verify enrollment/retry, identity-preserving repair, rotation/reconnect, company suspension, two-viewer rejection, and viewer update/rollback in staging before production rollout.

No production deployment, version bump, secret provisioning, or commit was performed in this task.

**Validation and limits**

Local validation covers Rust tests and Clippy on macOS, Windows cross-compilation/Clippy, server Wasm compilation, dashboard typecheck/lint/build/tests, and SQLite migration/transaction regressions. Cross-compilation does not execute Windows services or GPU APIs. Real Windows install/repair/update failures, secure desktop input, encoder behavior across drivers, two-machine ICE/TURN, and Cloudflare Durable Object event interleaving still require staging/hardware smoke tests.

The updater acknowledgement confirms configuration/logging initialization, not a successful remote GPU session or long-term application health. New-server/new-native compatibility must be exercised through the release workflow. Credential rotation returns 202 with `rotation_pending`, not a secret for manually replacing configuration. An old agent that cannot understand the command keeps its current working credential.

The broader audit recommendations remain separate product/security work: independently signed release manifests and provisioned signing identities, immediate WorkOS webhook-based user revocation, granular remote-control permissions, pervasive bounded control queues and log retention, richer audit/agent-health UI, and new monitoring/remediation features. No signing credentials or trust keys were invented or deployed.
