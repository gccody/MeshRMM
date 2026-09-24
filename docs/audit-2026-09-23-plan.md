# Audit fix plan — 2026-09-23

Tracks the fixes from the 2026-09-23 audit, which was made after the fixes in
[audit-fixes.md](audit-fixes.md). Work happens on branch `audit-fixes-2026-09-23`, cut from
`main` at `a237bb5`. Tasks go in priority order, and each one is committed before the next
starts. Line numbers in the task descriptions come from the audit and may have drifted.

## Status

| Task | Priority | Status | Commit(s) |
| --- | --- | --- | --- |
| T01 Installer: secure ProgramData dirs + uninstall-helper staging (LPE) | High | Done | `5618b00` |
| T02 macOS viewer: minimize ends session | High | Done | `2eb9bde` |
| T03 macOS viewer: modal alert while UI borrowed → abort | High | Done | `b7ba5bd` |
| T04 Windows QPC → µs overflow after ~21 days | High | Done | `bfb6f3c` |
| T04b Installer: legacy config trust + non-environment system paths | High | Done | `4debd96` |
| T05 Updater failure paths / restart loop / exe ACL | Medium | Done | `eb52c2c` |
| T05b Uninstall helper self-delete + viewer updater race | Medium | Done | `a5d39e4`, `1265e1c` |
| T06 Defer updates during sessions; graceful worker stop | Medium | Done | `662ecb1` |
| T07 Session-close actions bound to viewed user/session | Medium | Done | `c926dd9` |
| T08 Task Manager protection names / own processes | Medium | Done | `0159bd8` |
| T09 Capture-helper IPC hardening | Medium | Done | `30e0836` |
| T10 Agent logging flush + write-error recovery | Medium | Done | `de94bca` |
| T11 Background console `exit` cleanup | Medium | Done | `6178050` |
| T12 Windows viewer pointer mapping / DPI / letterbox | Medium | Done | `2bd0db6`, `8e95162` |
| T13 Relaunch replaces viewer without ending old session | Medium | Done | `233a875` |
| T14 macOS viewer keyboard (Cmd→Win, ISO/JIS, stuck Shift) | Medium | Done | `871e913` |
| T15 Windows viewer GUI subsystem + friendly errors (+ deep-link registration) | Medium | Done | `f8cb85d` |
| T16 Reconnect UX + recordings + wallpaper setting persistence | Medium | Done | `5a9b6bf` |
| T17 File transfer roles, limits, cache cleanup | Medium | Done | `66723ae`, `1d75e02` |
| T18 Quarantine / Mark-of-the-Web on received files | Medium | Done | `a048609` |
| T19 Server auth error mapping + JWKS cache | Medium | Done | `aba76c6` |
| T20 Company status transition guards | Medium | Done | `8e29720` |
| T21 Suspension fan-out resilience | Medium | Done | `92a395b` |
| T22 Presence failure must not block agent connect | Medium | Done | `bc5684d` |
| T23 Coordinator reconnect restarts healthy session | Medium | Done | `e5a50b2` |
| T24 `npm run deploy` wipes production downloads | Medium | Done | `b84c249` |
| T25 Release workflow branch restriction | Medium | Done | `f9e4582` |
| T26 Dashboard account-load retry + sign-out handling | Medium | Done | `094a4e9` |
| T27 Platform console error handling | Medium | Done | `0546992` |
| T28 Remove `dashboard/pulsermm-site.tar.gz` | Low | Done | `00d43c3` |
| T29 CI hardening | Low | Done | `d8307fa` |
| T30 Documentation accuracy / TLS 1.3 | Low | Done | `1297f0f` |
| T31 Token rotation edge cases | Low | Done | `f95d930` |
| T32 File transfer sliding window | Low | Done | `21316da` |
| T39 Native log rotation (+ viewer update helper logging) | Low | Done | `797f7a5` |
| T33 Local dashboard dev loop | QoL | Done | `c67249f` |
| T34 Server deploy script + schema version in /healthz | QoL | Done | `534ee4d` |
| T35 Viewer input QoL (Win/Alt+Tab hook, rebindable keys) | QoL | **Next** | |
| T36 Agent refactor: split large files, dedupe launch code | QoL | Todo | |
| T37 Viewer refactor: split window.rs / transport.rs | QoL | Todo | |
| T38 Dashboard refactor: split dashboard.tsx, modal a11y | QoL | Todo | |

## Working rules

- Stay on `audit-fixes-2026-09-23`. Don't deploy, publish releases, bump versions, run
  remote D1 migrations, or rewrite history.
- Before changing code, read the code paths involved. If a finding turns out to be wrong,
  record why in this file instead of changing code.
- Match the surrounding style. Add tests for the behavior you change.
- Mac checks: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`. For server changes, also run
  `cargo check -p meshrmm-server --target wasm32-unknown-unknown` and
  `python3 server/tests/sql_regressions.py`. For dashboard changes, run `cd dashboard && npm run verify`.
- Windows-only code does not compile under macOS clippy. Follow [AGENTS.md](../AGENTS.md):
  1. Sync the tree to `~\audit-fixes-2026-09-23` on DESKTOP-85R6S28 and verify it with a SHA-256 manifest.
  2. Run native fmt, clippy and tests there, checking `$LASTEXITCODE` after each.
  3. When installed-agent behavior changes, back up the exe, install, confirm the service starts
     and connects, exercise the change, and check the logs. Leave a working service.
- Commit each task (conventional-commit subject) before starting the next, then update the status
  table and notes here.

## Endpoint state

After T39, DESKTOP-85R6S28 runs a release build of `797f7a5` (SHA-256 `EF774B2A…77ED`), which
includes T17, T18, T23, T30, T32 and T39. The service is running and connected; the build it
replaced (`21316da`, SHA-256 `75508B37…ECD0`) is kept as
`meshrmm-agent.exe.before-local-20260924-164940`. Its first start rotated the 72.6 MB Agent log to
`agent.1.log`, which will age out after three more rotations.
T19–T22, T24–T29 and T31 did not change the Agent. Earlier builds are kept as
`C:\Program Files\MeshRMM\Agent\meshrmm-agent.exe.before-local-*`. T01 removed the explicit
`gccody` permission on `ProgramData\MeshRMM\Agent`, so a non-elevated `gccody` process can no longer
read that folder. `updates\local-3273ef617bd940a68963ee6b2d11b6ad` came from an earlier session and was
left untouched.

## Validation gaps in completed tasks

These need a live dashboard → viewer → agent session, which the agent-driven runs could not create:

- T02 and T03: minimize/restore/close, and the "Recording saved" and maintenance-error alerts in a live macOS session.
- T04: actual behavior after more than 21 days of uptime. Unit tests cover the conversion.
- T04b: the updater's directory check during a real update, and a real PulseRMM legacy install.
- T05: the forced-termination path for an old instance that hangs.
- T05b: a real uninstall (re-enrolling afterwards needs a dashboard token), and a real viewer self-update from a published manifest.
- T06: update deferral while a session is live, lock or clear-clipboard on service stop, stopping
  with a staged update, and system shutdown.
- T07: a close action in a live session, and a user switch after the viewer leaves. A temporary
  test run as SYSTEM on the endpoint resolved the console's session, logon ID and user.
- T08: ending Agent processes from the Task Manager on the background desktop. Unit tests on the
  endpoint cover the ancestor chain, Agent copies, and the legacy service name.
- T09: helpers started by the installed service during a live session. A temporary probe run as
  SYSTEM on the endpoint started all four helper kinds and shut them down through the new launch
  path. It confirmed that an unrelated inheritable handle reached neither a SYSTEM nor a user-token
  helper, while a `std::process::Command` child did inherit it.
- T10: recovery from a real disk-full or lost log file. Unit tests cover reopening with a backoff;
  stopping the installed service now logs "the Agent service stopped".
- T11: `exit` in a live background session. The ignored `console_exit_closes_window_task_and_input_helper`
  test, run as SYSTEM in Session 0 on the endpoint, passes, and fails when the helper's watcher is
  removed. Not tested: a program started from the console that outlives it keeps the console open.
- T12: a live session. The endpoint has no hardware H.264 decoder, so temporary probes drove the
  real window and D3D11 renderer with a synthetic NV12 frame in the interactive session: resize,
  minimize/restore and a simulated 144-DPI change resized the swap chain without errors, and posted
  mouse moves mapped the letterboxed corners to 0 and 65535 and ignored the bars and the toolbar.
  A real monitor with another DPI was not available. The probes also showed that, before
  `8e95162`, the first presented frame hid every toolbar control, the chat panel and the
  diagnostics overlay; after it, `PrintWindow` captures show all of them.
- T13: a real replaced session. On Windows, two real viewer processes for one device (with an
  unreachable server) showed the second signaling the first and waiting for it to exit; unit
  tests cover takeover, an abandoned mutex and a timeout. The dashboard adds the device ID to the
  link, so Windows links from an undeployed dashboard keep the old behavior. macOS replacement
  was not exercised. A server-side "takeover by the same user" was not added.
- T14: real key events. Unit tests cover the Command state machine, per-side modifiers, ISO and
  JIS mapping; the key-up monitor for Command combinations was not observed with a keyboard.
- T15: a real 401/403/404/409 from the server (unit tests cover the messages). On the endpoint,
  the build is a GUI-subsystem executable, an unreachable server and an injected option each showed
  their message box, `--identity-fingerprint` still printed through a pipe, and the handler was
  registered as `"exe" -- "%1"`; the previous handler value was restored afterwards.
- T16: a live reconnect on either platform, and a recording across one. On Windows, a probe showed
  the "Reconnecting…" popup centered over the video, the title suffix, and new windows taking the
  previous window's position, size and maximized state. The macOS label, Quit handling and frame
  reuse are untested beyond compiling and the unit tests.
- T17: a live transfer through the installed service. A temporary probe on the endpoint ran a
  viewer-role and an agent-role worker against each other in the interactive session: a second
  Documents copy got a new name instead of replacing the first, the viewer refused an unrequested
  Documents transfer and a drop and ignored the peer's pick request, an explicit clipboard copy
  reached the Windows clipboard, and a 513 MiB clipboard copy was refused before any data was sent.
  The probe could not script the agent's real file picker, so the Receive-files path is covered by
  unit tests only. A clipboard copy over the limit made on the device is only logged there: the
  protocol has no message that would tell the viewer.
- T18: Gatekeeper and SmartScreen prompts for a received app or installer. Unit tests read the
  `com.apple.quarantine` attribute (`0081;…;MeshRMM;<uuid>`) on macOS and the `Zone.Identifier`
  stream (`ZoneId=3`) on Windows.
- T19: a real WorkOS or D1 outage. The Miniflare suite covers one JWKS fetch across requests, no
  refetch for unknown key IDs within 30 seconds, and a D1 failure returning 503 with `Retry-After`.
  The dashboard shows a 503 as an error without locking; retrying the account load is T26. The
  wasm32 check ran on the Mac with rustup's toolchain: Homebrew's `cargo` comes first in `PATH`
  there and has no wasm32 target.
- T20: provisioning against WorkOS. SQL regressions cover every status.
- T21: a real suspension. The Miniflare suite covers a failing coordinator during suspension and a
  coordinator revoking its Agent at the next request after a missed revocation. The coordinator
  does not re-check on a timer, because hibernated Agent sockets wake it for nothing else and the
  suite deliberately forbids a recurring alarm. An idle Agent that missed its revocation stays
  connected until its next session request, presence alarm or reconnect; every route that could
  send it work already refuses a suspended company.

- T22: a real presence outage. The Miniflare suite covers a reconnect while publishing to the
  company's presence fails: the socket is accepted, the live session is resumed on it, the
  publication stays in the outbox with an alarm, and the alarm delivers it.
- T23: a real coordinator reconnect during a live session. The Miniflare suite shows the replay
  after a lease renewal is byte-for-byte the request the Agent received (it fails without the
  server change), and Agent unit tests, also run on the endpoint, cover the comparison. The
  installed service started and connected with the new build. The Agent compares whole requests
  except the expiry rather than only the session ID and token, because a resume sends new TURN
  credentials and must still restart the session, as before.
- T24: the release workflow's deploy. `npm run predeploy` was run in both modes on the Mac; deploy
  itself was not run. The guard also refuses unexpected files in `public/downloads/` and URLs off
  `download_origin`. `scripts/verify-dashboard-deploy.test.mjs` is not yet run in CI (T29).
- T25: a real manual run. The YAML parses, and the new step's download and check were run on the
  Mac against the live manifest (0.2.8, passed).
- T26: a live dashboard. The local dev loop cannot create a company session yet (T33); unit tests
  cover the retry policy and `npm run verify` passes.
- T27: a live platform console (the same T33 limit). Unit tests cover which failures refuse owner
  access and which error a reload keeps; `npm run verify` passes.
- T28: nothing left. `git check-ignore` confirmed the new patterns match a `*-site.tar.gz` archive
  at the root and in the dashboard, and a `__pycache__` directory.
- T29: a real CI run. Both workflows parse with js-yaml and Ruby, every action is pinned to a
  SHA resolved with `git ls-remote`, `cargo metadata --locked` passes, and
  `node --test scripts/*.test.mjs` passes locally. The concurrency group includes the workflow
  name so the release workflow's reusable CI call never shares a group with, and is cancelled
  alongside, the push-triggered CI run for the same commit on main.
- T30: the Agent's enrollment request (needs an installer token) and the viewers' update checks
  against production. Handshake tests against local TLS 1.2-only and 1.3-only servers pass on
  the Mac and the endpoint for the WebSocket, ureq and reqwest clients; the probe script showed
  all public hosts reject TLS 1.2; temporary Mac probes completed TLS 1.3 handshakes with
  `api.meshrmm.com` (WSS) and `meshrmm.com` (HTTPS). The installed service connected its
  signaling WebSocket and ran its startup update check without an error. The unused
  `WORKOS_REDIRECT_URI` was removed from `wrangler.jsonc` and edited out of the generated
  `worker-configuration.d.ts` by hand, because `wrangler types` also rewrote the runtime types
  for a newer workerd.
- T31: a real rotation. The Miniflare suite (run on the Mac) covers offline refusal, resending
  a pending credential, promotion deleting the plaintext copy, replacing a pending hash that was
  never sent, and not sending one D1 withdrew; it fails against the previous code. A failed
  send cannot be simulated there, so its withdrawal is covered by the SQL regressions only.
- T32: a live transfer over WebRTC. A temporary probe on the endpoint ran a viewer and an agent
  worker against each other through a relay adding 25 ms each way. 8 MiB took 14.7 s (0.54 MiB/s)
  each way with the previous code and 1.1 s (7.3 MiB/s) with the window; both copies matched their
  SHA-256, at most 16 messages were in flight, and an unrequested push still failed at Begin with
  the viewer showing the real reason. A real session will be slower: the file channel keeps at
  most 64 KiB unacknowledged by SCTP (about 96 KiB per round trip with the message in progress),
  which the plan's 1–4 MiB would exceed. All data channels share one SCTP association whose send
  queue is FIFO and capped at 128 KiB, so more buffered file data would queue input behind it.
  The protocol is unchanged; a window of 16 fits the 32-command queue of deployed receivers. The
  installed service started and connected with the new build.
- T39: a log rotating in production after weeks of uptime, and a real viewer self-update on either
  platform. On the endpoint, the installed service's first start rotated its 72.6 MB log, and both
  files kept the inherited SYSTEM and Administrators ACL; the unit test that rotates a log another
  handle holds open passes there too. The viewer's Windows update helper, run by hand with
  stand-in executables, logged its start, the install and launch, and, when the update was
  missing or never reported ready, the restore, the relaunch and the final error. The macOS
  helper's logging and the Mac viewer's 90 MB log (rotated at the next launch) were not exercised.
- T33: a full sign-in, which needs a WorkOS staging environment with `*.localhost` redirect URIs.
  On the Mac, after `npm run dev:seed -- acme`, `npm run dev` served the marketing site, owner
  console and company dashboard at `localhost`, `admin.localhost` and `acme.localhost:3000`
  (`other.localhost` returned 404 and `www.localhost` redirected), and `/v1/account` reached the
  local control plane (401 without a token). Headless Chrome treated `http://acme.localhost` as a
  secure context and kept the `__Host-` sign-in cookie: `/auth/callback` with the issued state got
  as far as WorkOS, which rejected the fake code. The production build contains only the
  dashboard Worker and none of the development settings. The Agent list does not load locally,
  as documented, so T26 and T27 still lack a live check.
- T34: the script against production, which the working rules forbid, including its read-only
  `--dry-run` (it lists remote migrations). Its unit tests cover the step order, the dry run, the
  health URL and the health wait; `wrangler deploy --dry-run` built the Worker on the Mac. Through
  `npm run dev`, `/healthz` on the seeded local D1 answered 200 `ok` and, with the newest row
  removed from `d1_migrations`, 503 `schema_behind`. The Miniflare suites pass with the new build.
  `/healthz` used to return the text `ok`; nothing in the repository read it.

## Remaining tasks

---------------------------------------------------------------------------------------------------
## MEDIUM — Viewer

---------------------------------------------------------------------------------------------------
## MEDIUM — Shared crates

### T17 — File transfer: roles, size limits, cache cleanup
crates/file-transfer/src/lib.rs. (a) Same symmetric worker on both sides: viewer handles peer `Pick`
(:214-224 → native file picker pops on technician's Mac, chosen file uploaded) and any `Begin`
(:249-270) incl. Documents/Drop into ~/Documents/MeshRMM Transferred Files; remote/src/transport.rs:~895
passes every agent FileMessage. Fix: add a Role (Viewer/Agent) to the worker; viewer ignores Pick and
only accepts Documents transfers after its own Pick request; keep agent behavior intact.
(b) No byte/disk limits (:506-513, :528-558): Entry.size any u64, Totals.bytes informational. Fix:
require Totals first and enforce against actual bytes; per-transfer caps (clipboard smaller than
documents); check free space; auto clipboard file sync (:348-362) only below a size threshold (above:
skip with a user-visible notice/log rather than silently streaming GBs).
(c) Cache never deleted (:577-579, :302, windows.rs:559-565, macos.rs:112): sweep stale `.partial-*`
and old cache batches at startup (not currently on clipboard / older than N hours); delete cache batch
after successful commit_documents.
(d) While here: race in exists-check-then-rename (:584-588) can overwrite — use no-replace rename
(renamex_np RENAME_EXCL on macOS, MoveFileExW without REPLACE_EXISTING on Windows) with retry on
collision; complete reserved Windows names (COM0, LPT0, CONIN$, CONOUT$, superscript digits, trimmed
stem) and reject Windows-invalid chars (:453-461); sender reads exactly declared size with take()
(:618-631, :675-688). Add hostile-input tests. Native Windows checks on endpoint.

### T18 — Tag received files with quarantine / Mark-of-the-Web
crates/file-transfer/src/lib.rs:530-538, 577-590. Received files get no com.apple.quarantine xattr (macOS)
or Zone.Identifier ADS (Windows), so a pasted setup.exe/.app skips Gatekeeper/SmartScreen. Fix: tag
files on the receiving side where they land on the technician's machine (viewer) — macOS quarantine
xattr (with agent name "MeshRMM"), Windows Zone.Identifier ZoneId=3. Decide deliberately whether the
agent side (endpoint receiving files from the technician) should also tag (probably yes for consistency;
document the choice). Tests on both platforms (endpoint for Windows).

---------------------------------------------------------------------------------------------------
## MEDIUM — Server (Cloudflare Worker)

### T19 — Auth: outages returned as 401 (locks dashboards); JWKS fetched every request
server/src/infrastructure.rs:363-432, server/src/lib.rs:436-457, 482-489. Any error in
authorize_workos_user maps via workos_auth_error to 401 "sign out and sign in again" — incl. D1 errors,
JWKS network failures, non-2xx. Dashboard locks on any 401 (dashboard/features/workspace/dashboard.tsx:~162).
Fix: typed auth error enum (replace string-matching `detail.contains`), 503 for upstream/D1 failures,
401 only for token failures; cache JWKS (isolate memory with TTL, refetch on unknown kid, bounded).
Check dashboard handles 503 gracefully (retry, no lock). Tests.

### T20 — Company status transitions not guarded (provisioning/retry)
server/src/routes/platform.rs:334-372, 580-596, 851-866. provision_company's final write and
mark_provisioning_failed are unconditional; retry_platform_company accepts any status. Suspended-while-
provisioning → becomes active; Retry on suspended → reactivated; Retry on active hitting WorkOS error →
`failed` (locks users + agents). Fix: retry only when status IN ('provisioning','failed') (409 otherwise);
conditional writes `AND status IN ('provisioning','failed')`. Add SQL regression tests.

### T21 — Suspension revocation fan-out stops at first error
server/src/routes/platform.rs:413-445. Company marked suspended, then each agent coordinator revoked
sequentially with `?`; one failure returns 500 and skips the rest + presence revoke; big companies may hit
subrequest limits. Fix: revoke presence first; continue past errors (collect/log failures); bound
concurrency or use wait_until/batching; have the coordinator re-check company status (e.g. on its alarm
or on next request) so missed revocations self-heal. Tests.

### T22 — Presence publish failure blocks agent connect
server/src/agent_coordinator.rs:83-87. /connect closes the socket 1011 and errors if the CompanyPresence
publish fails, although an outbox + 30s alarm retry exists. Fix: accept the socket, persist the delivery
record, let the alarm retry. Update server/tests/presence.mjs etc. if relevant.

### T23 — Coordinator reconnect restarts a healthy session
server/src/agent_coordinator.rs:88-95, 219-230 vs agent/src/remote/mod.rs:171-180. /lease (viewer every
30s) rewrites expires_at_unix_ms in the stored session request; on agent coordinator reconnect /connect
replays it; the agent compares full-struct equality → mismatch → aborts and restarts the live session.
Fix: agent compares session identity (session_id + signaling_token) instead of whole struct (update the
lease expiry in place), and/or server sends lease expiry separately. Keep backward compat with deployed
server/agents. Tests. Installed-agent validation if agent code changes.

---------------------------------------------------------------------------------------------------
## MEDIUM — Dashboard & release

### T24 — `npm run deploy` from a normal checkout wipes production downloads
docs/company-domains.md:48-57, dashboard/README.md (local dev lists npm run deploy),
dashboard/wrangler.jsonc:8-12, root .gitignore:16-19. public/downloads is gitignored → deploy ships no
installers/update-manifest.json (or local unsigned builds). Fix: `predeploy` guard running
scripts/verify-release-assets.mjs (check its interface) that refuses to deploy without the full asset
set matching release.json; remove `npm run deploy` from the local-dev docs; document the release
workflow as the deploy path. Test the guard (node test). Do NOT run deploy.

### T25 — Release workflow can deploy from any branch via workflow_dispatch
.github/workflows/native-release-build.yml:7, 39-40, 175-179. Add `if: github.ref == 'refs/heads/main'`
(or equivalent) to validate/deploy jobs; for dispatch runs, require version ≥ live/previous manifest
version (fail closed). Keep YAML valid (use a YAML parser to check, e.g. python yaml or node).

### T26 — Dashboard: failed /v1/account load never retried; idle enforcement off
dashboard/features/workspace/dashboard.tsx:186, 218-232, 544. One attempt; on failure Settings shows
"Loading…" forever, inventory never subscribes, idle lock disabled while admin widgets still usable.
Fix: retry with backoff + a visible error + Retry button; don't render management views until policy
loaded (show error state instead). Treat 503 (after T19) as retryable, 401 as lock. Also:
handleSignOut (:~420) doesn't await/catch logout → unhandled rejection, no navigation on 5xx: fix.
Add tests (dashboard/tests).

### T27 — Platform console hides errors and misreads transient failures
dashboard/features/platform/platform-dashboard.tsx:54, 58, 65, 93-95. createCompany's error handler sets
error then calls loadCompanies() which does setError(null) → duplicate-slug error invisible; any
loadCompanies failure sets hasOwnerAccess=false → a 503 shows "Platform owner access required". Fix:
don't clear errors on the follow-up reload; only 401/403 mean no owner access; add role="alert" to the
error banner. Tests where practical.

---------------------------------------------------------------------------------------------------
## LOW

### T28 — Remove stale committed artifact dashboard/pulsermm-site.tar.gz
7.8 MB pre-rename dist build incl. unsigned pulsermm-agent-windows-x64.exe and .openai/hosting.json.
Unreferenced. `git rm` it (do NOT rewrite history). Broaden .gitignore to cover `*-site.tar.gz` (check
existing `/meshrmm-site.tar.gz` pattern and any script that produces it). Add `__pycache__/` to root
.gitignore.

### T29 — CI hardening
.github/workflows/ci.yml (and native-release-build.yml where applicable): cargo commands with `--locked`;
run `node --test scripts/release-config.test.mjs` (and any other scripts/*.test.mjs) in CI; pin actions
to full commit SHAs with a `# vX` comment (resolve SHAs with `git ls-remote https://github.com/<owner>/<repo> refs/tags/<tag>`
— read-only; dereference annotated tags with ^{}); add `concurrency: {group: ci-${{ github.ref }},
cancel-in-progress: true}` for CI (NOT for release deploy unless cancel-in-progress false). Validate YAML.

### T30 — Documentation accuracy
docs/transport-security.md:~8 claims "TLS 1.3 remain enforced" but rustls configs (tokio-tungstenite,
reqwest, ureq) allow TLS 1.2 and WebRTC uses DTLS 1.2. Decide: enforce TLS 1.3 for HTTPS/WSS clients
(with_protocol_versions(&[&TLS13])) if all endpoints (Cloudflare) support it — they do — or correct the
doc. Prefer enforcing for the HTTPS/WSS clients and correcting the DTLS wording. docs/audit-fixes.md
item 12 no longer matches presence design (snapshots no longer query coordinators; see
server/tests/presence.mjs asserting statusCalls == 0) — update. Also fix stale docs: dashboard/README.md
idle timeout location (now Settings → Dashboard security); root README "current stable Rust" vs pinned
rust-toolchain.toml; unused WORKOS_REDIRECT_URI in dashboard/wrangler.jsonc (verify unused first);
scripts/provision-cloudflare.ps1 closing message URL (companies are created at admin.meshrmm.com).
If you change TLS config, run native Windows checks too.

### T31 — Token rotation edge cases
server/src/routes/agents.rs:227-267, agent_coordinator.rs:101-123, lib.rs:420-422. Pending hash written
to D1 before coordinator call; if that errors, pending_auth_token_hash stays set → every later rotation
409 "already pending", no way to clear. Coordinator /rotate-token always 200 → "Agent must be online"
409 branch dead. Plaintext pending_rotation token stays in DO storage forever after promotion. Fix:
deliver first or roll back on failure (or allow re-rotation to replace a stale pending hash after a
timeout); make coordinator return a status reflecting delivery (offline → 409); delete pending_rotation
after promotion. SQL regressions + tests.

### T32 — File transfer throughput: stop-and-wait per 32 KiB chunk
crates/file-transfer/src/lib.rs:387-395, :330. Each chunk waits for an ack → ~320 KB/s at 100ms RTT.
Fix: sliding window (e.g. 1–4 MiB in flight), ack per window/entry; keep backpressure; stay compatible
with the peer protocol version where feasible (if protocol change required, version-gate or make
receiver tolerant). Tests. Native Windows checks.

---------------------------------------------------------------------------------------------------
## QUALITY-OF-LIFE / DX

### T33 — Local dashboard dev loop
dashboard/worker/index.ts returns "Company not found" for localhost; __Host-/Secure cookies need HTTPS.
Add a dev-only hostname mapping (e.g. `*.localhost` → fixture org via env var, never active in
production), `.dev.vars.example` with needed vars (DASHBOARD_SESSION_KEY etc.; no real secrets), a local
D1 seed script for a fixture company, and a documented `npm run dev` flow in dashboard/README.md. Make
sure production behavior is unchanged (tests).

### T34 — Deploy script applying D1 migrations + schema version in /healthz
Add a server deploy script (e.g. scripts/deploy-server.mjs or npm script) that applies pending D1
migrations (`wrangler d1 migrations apply --remote`) before `wrangler deploy`, with a dry-run/--list
mode. Add a schema-version check to /healthz (report latest applied migration vs expected; e.g. compare
d1_migrations table to the embedded latest migration name) and fail/flag on mismatch. Don't run it
against production. Also extend server/tests/sql_regressions.py to prepare every SQL string in server/src
against the migrated schema (catches column mismatches) if feasible.

### T35 — Viewer input QoL: capture Win/Alt+Tab on Windows; rebindable reserved keys
Windows viewer: low-level keyboard hook (WH_KEYBOARD_LL) active only while the viewer window is focused
and input-enabled, forwarding Win, Alt+Tab, etc. to the remote (with a way to release — e.g. the
existing reserved key). Reserved local keys: F12 (both platforms) and F8 (Windows) cannot be sent to the
remote — make them configurable (preference) with a sensible default, and allow sending the key itself
(e.g. via menu/toolbar "Send F12"). Validate on endpoint as far as possible.

### T36 — Refactor: split giant files and dedupe background launch code (agent)
agent/src/remote/capture_helper.rs (4.2k lines): split into parent orchestration, child run loops, wire
protocol (tests start ~:3210). background_tasks.rs / background_files.rs: separate window-proc and state
modules; background.rs: move the ~1,450 lines of tests to a tests submodule file. Deduplicate the three
"launch on background desktop" implementations (background.rs:379-431, background_files/launch.rs:85-136,
background_tasks.rs:1555-1580), the multiple `wide()` helpers and Handle/OwnedHandle types. Pure
refactor: no behavior change. Native Windows fmt/clippy/tests + installed-agent smoke validation.

### T37 — Refactor: split viewer window.rs / transport.rs; share input state; ControlSink struct
remote/src/platform/windows/window.rs (2.1k lines) → window proc, toolbar, settings, input modules.
remote/src/transport.rs → signaling loop, services, video. Move shared key/modifier state, release-all
input and pointer mapping into a shared module used by both platforms. Replace 15-arg ControlSink::new
with a struct. Fix window_context handing out overlapping `&'static mut` (UB on message-box re-entry).
Behavior-preserving. Checks on Mac + endpoint.

### T38 — Refactor: split dashboard.tsx
dashboard/features/workspace/dashboard.tsx (~641 lines): extract SettingsPage, useInstallerDownload,
useRemoteHandoff (and modal a11y: Escape handling, focus trap/return for enrollment and account modals;
nav items as links so open-in-new-tab works). Behavior-preserving otherwise. npm run verify.

---------------------------------------------------------------------------------------------------
## LOW (added during implementation)

### T39 — Native log retention/rotation (viewer + agent)
Viewer log `~/Library/Logs/MeshRMM/remote.log` observed at ~90 MB with no rotation (Windows viewer and
agent logs likely similar; see agent/src/logging.rs and remote/src/debug.rs or wherever the viewer log is
opened). Add size-based rotation (e.g. rotate at 10 MB, keep 3–5 files) for viewer and agent logs, and
consider reducing per-frame "presentation statistics" log frequency. Keep log files' existing ACLs/paths.
Native Windows checks + installed-agent smoke validation.
Also: the viewer's Windows update helper runs detached and has no logging — log its steps to the viewer log.

Note for T36: while refactoring, replace remaining `SystemRoot` environment reads in
agent/src/remote/background.rs, credentials.rs, background_tasks.rs with GetSystemWindowsDirectoryW (or the
helper added in T04b in agent/src/installer.rs / private_directory.rs).
