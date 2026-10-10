# Remote sessions

What the viewer does during a session, and how each feature behaves. The
Windows and macOS viewers offer the same features; where a control sits
differs, and each section says where. For how a session is set up, see
[architecture](architecture.md#how-a-session-is-set-up); for the video
itself, see [video](video.md).

## Starting and ending

**Connect** on a device opens the viewer with a single-use link. If the
company requires it, the technician can give a reason and the computer's user
approves the connection first (see
[connection approval](maintenance-controls.md#connection-approval)). The
viewer's connecting window says what it is waiting on: the session, an
update, the download's progress, the user's approval.

A viewer can also redeem a link from the command line:

```powershell
.\meshrmm-remote.exe "meshrmm://connect?handoff=<one-time-token>&server=https%3A%2F%2Frmm.example.com"
```

An Agent accepts one session at a time. A second viewer gets a busy
response; closing the first releases the device. On Windows a new link for
the same device replaces the viewer already open for it.

Closing the viewer asks for confirmation first. **Disconnect confirmation**
is a per-user preference, on by default.

If the connection drops, the viewer reconnects to the same session and shows
why it dropped, for how long it has been trying, and when the next attempt
starts, with **Retry now**. Session choices described below survive a
reconnect unless they say otherwise. Capture, encoder, decoder and
presentation failures end only the session; the Agent goes back to waiting
and stays available.

## Sessions and displays

The toolbar has a user-session menu (person icon) and a display menu
(monitor icon).

- **Console** lists the monitors of the computer's own desktop.
- Each active **Remote Desktop** session lists its own monitors as Display 1,
  2, 3 and so on.
- **Background** is the [background desktop](background-mode.md), with one
  display.

**All displays** shows every monitor of the selected session in one view,
keeping their positions, with black between them. It is captured with GDI
instead of the GPU path, so its frame rate can be lower, and the combined
size must be one the encoder and decoder support.

Choosing a session selects its primary monitor. The service looks for
arriving and departing sessions every five seconds. When the session
changes, the capture, input, clipboard, file transfer and chat helpers move
together, and a switch that fails goes back to the previous session. Console
audio and Ctrl+Alt+Del are unavailable while viewing a Remote Desktop
session or the background desktop.

The viewer title shows the active display. While the computer's own user has
control of the mouse, the display menu marks the monitor their pointer is on
with `➤`.

## Input

The viewer sends mouse, wheel and physical keyboard input while its window is
in the foreground and the pointer is inside the video. Letterbox bars do not
control the device, so the local pointer is free to leave. Pointer
coordinates are relative to the display being streamed and each event
carries that display's ID, so the Agent rejects input left over from another
display. Unfocusing the viewer, switching displays or ending the session
releases held keys and buttons.

The computer's user and the technician share control: neither locks the
other out, and the newest action wins. A remote click or wheel step carries
its position and is injected in one piece, so local mouse movement cannot
split it.

Shortcuts, each of which can be set to F8–F12 or turned off so the key goes
to the device:

| | Windows viewer | macOS viewer |
|---|---|---|
| Next display | F8 | Control-Option-Left/Right Arrow |
| Diagnostics overlay | F12 | F12 |
| Where to change | **Settings → Keyboard** | gear menu, **Diagnostics shortcut** |

While it has keyboard focus the Windows viewer also sends the Windows key,
Alt+Tab, Alt+Esc and Ctrl+Esc to the device; turn that off under
**Settings → Keyboard**. Ctrl+Alt+Del and Windows+L always stay local. With a
Mac on the other end, the macOS viewer's Command key is Command and the
Windows viewer's Windows key is Command too.

**Send Ctrl+Alt+Del** (keyboard icon) sends the secure attention sequence to
a Windows device through the installed service. When Windows policy blocks
service-generated secure attention, the Agent allows it for the moment, calls
`SendSAS`, and puts the previous registry value back. A crash in between can
leave the override in place; `SendSAS` itself reports nothing.

**View only** stops the viewer's input; see
[maintenance controls](maintenance-controls.md).

## Clipboard

Text, HTML rich text with a plain-text alternative, images and copied files
are synced in both directions. The viewer's clipboard is copied to the device
when the session connects, and later copies on either side are picked up
every 250 ms. A copy is limited to 32 MiB (of uncompressed pixels, for an
image) and sent in paced 60 KiB chunks. RTF-only formatting is not synced.

**Sync clipboard** (gear menu on macOS; **Settings → Troubleshooting** on
Windows) turns the exchange off in both directions, copied files included.
It is saved per viewer user and is on by default. Turning it back on sends
only later copies.

**Type clipboard** (clipboard icon) types the viewer's clipboard text as
keystrokes, for places a paste can't reach.

## Files

The folder icon offers **Send files…** and **Receive files…**, each with a
native multi-file and folder picker on the computer the files come from.
Other ways to move files:

- Drag files from Finder or Explorer onto the remote view. They arrive as a
  native Windows drop at that position: in Explorer, on the desktop, or on a
  browser's drop target. A target that declines them leaves them in the
  transfer folder.
- Copy files on one computer and paste on the other, while **Sync
  clipboard** is on. A remote paste shortcut waits for the transfer.

Sent and received files land in the signed-in user's
`Documents/MeshRMM Transferred Files`. Nested and empty folders are kept, and
a name that exists gets a number instead of being overwritten.

- Files are sent in bounded, acknowledged chunks. The receiver publishes them
  only after every chunk and SHA-256 check passes.
- Clipboard copies are limited to 512 MiB and other transfers to 64 GiB, and
  the receiver must have the free disk space.
- The viewer saves files from the device only after **Receive**, one
  transfer per request within 15 minutes. It ignores the device's own
  requests to pick files.
- A progress window appears on the receiving computer with the file name,
  percentage, size and item count.
- Received files are tagged as downloads: the quarantine attribute on macOS,
  the Internet zone's Mark of the Web on Windows. Opening a received app or
  installer gets the Gatekeeper or SmartScreen check.
- Interrupted transfers are removed after 24 hours, and cached clipboard and
  drop copies an hour after they leave the clipboard.

On Windows the file helper runs as the signed-in user, separately from the
privileged capture and input helpers. A drop is a real OLE drag: the helper
owns a message queue, supplies the drop position through a thread message
hook, and pumps drag-over events before releasing, so browser targets can
negotiate.

## Chat

The chat bubble in the viewer's toolbar opens a chat with the person at the
computer. On the device, the MeshRMM Agent icon in the notification area
opens it, and a message from the technician opens it by itself. A message
from the device shows an unread mark on the viewer's chat icon instead.

Messages go over their own peer-to-peer channel, never through the server.
Each is limited to 4 KiB of UTF-8, the latest 200 are kept in memory, and
nothing is written to disk. Chat ends with the connection. A Windows desktop
switch, such as a UAC prompt, recreates the device's chat window and clears
its history.

## Audio

The device's system audio (the output mix, not the microphone) plays in the
viewer. It starts muted, and the Agent captures and sends nothing while
muted. Turn it on with **Play remote audio** in the macOS gear menu or by
clearing **Mute audio** under **Settings → Troubleshooting** on Windows; the
choice is saved for the viewer's user.

Audio is 48 kHz stereo Opus at about 96 kbps, downmixed from surround, on a
channel of its own that drops audio instead of falling behind. Video pacing
leaves room for it. Peers without Opus use PCM16. Capture follows changes of
the default playback device.

## Recording

**Record video to Downloads** (macOS gear menu, **Session**; Windows
**Settings → Troubleshooting**) records the remote display on the viewer's
computer. A red **REC** item stays in the toolbar; click it to stop.

- Files are video-only Matroska (`.mkv`) under
  `Downloads/MeshRMM Recordings/session-…`, holding the H.264 or H.265 stream
  as received. Audio, chat and the viewer's own controls are not recorded.
- Recording starts at the next keyframe and starts a new part when the stream
  changes, for example when another display or quality is chosen.
- Frames are written as they arrive by a bounded background writer, so
  nothing is converted at the end. A disk error or a full queue stops the
  recording with a message and leaves the session running.
- Disconnecting saves what was captured and stops recording.

## Annotations

The pen icon turns on drawing, in view-only sessions too. Drag on the remote
view to draw and right-click to erase. While it is on, the mouse draws
instead of reaching the device; the keyboard still does. The Agent draws the
red strokes in a click-through window above everything else on the viewed
monitor, so the user sees them and they appear in the video and in
recordings. Turning the pen off, switching displays or ending the session
erases them. There are no annotations on the background desktop.

## Toolbox, credentials and power

- The toolbox button runs the user's scripts and sends library files to the
  device. See [toolbox](toolbox.md).
- The key icon prompts for, fills and forgets saved Windows credentials. See
  [credential autofill](credential-autofill.md).
- The power button restarts the device, into Safe Mode with Networking if
  wanted. See
  [restart and Safe Mode](maintenance-controls.md#restart-and-safe-mode).

## What the user sees

Unless company policy turns them off, the person at the computer sees:

- **A banner** at the top of the primary screen with the technician's name,
  while their desktop is shared. Click it to collapse it to a small tab and
  drag it sideways to move it. It never takes keyboard focus. Administrators
  turn it off under **Settings → Remote sessions**; chat still works without
  it.
- **A border** on the viewed monitor: a three-pixel red outline, or one on
  each monitor in **All displays**. It is left out of the video and cannot
  be clicked. **Highlight viewed monitor on agent** turns it off for a
  session; the company sets the default.
- **A [connection notification](maintenance-controls.md#connection-notification)**
  when the technician connects.

## Session options

These are chosen per session in the gear menu (macOS) or **Settings**
(Windows). The company sets the defaults of those marked *policy* under
**Settings → Remote sessions** in the website, and whether users may change
them; when they may not, the Agent enforces the default whatever the viewer
asks.

- **Prevent idle lock** (*policy*, on by default). A helper on the device
  holds a power request and sends tagged zero-movement input, which resets
  Windows' idle timer without moving the mouse or unlocking a locked
  computer.
- **Disconnect when idle** (*policy*, **Never** by default). Ends the session
  after the technician has been idle in the viewer for 5, 10, 15 or 30
  minutes or 1, 2, 4 or 8 hours. Input over the remote display, viewer
  controls and sent chat messages count as activity; time spent reconnecting
  does not.
- **On session close**: **No action** (default), **Lock** or **Logout**. What
  happens to the Windows session being viewed when the remote session ends.
  It is not saved: each session starts with **No action**. Logout does not
  save open work.
- **Clear clipboard on session close** (*policy*, on by default). Empties the
  viewed session's clipboard at the same point, before any lock. Windows
  clipboard history (Win+V) is left alone.
- **Hide wallpaper** while connected.

The close actions run once, when the server ends the session: the viewer
closed it, it was closed from the website, or an unreachable viewer reached
the idle timeout. They don't run while the viewer is reconnecting, when
nobody is signed in, on the background desktop, or when the Agent service is
updated or restarted. Lock and clear-clipboard run in a helper as the
signed-in user; logout uses `WTSLogoffSession`.

## Diagnostics

- The viewer logs to `%LOCALAPPDATA%\MeshRMM\remote.log` on Windows and
  `~/Library/Logs/MeshRMM/remote.log` on macOS, rotated at 10 MiB with three
  older files kept. Connection and video statistics are logged every 30
  seconds, and ICE logs say whether the connection is `direct` or `turn`.
- The diagnostics overlay (F12) updates every two seconds.
- Viewer preferences are in `MeshRMM/viewer-preferences.json` under
  Application Support (macOS) or `%APPDATA%` (Windows).
- An optional `remote.json` beside the executable (in `Contents/MacOS` on
  macOS) holds local settings: `auto_update` and `json_logs`.

## Updates

The viewer checks its server's update manifest at the start of each launch
from the website, after it has redeemed the link. For a newer release it
checks the signature and SHA-256, has a helper replace the executable or app
bundle, and relaunches with the session it already holds. With no update, or
when the check can't reach the server, it carries on at once. A replacement
that fails rolls back.

## Limits

- One viewer per device at a time, and no browser client.
- Video goes over a WebRTC data channel built for this, not an RTP track.
- Ctrl+Alt+Del needs the installed Windows service; a console-mode Agent
  cannot send it.
- The macOS viewer decodes 4:2:0 only.
