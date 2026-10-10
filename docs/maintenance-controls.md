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

Agent controls become available once the Agent's desktop helper announces
them. The UI reflects the state the Agent acknowledged, and a failure shows an
error instead of silently claiming success. Blackout requires Windows 10
version 2004 or newer.

These are session maintenance controls, not a Windows security boundary. Windows
secure attention (Ctrl+Alt+Delete), secure desktop transitions, and privileged
software remain under OS control. A desktop-helper replacement resets agent
controls and reports the new state; re-enable them on the new desktop if needed.
Disconnecting, closing the viewer, service shutdown, helper exit, or capture failure
removes blackout and input hooks. Technician input blocking remains a viewer choice
for a reconnect, while agent restrictions reset.

## Blackout message

An administrator can edit **Settings → Blackout message**.
The default is:

> This machine is under maintenance by {user_name}.

`{user_name}` is replaced with the authenticated technician name used in the agent
banner. Templates support newlines and Unicode, are limited to 2048 UTF-8 bytes,
and must be nonempty. The preview uses the signed-in administrator's name.
**Restore default message**, followed by **Save settings**, resets it.
Changes apply to new remote sessions; a reconnect retains its session's message.

The server resolves the template from its settings and retains it in the
session record. The viewer cannot supply a replacement template.

## Connection notification

When a technician connects, the Agent shows a notification in the bottom-right
corner of the primary monitor's work area. It does not take focus. It closes when
clicked or after 15 seconds; resting the pointer on it restarts the countdown.
Unlike the blackout notice, it is not excluded from capture, so the technician
sees what the user sees.

An administrator configures it under **Settings → Connection
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
users with the settings permission can save the setting. There is no
per-session or viewer override: the server resolves the setting and template
from its settings. It sends them only in the authenticated Agent session
request and keeps them in the session record. Changes apply to new remote
sessions.

The notification appears once per remote session. Viewer reconnects and resumes
of the same session do not repeat it. As a service, the Agent shows it from a
separate LocalSystem helper on the interactive desktop (the console, or the
viewed RDP session). Background-mode sessions can therefore notify the
signed-in user, and desktop switches that replace the other helpers leave it
open. The notification closes when the session ends.

## Connection approval

An administrator can make the Agent's user approve each connection under
**Settings → Connection approval**. It is off by default. When it is on, the
technician sees a dialog after choosing **Connect** or **Connect in
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
no per-session or viewer override: the server resolves it from its settings
and sends it only in the authenticated Agent session request.
The reason is stored with the one-time handoff, recorded in the
`remote.handoff_create` audit event, and kept in the session record. Viewer
reconnects and resumes of the same session do not ask again; a session the user
declined stays declined. As a service, the Agent shows the prompt from a
separate LocalSystem helper on the console's desktop, which also measures the
session's input idle time.

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

## Tests

Unit tests cover the wire format, name and template rendering, keeping the
session's policy and approval answer in the session record and across a
restart, input gating across focus changes and reconnects, helper command
serialization, and hook suppression.

The interactive Windows tests are ignored by default because they briefly
block input and cover the monitors. Run them on the logged-in desktop with no
maintenance session active:

```powershell
cargo test -p meshrmm-agent live_ -- --ignored --nocapture --test-threads=1
```

They check that tagged remote key events pass and untagged ones are
suppressed, that held input is released and input recovers on teardown, and
that blackout covers the whole virtual desktop, is excluded from capture, and
is destroyed on teardown. Also check by eye that the device's pointer stays
hidden while the remote mouse moves across applications and monitors, and
comes back after restoring the monitors or disconnecting; the technician's
viewer should keep showing cursor shapes. The software encoder Safe Mode
relies on has its own test:

```powershell
cargo test --release -p meshrmm-remote-screen software -- --ignored --nocapture
```
