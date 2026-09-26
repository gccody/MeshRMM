# UX improvement plan — 2026-09-26

Tracks the user-experience fixes chosen from the 2026-09-26 UX review. Work happens on branch
`ux-improvements-2026-09-26`, cut from `main` at `c4824f1`. Task IDs match the review's numbering
(`0.x` = P0, `2.x` = P2, `3.x` = P3). Line numbers were taken at `c4824f1` and will drift; search
for the quoted code rather than trusting the number.

The tasks are meant to be implemented by separate agent threads. Each task section is written to
stand alone; read [Working rules](#working-rules), [Merge order](#merge-order), and
[File overlap](#file-overlap) first, then only your own task's section.

## Status

| Task | Area | Wave | Depends on | Status | Commit(s) |
| --- | --- | --- | --- | --- | --- |
| 0.1 Viewer-missing detection + "Get the viewer" | Dashboard | 2 | 2.9 (soft) | Not started | |
| 0.2 Startup failures: typed errors, retry cap, Cancel | Viewer, agent, protocol | 1 | — | Merged | `c424a4d`, `b63f8cd`, `41a1baa`, `32bc917` |
| 2.9 Persistent workspace shell + filters in URL | Dashboard | 1 | — | Merged | `200591b`, `817785b` |
| 2.10a Fewer D1 round trips + cron cleanup | Server | 1 | — | Merged | `9d5b338`, `3dde5f0`, `e962fef` |
| 2.10b Start event subscription in parallel with account | Dashboard | 2 | 2.9, 2.10a (soft) | Not started | |
| 2.11 Inventory keeps data, stale state, per-source errors | Dashboard | 2 | 2.9 | Not started | |
| 2.12 Scope WorkOS widgets and Radix CSS to admin pages | Dashboard | 2 | 2.9 (soft) | Not started | |
| 3.14 Backoff reset, reconnect reason, Retry now | Viewer, agent | 2 | 0.2 | Not started | |
| 3.16a Audio: send only when unmuted, stereo, Opus | Agent, viewer, protocol | 3 | 3.16b, 3.17 | Not started | |
| 3.16b HEVC bitrate adaptation via restart ladder | Agent | 1 | — | Not started | |
| 3.17 Windows viewer resets the stream in place | Windows viewer | 1 | — | Merged | `5ce8cc7` |

"Soft" dependencies can start early if the task keeps its edits to the listed wiring points; see
the task section.

## Working rules

- **One thread per task, one worktree per thread.** Create a worktree and sub-branch from the
  current tip of `ux-improvements-2026-09-26`, named `ux/<task-id>` (for example `ux/2.9`). Do not
  work directly in another thread's worktree. When the task is done and verified, merge (no
  squash, no history rewrite) back into `ux-improvements-2026-09-26` in the [merge order](#merge-order),
  rebasing your sub-branch onto the integration branch first if it moved.
- Don't deploy, publish releases, bump versions, run remote D1 migrations, or push unless the user
  asks.
- Before changing code, read the code paths involved. If a finding here turns out to be wrong,
  record why in this file (in your task's **Notes**) instead of changing code.
- Match the surrounding style and comment density. Add tests for the behavior you change.
- Commit with conventional-commit subjects, then update the status table and your task's
  **Notes** in this file in the same merge.
- **Checks on the Mac:**
  - Rust: `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`,
    `cargo test --locked --workspace`.
  - Server: also `cargo check --locked -p meshrmm-server --target wasm32-unknown-unknown`,
    `python3 server/tests/sql_regressions.py`, and every `node server/tests/*.mjs` that CI runs
    (`.github/workflows/ci.yml`). Use rustup's `cargo`; Homebrew's has no wasm32 target.
  - Dashboard: `cd dashboard && npm run verify`.
- **Windows.** Windows-only code does not compile under macOS clippy. Follow
  [AGENTS.md](../AGENTS.md):
  1. Sync the tree to a per-task directory on DESKTOP-85R6S28 (`~\ux-2026-09-26-<task-id>`) and
     verify it with a SHA-256 manifest. Never reuse another task's directory.
  2. Run native fmt, clippy, and tests there, checking `$LASTEXITCODE` after each command.
  3. When installed-agent behavior changes (0.2, 3.14, 3.16a, 3.16b), back up the exe, install,
     confirm the service starts and connects, exercise the change, and check the logs. Leave a
     working service.
- **The installed agent service on the endpoint is shared.** Only one thread may have a
  non-`main` build installed at a time. Record who installed what in
  [Endpoint state](#endpoint-state) before and after installing, and restore the previous
  working build when you're done unless the next thread takes over.
- The endpoint has **no hardware H.264 decoder** (see the 2026-09-23 plan, T12). A Windows viewer
  on it cannot show live video; use synthetic-frame probes or a second Windows machine for live
  viewer checks, and state which you used.
- Validate macOS viewer behavior on macOS. Report the host used for server and dashboard checks.

## Merge order

Wave 1 has no dependencies and can run in parallel. Waves 2 and 3 start after the tasks they depend
on have merged into the integration branch.

1. **Wave 1:** 2.9, 2.10a, 0.2, 3.16b, 3.17.
   - Merge 2.9 before any other dashboard task.
   - Merge 3.17 before 3.16b, or at the same time: each HEVC step restart sends a new
     `DisplayConfiguration`, which without 3.17 recreates the whole Windows viewer window.
2. **Wave 2:** 0.1, 2.10b, 2.11, 2.12 (after 2.9); 3.14 (after 0.2).
   - Dashboard tasks in this wave edit different new files; merge in any order and rebase.
   - 2.10b and 2.11 both touch `use-agent-inventory.ts`. Whichever merges second rebases; 2.10b's
     diff is only the option rename (see its section).
3. **Wave 3:** 3.16a, after 3.16b (it moves the video pacer to `agent/src/remote/bitrate.rs`) and
   after 3.17 (adjacent hunks in `remote/src/transport/control.rs`).

## File overlap

| File | Tasks | Notes |
| --- | --- | --- |
| `dashboard/features/workspace/dashboard.tsx` | 2.9 (deletes it), 0.1, 2.10b, 2.11, 2.12 | After 2.9 it no longer exists; the others edit the new shell/panel files. |
| `dashboard/features/agents/use-agent-inventory.ts` | 2.11 (heavy), 2.10b (option rename) | 2.10b changes only `companyId` → `subscriptionKey`. |
| `dashboard/features/session/use-remote-handoff.ts` | 0.1 | Keep the `reportError: (message: string \| null) => void` option shape; 2.11 changes only what the shell passes in. |
| `dashboard/app/globals.css` | 2.12 (lines 1–2 + root baseline), 0.1, 2.11 (append) | Append your block at the end with a comment header. |
| `dashboard/tests/rendered-html.test.mjs` | 2.9, 0.1, 2.12 (line ~96) | Append new tests; only 2.12 edits existing assertions. |
| `remote/src/main.rs` (`run_resumable_session`) | 0.2, 3.14 | 3.14 rebases onto 0.2. |
| `remote/src/reconnect.rs` (new) | 0.2 creates, 3.14 extends | 0.2 owns `disposition`; 3.14 owns reason/status/retry-now. |
| `remote/src/transport.rs`, `transport/receiver.rs`, `transport/video.rs` | 0.2, 3.14, 3.16a (receiver audio arm) | |
| `remote/src/transport/control.rs` | 3.17 (lines ~316–353), 3.16a (preference block ~368–383) | Adjacent hunks. |
| `remote/src/platform/macos/app.rs` | 0.2 (connecting window, Quit refactor), 3.14 (reconnect panel), 3.16a (mute menu) | Different regions. |
| `agent/src/remote/transport.rs` | 0.2 (error codes, `report_sender_failure`), 3.14 (`Streaming` hook ~855), 3.16b (AIMD, Bitrate handler, video sender), 3.16a (audio ~343–391, `on_message`, pacer call) | Different regions; 3.16b moves AIMD/pacer out first. |
| `agent/protocol/src/control.rs` | 3.16a only | Appends `SessionMessage` tag 39. No other task adds `SessionMessage` variants. |
| `crates/protocol-types/src/signaling.rs` | 0.2 only | Adds an optional field, not a variant. |

---

## 0.1 Viewer-missing detection and "Get the viewer"

**Problem.** `dashboard/features/session/use-remote-handoff.ts:58` calls
`window.location.assign(remoteViewerLink(...))` and clears the spinner after 1200 ms whatever
happens. With no `meshrmm:` handler, Chrome/Edge do nothing and the user gets no feedback. The
viewer builds (`public/downloads/meshrmm-remote-windows-x64.exe`, `meshrmm-remote-macos-arm64.zip`)
are not linked anywhere in the dashboard. Handoff tokens are single-use and last 60 s
(`server/src/lib.rs:30`), so a user who installs the viewer after clicking Connect needs a fresh
handoff.

How the viewer registers the link: on Windows the `.exe` *is* the viewer and registers `meshrmm:`
in HKCU on first run (`remote/src/deep_link.rs:15`). On macOS the app declares
`CFBundleURLSchemes` and must be unzipped and opened once. Downloads are provisioned at release and
404 in local dev.

**Design.**

1. `features/session/viewer-downloads.ts` (pattern: `INSTALLER_ASSETS` in `installer.ts`):
   `VIEWER_DOWNLOADS` for `windows-x64` and `macos-arm64` with label, href, and one-line setup
   text ("Open the downloaded MeshRMM Remote once so your browser can start it." / "Unzip it, move
   MeshRMM Remote to Applications, and open it once."). `detectViewerPlatform(nav)`:
   - UA-CH `Windows` or `/Windows NT/` → `windows-x64` (Windows on ARM runs x64 under emulation).
   - `/Macintosh/` with `maxTouchPoints <= 1` → `macos-arm64` (excludes iPad's desktop UA; say
     "Apple silicon" in the label since Safari can't report the CPU).
   - Otherwise `null` → show both links.
   - Read `navigator` with `useSyncExternalStore` (server snapshot `null`) — the lint config forbids
     setState in effects, and this avoids a hydration mismatch.
2. `features/session/viewer-launch.ts`, pure with injectable event targets and timers:
   `watchViewerLaunch(env, onOutcome) → cancel`.
   - Start watching **before** `location.assign`.
   - Already hidden/unfocused at start → `"unknown"`.
   - `blur`, `pagehide`, or `visibilitychange`→hidden within 2.5 s → `"handed-off"`.
   - Nothing by 2.5 s → `"not-detected"`, but keep listening until 60 s (the token TTL); a late
     signal upgrades to `"handed-off"`.
3. `use-remote-handoff.ts`: replace the 1200 ms timeout with a `launch` state
   `{ agentId, agentName, background, phase: "opening" | "handed-off" | "not-detected" | "unknown" }`.
   Clear the connecting spinners at the first outcome (or immediately on a fetch error). Keep the
   cancel function in a ref; a new `connect` or unmount cancels the previous watcher. Expose
   `dismissLaunch()`. "Try again" re-resolves the agent from the current list and calls
   `connect(agent, background)` — always a fresh handoff. `sessionNotice` stays for close-session
   results only.
4. `features/session/viewer-launch-notice.tsx`, where the plain-text notice renders today. Every
   state keeps the recovery actions so the copy stays true when detection guesses wrong:
   - *opening* (`role="status"`, spinner): "Opening MeshRMM Remote for {name}…"
   - *handed-off / unknown* (neutral, auto-dismiss ~10 s): "Continue in MeshRMM Remote. Didn't
     open? **Try again** · **Get MeshRMM Remote**"
   - *not-detected* (amber, persistent, `role="status"`): "MeshRMM Remote hasn't opened yet. If your
     browser asks to open MeshRMM Remote, choose **Open**. If nothing appears, it may not be
     installed on this computer." + **Download for {OS}** (with setup line), **Try again**,
     **Dismiss**. Never state "not installed" as fact.
5. Permanent sidebar card after the nav, visible to all roles: "MeshRMM Remote — needed to connect
   to devices", detected-OS link primary, "Other platforms" reveals both. Reuse the unused
   `.support-card` styles (`globals.css:41-45`) with dark-sidebar overrides.

**Known false positives/negatives** (document, don't try to eliminate): a browser "Open MeshRMM
Remote?" prompt may or may not blur the page; Safari's "address is invalid" alert may blur and read
as success; alt-tab within 2.5 s reads as success; accepting the prompt after 60 s gives an expired
token (the notice still offers Try again).

**Tests.** `tests/viewer-launch.test.mjs`: UA table (Windows Chrome, UA-CH Windows, mac Safari,
iPad desktop UA with `maxTouchPoints: 5`, Linux, Android); watcher with `EventTarget` fakes and
`t.mock.timers` (blur at 1 s; nothing → not-detected at 2500 ms; blur at 30 s upgrades; blur at
61 s ignored; cancel → no callbacks; hidden at start → unknown). Optional drift guard against
`scripts/verify-release-assets.mjs`. `rendered-html`: tenant `/` contains both download hrefs.

**Acceptance.** On a machine without the viewer, Connect shows "Opening…" and within ~2.5 s the
notice with an OS-correct download. Try again makes a new `POST /v1/remote/handoffs`. With the
viewer installed, the spinner clears as soon as the viewer takes focus. Manual matrix: Chrome and
Edge on Windows (installed with "always allow", installed without it, not installed), Firefox on
Windows, Safari and Chrome on macOS (installed / not installed).

**If starting before 2.9 merges:** keep logic in the new modules and limit `dashboard.tsx` edits to
the hook call (~line 167), the notice (~385), and the sidebar card (~286–288).

**Notes.**

---

## 0.2 Startup failures: typed errors, retry cap, Cancel

**Problem.** Before the first frame, the viewer retries forever and the user can't cancel:
- `remote/src/main.rs:197-250` `run_resumable_session` loops on anything that isn't a 4xx or
  identity error, with no cap.
- `remote/src/transport/receiver.rs:173-177` turns `SignalMessage::Error { message }` into a
  retryable `anyhow` error; the 30 s video timeout (`:233-236`) and ICE `Failed` are retryable too.
- The Windows connecting window ignores `WM_CLOSE` (`launch_window.rs:239-245`); the macOS one has
  no close button (`app.rs` ~1537, `Titled` only).

Also found during planning:
- **F1.** The agent sends *every* sender failure to the viewer, including transient ones
  (`"WebRTC connection ended in state Failed"`), via `report_sender_failure`
  (`agent/src/remote/transport.rs:1580`). The server stores the last one when no viewer is
  connected and replays it to the next connection (`server/src/remote_session.rs:173-205`,
  `:481-496`), so a stale transient error can kill a fresh attempt.
- **F4.** Error types are lost on the agent: capture failures travel as `String` over
  `video_failure_tx` (`transport.rs:324`, `:727`), and helper failures arrive as strings. Codes must
  be attached where errors happen.
- **F5.** `shutdown::request` exists (`remote/src/shutdown.rs:16`), but several steps can't be
  cancelled: `authenticated_websocket` (up to 15 s), `resume_session` (10 s), `create_session`,
  Windows `single_instance::claim` (up to 30 s), the updater.
- `SignalMessage` has no `deny_unknown_fields`, so a new *field* is wire compatible; a new
  *variant* is not (the server closes with 1007 on parse failure).

**Design.**

1. **Wire format** (`crates/protocol-types/src/signaling.rs`):
   ```rust
   Error {
       message: String,
       #[serde(default, skip_serializing_if = "Option::is_none")]
       code: Option<SignalErrorCode>,
   },

   #[serde(rename_all = "snake_case")]
   pub enum SignalErrorCode {
       HardwareEncoderUnavailable, // terminal before first frame
       NoMutualProfile,            // terminal before first frame
       IdentityMismatch,           // always terminal
       CaptureUnavailable,         // retryable; counts toward the startup cap
       #[serde(other)] Unknown,
   }
   ```
   `CaptureUnavailable` is deliberately *not* terminal: initial capture fails transiently on UAC,
   lock screen, and RDP switches. Compile fixes: `server/src/remote_session.rs:137` and
   `agent/src/remote/transport.rs:826` become `Error { message, .. }`.
2. **Agent** (`agent/src/remote/transport.rs`):
   - A `CodedSenderError { code, message }` error type.
   - `start_first_profile` (~244–271): empty candidates → `NoMutualProfile`; every failure's chain
     contains `encoder::Error::HardwareEncoderUnavailable` (or helper text `"no hardware Media
     Foundation"`) → `HardwareEncoderUnavailable`.
   - Initial start in `run_capture_control` wraps other errors as `CaptureUnavailable`.
   - `video_failure_tx/rx` carries `anyhow::Error` instead of `String`.
   - `report_sender_failure` sets `code` by downcasting, and **does not send** transport-level
     failures the viewer detects itself (signaling closed, viewer disconnected, WebRTC
     Failed/Disconnected). Mark those at their sites (~793, 824, 872, 878) with a private
     `SenderFailureKind::Transport` rather than matching strings. This fixes F1 without a server
     change.
3. **Viewer typed failures** — new `remote/src/transport/failure.rs`:
   `FailureKind { SignalingLost, PeerNeverConnected, PeerConnectionLost, VideoTimeout,
   PresentationFailed, AgentReported(Option<SignalErrorCode>), AgentLeft }` and
   `SessionFailure { kind, detail }` whose `Display` is `detail` (logs unchanged). Map each exit in
   `receiver.rs` (stream ended, `PeerLeft`, `Error`, liveness timeout, Failed/Closed/Disconnected —
   `PeerNeverConnected` if `Connected` was never seen this attempt — presenter/video channel, 30 s
   timeout). `IdentityMismatch` code → `IdentityError`. Make the connect at `:61` cancellable with
   `select!` on `shutdown::wait()`.
4. **First-frame tracking** (shared with 3.14): `AttemptProgress { ever_presented,
   attempt_first_frame_at }` behind an `Arc<Mutex<_>>` on `ViewerResumeState`, with
   `begin_attempt()`, `mark_frame_presented()`, `ever_presented()`,
   `attempt_streamed_for(now)`. Mark it at the first completed frame published to the presenter
   (`transport/video.rs` ~222–228), plumbed via `ReceiverLifecycle`. Not `launch_status::finish()`
   — that runs at presenter creation, before any decodable frame.
5. **Policy** — new `remote/src/reconnect.rs`, pure:
   `disposition(error, ever_presented, startup_failures, startup_elapsed) -> Retry | GiveUp | Terminal`.
   - Terminal session errors (existing check) → `Terminal`.
   - Before first frame and a terminal code → `Terminal`.
   - Before first frame and (3 failures or 60 s elapsed) → `GiveUp`. Checked only after a failure,
     so an attempt in progress is never cut short.
   - Otherwise `Retry`; always `Retry` after the first frame (existing reconnect UX).
   In `main.rs`, on `Terminal`/`GiveUp`: `signaling::end_session`, then return an error whose
   **root** is `errors::UserFacing(text)` (anyhow context layers don't downcast through `chain()`),
   so the existing dialogs (Windows `show_fatal_error`, macOS `show_connection_error`) show it.
   Add `shutdown::requested()` checks after `single_instance::claim` and `create_session`, and wrap
   `resume_session` in `select!` with `shutdown::wait()`. New
   `LaunchStatus::Retrying { attempt, max }` → "Could not start the remote display; trying again
   (attempt 2 of 3)…".
6. **Friendly messages** (`remote/src/errors.rs`, `Cause::Session(&FailureKind)`):
   - `VideoTimeout`: "The remote computer accepted the connection but did not send its screen
     within 30 seconds. The MeshRMM Agent may be unable to capture the display. Try again, or
     restart the remote computer if this keeps happening."
   - `PeerNeverConnected`: "Could not open a network path to the remote computer. A firewall on
     either network may block the UDP traffic MeshRMM uses. Try another network or ask your
     administrator to allow UDP."
   - `PeerConnectionLost`: "The connection to the remote computer was lost and could not be
     restored."
   - `HardwareEncoderUnavailable`: "The remote computer has no hardware video encoder MeshRMM can
     use, so its screen cannot be streamed. Updating its display driver may help."
   - `NoMutualProfile`: "The remote computer and this viewer have no video format in common.
     Update the MeshRMM Agent and viewer."
   - `CaptureUnavailable`: "The remote computer could not capture its screen. It may be at a secure
     or locked screen, or have no active display."
   - Unknown/none: keep the current fallback.
7. **Cancel.**
   - Windows (`launch_window.rs`): style `WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU`; client height
     ~150; a "Cancel" `BUTTON` with `IDCANCEL`. On `IDCANCEL` or `WM_CLOSE` while not closed:
     `shutdown::request("the user cancelled the connection")`, label "Cancelling…", disable the
     button. Don't destroy the window — `close_launch_status` still owns that. Disable Cancel during
     `InstallingUpdate`; check partial-file cleanup in `updater/windows.rs` before allowing it during
     `DownloadingUpdate`.
   - macOS (`app.rs` ~1520–1614): `Titled | Closable`; an `NSButton` "Cancel" (Esc) targeting a new
     `ConnectingWindowController` (`cancelConnection:`, `windowShouldClose:` → false), stored in
     `ConnectingWindow` because targets/delegates are weak. Factor the Quit path (`app.rs:71-80`)
     into `end_running_session(reason)` and call it from both.

**Edge cases.** Cancel during `ClosingPreviousViewer` is honored after `claim` returns. Cancel
between `create_session` and the loop releases the lease via `end_session`; cancel during an
in-flight `create_session` leaves the lease to expire server-side (document it). Old agents send no
code: the cap still applies. `HardwareEncoderUnavailable` after the first frame is retryable.

**Tests.** Protocol: no-code decode, round trip, unknown code → `Unknown`, old-shape enum decodes
new message. Agent: empty candidates → `NoMutualProfile`, synthetic encoder chain → code, transport
failures suppressed. Viewer: `disposition` table (terminal code before/after first frame, 3
failures, 2 failures + 61 s, 100 failures after first frame, identity error); every `FailureKind`
renders plain text; a `UserFacing` root under a context is returned verbatim; `Retrying` text.
Tests are in-module `#[cfg(test)]` (`remote` is a binary crate); tokio has no `test-util`, so keep
policy functions pure over `Instant`/`Duration`.

**Acceptance.** UDP blocked and no relay: after ≤3 attempts or ~60 s the viewer shows the UDP
dialog and exits, and the server logs the lease ended by client. Agent can't encode (new agent):
one dialog, no retries. Cancel (button, close box, Esc on macOS) in any connecting phase ends the
process within ~5 s with no error dialog. After the first frame, network loss still shows the
reconnect overlay and retries indefinitely. Old agent + new viewer and new agent + old viewer both
still work.

**Validation.** Mac: Rust checks, server wasm check, macOS connecting window (Cancel, Esc, close
box), startup cap against an offline agent or with UDP blocked via `pf`. Endpoint: native clippy and
tests for `meshrmm-remote` and `meshrmm-agent`; Windows connecting-window Cancel/close box. Install
the agent: first run a new viewer against the currently installed agent (legacy error, no code),
then upgrade and run a normal session and a service restart mid-session. No hook forces
`HardwareEncoderUnavailable`; rely on unit tests and say so. No server deploy needed (pattern change
only).

**Notes.** Implemented in `c424a4d`, `b63f8cd`, `41a1baa` and `32bc917`. Deviations:
- `HardwareEncoderUnavailable` is detected by the text `"no hardware Media Foundation"` anywhere in
  the error chain, not by downcasting: `encoder::Error` is in a private module of
  `meshrmm-remote-screen`, and the installed service only receives desktop-helper failures as
  text. The text match covers both capture paths.
- The agent-side types (`CodedSenderError`, `SenderFailureKind::Transport`) live in a new
  `agent/src/remote/sender_failure.rs`, to keep `transport.rs` hunks small for 3.16b; its tests
  run on the Mac. "Video data channel send failed" is also marked as a transport failure (it
  happens when the viewer disconnects — the same stale-error case as F1).
- A 30 s video timeout counts as `PeerNeverConnected` (UDP message) when the agent answered but
  WebRTC never connected, rather than the "did not send its screen" text.
- Cancel covers more than listed: session request, update check, signaling connect and resume all
  `select!` on `shutdown::wait()`. Update downloads are held in memory (no partial file), so
  Cancel is disabled only during `InstallingUpdate`, via `LaunchStatus::cancellable()`. Esc also
  cancels on Windows (`IsDialogMessageW`).
- Every give-up or terminal stop, including the existing terminal session errors, returns a
  `UserFacing` root; the dialog text is unchanged. A failure after the user cancelled no longer
  shows a dialog.
- `AttemptProgress::mark_frame_presented` returns whether it was the attempt's first frame, for
  3.14.
- Added in review (`32bc917`): after a user-requested stop, the lease release gets one attempt
  bounded to 3 s (`shutdown::lease_release_budget`); ordinary disconnects and the give-up path keep
  the 3×5 s retries. Before this, Cancel against an unreachable server took 15.7 s to exit.

Validation. macOS development host, on the rebased tree: `cargo fmt --check`, `cargo clippy -D
warnings`, `cargo test --workspace`, wasm32 `cargo check -p meshrmm-server`, `sql_regressions.py`,
`scripts/tests`, worker build, and all four Miniflare tests pass. macOS connecting window (debug
build, unreachable server): Cancel, Esc and the close box exit with code 0 and no dialog in
3.2–3.6 s; the startup cap against a refusing server shows "attempt 2 of 3", gives up after 3
failures, and shows the "Could not reach the MeshRMM service…" dialog. DESKTOP-85R6S28 (synced tree
at the pre-rebase commits, SHA-256 manifest of 413 files matched; the rebase only added server and
dashboard changes): native fmt, clippy, `cargo test -p meshrmm-remote` and `-p meshrmm-agent` pass;
the Windows connecting window's Cancel, close box and Esc exit with code 0 in ~0.5 s ("the user
cancelled the connection" in the log); the startup cap reaches "attempt 2 of 3" and then the "Could
not reach…" message box after 22 s. The agent built from the pre-review tip (`7209a81`, same agent
code as `b63f8cd`) was installed, started, connected, and reconnected 1.7 s after a service
restart with no WARN/ERROR lines; the previous build was then restored (see Endpoint state).

**Incomplete:** no authenticated dashboard link was available, so new viewer vs. the old installed
agent (legacy uncoded error), new agent vs. old viewer, a normal session and a mid-session service
restart on the new agent, the UDP-blocked startup cap, and the agent-cannot-encode dialog were not
exercised. `HardwareEncoderUnavailable` has no forcing hook and rests on unit tests.

---

## 2.9 Persistent workspace shell and filters in the URL

**Problem.** `app/page.tsx`, `app/settings/page.tsx`, `app/users/page.tsx`, and
`app/authentication/page.tsx` each render `<Dashboard view=…/>`. Verified in vinext
`1.0.0-beta.8`: page slots are keyed by a bfcache identity that differs per route, so every
navigation remounts `TenantDashboard` (`features/workspace/dashboard.tsx:59-448`) — account
refetch, new inventory WebSocket, idle-session reset, and loss of search/status filters and the
unsaved settings draft. The root layout and providers do not remount.

vinext facts the design relies on: route groups are URL-transparent and a group layout's identity
skips `(group)` segments, so `app/(workspace)/layout.tsx` persists across its pages (don't add a
`template.tsx` — templates remount per route). `next/dynamic` works, including `ssr: false`.
`window.history.replaceState` is patched and updates `useSearchParams()` without an RSC refetch
(`router.replace` would refetch).

**Target structure.**
```
app/layout.tsx                           unchanged (surface calc moved to helper)
app/login/page.tsx                       unchanged (outside the group → no shell)
app/(workspace)/layout.tsx               NEW server layout: surface dispatch
app/(workspace)/page.tsx                 → <DevicesPanel/>
app/(workspace)/settings/page.tsx        → <SettingsPanel/>   (keeps `metadata`)
app/(workspace)/users/page.tsx           → <UsersPanel/>
app/(workspace)/authentication/page.tsx  → <AuthenticationPanel/>
lib/request-surface.ts                   NEW: headers() + classifyHost → "marketing" | "platform" | "tenant"
features/workspace/workspace-shell.tsx   NEW "use client": TenantDashboard minus panel bodies
features/workspace/workspace-context.tsx NEW: context + useWorkspace()
features/workspace/views.ts              NEW pure: View, VIEW_PATHS, viewForPath(pathname), VIEW_COPY
features/agents/device-filters.ts        NEW pure: parse / serialize / filterAgents
features/agents/devices-panel.tsx        NEW "use client"
features/settings/use-settings-draft.ts  NEW: draft state hoisted into the shell
features/workos/users-panel.tsx, authentication-panel.tsx  NEW (widgets moved verbatim)
features/workspace/dashboard.tsx         DELETE
```

**Design.**
1. The group layout dispatches on the server: marketing → `<MarketingPage/>` (can become a server
   component), platform → `<PlatformDashboard/>`, tenant → `<WorkspaceShell>{children}`.
   `requestSurface()` must match `app/layout.tsx:40-46`; refactor the root layout to use it. Each
   page returns `null` when the surface isn't tenant, so its client reference isn't serialized or
   modulepreloaded on marketing.
2. `WorkspaceShell` takes, verbatim from `TenantDashboard`: auth/account and `accountLoader`
   (~60–200), `authorizedFetch`, `useAgentInventory`, `useIdleSession`, the
   `AUTH_REFRESH_FAILED_EVENT` listener, installer download/enrollment modal, `useRemoteHandoff`,
   delete, account modal, sidebar, topbar, page heading/actions, the gate cards, and the
   account-pending loader. `view = viewForPath(usePathname())`; `{children}` renders where the
   panels render today.
3. `useWorkspace()` exposes `account`, `company`, `setAccount`, `isAdmin`, `displayName`,
   `authorizedFetch`, `getAccessToken`, `inventory`, `remote`, `deleteAgent`, `deletingId`,
   `reportError`, `devicesSearch`/`setDevicesSearch`, and `settingsDraft`.
4. Filters in the URL (`devices-panel.tsx`): `parseDeviceFilters(useSearchParams())` (unknown
   status → `all`, query capped at ~200 chars). Local `queryInput` for typing, resynced from the
   URL on back/forward using the derive-during-render pattern in `settings-page.tsx:36-39`. Write
   with `history.replaceState` — status immediately, query debounced ~250 ms (Safari rate-limits
   `replaceState`). Also `setDevicesSearch(search)` so the Devices nav link keeps the filter.
   `filterAgents` moves from `dashboard.tsx:202-209`.
5. `useSettingsDraft(company)` owns `draft`, `draftSource`, `settingsTab`, and the
   reset-on-company-change logic from `settings-page.tsx:29-39`; `SettingsPage` keeps only
   `isSaving` and the notice. Add a `beforeunload` guard while the draft is dirty.
6. Keep the SSR markup of `NavItem` identical so existing `rendered-html` assertions pass.

**Edge cases.** A session pause or gate card unmounts `children` (acceptable; the hoisted draft
survives). Sign-out nulls `account`, which resets the draft. `/login` stays outside the group.
`returnTo: "/"` in resume/sign-in is unchanged.

**Tests.** `tests/device-filters.test.mjs` (defaults, invalid status, serialization omits defaults
and encodes reserved characters, case-insensitive name/id match). `tests/workspace-views.test.mjs`
(`/`, `/settings`, `/users`, `/authentication`, trailing slash, unknown → `agents`).
`rendered-html`: `/?q=alpha&status=offline` on a tenant host renders `value="alpha"` and offline
selected; `/users` on `meshrmm.com` still renders marketing; `/settings` on the platform host
renders "Checking administrator access".

**Acceptance.** Devices → Settings → Users → Devices with the Network panel open: no second
`/v1/account`, no new subscription, WebSocket stays open, filter restored, unsaved settings edit
survives. Reloading `/?q=x&status=online` restores the filter. Marketing and platform unchanged.
`npm run verify` green.

**Notes.** Implemented as specified in `200591b` and `817785b`. Confirmed in vinext 1.0.0-beta.8: the
`(workspace)` layout's key drops the group segment, and the patched `replaceState` updates
`useSearchParams()` only for history state vinext doesn't own, so filters are written with
`history.replaceState(null, …)`. `SettingsPanel` lives in `features/settings/settings-panel.tsx`
(the target structure didn't name a file). `WorkspaceContext.Provider` wraps the whole shell, so
wave-2 tasks can call `useWorkspace()` from shell-level UI as well as from panels. The Devices
search box updates `devicesSearch` on every keystroke and only the URL write is debounced, so
leaving within 250 ms keeps the filter. Marketing pages no longer preload any workspace chunk.

Verified on the macOS development host: `npm run verify` (typecheck, lint, build, 72/72 tests) on
the rebased tree. In the task worktree, Chromium against the production build with mocked
`/auth/session`, `/v1/*` and WebSocket endpoints: one `/v1/account`, one subscription and one
WebSocket (never closed) across Devices → Settings → Users → Devices and back/forward, with the
shell's DOM node unchanged; the filter was restored via the nav link, back/forward and reload; an
unsaved settings edit and its tab survived. The old build showed three account loads and three
subscriptions for Devices → Settings → Devices. Not verified: real WorkOS sign-in and widgets
(CORS with the mock token), a live control-plane WebSocket, Safari's `replaceState` rate limit.

Known limitations: with an unsaved settings edit, starting a remote session from Devices may show
the browser's leave-page prompt, because opening the `meshrmm:` link can fire `beforeunload`
(`use-remote-handoff.ts` is left to 0.1). "Clear filters" leaves the old `q` in the URL for ~250 ms
before the final URL is written.

---

## 2.10a Fewer D1 round trips and cron cleanup (server)

**Problem.** On a warm isolate the page-load path to the first device snapshot is ~11 serial D1
round trips. `create_agent_event_subscription` (`server/src/routes/events.rs:11-51`) makes 5: the
company lookup in `authorize_workos_user` (`auth.rs` ~272), `ensure_company_exists` (same row
again), a table-wide `DELETE FROM agent_event_subscriptions WHERE expires_at <= ?1`, the `INSERT`,
and `canonical_company_url` (third read). The WebSocket upgrade adds `UPDATE … RETURNING` plus
`request_tenant_company` (`events.rs:79-97`). `create_handoff` (`routes/handoffs.rs:3-65`) makes 6,
including a global `DELETE FROM remote_handoffs` before every Connect. `create_agent_installer`
(`routes/agents.rs:20-59`) has the same pattern.

Verified: every consumer already filters `expires_at > now` (events.rs ~81, handoffs.rs ~82/92,
agents.rs ~111/121/131), so correctness doesn't depend on cleanup. Expiry indexes exist
(migrations 0001:35, 0002:15, 0003:13); no migration needed. `worker 0.8.5` supports
`#[event(scheduled)]`. `server/wrangler.jsonc` has no `triggers` yet. `record_active_user` is
cached per isolate/user/month and is not a per-request cost; leave it.

**Design.**
1. **Reuse the authorized company row.**
   - Add `slug: Option<String>` to `TenantCompany` (`lib.rs` ~51); select `slug` in
     `company_for_request` and `request_tenant_company` (`infrastructure.rs`).
   - Extract pure `company_url(env, slug) -> Result<String>`; keep `canonical_company_url` as a
     wrapper (installer redeem still uses it).
   - Add `authorize_workos_company(request, env) -> Result<(Identity, TenantCompany), AuthError>`
     with the current body (set `status = "active"` after the `awaiting_admin` update);
     `authorize_workos_user` maps it. Don't add slug to `Identity` (it's hand-built in audits).
2. **Subscription create:** auth + one `INSERT` (2 round trips). Drop `ensure_company_exists` and
   the `DELETE`; build the WebSocket URL from `company_url(env, company.slug)`.
3. **WebSocket upgrade:** compute the tenant slug from the hostname (no DB); 403 early if neither
   tenant nor legacy control-plane host. Fold the host/status check into the claim (1 round trip):
   ```sql
   UPDATE agent_event_subscriptions SET used_at = ?1 WHERE token_hash = ?2 AND used_at IS NULL AND expires_at > ?1 AND EXISTS (SELECT 1 FROM companies WHERE companies.id = agent_event_subscriptions.company_id AND companies.status IN ('active', 'awaiting_admin') AND (?3 = '' OR companies.slug = ?3 COLLATE NOCASE)) RETURNING company_id, user_id
   ```
   Behavior change (stricter): a token presented on the wrong tenant host now gets 401 and is
   **not** consumed (today it's consumed, then 403). The dashboard reconnects either way.
4. **Handoff create:** auth + one `metered_batch` (2 round trips): `INSERT INTO remote_handoffs …
   SELECT … WHERE EXISTS (agent in company, not deletion-requested)` — keep the 7-parameter order,
   `sql_regressions.py` reuses the literal — and a conditional audit insert `WHERE EXISTS (SELECT 1
   FROM remote_handoffs WHERE token_hash = ?)`. `changes != 1` → the same 404 `"Agent not found"`.
   `api_url` from `company_url`. Response shape unchanged.
5. **Optional, same pattern:** `create_agent_installer` → auth + batch [INSERT, audit].
6. **Cleanup off the request path:** cron, not `ctx.wait_until` (still a write per request, and the
   deferred future runs outside `usage::Scoped` so it's unmetered) and not a DO alarm (wrong scope).
   - `wrangler.jsonc`: `"triggers": { "crons": ["*/30 * * * *"] }`.
   - New `server/src/maintenance.rs`: `purge_expired_tokens(env, now_ms)` — one `metered_batch` of
     three `DELETE … WHERE expires_at <= ?1` (subscriptions, handoffs, install tokens), with
     `?1 = now - 10 min` grace for clock skew.
   - `#[event(scheduled)]` in `lib.rs` wrapped in `usage::metered(…, Source::Scheduled, …)`; add
     the `Scheduled` source in `usage.rs` (attributed to `_platform`; decide billable per
     `docs/cost-tracking.md` and comment it).
   - Remove the request-path DELETEs in events.rs, handoffs.rs (and agents.rs if doing 5).
   - Leave the `costs.rs` retention DELETEs alone (not on a user-facing path).

**Tests.**
- `sql_regressions.py` (keep SQL literals on one line; the extractor is line-based): handoff
  `INSERT…SELECT` inserts nothing for another company's, a deleted, or an unknown agent; the
  conditional audit only inserts when the handoff exists; the upgrade `UPDATE` with `?3` (legacy
  `''` matches, same slug different case matches, other slug doesn't and leaves `used_at` NULL,
  suspended doesn't match, expired doesn't match); maintenance DELETEs remove only rows at or
  before the cutoff.
- New `server/tests/request_path.mjs` (Miniflare, bootstrap like `presence.mjs`; add to the
  `server-wasm` job in `ci.yml` after `token_rotation.mjs`). Wrap `DB` to count round trips (each
  `batch` counts once). Warm up once, then assert: subscription create = 2 round trips and correct
  `websocket_url` (tenant host; legacy host with slug; legacy host without slug); suspended → 403
  with no row; `awaiting_admin` → 200 and becomes active; pre-inserted expired rows survive
  subscription and handoff creation; expired token upgrade → 401; wrong-host upgrade → 401 and
  still unused, then the right host → 101 in 1 round trip; own-agent handoff → 200, correct
  `api_url`, audit row, 2 round trips; other-company or deleted agent → 404, no rows; invoking
  `scheduled` purges expired rows past the grace period in all three tables and keeps live ones.
- Re-run `presence.mjs`, `session_cleanup.mjs`, `token_rotation.mjs` unchanged.

**Acceptance.** Warm-isolate D1 round trips: subscription create ≤ 2, WS upgrade 1 (Worker side),
handoff create 2. No `DELETE … expires_at` left in `server/src/routes/*`. Cron configured and
`scheduled` exported in the built worker. Response shapes of `/v1/agents/events/subscriptions` and
`/v1/remote/handoffs` unchanged. All server checks in [Working rules](#working-rules) pass.
Backward compatible with the current dashboard — no client change required.

**Notes.** Implemented in `9d5b338`, `3dde5f0` and `e962fef`. Subscription create, handoff create and
installer create (optional step 5, done) now take 2 D1 round trips each, and the WebSocket upgrade
takes 1; `server/tests/request_path.mjs` (added to the `server-wasm` CI job) asserts these counts
with a counting D1 wrapper. A `*/30` cron (`maintenance::purge_expired_tokens`) deletes rows from
all three token tables once they are more than 10 minutes past expiry, in one batch. It is metered
as billable platform usage under the new source label `scheduled`, whose CPU time `costs.rs` counts
with the API Worker's runs (noted in `docs/cost-tracking.md`). `ensure_company_exists` was removed
(no other callers); `audit()` now builds on a new `audit_statement()` helper that the installer
batch uses. Response shapes are unchanged.

Correction to the design: `api.meshrmm.com` parses as the company hostname `api`, so computing the
tenant slug first would reject every token on the legacy host. The upgrade therefore checks for the
legacy host before the company hostname (`api` is a reserved slug no company can have). Separately,
and not caused by this change: dashboard-authorized routes on `api.meshrmm.com` already return 404
because they resolve that host as company `api`; only `localhost` works as a legacy host there, so
the test creates legacy-host subscriptions through `localhost`. Worth a separate look. A token
presented on another company's hostname now gets 401 and stays unused (before: consumed, then 403).

Checks (macOS development host, at the merged tree): `cargo fmt --check`, `cargo clippy -D warnings`,
`cargo test --workspace`, wasm32 `cargo check -p meshrmm-server`, `sql_regressions.py`,
`scripts/tests` unittest, `node --test scripts/*.test.mjs`, `worker-build --profile server-release`
(built worker exports `scheduled`), and `presence.mjs`, `session_cleanup.mjs`, `token_rotation.mjs`,
`request_path.mjs` — all passed. One earlier full `cargo test` run in the task worktree saw
`meshrmm-file-transfer`'s `windowed_transfer_keeps_a_window_in_flight` fail once (13 vs 16) under
parallel load; it passed on every rerun and is unrelated. No endpoint validation needed
(server-only).

---

## 2.10b Start the event subscription in parallel with the account load (dashboard)

**Problem.** The subscription waits for `companyId` from `/v1/account`
(`use-agent-inventory.ts:74` `if (!enabled || !companyId) return;`), even though the server derives
the company from the JWT organization and host — the client never sends `companyId`. It's only an
effect key and gate. `hasTenantSession` already guarantees the organization matches the tenant.

**Design.**
- `use-agent-inventory.ts`: rename the option `companyId?: string` → `subscriptionKey?: string`.
  Touch only the option type, the gate (~74), and the effect deps (~209); don't change the
  `connect()` body (2.11 owns it).
- In the shell (after 2.9): `useAgentInventory({ enabled: Boolean(hasTenantSession &&
  !sessionPauseReason && !(accountError && !accountError.retrying)), subscriptionKey:
  workosOrganizationId, … })`. Drop the unused `companyId`.
- Tell 2.11: the subscription can now fail **before** the account loads. The `accountError` gate
  stops reconnecting once the account failure is non-retryable; ideally the inventory stream also
  treats 403/404 as non-retryable.

**Not doing:** returning the subscription from `/v1/account` (couples the account retry loop to a
60 s one-use token). **Optional, needs a security review, not in scope unless the user asks:**
caching the access token in the sealed session so `/auth/session` skips the WorkOS refresh while
the token has > 120 s left (`dashboard/worker/auth.ts`); trade-off is that role changes and
WorkOS-side revocation are noticed up to ~5 min later on reload, and the cookie grows ~2 KB.

**Edge cases.** Suspended company or account 404: both requests fail; one error may show above
"Loading your company workspace" briefly. Snapshot before account: agents populate, but the list
stays hidden while the account is pending, then renders instantly. The idle lock still waits for
the account.

**Tests / acceptance.** Existing dashboard tests pass. DevTools waterfall on reload:
`POST /v1/agents/events/subscriptions` starts in the same tick as `GET /v1/account`, right after
`/auth/session`.

**Notes.**

---

## 2.11 Inventory keeps data, shows stale state, and errors have owners

**Problem** (`features/agents/use-agent-inventory.ts`, `features/workspace/dashboard.tsx`,
`features/agents/agent-overview.tsx`):
- A failed refresh wipes the list (`setIsLive(false); setAgents([]);` ~58–59). A successful
  `loadAgents` never sets live/has-data; `lastUpdated` starts at `new Date()` before any data.
- Backoff 1 → 30 s with no wake-up on `online` or tab visibility; nothing prevents two concurrent
  `connect()` calls.
- One shared `error` state is written by inventory, delete, handoff, settings, and resume; the
  socket `open` handler's `reportError(null)` clears other actions' errors; the banner always uses
  `WifiOff`. The resume error is invisible (the banner isn't rendered in the paused branch).
- Everything keys off `isLive`: metrics show `—`, header says "Updating…", rows keep "Online", and
  Connect stays enabled while the socket is down.

**Design.**
1. Extract a testable controller `features/agents/inventory-stream.ts` (style of
   `subscription-renewal.ts`) from the socket logic (~73–209):
   `inventoryStream({ subscribe, openSocket, renewal, onAgents, onConnection, onError, timers })
   → { wake, stop }`. Add a `connecting` re-entrancy guard. `wake()`: no socket and not connecting
   → clear the timer, reset the delay to 1 s, connect now; socket open → `send("refresh")` (the
   server already handles it); otherwise nothing. The hook calls `wake()` on `online`, on
   `visibilitychange` to visible, and from Refresh; `offline` sets `connection = "offline"`.
   Consider treating 403/404 from subscribe as non-retryable (see 2.10b).
2. Hook state: `agents` (wiped only by `reset()`, which idle lock and sign-out still use), `hasData`,
   `lastUpdated: Date | null`, `connection: "connecting" | "live" | "reconnecting" | "offline"`,
   inventory-owned `error`. Pure `inventoryStatus()`: `!hasData` → loading; live → live; else stale
   **after a 5 s grace** (the renewal close `4001` + 1 s reconnect must not flash stale).
   `loadAgents` success sets agents/hasData/lastUpdated and clears the error but doesn't claim live;
   failure keeps agents and sets "Couldn't refresh devices: …". Keep the revision check.
3. Error ownership: `features/workspace/action-errors.ts`, a pure reducer keyed by
   `"remote" | "delete" | "close-session" | "resume"`; clearing one source never clears another.
   - Inventory error → a connection strip in the Devices panel (`WifiOff`, `role="status"`):
     "Live updates are reconnecting. Showing devices as of 10:32. [Reconnect now]".
   - Action errors → a dismissible banner above the table (`CircleAlert`, `role="alert"`), via
     `reportActionError(source)`. `useRemoteHandoff`'s option signature stays the same.
   - Settings: local `saveError` inline next to Save; drop the `reportError` prop.
   - Resume error: render inside the paused card.
   - Remove the shell-level error banner.
4. `AgentOverview`: replace `isLive`/`lastUpdated` props with
   `inventory: { status, connection, lastUpdated, error }` and `onReconnect`. Counts and header use
   `hasData`. Stale: rows get a `stale` class (dimmed but AA contrast), badge "Online · last known",
   footer "Last updated 10:32 · Reconnecting…". Offline: "You're offline · showing devices from
   10:32". Connect stays enabled while stale (the handoff API is authoritative), disabled when
   offline, with a "Status may be out of date" title while stale. Sidebar count and topbar pill use
   `hasData`; the pill says "Reconnecting" when stale.

**Tests.** `tests/inventory-stream.test.mjs` (fake `SocketLike extends EventTarget`, mock timers):
backoff 1, 2, 4 … 30 s; `wake()` while waiting connects now and resets to 1 s; `wake()` while open
sends `"refresh"`; no duplicate subscribe while one is in flight; snapshot + buffered deltas match
current behavior; close → reconnecting with agents retained. `tests/action-errors.test.mjs`.
`inventoryStatus` table including the grace period.

**Acceptance.** DevTools offline: list stays, stale styling after 5 s, Connect disabled. Back
online: reconnects within ~1 s. Sleep/wake: resync on visible. Failed Refresh keeps the rows. A
delete error survives a socket reconnect. Settings error shows beside Save.

**Notes.**

---

## 2.12 Scope WorkOS widgets and Radix CSS to the admin pages

**Problem.** `app/globals.css:1-2` imports `@radix-ui/themes/styles.css` and
`@workos-inc/widgets/styles.css` globally; `providers.tsx:4,41` wraps every page (marketing
included) in `<WorkOsWidgets>`; `dashboard.tsx:4-8` statically imports the widgets. Measured on the
`c4824f1` build: CSS 758 KB raw / 94.8 KB gzip, `dashboard-*.js` 307 KB / 84 KB gzip, ≈348 KB gzip
per page in total.

**Key finding.** The app's own UI uses no Radix Themes or Radix tokens — only the WorkOS widgets
need them. But `<WorkOsWidgets>` renders a root Radix `<Theme>` around the **whole app**, and its
root rules currently apply everywhere: `line-height: 1.5`, `color: #202020`,
`overflow-wrap: break-word`, `text-size-adjust: none`, grayscale smoothing, `background: white`
(masking `body { background: #f7f8fb }`), `min-height: 100vh`. Removing it changes the look unless
these are replicated.

**Design.**
1. Delete the two `@import`s. Add a root baseline reproducing the current rendering on `body` (or an
   `.app-root` wrapper in `Providers`). **Decision for the user:** keep today's effective white
   background, or switch to the `#f7f8fb` that `globals.css` declares. Default: keep white (no
   visual change). Take before/after screenshots: marketing, platform, `/`, `/settings`, `/login`,
   desktop and mobile.
2. `providers.tsx`: remove `WorkOsWidgets`; keep `AuthProvider` and `RuntimeConfigContext`.
3. New `features/workos/widgets-scope.tsx` (`"use client"`): imports a local
   `workos-widgets.css` that `@import`s both stylesheets (vinext's client build uses
   `moduleSideEffects: "no-external"`, so import CSS through a local file), and renders
   `<WorkOsWidgets className="workos-widgets-scope" theme={{ accentColor: "violet", radius:
   "medium", fontFamily: "var(--font-geist-sans)", hasBackground: false }}>`. Add
   `.workos-widgets-scope { min-height: 0; }`.
4. Use the widget subpath exports (`@workos-inc/widgets/users-management`,
   `/admin-portal-domain-verification`, `/admin-portal-sso-connection`). After 2.9, the users and
   authentication panels check `isAdmin`, then render
   `dynamic(() => import("./users-widgets"), { ssr: false, loading: … })`. If this lands before 2.9,
   apply the same `dynamic()` in `dashboard.tsx` (~409–413) and remove its static imports.
5. Don't trim the Radix CSS further: swapping full tokens for base + needed scales saves ~22 KB gzip
   on two admin pages, and widgets pick colors dynamically, so a missing scale fails silently.

**Expected result** (estimates; measure): marketing ≈135 KB gzip JS (no dashboard chunk) and
≈10 KB gzip CSS (was 94.8); tenant devices/settings load no Radix CSS; `/users` and
`/authentication` add the widget chunk and ≈86 KB gzip CSS.

**How to measure.** `npm run build`; size `dist/client/_next/static/{css,chunks}/*` raw and
`gzip -9`; inspect `<link rel="stylesheet|modulepreload">` in the SSR HTML per route and
`clientReferenceDeps` in `dist/server/__vite_rsc_assets_manifest.js`; DevTools Network with cache
disabled and the Coverage tab on `/`, `/users`, and marketing. Record before/after in **Notes**.

**Tests** (`rendered-html`): flip the existing widget-root assertion (~line 96) to
`doesNotMatch(/data-woswidgets-root/)`; add a budget helper that reads linked asset sizes from
`dist/client` and asserts marketing stylesheet bytes < 80,000 raw, no preloaded chunk contains
`UsersManagement` or `radix-themes`, and tenant `/settings` stylesheets don't contain
`.rt-BaseDialogContent`.

**Acceptance.** Marketing, devices, and settings load no Radix/WorkOS CSS or JS. `/users` and
`/authentication` render the widgets correctly, including dialogs. No visual regressions in the
screenshots. `npm run verify` green.

**Notes.**

---

## 3.14 Backoff reset, reconnect reason, and Retry now

**Depends on 0.2** (`remote/src/reconnect.rs`, `SessionFailure`, `AttemptProgress`). Parts with no
overlap — `crates/signaling-client`, `agent/src/remote/session.rs`, and the platform overlay UI —
can start before 0.2 merges.

**Problem.** Viewer (`remote/src/main.rs` ~193, 210) and agent (`agent/src/remote/session.rs` ~37,
65) create `ReconnectBackoff::new(1s, 15s)` once per session and call `next_delay()` on every
failure; `reset()` (`crates/signaling-client/src/lib.rs` ~47) is never called. After a few blips
in a long session, every reconnect waits 15 s (plus the 10 s disconnected grace). The overlay is a
fixed "Reconnecting to the remote computer…" (macOS `app.rs` ~777–800, Windows `toolbar.rs`
~466–482).

Also found during planning:
- **F2.** The current server never sends `PeerLeft` (`server/src/remote_session.rs:210-228` only
  logs), so a PeerLeft-based "remote restarting" message would only fire with old servers.
- **F3.** Each viewer retry calls `resume_session`; the coordinator re-sends the session request
  with new TURN credentials, so the agent aborts and restarts its session task (fresh backoff,
  fresh capture/encoder). The viewer's backoff therefore sets reconnect latency; the agent's only
  grows when its sender fails without a viewer resume.

**Design.**
1. **Stable-connection reset** (`crates/signaling-client/src/lib.rs`):
   ```rust
   pub const STABLE_CONNECTION: Duration = Duration::from_secs(20);
   impl ReconnectBackoff {
       /// An attempt that streamed for at least STABLE_CONNECTION restarts the schedule;
       /// a connect-then-fail loop keeps growing.
       pub fn delay_after(&mut self, streamed_for: Option<Duration>) -> Duration { … }
   }
   ```
   Resetting at the first frame would let a crash-3-s-after-connect loop retry every ~1 s forever.
2. **Where:** viewer `main.rs` uses `backoff.delay_after(resume_state.attempt_streamed_for(now))`
   (0.2's `AttemptProgress`). Agent: a `SenderProgress` (`Mutex<Option<Instant>>`) set where
   `session_state` becomes `Streaming` (`transport.rs` ~855–858), and
   `backoff.delay_after(progress.take().map(|t| t.elapsed()))` at `session.rs` ~65. (Per F3 the
   agent side matters only for sender failures without a viewer resume; do it for consistency.)
3. **Reason, elapsed, state** (`remote/src/reconnect.rs`):
   `ReconnectReason { RemoteUnavailable, NetworkLost, ConnectionInterrupted, VideoRestarting }`,
   `ReconnectPhase { Waiting { until }, Attempting }`, `ReconnectStatus { reason, since, phase }`
   with `render(now) -> (title, detail, retry_enabled)`, and `classify(&anyhow::Error)`:
   - `AgentLeft`, resume failing with 5xx/409 (agent offline; surfaced as 500 by `ensure_success`)
     → "The remote computer is restarting or offline…"
   - `SignalingLost`, tungstenite `Io`/`Tls`, reqwest connect/timeout → "Network connection lost…"
   - `PeerConnectionLost`/`PeerNeverConnected` with healthy signaling → "Connection to the remote
     computer was interrupted…"
   - `PresentationFailed`, mid-session `AgentReported` → "Restarting the remote display…"
   Detail line: "Disconnected for 1:05 · retrying in 8 s" / "· reconnecting…". `since` is set at
   the first failure after streaming and cleared by `mark_frame_presented`. The main loop sets the
   status after each error, updates the reason after the resume result, and sets `Attempting`
   before `run_receiver`. `ViewerResumeState::set_reconnect_status` forwards to the kept presenter;
   `keep_while_reconnecting` takes the error to classify.
4. **Retry now:** a generation counter + `Notify` in `reconnect.rs` (modeled on `shutdown.rs`):
   `request_retry_now()`, `retry_generation()`, `wait_for_retry_after(generation)`. Capture the
   generation when entering the backoff wait in `main.rs` and add a third `select!` arm. Clicks
   before the wait are ignored; the backoff is skipped, not reset; the button is disabled while
   attempting.
5. **UI.** Presenter API `set_reconnecting(bool)` → `set_reconnect_status(Option<ReconnectStatus>)`
   on both platforms.
   - Windows: `Shared.reconnecting` becomes `Mutex<Option<ReconnectStatus>>`; the worker loop
     re-renders only when the text changes (~1/s); `window.rs` updates the label (two lines) and a
     new "Retry now" owned `WS_POPUP` button (set its ID with `SetWindowLongPtrW(GWLP_ID)`; handle
     it in `toolbar.rs` `command()`); label height ~64 px. **Check on the endpoint** that
     `BN_CLICKED` from an owned popup button reaches the owner; if not, use a small registered popup
     class that forwards `WM_COMMAND` to `GetWindow(GW_OWNER)`.
   - macOS: replace `reconnecting_label` with a panel (title, detail, "Retry now" `NSButton`
     targeting a new `retryReconnect:` on `RemoteView`); update elapsed time with a
     self-rescheduling `DispatchQueue::main().after(1s)` that stops when the status is `None`.

**Deferred: ICE restart.** webrtc 0.14 has `restart_ice()`, but using it needs a mid-session
re-offer from the agent (the only offerer) and a new trigger signal — a new `SignalMessage` variant
that old peers and old servers reject — a "soft resume" path that reconnects signaling without
`/resume` while keeping both peers alive, TURN credential refresh on long outages, and a spike on
webrtc-rs behavior after `Failed`. It also doesn't help agent restarts. **Cheaper follow-up** (not
in this plan): keep the agent's streamer alive when a resume for the same session arrives
(`agent/src/remote/mod.rs` ~220–238) instead of aborting the task.

**Tests.** `signaling-client`: `delay_after(None | 19 s)` keeps growing, 20 s resets to 1 s, growth
resumes. `reconnect.rs`: `classify` over constructed failures, a tungstenite `Io` error, and
`ApiError { status: 500 }`; `render` formatting (0:05, 1:02, 1:02:03; waiting vs attempting; retry
flag); retry generation (request after capture wakes; request before capture doesn't skip; use
short `tokio::time::timeout`s like `shutdown.rs`). Agent: `SenderProgress` take/elapsed.

**Acceptance.** After a stable session ≥ 20 s, the first reconnect waits 1 s. A session failing
5 s after each connect waits 1, 2, 4, 8, 15 s. The overlay shows the reason, a live elapsed counter,
and a countdown. Retry now reconnects within ~1 s. Pulling the viewer's network shows "Network
connection lost"; stopping the agent service shows "restarting or offline".

**Validation.** Mac: Rust checks; macOS overlay and Retry now (toggle Wi-Fi mid-session).
Endpoint: native clippy/tests; Windows overlay and the owned-popup button (probe, since live video
isn't possible there). Install the agent (session.rs/transport.rs change), confirm it connects,
restart the service mid-session from a macOS viewer, and check the reason and the reset backoff.

**Notes.**

---

## 3.16a Audio: send only when unmuted, stereo downmix, Opus

**Depends on 3.16b** (moves the video pacer) **and 3.17** (adjacent `control.rs` hunks). Can be
split into **a1** (enable message, downmix, persisted mute — no new dependencies, most of the
bandwidth win) and **a2** (Opus). If a1 defines the message with a `formats` field, a2 needs no
protocol change.

**Problem.** The agent captures and sends raw 16-bit PCM at the device's native rate and channel
count whenever it's on the console session (`agent/src/remote/transport.rs` ~343–391,
`crates/audio/src/capture.rs:65-77`) — about 1.5 Mbps for 48 kHz stereo, ~6 Mbps for 7.1 — even
though the viewer starts muted (`crates/audio/src/lib.rs:56`) and mute is local only. That exceeds
the whole Ultra data saver budget (1 Mbps). The existing `Resampler` downmix (`index % channels`)
sends centre to L only and LFE to R.

How compatibility works today: `PROTOCOL_VERSION` is unused; compatibility comes from appending
`SessionMessage` variants (postcard tags pinned by tests; last is `CredentialState`, tag 38),
opt-in messages, and separate channel labels (the viewer ignores unknown channel labels). Unknown
tags are logged and discarded by both peers.

**Design.**
1. **Protocol** (`agent/protocol/src/control.rs`), appended after `CredentialState`:
   `SetAudio { enabled: bool, formats: Vec<AudioFormat> }` (tag 39) and append-only
   `AudioFormat { Pcm16, Opus }`. New channel `OPUS_CHANNEL = "meshrmm-audio-opus-v1"`,
   `OPUS_PROTOCOL = "meshrmm.audio.opus.v1"` (constants in `crates/audio/src/lib.rs`); packet
   `[seq: u16 LE][one Opus packet]`, 48 kHz stereo 20 ms fixed by the protocol id. The agent creates
   both audio channels up front (it's the offerer and can't know the viewer version yet).
2. **Compatibility:**
   - New viewer / new agent: viewer sends `SetAudio` **before** `ViewerCapabilities`; agent
     captures only while enabled and uses Opus.
   - Old viewer / new agent: `ViewerCapabilities` with no prior `SetAudio` → Legacy (always
     capture, PCM on v1, now downmixed to stereo). A 3 s timer after the control channel opens is a
     backstop for pre-`ViewerCapabilities` viewers.
   - New viewer / old agent: old agent logs a warning per toggle and keeps sending PCM; the new
     viewer keeps the PCM path.
   - The agent's `on_message` updates a `tokio::sync::watch<AudioMode>` synchronously
     (`Undetermined | Legacy | Off | Pcm | Opus`); control messages are ordered, so no races. Keep
     the mode logic a pure function in a `cfg(any(windows, test))` module so it's tested on macOS.
3. **Agent capture/sender:** add `mode.changed()` to the capture `select!` so enabling starts
   immediately; drop the stream (stops WASAPI loopback) when Off/Undetermined; keep the console
   check. Opus mode feeds PCM into an encoder and sends on the Opus channel; Legacy/Pcm as today.
   Replace the fixed `< 32_000` buffered threshold with ~150 ms of the active rate. Reset the
   encoder when leaving Opus mode or after a > 100 ms gap.
4. **Downmix** in `capture.rs` `input()` for all modes (mono stays mono), using the WASAPI order
   FL, FR, FC, LFE, BL, BR, SL, SR: L = FL + 0.707·FC + 0.707·(BL|SL), R = FR + 0.707·FC +
   0.707·(BR|SR), LFE dropped, normalised and clamped; fall back to even→L / odd→R for unknown
   layouts. Fix the `Resampler` downmix with the same helper. Recompute the header's channel count.
5. **Opus** (`crates/audio/src/opus.rs`, Windows and macOS): decode packet → `Resampler::convert`
   to 48 kHz stereo → 960-frame chunks → encode. Settings: application Audio, 96 kbps constrained
   VBR, complexity 5, in-band FEC, `packet_loss_perc` 5. Viewer playback (`playback.rs`): queue
   `Incoming::{Pcm, Opus}` tagged with the mute generation; the playback thread owns a decoder;
   gap ≤ 5 frames → PLC/FEC, larger → reset; share the resample and 100 ms latency-bound path.
6. **Viewer:** `receiver.rs` ~511–519 adds an `OPUS_CHANNEL` arm; `control.rs` ~368–383 sends
   `SetAudio { enabled: !muted, formats: vec![Opus, Pcm16] }` before `ViewerCapabilities`;
   `platform.rs` `toggle_audio` toggles, persists, and sends `SetAudio` (covers the Windows settings
   toggle and the macOS menu).
7. **Persisted mute:** `preferences.rs` gains `audio_muted: bool` (default `true`, today's
   behavior; `#[serde(default)]` handles old files); `PlaybackState::new(muted)`; `main.rs` builds
   the resume state from the preference. Keep `ViewerResumeState::default()` muted so tests never
   read the user's file.
8. **Pacer:** audio sender publishes its nominal rate in an `Arc<AtomicU32>` (0 off, ~110 kbps
   Opus, rate×channels×16 PCM); the pacer (moved to `bitrate.rs` by 3.16b) uses
   `ceiling.saturating_sub(audio).max(ceiling / 2)`. Don't add audio to `AdaptiveBitrate`.

**Decision needed before a2 — Opus crate** (versions from planning research; verify on crates.io):
- **`opus` 0.4 over `opusic-sys` (recommended):** maintained, current libopus with SIMD; the
  `bundled` feature builds libopus via the `cmake` crate, so **CMake is required**. GitHub
  runners and the Mac have it; on DESKTOP-85R6S28 it exists only inside VS BuildTools and is not on
  `PATH` — update `scripts/build-agent.ps1` and `scripts/build-remote.ps1` to locate it with
  `vswhere` and set `$env:CMAKE`, and document it in AGENTS.md prerequisites. Verify one macOS
  x86_64 cross build.
- **`unsafe-libopus` (fallback):** pure Rust transpile of libopus 1.3.1, no C toolchain, but older,
  slower (no SIMD), and thinly maintained.
- Either way libopus is BSD-3-Clause: add a third-party notices file for the agent, the Windows
  viewer, and the macOS app bundle (the repo has none today).

**Edge cases.** Unmute during UAC/lock/background/RDP: capture stays off until console, mode
persists. Rapid toggles: the watch keeps the latest; the viewer's generation discards stale audio.
Reconnect: `SetAudio` is resent with each connection's preferences. Default device changes: the
existing `healthy()` restart; reset resampler and encoder on a rate change. 44.1/96 kHz capture is
resampled to 48 kHz.

**Tests.** Protocol: `SetAudio` round trip, `encode()[0] == 39`, `AudioFormat` tags pinned,
existing tag tests unchanged. Audio: downmix for 1, 2, 4, 6, 8 channels (centre reaches L and R
equally; LFE excluded); Opus round trip SNR with uneven 10 ms input; PLC on a gap; decoder reset on
a large gap; `Packet::decode` still rejects malformed input. Agent: mode transitions
(Undetermined → Legacy on `ViewerCapabilities` without `SetAudio`; `SetAudio{false}` → Off;
`{true,[Opus]}` → Opus; `{true,[Pcm16]}` → Pcm). Pacer: audio budget lowers the rate, never below
ceiling/2. Preferences: `audio_muted` defaults true, persists, old files keep the default.

**Acceptance.** Muted viewer: the agent logs no "system audio capture started" and
`log_network_stats` shows no audio bytes. Unmute: audible within ~300 ms. New/new negotiates Opus
at ≤ ~100 kbps (log format and bitrate). Old viewer + new agent still gets stereo PCM; new viewer +
old agent still plays PCM. Mute persists across viewer restarts on both platforms. Clippy and
tests pass on the Mac and the endpoint.

**Validation.** Build and install the agent on the endpoint. From the macOS viewer: toggle mute and
check capture start/stop in the service log; compare `bytes_sent` deltas muted vs unmuted; play
audio on the console; lock/unlock and UAC. Run a previous release viewer against the new agent.
Build the Windows viewer on the endpoint (Opus decode must build there).

**Notes.**

---

## 3.16b HEVC bitrate adaptation via a restart ladder

**Problem.** `agent/src/remote/platform.rs` ~320–327 skips every live bitrate change for H.265
(`"skipping an unsafe live HEVC bitrate adjustment"`), so the preferred codec never adapts. The
reason (commit `69a1f62`, comment at `transport.rs` ~1233–1238): several hardware HEVC MFTs accept
the CodecAPI call and then terminate asynchronously on the next frame, which
`runtime_bitrate_disabled` can't catch.

Related bug to fix here: for HEVC, `AdaptiveBitrate` (~127–230) keeps lowering its `current` value
even though the encoder never changes, and `congested_bytes()`/`drain_bytes()` derive from that
fictitious value — so under congestion HEVC drops frames and requests keyframes *more*
aggressively.

**Choice: restart the H.265 encoder one step lower** rather than fall back to H.264. The static
bitrate restart is the documented safe path (the Quality handler, ~1246–1312); HEVC's efficiency
matters most at low bitrates; it keeps 4:4:4; macOS resets in place when codec and pixel format are
unchanged (an H.264 fallback would force a new macOS window); and H.264 decode isn't guaranteed
(the endpoint has none). Cost: ~230–320 ms to first frame plus one IDR per step, kept rare by
hysteresis.

**Design.**
1. **First commit:** move `AdaptiveBitrate`, `VideoPacer`, and `bitrate_duration_bytes` with their
   tests to new `agent/src/remote/bitrate.rs` (`#[cfg(any(windows, test))]`). Tell 3.16a the new
   pacer location.
2. **`RestartLadder`** (pure, µs times): steps at ceiling × 1.0, 0.7, 0.5, 0.35, 0.25, never below
   AIMD's minimum (`max(C/8, 500 kbps)`).
   - Down one step: congested continuously ≥ 2.5 s (the AIMD predicate on the *actual* encoder
     bitrate) and ≥ 5 s since the last restart.
   - Up one step: healthy (buffered ≤ drain and ≤ 1 queued frame) for a hold that starts at 20 s,
     doubles (to 120 s max) if an up-step congests within 10 s, and resets after 60 s stable.
   - `set_maximum` (quality change) resets to step 0.
   - No up-steps while recording (each restart starts a new recording part); down-steps allowed.
3. **Shared state** set after every successful capture start: `encoder_bitrate: Arc<AtomicU32>`
   and `live_bitrate: Arc<AtomicBool>` (`codec == H264 && streamer.live_bitrate_supported()`; add
   that to `ScreenStreamer` — or just `codec != H265` if exposing `runtime_bitrate_disabled` is
   costly).
4. **Video sender** (~1724–1852): live bitrate → AIMD exactly as today. Otherwise rebase AIMD on the
   actual encoder bitrate when `stream_id` changes (new `AdaptiveBitrate::rebase`), skip AIMD
   `Bitrate` emissions, feed the ladder, and send `ControlCommand::RestartBitrate(step)`.
5. **Capture control** (~1232): keep the HEVC guard on `Bitrate` as defense in depth. New
   `RestartBitrate(v)`: clamp to the quality ceiling; ignore if equal to the current bitrate or
   capture isn't running; `set_bitrate`, `stop`, clear the slot, bump `stream_id`,
   `start_first_profile`, send `DisplayConfiguration`, log old/new rate. **Don't propagate a start
   failure with `?`** — set `capture_running = false` and `capture_retry_after = now` so the
   existing desktop-interval recovery retries; a congestion step must never end the session. Add
   the command to the forwarding arm (~852–857).
6. No new `SessionMessage` variants.

**Edge cases.** Later restarts (display switch, UAC/lock, cursor capture) keep the stepped-down
bitrate. If HEVC fails to restart and `start_first_profile` falls back to H.264, `live_bitrate`
becomes true and AIMD takes over. Before 3.17 merges, each step recreates the Windows viewer window.

**Tests** (`bitrate.rs`, run on the Mac and endpoint): no step on < 2.5 s congestion; one step
after 2.5 s; no second step before 5 s; stops at the floor; up-step after the hold; hold doubles
after a failed up-step; ceiling change resets; no up-step while recording; `rebase` keeps thresholds
proportional. Existing AIMD and pacer tests move unchanged.

**Acceptance.** Throttled HEVC link: at most one step-down per ≥ 5 s, each followed by a
`DisplayConfiguration` with a lower bitrate; dropped-frame and keyframe-request rates fall; the
macOS viewer logs an in-place reset with no new window; after removing the throttle it steps back
up without oscillating. H.264 unchanged. No "skipping an unsafe live HEVC bitrate adjustment" spam.

**Validation.** Install the agent on the endpoint (its RTX 3080 has hardware HEVC encode). Connect
from the macOS viewer (Apple silicon HEVC decode). Throttle with macOS Network Link Conditioner
(e.g. 3 Mbps / 50 ms / 1% loss) or on the endpoint with `New-NetQosPolicy
-AppPathNameMatchCondition … -ThrottleRateActionBitsPerSecond` — remove the policy afterwards.
Check step timing in the service log, then remove the throttle and confirm step-up.

**Notes.**

---

## 3.17 Windows viewer resets the stream in place

**Problem.** `remote/src/transport/control.rs:352-353`
(`#[cfg(not(target_os = "macos"))] let reset_in_place = false;`) sends every
`DisplayConfiguration` to `Presenter::start` (~391–423), which creates a new worker thread, COM/MF
runtime, D3D device, **top-level window** (`window.rs:478-659`: toolbar, settings, chat popup),
swap chain, and decoder. Focus is lost, the keyboard hook is reinstalled, popups are rebuilt, held
input is released. Triggers: F8/display combo, quality or chroma change, profile fallback, capture
restart after UAC or lock, and 3.16b's steps. macOS already resets in place.

**macOS contract to mirror** (`macos/presenter.rs:167-208`, `549-575`): `reset_stream(format,
display, displays) -> anyhow::Result<()>`; set `resetting` and `recovering`; clear the queue (count
as replaced); run on the UI thread with a 5 s timeout — flush the decoder but keep the last frame,
update the title, reconfigure input; clear `resetting` (`recovering` stays until a keyframe;
`publish` drops frames while resetting). The caller (`control.rs` ~317–351) updates
`ActivePresenter`, then sends `RequestKeyframe` and the resumed `SelectDisplay`; on `Err` it falls
back to `Presenter::start`.

**Windows threading.** One `meshrmm-decode-present` thread (`windows.rs:109-122`, `run_worker`
~234–368) owns COM, MF, the D3D device, the window, the swap chain, and the async MFT, and pumps
window messages itself. The reset must be handed to that thread and handled in its loop — never
from a tokio thread and never inside the window procedure (re-entrant `RefCell` borrows).

**Design.**
1. `platform/windows.rs`: `Shared` gains `resetting: AtomicBool` and
   `reset: Mutex<Option<PendingReset { format, display, displays, reply: SyncSender<Result> }>>`.
   `Presenter::reset_stream`: `Err` if not running; set `resetting`/`recovering`, clear the queue,
   store the reset, `notify_all`, `recv_timeout(5 s)`; on timeout `take()` the slot back so a stale
   reset is never applied; clear `resetting`. `publish` drops frames while resetting. `run_worker`
   takes a pending reset after the message pump, runs `pipeline.reset_stream`, replies, and clears
   `decoder_blocked_since`; extend the `wait_timeout_while` predicate (~335) to wake on a pending
   reset. Add `Presenter::can_reset_in_place(current, next) -> bool` (Windows: always true; move the
   macOS predicate from `control.rs:22-30` onto its presenter) so `control.rs` needs no `cfg`.
2. `pipeline.rs` `WorkerPipeline`: store the `ID3D11Device`. `reset_stream` does fallible steps
   first: `HardwareDecoder::new(&device, format)?` (always recreate — cheap, and it clears pending
   metadata/dimensions/codec; on failure nothing changed and the caller's fallback reaches
   `VideoProfileRejected` as today), then `window::reset_stream`, then `renderer.reset_stream`, then
   swap in the decoder. Optional: if creation fails with the old decoder alive (single-instance
   MFTs), drop the old one and retry once.
3. `renderer.rs`: split the video-processor setup (~66–142) into `configure_stream(format)`, used
   by `new` and `reset_stream`. Recreate the processor only when width, height, fps, or pixel
   format change. Dimensions changed → `last_frame = None` (the flip swap chain keeps showing the
   last image until the keyframe). Then `configure_output(layout)`, releasing `output_view` first.
4. `window.rs` `WindowContext` (~128–148): make `video_width`/`video_height` `Cell<u32>` and
   `active_display`/`displays` `RefCell`s with cloning accessors (no borrow held across
   `SendMessageW`); update users in `window/input.rs`, `toolbar.rs`, `settings.rs`. New
   `window::reset_stream`: if the display id changes, `release_input()` **first** (key-ups carry the
   old id); update fields and title (respect the reconnecting suffix); repopulate the user and
   display combos (extract `populate_session_controls` from `create_toolbar` ~538–575, reuse in
   `set_agent_pointer_display`); re-apply quality and chroma; `resize_pending(true)`. Leave the
   window, placement, DPI, settings, chat popup, debug overlay, recording indicator, and keyboard
   hook untouched.
5. `control.rs`: remove the `cfg` split; use `Presenter::can_reset_in_place`; platform-neutral log
   text ("reconfigured the video decoder in place …").

**Edge cases.** Size change (F8, All monitors, portrait): processor recreated, letterbox and pointer
mapping use the new dimensions immediately. Codec/chroma change: new decoder on the same device, or
fall back. UAC/lock restart: combos rebuilt. A modal MessageBox on the worker (maintenance error,
recording notice) blocks the loop — the reset times out after 5 s and falls back (same as today;
document it). Minimized window: recreate the processor, configure output on the next resize.
Reconnect still opens a new window (the kept window's sink points at the old connection; out of
scope). Recording parts follow `stream_id`, unaffected.

**Tests.** Pure `ResetPlan::between(old_format, new_format, old_display, new_display) ->
{ recreate_processor, drop_last_frame, display_changed }` without `cfg`. A Windows test in
`control.rs` next to the macOS one: resets allowed across resolution, codec, and chroma. An
`#[ignore]` interactive probe (below).

**Acceptance.** F8/combo switch, quality or chroma change, lock/unlock, and UAC keep the same
top-level HWND; settings and chat popups keep their contents; focus stays; the keyboard hook stays
(Win and Alt+Tab still reach the device after a switch); title, combos, and pointer mapping follow
the new display. Log shows "reconfigured without replacing its window", not "replacing the active
presenter". A failed decoder for a new profile still reaches `VideoProfileRejected`. macOS
unchanged.

**Validation.** The endpoint has no hardware H.264 decoder, so:
1. Commit an `#[ignore]` probe in `remote/src/platform/windows/` and run it from the endpoint's
   interactive desktop: build the window and `D3d11Renderer` with a synthetic NV12 texture
   (make decoder construction a separate step in `WorkerPipeline` to allow this), present, reset
   with different dimensions, display lists, and chroma, and assert that the HWND and child/popup
   HWNDs are unchanged (`IsWindow`), the combo count and title updated, a new-size synthetic frame
   presents, `PrintWindow` shows the toolbar and chat, and posted mouse moves map the corners to 0
   and 65535. Also assert that a reset to a codec with no hardware MFT returns `Err`.
2. End to end needs a second Windows machine whose GPU registers hardware MFT decoders, connected
   to the endpoint's installed agent (first run `supported_video_profiles` on the endpoint to
   confirm HEVC decode is also missing). Exercise F8, All monitors, portrait, quality/chroma,
   lock/unlock, UAC, and (after 3.16b) HEVC step restarts. If no second machine is available, mark
   end-to-end validation incomplete.
3. macOS regression session.
4. Viewer-only change: no agent reinstall needed.

**Notes.** Implemented in `5ce8cc7`. The Windows presenter hands resets to its decode/present thread through
a pending-reset slot (5 s timeout; a timed-out reset is taken back so it is never applied late;
frames are dropped meanwhile) and creates the new decoder and video processor before touching the
window, so any failure changes nothing and falls back to `Presenter::start` — an undecodable
profile still reaches `VideoProfileRejected`. `Presenter::can_reset_in_place` exists on both
platforms (the macOS predicate moved onto its presenter) and `control.rs` has no `cfg` split. The
pure `ResetPlan` lives in `remote/src/stream_reset.rs`; the probe is
`remote/src/platform/windows/reset_probe.rs` (`#[ignore]`).

Deviations:
1. The reset re-selects quality and chroma in the toolbar controls without sending them: the Agent
   answers every `SetQuality`, even an unchanged one, with a new `DisplayConfiguration`, so
   resending would loop resets.
2. Related finding, not fixed: `create_window` still sends `SetQuality` and `SetChroma` on every
   new window. By code reading, before 3.17 that echo recreated the window a second time; now it
   causes one harmless extra in-place reset. Follow-up: drop those sends (the capabilities
   message already carries both).
3. No `resize_pending(true)`: the renderer resizes and redraws during the reset, and a minimized
   window uses the swap chain's current size.
4. The last frame is also dropped on a chroma change (a 4:2:0 frame can't feed a 4:4:4
   processor), not only on a size change.
5. The renderer creates its new processor before the window is touched, so a processor failure
   also leaves everything unchanged. The optional "drop the old decoder and retry" was skipped (the
   old path also had two decoders alive at once).
6. `WorkerPipeline` is now a decoder plus a new `Presentation` (window, renderer, device,
   runtimes), so the probe runs without a decoder. Teardown order is now decoder → window/renderer
   → device → Media Foundation → COM (before, COM and MF shut down first).
7. The display-list pointer marker is stored in the window so rebuilt lists keep it.

Validation. macOS development host, on the rebased tree: `cargo fmt --check`, `cargo clippy -D
warnings`, `cargo test --workspace` pass (the macOS reset rule test included). DESKTOP-85R6S28:
the task thread's pre-rebase tree (`681cfa7`, manifest of 412 files matched) passed native fmt,
clippy and workspace tests, and the ignored `reset_probe` passed in console session 1: the same
top-level, child and popup HWNDs, keyboard focus, chat transcript and unsent draft through a display
switch, portrait, 4:4:4 and minimize/restore; title and combos followed the new display; pointer
corners mapped to 0/65535 after each reset; a new-size frame presented; `PrintWindow` showed the
toolbar and chat; quality/chroma were never resent; all four profiles were refused with nothing
changed. After rebasing onto 0.2, the rebased tree (`5ce8cc7`, 429 files matched by SHA-256
manifest, in `~\ux-2026-09-26-3.17\rebased-5ce8cc7`) passed native fmt, clippy and `cargo test
--workspace` (remote: 82 passed, 2 ignored). The endpoint's RTX 3080 registers no H.264 or HEVC
hardware decoder MFTs (the profile check returned only the always-added H.264 4:2:0 profile). No
agent install; the `meshrmm:` handler was not changed by this task.

**Incomplete:** end-to-end validation (F8, All monitors, portrait, quality/chroma, lock/unlock,
UAC, HEVC step restarts) needs a second Windows machine with hardware decoders; none was available.
The keyboard hook and Win/Alt+Tab after a switch were checked by code reading only (the reset never
touches the hook and focus doesn't change). No live macOS regression session (unit tests only).

---

## Out of scope (found while planning)

- The dashboard's SSR HTML preloads Geist fonts from a local filesystem path
  (`/Users/gccody/Code/MeshRMM/dashboard/.vinext/fonts/…woff2`, seen in `dist/server/index.js`).
  In production that 404s and leaks a local path. Check whether release builds are affected; file
  separately.
- Keeping the agent's streamer alive across a same-session resume (see 3.14, Deferred).
- Skipping the WorkOS refresh on `/auth/session` while the access token is valid (see 2.10b).

## Endpoint state

At plan creation (2026-09-26), DESKTOP-85R6S28 is as the 2026-09-23 plan's **Endpoint state**
section left it. Record every agent install or link-handler change here: task, build/commit,
SHA-256, backup file name, and whether it was restored.

- **0.2 (2026-09-26).** Before: installed agent SHA-256 `6BF0C0296F87C5719B99670137B3DC6C0A14E6CE851B79A1FED4E1FA58DD4D60`
  (release 0.3.1, package 0.2.0). 12:15 install: release build of `7209a81` (pre-rebase), SHA-256
  `D017CA5AED06FC349978ED8D6926E4215D8A30FB928546546431E4CDBD643713`, backup
  `meshrmm-agent.exe.before-local-20260926-121512`. 12:15 restore: `6BF0C029…4D60` reinstalled with
  the install script, backup `meshrmm-agent.exe.before-local-20260926-121536` (the 0.2 build);
  service running and connected; install lock released. The viewer tests re-registered the
  `meshrmm:` handler to `~\ux-2026-09-26-0.2\target\debug\meshrmm-remote.exe`; it was restored to
  `"C:\Users\gccody\audit-fixes-2026-09-23\audit-builds\viewer-8b3980a\meshrmm-remote.exe" -- "%1"`
  and confirmed after the review round. Work tree: `~\ux-2026-09-26-0.2`. Every install/restore is
  also logged in `~\ux-2026-09-26-endpoint-log.txt`.
