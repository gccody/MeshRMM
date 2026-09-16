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
