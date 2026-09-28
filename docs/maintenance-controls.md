# Remote maintenance controls

The native viewer provides three independent controls. On macOS, open the gear
menu in the toolbar: **View only** is under **Session**, and the agent controls
are under **Remote computer** as **Block user's keyboard and mouse** and **Black
out screens**; each is a checkmarked toggle. On Windows, open **Settings →
Advanced**, where they are named as below.

- **Block technician input** stops the viewer's keyboard, mouse, and wheel events.
  It releases held remote keys/buttons before blocking. The choice survives focus
  changes, display changes, and network reconnection. Choose **Allow technician
  input** to resume.
- **Block agent keyboard and mouse** suppresses endpoint input on the interactive
  desktop while allowing MeshRMM's tagged remote input. Existing held keys/buttons
  are released when blocking starts. Choose **Allow agent input** to restore it.
- **Black out all agent monitors** places a black maintenance notice across the
  entire virtual desktop, with the message centered on every monitor. The notice
  is click-through and excluded from capture so the technician can still work.
  The endpoint's mouse pointer is hidden until blackout ends. Choose
  **Restore agent monitors** to remove the notice and restore the pointer.

Agent controls become available after the updated desktop helper announces support.
The UI reflects acknowledged agent state. Failures show an error instead of silently
claiming success. Blackout requires Windows 10 version 2004 or newer.

These are session maintenance controls, not a Windows security boundary. Windows
secure attention (Ctrl+Alt+Delete), secure desktop transitions, and privileged
software remain under OS control. A desktop-helper replacement resets agent
controls and reports the new state; re-enable them on the new desktop if needed.
Disconnecting, closing the viewer, service shutdown, helper exit, or capture failure
removes blackout and input hooks. Technician input blocking remains a viewer choice
for a reconnect, while agent restrictions reset.

## Company message

A company administrator can edit **Profile & session → Agent blackout message**.
The default is:

> This machine is under maintenance by {user_name}.

`{user_name}` is replaced with the authenticated technician name used in the agent
banner. Templates support newlines and Unicode, are limited to 2048 UTF-8 bytes,
and must be nonempty. The preview uses the signed-in administrator's name.
**Restore default message**, followed by **Save company settings**, resets it.
Changes apply to new remote sessions; a reconnect retains its session's message.

The server resolves the template from the enrolled agent's company and retains it
in the session record. The viewer cannot supply a replacement template. Legacy
session records and older servers use the default message. Apply migration
`0008_blackout_message.sql` before deploying the updated server and dashboard.

## Connection notification

When a technician connects, the Agent shows a notification in the bottom-right
corner of the primary monitor's work area. It does not take focus. It closes when
clicked or after 15 seconds; resting the pointer on it restarts the countdown.
Unlike the blackout notice, it is not excluded from capture, so the technician
sees what the user sees.

A company administrator configures it under **Settings → Connection
notification**, with two settings:

- **Notify the agent's user when a technician connects to their session** (on
  by default) covers sessions that view the console or an RDP session.
- **Also notify the user when a technician connects in background mode** (off
  by default) covers sessions on the background desktop, which the user cannot
  see. If the technician switches from background mode to the user's session,
  the first setting applies and the user is notified then.

Both use the same message. The default is:

> {user_name} has connected to this computer.

`{user_name}` works as it does in the blackout message. The template supports
newlines and Unicode, must be nonempty, and is limited to 512 UTF-8 bytes. Only
company administrators can save the setting. There is no per-session or viewer
override: the server resolves the setting and template from the enrolled agent's
company. It sends them only in the authenticated Agent session request and keeps
them in the session record. Requests from servers without the fields show no
notification. Changes apply to new remote sessions.

The notification appears once per remote session. Viewer reconnects and resumes
of the same session do not repeat it. As a service, the Agent shows it from a
separate LocalSystem helper on the interactive desktop (the console, or the
viewed RDP session). Background-mode sessions can therefore notify the
signed-in user, and desktop switches that replace the other helpers leave it
open. The notification closes when the session ends. Apply
migration `0015_connection_notification.sql` before deploying the updated server
and dashboard; `/healthz` expects it.

## Validation

Automated checks cover protocol compatibility, name/template rendering, company
isolation, legacy policy updates, session-record persistence, input gating across
focus/reconnection, helper command serialization, and hook suppression.

Interactive Windows tests are opt-in because they briefly block input and cover
monitors. Run on the logged-in desktop, with no maintenance session active:

```powershell
cargo test -p meshrmm-agent live_ -- --ignored --nocapture --test-threads=1
```

They verify that tagged remote key events pass, untagged events are suppressed,
held input is released, input recovers on teardown, blackout covers the full
virtual desktop with capture exclusion, and its window is destroyed on teardown.
Also check visually that the endpoint pointer stays hidden while moving the remote
mouse across applications and monitors, then returns after restoring monitors or
disconnecting. The technician's viewer should continue to show cursor shapes.

### Test-machine validation (2026-09-14)

- Installed the release agent on `192.168.1.152`; installer verified its SHA-256,
  unchanged enrollment configuration, service startup, and signaling reconnection.
- Windows: 29 agent tests and 19 viewer tests passed. Both opt-in desktop tests
  passed, including release of a key held before blocking.
- macOS viewer: 27 tests passed; built and installed the local application bundle.
- Protocol: 20 binary-protocol and 3 shared-type tests passed.
- Server: 10 Rust tests, 10 SQL regression tests, and the WebAssembly build passed.
- Dashboard: typecheck, lint, production build, and 9 tests passed.
- In an authenticated remote session, enabled all three controls, observed agent
  acknowledgment, and confirmed the captured desktop remained visible with
  blackout active. Closing the viewer removed the desktop helpers; a new session
  reported agent restrictions off.
- Saved a multiline custom template through the admin form, verified persistence,
  and opened a new session with it. Restored the default template afterward.
- Deployed database migration 0008, server, and dashboard. Native builds were
  installed locally for testing; public native release assets were not republished.

Local installation and desktop-test evidence is in the ignored
`dist/maintenance-validation/` directory. The final test-agent SHA-256 was
`FCF058704D5AC574F5FAA2BC8C2A85A7B4E88746E263C69AFD1022F75BD33B15`.
