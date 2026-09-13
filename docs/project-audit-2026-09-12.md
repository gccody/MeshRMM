**MeshRMM project audit — 2026-09-12**

Reviewed commit `abff4d5`. This is a source and local-check audit, not proof that every bug has been found. No application fixes or production changes were made. Findings below distinguish deterministic code defects from failure paths that need deployment or hardware reproduction.

The review followed company authentication/provisioning, enrollment, inventory events, remote handoff/signaling, agent lifecycle, native transport/input/video, updates, and release automation. Windows GPU, service, secure desktop, and two-machine ICE/TURN behavior could only be inspected statically on this Mac.

**Checks and evidence**

- `cargo test --workspace`: passed, 52 tests on macOS. Windows-only agent/capture modules do not execute in this result.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets -- -D warnings`: passed on macOS.
- Server Wasm compilation: passed using the installed rustup compiler explicitly. The initial Homebrew compiler could not find the target.
- Windows cross-check: blocked in native dependencies by missing Windows C headers/toolchain, not a confirmed project compilation defect.
- `npm ci --ignore-scripts` followed by `npm run verify`: passed typechecking, lint, production build, and 9 tests.
- `node --test scripts/release-config.test.mjs`: 2 tests passed.
- All six D1 migrations applied successfully to an in-memory SQLite database. Running the actual tenant handoff redemption query against a suspended company returned its handoff row, confirming finding 2.
- A standalone Rust channel-lifetime reproduction confirmed finding 4: a second send succeeds while the first session runs even though no further receive is performed.
- `npm audit --json`: reported 9 vulnerable dependency entries (7 high, 2 moderate), including transitive effects. These are dependency findings, not nine demonstrated application exploits. The reported packages are `@cloudflare/vite-plugin`, `wrangler`, `miniflare`, `sharp`, `browserslist`, `fast-uri`, `js-yaml`, `fflate`, and `baseline-browser-mapping`. Assess their build/development exposure and update the lockfile in a dedicated change; no automatic fixes were applied. A Rust advisory scan was not performed.

**Findings, in priority order**

1. **High — Aborting an agent session bypasses transport cleanup and detaches spawned work.** [agent/src/remote/mod.rs](/Users/gccody/Code/MeshRMM/agent/src/remote/mod.rs:138), [transport cleanup](/Users/gccody/Code/MeshRMM/agent/src/remote/transport.rs:831), [control startup task](/Users/gccody/Code/MeshRMM/agent/src/remote/transport.rs:973).

   Replacement and EndSession call `active.task.abort()`. Cleanup that aborts `video_sender`, releases input, and closes the peer appears after the awaited transport loop, so cancellation skips it. Dropping the video JoinHandle detaches its task; the control-start task's handle is discarded altogether and it can wait forever if its channel never opens. This can accumulate tasks and retained data-channel resources during reconnects. Capture destructors provide some cleanup, but do not cancel these independent Tokio tasks. Use cooperative cancellation, join all child tasks, and ensure teardown also covers initialization errors. Validate with repeated failed negotiations, session replacement, and expiration while input is held.

2. **High — Company suspension does not revoke established access and can be bypassed during handoff redemption.** [suspension](/Users/gccody/Code/MeshRMM/server/src/routes/platform.rs:388), [handoff redemption](/Users/gccody/Code/MeshRMM/server/src/routes/handoffs.rs:65), [session resume](/Users/gccody/Code/MeshRMM/server/src/remote_session.rs:221), [agent authorization](/Users/gccody/Code/MeshRMM/server/src/lib.rs:326).

   Suspension changes a D1 status but does not close coordinator, remote-session, or inventory sockets. RemoteSession has no company/user authorization state; Activity and resume continue extending an existing session. Redemption checks company ID but not company status. An unexpired handoff issued before suspension can therefore still create a session against an already-connected agent. Legacy API agent authorization also skips the company's status check when no tenant is resolved. Centralize company status validation across token-based routes and propagate revocation to live sessions. The handoff SQL bypass was reproduced locally; live revocation needs integration coverage.

3. **High — Two viewers can repeatedly replace each other's session.** [coordinator request](/Users/gccody/Code/MeshRMM/server/src/agent_coordinator.rs:90), [resume request](/Users/gccody/Code/MeshRMM/server/src/agent_coordinator.rs:125), [agent replacement](/Users/gccody/Code/MeshRMM/agent/src/remote/mod.rs:131).

   Both `/request` and `/resume-request` overwrite `active_session` without checking the current session ID. A new viewer replaces the old one, but the old RemoteSession remains valid. Its reconnect loop can resume and replace the new viewer again. This creates competing reconnects and involuntary takeovers within a company. Add an atomic per-agent session lease; reject a busy agent or require an explicit takeover that expires the old session. Resume must match the active lease.

4. **High — macOS ignores additional dashboard launches while a session is running.** [URL handling](/Users/gccody/Code/MeshRMM/remote/src/platform/macos/app.rs:20), [network thread](/Users/gccody/Code/MeshRMM/remote/src/platform/macos/app.rs:970).

   The receiver reads one link, then remains alive throughout `network(deep_link)`. The URL callback starts a replacement process only when `send` fails. A second URL instead sends successfully into the still-live channel and is never read. This affects switching endpoints or relaunching a stuck session through the dashboard. Explicitly drop the receiver before entering the network closure, or implement an ongoing launch-command consumer. Rust channel lifetime behavior was reproduced independently.

5. **Medium — Expired sessions become generic errors and can retry indefinitely.** [resume lookup](/Users/gccody/Code/MeshRMM/server/src/remote_session.rs:227), [signaling lookup](/Users/gccody/Code/MeshRMM/server/src/remote_session.rs:307), [terminal classification](/Users/gccody/Code/MeshRMM/remote/src/signaling.rs:62).

   Expiration deletes the session record. Subsequent requests produce `Error::RustError("unknown session")` instead of an explicit 401/404/410 response. The clients stop retrying only selected HTTP statuses, while internal Worker errors are treated as transient. After an idle deadline or cancellation, reconnect can continue permanently. Return an explicit terminal status for missing records and preserve terminal close reasons rather than reducing all Close frames to generic errors.

6. **Medium — Inventory subscriptions have no server-enforced lifetime or user revocation.** [subscription redemption](/Users/gccody/Code/MeshRMM/server/src/routes/events.rs:80), [presence subscription](/Users/gccody/Code/MeshRMM/server/src/company_presence.rs:147).

   The 60-second token lifetime applies only to opening the socket. The resulting subscription retains neither user ID nor an expiry, and accepts refresh requests indefinitely. Removing company membership or revoking a WorkOS session does not revoke an existing inventory connection. Browser cleanup on logout is insufficient for a retained or custom client. Attach a bounded authorization lease and identity to each socket and close it on expiry or revocation. Also bind redemption to the requested tenant instead of checking only token possession and a same-origin header.

7. **Medium — Automatic viewer updates can consume the entire handoff lifetime before redemption.** [Windows launch order](/Users/gccody/Code/MeshRMM/remote/src/main.rs:190), [macOS update request](/Users/gccody/Code/MeshRMM/remote/src/updater/macos.rs:25), [handoff lifetime](/Users/gccody/Code/MeshRMM/server/src/lib.rs:25).

   The viewer checks and downloads updates before redeeming the 60-second handoff. macOS permits 60 seconds per HTTP request, plus extraction, replacement, and relaunch; Windows permits two 30-second requests plus replacement. Even a stalled manifest request can exhaust the macOS handoff. The new app relaunches with the same expired token. Introduce a resumable launch ticket before updating, or update outside this short-lived launch flow. Test slow downloads and unavailable release hosting.

8. **Medium — macOS updater can delete the backup without establishing that the new app launched.** [replacement](/Users/gccody/Code/MeshRMM/remote/src/updater/macos.rs:142), [launch](/Users/gccody/Code/MeshRMM/remote/src/updater/macos.rs:215).

   `launch` only spawns `/usr/bin/open`; it does not wait for its exit status or receive app readiness. `open` can fail after spawning, yet the helper reports success and deletes the backup. In addition, moving an extracted bundle from the temporary directory with `rename` fails across filesystems; that branch restores the old bundle but exits without relaunching it. Stage beside the target, check launcher status, retain the backup until app readiness, and relaunch the old app on every replacement failure. Windows also treats successful process spawning as health, so an immediate post-start crash does not trigger rollback there.

9. **Medium — Enrollment is not recoverable after partial success.** [token consumption](/Users/gccody/Code/MeshRMM/server/src/routes/agents.rs:91), [agent creation](/Users/gccody/Code/MeshRMM/server/src/routes/agents.rs:117), [installation](/Users/gccody/Code/MeshRMM/agent/src/installer.rs:208).

   Token consumption, agent insertion, auditing, and response construction are separate operations. If a later step fails, the token has already been consumed. If the response is lost after insertion, the server holds only the credential hash and the endpoint cannot recover its configuration. Installation likewise redeems before binary/config/service changes finish. This produces unusable installers and orphaned offline agent records. Use a transactional enrollment state machine plus narrowly scoped, authenticated idempotent response recovery and installation acknowledgement; a database transaction alone does not solve lost responses.

10. **Medium — Reinstalling replaces the local identity but leaves the previous agent record.** [installer](/Users/gccody/Code/MeshRMM/agent/src/installer.rs:184), [redemption creates UUID](/Users/gccody/Code/MeshRMM/server/src/routes/agents.rs:112).

   An existing service is detected, but setup still creates a new agent identity and overwrites the prior configuration. The old agent is not retired, so a repair install adds a duplicate, permanently offline device. A later replacement failure can also leave the existing service stopped without restoring its files. Provide distinct repair and re-enrollment paths, authenticate identity migration, and retain a recoverable local backup until startup succeeds.

11. **Medium — Token rotation invalidates the installed agent without delivering its replacement credential.** [rotation](/Users/gccody/Code/MeshRMM/server/src/routes/agents.rs:202).

   The route changes the stored hash and returns the new secret to its caller. There is no agent command or protected configuration update for rotation, and the existing coordinator socket is not closed. Consequently, the current connection survives rotation while the legitimate installed agent fails authentication on reconnect. Implement an acknowledged staged rotation protocol, or clearly expose this as a manual recovery action with the necessary installation steps and immediate old-connection revocation.

12. **Medium — A failed presence publication can leave an agent online indefinitely.** [disconnect publication](/Users/gccody/Code/MeshRMM/server/src/agent_coordinator.rs:269), [snapshot](/Users/gccody/Code/MeshRMM/server/src/company_presence.rs:164).

   A disconnect publication failure is logged and discarded. CompanyPresence persists the connected-ID set and snapshots do not reconcile it against coordinator state. Therefore the Refresh button cannot repair an online bit after a missed disconnect update. Add retry/outbox delivery and a bounded heartbeat lease or reconciliation mechanism. Validate by failing the inter-object publication during disconnect and then requesting a snapshot.

13. **Medium — The dashboard applies a default idle policy before loading the company's actual policy.** [policy initialization](/Users/gccody/Code/MeshRMM/dashboard/app/page.tsx:118), [idle hook](/Users/gccody/Code/MeshRMM/dashboard/app/page.tsx:166).

   Idle enforcement becomes enabled as soon as the WorkOS tenant session exists, while `/v1/account` has not necessarily returned. It initially uses four hours. A user in an eight-hour company returning after five hours can be signed out before the eight-hour policy loads. Conversely, shorter policies are temporarily not applied during loading. Gate enforcement on the authoritative company policy or load a trustworthy cached policy before enabling the dashboard.

14. **Medium — Connection establishment lacks bounded application deadlines.** [initial redemption HTTP](/Users/gccody/Code/MeshRMM/remote/src/signaling.rs:15), [WebSocket handshake](/Users/gccody/Code/MeshRMM/crates/signaling-client/src/lib.rs:98).

   Initial redemption uses a default reqwest client without an overall request timeout. WebSocket connect/handshake awaits are also unbounded by the application. A server or middlebox that accepts a connection and stops responding can leave startup or agent reconnect hung before the heartbeat and retry logic starts. Add explicit connect/handshake/response deadlines and cancellation covering the full setup path, and test a server that accepts TCP but never completes the response.

15. **Medium, hardware-dependent — Recovery keyframes are not guaranteed to contain decoder configuration.** [encoder output](/Users/gccody/Code/MeshRMM/agent/windows/remote-screen/src/encoder.rs:335), [macOS decoder bootstrap](/Users/gccody/Code/MeshRMM/remote/src/platform/macos/presenter.rs:501), [retained keyframe](/Users/gccody/Code/MeshRMM/agent/src/remote/video.rs:43).

   The encoder attaches its sequence header only once. Later IDRs replace the cached bootstrap IDR. If a hardware encoder omits repeated in-band parameter sets, a later cached keyframe, a lost first frame, or a decoder flush leads to a keyframe missing SPS/PPS (and VPS for HEVC). macOS explicitly errors on this. Ensure each independently usable keyframe includes the required parameter sets, or reliably distribute and retain codec configuration separately. The missing guarantee is visible in source; driver-specific reproduction remains necessary.

**Further risks and coverage gaps**

The passing server tests cover pure helpers and serialization, not authenticated route behavior, D1 transactions, alarms, or Durable Object interleaving. Dashboard tests cover server rendering and pure models, not actual login, idle-policy loading, live reconnect, enrollment download, or native launch. These gaps explain why the lifecycle issues above survive current checks.

Release publication is a separate workflow and does not wait for the CI test jobs. Its previous-version guard fetches only two commits and treats inability to read the previous commit as a first release; a multi-commit push can skip the version comparison. Require validation and test success for the exact release commit, and fail closed when the previous version cannot be loaded.

Update trust currently relies on HTTPS and a digest in a manifest obtained from the release host. There is no independent manifest signature verified by a pinned release key. The macOS helper does not verify a publisher identity before replacement, and production automation allows ad-hoc or non-notarized releases when signing secrets are absent. These are distribution hardening gaps, not evidence of a compromised release.

Deep links accept any HTTPS signaling host and the viewer automatically synchronizes its initial clipboard. Consider a deployment/tenant trust policy and visible remote identity before enabling control and clipboard for a host introduced by a link. No malicious-server test was performed.

Input/control queues and pending signaling candidates are unbounded in several paths. Viewer update downloads check the byte limit only after buffering the full response when Content-Length is absent. Add streaming byte limits, candidate/message quotas, and bounded queues with explicit overflow behavior. Review log rotation as well: native logs append without retention limits.

The product's "remote idle timeout" is refreshed on a timer even when nobody interacts with the viewer. That matches the README's connected-viewer behavior, but it is a liveness timeout rather than a human inactivity policy. Decide whether unattended open sessions should remain authorized indefinitely and reflect the decision in both UI and server policy.

**What to implement next**

1. **First: a reliable, revocable remote-session lifecycle.** Add per-agent ownership leases, explicit busy/takeover behavior, cooperative task shutdown, terminal error codes, and company/user revocation across all live connections. Include fault-injection tests for two viewers, suspended companies, expired sessions, interrupted handoffs, and repeated reconnects. Acceptance: one current owner, no old-session resurrection, and no growing task/resource counts.

2. **Then: recoverable enrollment and updates.** Implement idempotent enrollment, repair/re-enrollment separation, acknowledged credential rotation, signed release metadata, and readiness-based rollback. Move updates out of the 60-second launch window. Acceptance: interruption at every installation/update step preserves either a working previous version or a recoverable pending operation.

3. **Then: operational visibility.** Persist session start/end/result, actor, device, and correlation IDs; expose company audit history and connection failure reasons. Add agent version, OS/build, last-seen timestamp, service/update health, and a support diagnostics export. Distinguish "online", "busy", "updating", "pending deletion", and "unreachable" so the dashboard describes what operators can actually do.

4. **Then: authorization and regression coverage.** Separate inventory-read, remote-control, clipboard, agent-management, and company-settings permissions. Add staging integration tests using real Worker/D1/DO behavior, browser interaction tests, and a Windows/macOS hardware matrix covering static screens, packet loss, lock/unlock, user switch, multi-monitor/DPI changes, driver failures, and long TURN sessions. Gate release publication on those checks appropriate to each change.

5. **After stability: expand actual RMM capabilities.** Start with endpoint hardware/software inventory, service/resource health, alerts, and device grouping/search. Then consider a tightly permissioned, audited job system for scripts and remediation. File transfer, browser viewing, and multi-viewer support should follow the lifecycle and authorization foundation because they add new transport and access-control requirements.

The next milestone should be session reliability and revocation, followed by recoverable installation/update flows. These address current failures before adding more remote-control features.
