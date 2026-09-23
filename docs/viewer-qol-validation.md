# Viewer quality-of-life validation

## Agent display border

The viewer's session controls include **Highlight viewed monitor on agent**.
The setting starts enabled unless the company disables its default. Session
choices survive transport reconnects. The agent draws a three-pixel red outline
on the selected physical monitor, or on each monitor in **All monitors** mode.
The outline is excluded from captured video, cannot take focus or intercept
input, and is destroyed when capture ends. Company defaults apply to new sessions.

Deploy migration `0009_display_border.sql` with the server/dashboard update.
No production deployment is part of this change.

Validation on September 16, 2026:

- macOS: viewer Clippy and unit tests; protocol and server tests; server WASM
  check; SQL regressions; dashboard `npm run verify` using Node 22.22.0.
  Server/dashboard CI normally uses Linux.
- Windows `DESKTOP-85R6S28`: source hashes checked against the local working tree;
  workspace Clippy and tests; release agent build. The native overlay test runs
  separately on the interactive desktop because SSH has no composited desktop.
- Native overlay assertions cover geometry, capture exclusion, input transparency,
  and destruction on drop. Protocol tests preserve existing wire tags; company
  tests cover SQLite boolean conversion and legacy defaults.
- Installed-service testing: viewer toggle removes/restores the overlay; changing
  from the primary landscape monitor to the left portrait monitor moves it to
  the correct negative coordinates; closing the viewer removes every border
  window. The viewer's captured video contains no red outline.

The Windows cross-check attempted on macOS lacked MSVC C headers; native Windows
validation was used. An initial SSH-only overlay test could not create composited
windows; the same test passed in the logged-in Windows desktop.

Final installed border-build SHA-256:
`8B521281D7DCAFFD3359A547BB05A5FF22ACF2F25729D8A3FE59109DADA9825D`.
The all-monitor service test produced eight excluded border windows (four per
physical monitor), with no outline visible in the combined captured video.

## Agent pointer monitor indicator

The display selector marks the physical monitor containing the agent-side mouse
with `➤` while the local user owns input. The marker clears when the technician
controls input. Status messages are sent on monitor/ownership changes, without
sending every mouse position or changing the selected video display.

Validation: macOS viewer Clippy/tests and protocol tests; Windows workspace
Clippy/tests and release build; formatting and diff checks. Geometry tests cover
negative origins, exact shared edges, gaps, and exclusion of the synthetic
all-monitor entry. IPC round trips include pointer monitor identity.

Installed Windows build SHA-256:
`2670718902E23E9E0036BC566320B29564998DBB30C9C99F5D5FF157F56652F2`.
The service started and connected with its configuration preserved. A live macOS
viewer session displayed `➤ LG ULTRAGEAR` in the monitor list, then cleared the
marker when the viewer clicked a harmless remote background area. Session close
completed normally. Windows viewer behavior was compiled and unit-tested natively;
its UI was not manually exercised.

## Prevent idle lock

Company settings independently control the default and whether session users may
change it. Both default to on. The authenticated agent request carries the policy;
the agent clamps every viewer request to the company default when overrides are
forbidden. Preferences survive reconnects but reset for a new session. Deploy
`0010_prevent_idle_lock.sql` with the server/dashboard changes.

An elevated desktop helper owns a power request and periodically sends tagged
zero-movement input. This resets Windows idle accounting without moving the mouse,
claiming input ownership, changing configured timeouts, or unlocking a locked
machine. Stop, pipe EOF, and helper exit release the request. Windows policies that
reject simulated input remain authoritative.

Validation: macOS viewer Clippy/tests, protocol/policy/server tests, WASM check,
SQL regressions, and dashboard verification. Native Windows workspace Clippy/tests
and release build passed. Policy tests exercise both locked default values and
both override directions; input-hook tests verify keep-awake events preserve
ownership. The interactive Windows test verifies idle resets, unchanged pointer
coordinates, and cessation after dropping the guard.

Installed build SHA-256:
`FF1180BA5C7AF70F54351D3A54B1EE286C11FD401638323D9CAAA0F74FF6B723`.
Live service measurements stayed below one second of idle time with prevention
on, rose from 11.4 to 15.2 seconds after toggling off, and rose from 9.4 to 13.2
seconds after disconnecting with prevention on. Service logs recorded enable and
disable transitions; the service remained running and connected.

The initial Windows test link exhausted C: disk space. The existing Rust cache
was preserved and relocated to `D:\MeshRMM-qol-build-cache-20260916`, with a junction
at its original path. This freed about 18 GB; the final checks passed afterward.

## Disconnect confirmation

**Disconnect confirmation** defaults to on and is stored per OS user in
`MeshRMM/viewer-preferences.json` under Application Support (macOS) or APPDATA
(Windows). The preference is independent of agent and session IDs. Writes replace
the file atomically; missing or malformed preferences default to confirmation on.
Window-close confirmation defaults to cancellation. macOS Quit also uses the
confirmation and lets the transport finish cleanup before exiting.

Validation: macOS and Windows viewer Clippy/tests, including preferences loaded
from fresh instances, replacement of existing files, and malformed-file fallback;
formatting and diff checks. Live macOS sessions verified window-close Cancel,
Quit Cancel, immediate close when disabled, persistence after launching a new
session, and confirmed Quit. Confirmation was restored to on after testing.
The installed agent from the previous feature remained in use; no service update
was needed for this viewer-only change. Windows UI was compiled/tested natively,
not manually exercised.

## On session close

**On session close** (No action by default, Lock, or Logout) is a per-OS-user viewer
preference stored with **Disconnect confirmation**. The viewer sends it to the Agent
on each connection. The Agent keeps it per remote session, across resumes, and
tracks the viewed console or RDP session. It runs the action once, when the server
ends the session (end-session command, a replacing session, or terminal signaling
rejection). A closed control channel alone does not trigger it. Lock launches a
`--lock-session` helper as the signed-in user on `winsta0\default`, which calls
`LockWorkStation`. Logout calls `WTSLogoffSession`. Sessions with no user, and the
background desktop, are skipped.

Validation: macOS and native Windows workspace Clippy, protocol/agent/viewer tests,
and formatting. The Windows test source was verified against the working tree by
SHA-256. Installed build SHA-256:
`6A418BED032CEA41CD451705CEB3E3705512BE8C9BA8CD17C2ED0F3D0A716F65`.
The service started and connected with its configuration preserved. Two live macOS
viewer sessions were run against the console. With Lock, closing the viewer logged
`action=Lock session=Console` 28 ms after the server ended the session, and
LogonUI appeared in session 1. With Logout, the action ran 2 ms after the session
ended and `query user` reported no signed-in users. The service stayed running and
connected. The Windows viewer's radio buttons were compiled and unit-tested
natively; that UI was not manually exercised.

## Clear clipboard on session close

**Clear clipboard on session close** is a per-OS-user viewer preference, on by
default, stored with **On session close** and sent to the Agent on each connection.
The Agent keeps it with the session close action and runs it at the same points,
once, before Lock. It launches a `--clear-clipboard` helper as the signed-in user on
`winsta0\default`, which calls `EmptyClipboard` and retries briefly while another
application holds the clipboard. Logout, the background desktop, and sessions with
no signed-in user are skipped. Windows clipboard history is not cleared.

Validation: macOS and native Windows workspace Clippy, protocol/agent/viewer tests,
and formatting. The Windows test source was verified against the working tree by
SHA-256. Installed build SHA-256:
`2BAFC0ED9D957B76181A748CD3268716F34E196680D4E38606AAF03386344E3F`.
The service started and connected with its configuration preserved. Running the
installed helper in console session 2 emptied text placed there by the signed-in
user. A live viewer session closing with the toggle on has not been exercised yet.

## Dedicated settings page

- Company settings now live at `/settings`, categorized as dashboard security,
  remote sessions, and blackout message. Account details remain separate. Users
  and authentication also have addressable routes; non-admin visits retain a
  permission gate. Company policy controls are read-only for non-admins.
- Preserved company PUT payload, byte-length/empty-message validation, default
  values, session pause behavior, and tenant resolution. Saves provide feedback.
- On macOS with Node 22.22.0, `npm run verify` passed TypeScript, ESLint,
  production build, and all 10 tests. The new route regression checks title,
  navigation, absence of policy values before authorization, and rejection of
  unknown tenants. `git diff --check` passed.
- No server deployment or Windows update is required for this dashboard-only
  change. Authenticated settings saves against a deployed new dashboard remain
  unexercised; the production dashboard was not replaced.

## Dashboard presentation

- Updated the workspace with a dark navigation sidebar, larger typography,
  consistent spacing, accessible focus states, and responsive device rows.
  Company settings use category links and clearly separated policy cards.
- The device summary filters the list. Search has one location; device IDs are
  available under each name. Offline Connect remains disabled. Administrative
  close/delete actions retain their existing authorization and handlers.
- Simplified sign-in, enrollment, status, and workspace copy. Replaced the
  redundant inventory-status metric with an automatic-update indicator and
  retained useful empty-state guidance. No fixture data ships in the dashboard.
- Final `npm run verify` on macOS / Node 22.22.0 passed TypeScript, ESLint,
  production build, and 10 tests. `git diff --check` passed.
- Chrome layout checks used an isolated local fixture with the real device-list
  component and settings markup/CSS. Verified summary filtering, search,
  zero-result guidance, Clear filters, offline disabled Connect, desktop layout,
  and a 390px mobile layout with visible statuses/actions. Category navigation
  and remote-session checkbox layout were checked on mobile. The temporary
  viewport was reset and the preview server stopped afterward.
- These are component/layout checks, not authenticated production save or
  enrollment tests. No dashboard/server deployment or push was performed.

## Settings tabs follow-up

Replaced category jump links with tabs: dashboard security, remote sessions, and
blackout message. Only the selected panel is visible; draft values remain mounted
and survive tab changes. Tabs support arrow keys, Home/End, selected state, and
panel labels. Saving still applies all company settings, with a visible notice
if a hidden blackout-message draft prevents saving.

On macOS / Node 22.22.0, `npm run verify` passed TypeScript, ESLint, the production
build, and all 10 tests (including tab semantics and initial selection in the
settings route). `git diff --check` passed. This is a dashboard-only update.
