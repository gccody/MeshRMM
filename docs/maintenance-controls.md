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

## Connection approval

A company administrator can make the Agent's user approve each connection under
**Settings → Connection approval**. It is off by default. When it is on, the
technician sees a dialog after choosing **Connect** or **Connect to
background** and may give a reason, up to 500 UTF-8 bytes. The Agent then shows
a prompt in the middle of the console's primary monitor, above other windows:

> **Remote connection request**
>
> {user_name} would like to connect.
>
> Reason: *the technician's reason, when they gave one*
>
> Accepts automatically in 30 seconds. **[Deny] [Accept]**

The message is a template like the connection notification's: `{user_name}`
works as it does in the blackout message, and it is limited to 512 UTF-8 bytes.
Without a reason the prompt shows only the message. The prompt does not take
focus when it opens, so typing in another window cannot answer it. Once
clicked, Tab moves between the buttons, Enter or Space presses the focused
one, and Escape or closing the prompt denies the connection.

Until the user answers, the technician's viewer shows that it is waiting and
how long until the connection goes ahead; nothing is captured or streamed. A
denial ends the session and the viewer says the user declined; it does not
retry. The connection is accepted without an answer when:

- **Nobody answers in time.** The administrator sets the time, from 5 to 300
  seconds (30 by default).
- **The computer has been sitting at the lock screen.** The administrator sets
  how long it must have had no keyboard or mouse input, from 0 to 3600 seconds
  (60 by default). A console nobody is signed in to counts as locked. With 0,
  a locked computer accepts at once. A computer locked more recently keeps
  asking, on the user's desktop, until it has been idle that long, the user
  unlocks it and answers, or the time runs out.

The policy applies to every new session, including background mode, and has
no per-session or viewer override: the server resolves it from the enrolled
agent's company and sends it only in the authenticated Agent session request.
The reason is stored with the one-time handoff, recorded in the
`remote.handoff_create` audit event, and kept in the session record. Viewer
reconnects and resumes of the same session do not ask again; a session the user
declined stays declined. As a service, the Agent shows the prompt from a
separate LocalSystem helper on the console's desktop, which also measures the
session's input idle time. Apply migration `0018_connection_approval.sql`
before deploying the updated server and dashboard; `/healthz` expects it.

## Restart and Safe Mode

The toolbar's power button, next to **Send Ctrl+Alt+Del**, opens a menu with
**Restart…** and **Restart in Safe Mode with Networking…**. Each asks for
confirmation first. Windows then restarts at once and closes applications
without saving. The button is available once the Agent has connected,
including in view-only sessions, since restarting is not input.

The Agent tells the viewer its operating system when the session starts. A
Mac offers only **Restart…**: Apple silicon Macs enter Safe Mode only from the
power button at startup, and Macs have no Ctrl+Alt+Del, so that button is left
out too.

The session survives the restart. The viewer keeps the window open and shows
that the remote computer is restarting, the server keeps the session, and its
Agent coordinator hands the session back to the Agent once it reconnects.
Connection approval is not asked again: the Agent keeps the session's accepted
answer in its configuration directory for up to 30 minutes across the restart.

In Safe Mode the toolbar shows **Safe Mode** beside the power button, and its
menu offers **Restart normally…**. The Agent makes Safe Mode work as follows:

- **The service starts.** Windows starts only services listed under
  `HKLM\SYSTEM\CurrentControlSet\Control\SafeBoot\Network`. The installer
  and each Safe Mode restart register `MeshRMMAgent` there; uninstalling
  removes it.
- **The next restart is normal.** A Safe Mode restart sets the boot option
  with `bcdedit /set {current} safeboot network` and leaves a
  `safe-mode-restart` marker beside `agent.json`. When the service starts
  again, it clears the option and removes the marker. A computer that boots
  into Safe Mode but cannot reach the network therefore returns to normal
  mode the next time it restarts. **Restart normally** also clears a Safe
  Mode option that something else, such as `msconfig`, set.
- **Video works without a GPU driver.** Safe Mode has no hardware encoder,
  and Windows disables the Media Foundation platform there (`MFStartup`
  fails with `MF_E_DISABLED_IN_SAFEMODE`). The Agent copies frames to the
  CPU, converts them to NV12 there, and creates Microsoft's software H.264
  encoder transform directly through COM, which works without
  `MFStartup`. HEVC and 4:4:4 are unavailable, so profile negotiation falls
  back to H.264 4:2:0, and capture is capped at 30 FPS. Normal boots use
  the same path only as the final fallback, when the GPU cannot encode.

Other features degrade as Windows allows in Safe Mode; for example, system
audio is unavailable because the Windows Audio service does not run.

### Validation (2026-10-01)

On `DESKTOP-85R6S28` (Windows 11, wired Ethernet, NVIDIA GPU), with the
installed Agent service and the macOS viewer against production signaling:

- **Restart:** Windows booted again 22 seconds after the click, and the
  same viewer session reconnected 42 seconds after it.
- **Restart in Safe Mode with Networking:** Windows booted into Safe Mode.
  The service started, cleared the boot option and marker, and resumed the
  session. H.265 failed as expected, and the stream fell back to software
  H.264 at 1920×1080 and 30 FPS. The toolbar showed **Safe Mode**.
- **Restart normally** from Safe Mode booted Windows normally, and the
  session resumed with hardware H.265.
- The opt-in software encoder test took about 9 ms per 1080p frame, both in
  normal mode and in Safe Mode:

  ```powershell
  cargo test --release -p meshrmm-remote-screen software -- --ignored --nocapture
  ```

Connection approval was off for the test company. Unit tests cover keeping
its answer across the restart.

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
