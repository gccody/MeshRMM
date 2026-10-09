# MeshRMM

MeshRMM is a self-hosted remote monitoring and management project with
low-latency desktop streaming and remote control. A company runs its own
MeshRMM server, which serves the website, the API, the Agents' and viewers'
connections, STUN/TURN, and the Agent and viewer downloads; nothing calls a
hosted service. To install and run a server, see
[running a MeshRMM server](docs/self-hosting.md).

The code is a single Cargo workspace with platform and transport
responsibilities kept in focused crates:

- `agent/` — the native endpoint Agent, binary control/video protocol, and the
  Windows-specific `windows/remote-screen` capture/encoder package.
- `crates/` — lightweight shared JSON protocol types and signaling client code.
- `server/` — `meshrmm-server`, the self-hosted server: accounts and sign-in,
  devices, the toolbox, WebRTC signaling, STUN/TURN, and downloads. It embeds
  the website.
- `dashboard/` — the website, a Vite + React app prerendered at build time
  (see [its README](dashboard/README.md)).
- `remote/` — the native Windows/macOS viewer. Windows uses Media Foundation
  and D3D11; macOS uses AVFoundation, CoreMedia, Core Animation, and AppKit.
- `site/` — the static marketing page, separate from the server (see
  [its README](site/README.md)).

## MVP data path

```text
Windows.Graphics.Capture (BGRA ID3D11Texture2D)
  -> D3D11 video processor (pooled NV12 4:2:0 or AYUV 4:4:4 texture)
  -> Media Foundation hardware H.265/H.264 encoder
     (final fallback: CPU NV12 conversion -> Microsoft software H.264 encoder)
  -> latest encoded frame slot
  -> unordered/unreliable WebRTC DataChannel (12 KiB fragments)
  -> latest-only reassembly
  -> platform-native video presentation
       Windows: Media Foundation H.265/H.264 decoder (DXGI surface) -> D3D11
         (H.264 fallback: Microsoft's decoder, DXVA or software -> D3D11)
       macOS: AVSampleBufferDisplayLayer -> Core Animation/AppKit window
```

The viewer advertises the codec/chroma profiles it can decode: those with a
hardware decoder, plus H.264 4:2:0, which it can always decode in software.
The Agent prefers H.265/HEVC within the selected chroma mode, then falls back
through the mutually supported profiles if encoder or playback initialization
fails. H.264 4:2:0 comes last, and encodes in software when the Agent has no
GPU path for it. Windows viewers can select bandwidth-efficient
4:2:0 or crisp-text 4:4:4 when the GPU driver exposes the required AYUV and
High 4:4:4/RExt hardware path. Unsupported 4:4:4 controls are disabled and
macOS currently advertises 4:2:0 only. Quality remains independently
configurable as Ultra data saver (1 Mbps, grayscale, up to 24 FPS), Data saver
(3 Mbps), Balanced (6 Mbps), or Best quality (up to
12 Mbps).

The hardware encoders use a streaming-oriented CBR configuration: real-time
and low-latency modes, no B-frames/reordering, a short recovery GOP, an
approximately one-frame VBV with a 16 KiB detail floor, a speed-biased
quality-versus-speed setting, and an optional maximum-QP guard. The video data
channel drains and declares congestion using bitrate-relative time budgets
instead of fixed byte counts, keeping the three quality presets similarly
responsive.

The server is not in that data path. It keeps each authenticated Agent's
control connection, and for each remote session forwards bounded JSON SDP/ICE
messages between exactly one Agent and one viewer. The peer-to-peer connection
itself is encrypted by WebRTC DTLS. When no direct candidate pair connects,
ICE relays through the server's built-in TURN server, with credentials the
server issues for each session.

## Prerequisites

- Windows 10 version 1903 or newer for the Agent and Windows viewer.
- Local administrator approval to install the Agent as a Windows service.
- macOS 12 or newer for the macOS viewer.
- macOS 12.3 or newer for the macOS Agent, which is in development (see below).
- A Linux machine (x86_64 or arm64) for the server; see
  [running a MeshRMM server](docs/self-hosting.md).
- rustup. The first `cargo` command in the repository installs the toolchain
  pinned in `rust-toolchain.toml` (the MSVC host toolchain on Windows) with
  Clippy and rustfmt.
- CMake and a C compiler, which build the bundled libopus audio codec. On
  Windows, the Visual Studio C++ build tools provide the compiler, and their
  "C++ CMake tools for Windows" component provides CMake:
  `scripts\use-cmake.ps1`, which the build scripts dot-source, finds it with
  `vswhere` when `cmake` isn't on `PATH`.
- Ideally, a GPU/driver exposing Media Foundation hardware H.264 encode and
  decode plus D3D11 NV12 video processing. Without one, video falls back to
  software H.264 (see below). Hardware H.265/HEVC and AYUV 4:4:4 support are
  optional and negotiated only when available at both ends.
- Node.js 22.13 or newer to build the website, which the server embeds.

A `cargo` that rustup does not manage, such as Homebrew's, ignores
`rust-toolchain.toml`. Put rustup's `cargo` first in `PATH`.

Software H.264 4:2:0 is the final fallback at both ends. An Agent whose GPU
cannot convert or encode the stream, or that has no GPU, copies frames to the
CPU, converts them to NV12 there, and encodes them with Microsoft's software
H.264 encoder transform at up to 30 FPS. Safe Mode, where Windows loads no GPU
vendor driver and disables Media Foundation, always uses this path
(see [restart and Safe Mode](docs/maintenance-controls.md#restart-and-safe-mode)).
A Windows viewer without an H.264 hardware decoder transform uses Microsoft's
H.264 decoder, which decodes with DXVA when the GPU can (NVIDIA drivers
register no hardware decoder transform, so this is their GPU path) and in
software otherwise; without a D3D11 hardware video device it renders with
WARP. A macOS viewer lets VideoToolbox decode H.264 in software when there is
no hardware decoder. Startup still fails with a contextual error when neither
path exists, for example on a Windows N edition without the Media Feature
Pack.

## macOS Agent

The Agent also runs on macOS 12.3 or newer, and is still being ported.
ScreenCaptureKit captures 4:2:0 frames, VideoToolbox encodes them as H.265 or
H.264 (in hardware where the Mac has it), and Quartz events carry the viewer's
keyboard and pointer input; ScreenCaptureKit also captures the Mac's system
audio. Clipboard, file transfer, chat, Prevent idle lock and Hide wallpaper
work too. During a session a menu bar item opens the chat, which also opens
by itself for the technician's messages; the session banner, the connection
notification and the connection approval prompt follow company policy as on
Windows, the display border is left out of the capture, and the technician's
annotations are drawn over the shared display. The maintenance controls work
too: blocking the user's keyboard and mouse uses an event tap that lets only
the technician's tagged input through, the blackout shows the company's
message on every screen while the capture leaves it out, and the session close
actions lock the screen, log out or clear the clipboard of the same sign-in
the technician saw. The Devices page shows the Mac's screen thumbnail, and
the toolbox runs Shell (zsh) scripts as the console user or root and
delivers files to the user's Documents transfer folder.
VideoToolbox has no 4:4:4 encoder, so Mac Agents always stream 4:2:0.

The website's **Add device** dialog creates a one-time Terminal command
for Macs. It runs `install-agent-macos.sh` from the server, which downloads
the server's universal (Apple silicon and Intel) Agent, checks its code
signature, and runs `sudo meshrmm-agent --install <authorization>`.
That enrolls the Mac with the hex-encoded installer authorization and installs
the app bundle in
`/Library/Application Support/MeshRMM`. A launchd daemon runs the root
coordinator, which keeps the signaling connection and the WebRTC session.
Capture and input need a graphical session, so a launchd agent runs a session
helper in each one, the login window's included; helpers connect to the
coordinator's socket at `/var/run/com.meshrmm.agent.sock`, which admits only
processes running the Agent's own executable. A session follows the console:
when another user takes it, capture moves to that user's helper.
The coordinator updates the Agent like the Windows service does: every six
hours, postponed while a remote session is live, it stages a newer
`agent-macos` release once its SHA-256 and code signature check out: an
Agent signed with a Developer ID takes only builds its own team signed, and
an ad-hoc signed one only builds carrying the release signature. The new
Agent waits for the coordinator to stop, swaps the app bundle, restarts the
launchd jobs, and puts the previous bundle back if the new coordinator does
not keep running.
`sudo meshrmm-agent --uninstall` removes everything. The configuration and
WebRTC identity live in `/Library/Application Support/MeshRMM/Agent`, which
only root can read.

macOS lets only the user grant the Screen & System Audio Recording,
Accessibility and Input Monitoring permissions the helpers need; the first
helper asks for them. `meshrmm-agent --console --config agent.json` runs the
Agent in the current session for development.

## Computers without a monitor

Windows has no desktop to capture when no monitor is connected. When a session
finds the console in that state, the Agent adds a virtual monitor through
[SudoVDA](https://github.com/SudoMaker/SudoVDA), an Indirect Display Driver
bundled in the Agent executable. It installs the driver the first time a
computer needs it, trusting SudoMaker's code-signing certificate in the
machine's `Root` and `TrustedPublisher` stores. If another application already
installed SudoVDA, the Agent uses that copy instead. Uninstalling the Agent
removes only what the Agent installed.

The virtual monitor lasts until the session ends. Its size is a viewer
preference, 1280 × 720 by default, chosen under **Display** in the Windows
viewer's settings or **Remote computer** in the macOS viewer's session menu.
The viewer remembers the choice for later sessions and changes the size of a
connected computer's virtual monitor right away. The setting does nothing
while a real monitor is connected.

## Toolbox

The website's **Toolbox** page keeps PowerShell, Command Prompt and Shell (zsh) scripts and
a library of files, each private to the user who added it or shared with the
company. Scripts run on a device from the website or from the viewer's
toolbox button, as the signed-in user or as SYSTEM; with nobody signed in they
run as SYSTEM. The viewer's toolbox also sends library files to the connected
device's Documents transfer folder, or to Public Documents from the background
desktop. The server hands both to the Agent, so they work in background mode
too. See [the toolbox](docs/toolbox.md).

## Resource monitoring

Every five seconds an online Agent reports its computer's CPU load, memory
use, network throughput through its physical adapters, uptime, and the
capacity of its local fixed volumes, over the control connection it already
keeps open. Each device tile on the **Devices** page shows the CPU and memory
gauges, and a device's name opens its own page: the latest reading, every
volume's free space, and charts of the last 15 minutes live, or the last hour,
24 hours or 7 days. The server keeps the live readings in memory and averages
each minute into the database, where history is kept for 7 days.

## Configuration

The installed Agent reads its protected configuration from
`%ProgramData%\MeshRMM\Agent\agent.json` on Windows, written when it
enrolls with a server: the server's URL, the device ID and credential, and the
server's update manifest. The viewer takes its server from each website link
and loads optional local settings from a sidecar JSON file next to its
executable. No environment variables are required on Agent or viewer
machines. The server's own configuration is described in
[running a MeshRMM server](docs/self-hosting.md#configure).

### Releases

`release.json` holds the release version and the release signing public key:

```json
{
  "version": "0.4.0",
  "signing_public_key": "94d87ebff16c65b9c89fd143596e329f90080f8463dafbf1e9b9f1bd6ed98af3"
}
```

To publish a release, increase `version` and squash-merge the change into
`main`. The **Publish release** workflow builds the Windows and macOS Agents
and viewers, signs them with the release key, packs them with the static Linux
server into a tarball per architecture and a Docker image, tests both, and
publishes them to GitHub Releases and GHCR. Each server serves its release's
builds and update manifest itself. Releases carry no code signing
certificate: the macOS builds are signed only ad hoc, and each company's
server can sign them with its own Developer ID. See [releases](docs/releases.md) for the
contents, the signing scheme, the one-time setup and development keys.

For local builds, these wrappers read the same `release.json`. They put the
builds in `dist/downloads/`, which a development server can serve as its
downloads directory, and describe them in `dist/downloads/artifacts.json`:

```powershell
& .\scripts\build-agent.ps1
& .\scripts\build-remote.ps1
```

To test Agent changes on an already enrolled Windows machine without publishing
download assets or deploying the website, run:

```powershell
& .\scripts\install-agent-local.ps1
```

The script builds the current checkout, requests UAC, preserves the installed
configuration, and replaces the local Agent service executable. It verifies the
installed hash and signaling reconnection, and restores the timestamped backup
if installation or startup fails. The service restart interrupts active remote
sessions. Use `-SkipBuild` to install an existing `target/release/meshrmm-agent.exe`.
Results and a diagnostic log copy are saved under `dist/`. Normal automatic
updates remain enabled; a newer published version can replace this local build.

On a Mac, build an application bundle. The viewer takes its server from each
website link; an optional JSON file passed as the first argument becomes the
bundle's `Contents/MacOS/remote.json` for local settings:

```sh
sh scripts/build-remote-macos.sh
open "dist/remote-macos/MeshRMM Remote.app"
```

To build and install this checkout locally on your Mac without publishing a
release, run:

```sh
sh scripts/install-remote-macos.sh
```

This builds and signs a release-mode viewer, closes any running viewer,
installs it in `~/Applications/MeshRMM Remote.app`, and registers website links.
Use a fresh **Connect** link to start a session. Existing installs
are retained in a `.meshrmm-backup.*` folder under `~/Applications`.
The installed configuration sets `auto_update` to `false` so releases
cannot replace the local build. Normal builds default to automatic updates;
reinstall a normal release to restore that behavior. This script does not change
`release.json` or write download assets.
For a local bundle without installation, use `sh scripts/build-remote-macos.sh --local`.

The script builds for the Mac architecture it runs on, signs the bundle, and
archives it in `dist/downloads/`. A browser deep link supplies a 60-second,
single-use handoff token. The viewer redeems it before checking for updates and
carries the resulting session through an update relaunch. A session awaiting its
first viewer has a 15-minute startup window; connected sessions use the
configured sliding timeout.
The macOS build scripts sign with the keychain's Developer ID Application
certificate, which keeps the Agent's privacy permissions across rebuilds.
If the keychain holds several, set `MESHRMM_CODESIGN_IDENTITY` to the one to
use. Set it to `-` for an ad-hoc signature, as the release workflow does; an
installed Agent signed by a team refuses to update to an ad-hoc build.
Locally built apps aren't quarantined, so they need no notarization.

The native update version is compiled from `release.json`; Cargo package
metadata is not used to decide whether an update is newer. Manifest and release
URLs must use HTTPS. Each updater checks the release signature before
downloading and the SHA-256 digest before replacing anything. A Mac app signed
with a Developer ID instead takes only its server's build signed by the same
team, and checks that signature and the build's version before replacing
anything.

Existing installations are repaired without replacing their device identity. New
installations persist a private recovery key and pending configuration so a failed
or interrupted enrollment can be retried. Installer authorization remains limited
to its original expiry; recovery is restricted to the endpoint holding that key.

Agent credentials are created or rotated by the server, which stores only
their SHA-256 hashes.

Each Agent installer downloaded from the website contains a random,
single-use enrollment authorization that expires after 30 minutes.
During setup, the endpoint reads its Windows computer name and redeems that
authorization. The server generates the device ID and Agent credential, creates
the device's record, and returns the protected runtime configuration.
Delete the downloaded installer after it succeeds.
The installed copy stores only the executable under Program Files and protects
the credential under ProgramData so only LocalSystem and administrators can
read it. `dist/remote/remote.json` contains no viewer credential. Browser
handoffs expire after 60 seconds. Remote sessions use the server's sliding
`remote.idle_timeout_seconds` (900 seconds by default): a connected viewer
renews the deadline every 30 seconds, and the session expires after the viewer
stops reporting activity for that long. A session's TURN credentials work
only while the session lasts.

## Run

### Website

The server's website lists the company's devices. It receives inventory and
connection changes over a WebSocket, authenticated by the sign-in cookie,
instead of polling: a snapshot, then revision-numbered changes. The server
publishes Agents going online and offline, updating, and being enrolled or
deleted as they happen, and closes a user's socket when they sign out, are
disabled, or lose permission to view devices. **Refresh** requests a fresh
snapshot; snapshots never contact Agents.

**Connect** requests a one-time server
handoff, then opens the native viewer with a
`meshrmm://connect?handoff=...&server=...` deep link. No service credential is
entered into or retained by the browser.

Users with permission to close sessions can use **Close session** beside a device's **Connect**
button to disconnect its viewer and clear a stale session reservation. The Agent
stays online and can accept a fresh connection immediately. This action is safe
to repeat when no session exists and is recorded in the audit log.

During desktop sharing, the Windows endpoint shows a translucent banner at the
top of the primary screen with the connected user's display name. Click the banner to collapse
it to a tiny arrow tab with no name; click again to expand it. Drag either state
sideways to move it out of the way; its horizontal position persists when toggling. It does not take keyboard
focus and disappears when capture stops. The server resolves the name from the
authenticated handoff owner and retains it across session reconnects.

When a technician connects, the Windows endpoint also shows a connection
notification in the bottom-right corner of the primary monitor, above the
taskbar. It shows once per session and closes when clicked or after 15 seconds.
It is on by default for sessions that view the user's desktop. Background-mode
sessions notify the user only if an administrator enables that separately.
Administrators configure both and edit the message under **Settings →
Connection notification**; see
[maintenance controls](docs/maintenance-controls.md#connection-notification).

Administrators can also require the endpoint's user to approve each
connection under **Settings → Connection approval** (off by default). The
technician may give a reason when connecting, and the Windows endpoint shows a
prompt with the configurable message, the reason and Accept and Deny buttons.
Nobody answering accepts the connection after a configurable time, and a
computer that has been idle at the lock screen accepts at once; see
[maintenance controls](docs/maintenance-controls.md#connection-approval).

Administrators manage the server in the website: **Users** (invitations,
roles, disabling accounts, resetting two-factor), **Roles** (custom roles
built from a list of permissions), **Authentication** (the sign-in policy,
single sign-on with an OIDC provider, SCIM provisioning and email), **Settings**
(the company's remote-session policy) and **Audit**. Users sign in with a
password and an authenticator app, a passkey, or single sign-on; each manages
their own sign-in methods and sessions under **Account**. The website signs a
user out after the idle time set under **Settings → General**.

**Add device** asks for the platform. For Windows it downloads a setup
executable carrying a single-use enrollment authorization; for macOS it shows
an install command. The computer name comes from the endpoint, the device ID
is generated by the server, and no device record is created until setup
redeems its authorization.

On Windows, opening the viewer once registers the `meshrmm` protocol for the
current user. The macOS application bundle declares the same protocol in its
`Info.plist`.

On the target endpoint, sign in to the website as a user who may enroll
devices, choose **Add device**, select Windows, and download the installer.
Run it within 30 minutes and approve the Windows User Account Control prompt.
Setup reads the Windows computer name, obtains a server-generated device ID and
credential, installs the binary under `%ProgramFiles%\MeshRMM\Agent`, registers
the automatic `MeshRMMAgent` LocalSystem service with recovery actions,
protects its configuration under `%ProgramData%\MeshRMM\Agent`, and starts it.

Deleting a device in the website immediately removes it from inventory and
queues an authenticated self-uninstall. Online Agents remove the service,
binary, configuration, log, and empty MeshRMM directories immediately; offline
Agents perform the same cleanup the next time they connect.

The service remains in Session 0 and supervises a separate worker carrying the
same LocalSystem token in the active console session. This allows
Windows.Graphics.Capture and `SendInput` to target the interactive desktop
without running the Agent under the signed-in user's account. When nobody is
logged on, the service stays available and starts a worker when an interactive
console session appears. `--console` remains available only for local
development.

The Agent supervisor checks for a newer release when the service starts and
every six hours afterward. It stages a verified executable, stops cleanly,
replaces the installed binary from an independent LocalSystem helper, and
restarts the service. If the new service does not reach `Running`, the helper
restores and starts the previous binary. Update-check failures are logged and
do not disconnect the installed Agent.

Technicians download the viewer from the website's Remote app menu, or
copy `dist/remote/` from a local build, and open `meshrmm-remote.exe` once to register the protocol.
Remote sessions are then launched from the website. A viewer can also redeem
a handoff from the command line:

```powershell
.\meshrmm-remote.exe "meshrmm://connect?handoff=<one-time-token>&server=https%3A%2F%2Frmm.example.com"
```

For macOS, copy `MeshRMM Remote.app` to the Mac and open it. The app takes
its server from each link and reads optional local settings from
`remote.json` in its own `Contents/MacOS` directory.

The Windows and macOS clients check the server's update manifest at the start
of each launch from the website. When one is available, the client verifies it, replaces the
installed executable or signed application bundle through a helper, and
relaunches with the already-authorized session so the requested session continues.
If no update is available, or the check cannot reach the release host, the
current client continues immediately. A failed replacement rolls back to the
previous client.

The Agent defaults to the primary display's actual resolution (cropped by at
most one row/column for NV12), 60 FPS, and 12 Mbps. Frame rate and a lower
bitrate can be selected in the protected Agent configuration; the application
always caps the configured bitrate at 12 Mbps. CLI flags remain available for
development overrides. ICE
candidate-pair logs identify a `direct` or `turn` connection; periodic WebRTC
logs include measured RTT.

The viewer also offers independent technician-input blocking, agent-input blocking,
and all-monitor blackout. Administrators customize the blackout notice under
**Settings → Blackout message**. See [maintenance controls](docs/maintenance-controls.md)
for usage, Windows requirements, and cleanup behavior.

**On session close** (**When the session ends → Remote user** in the macOS gear
menu; Troubleshooting settings on Windows) chooses what the Agent does to the Windows session being viewed when the remote
session ends: **No action** (default; **Leave signed in** on macOS), **Lock**, or
**Logout** (**Sign out** on macOS). The choice is not saved: every new remote
session starts with **No action**, so choose it again each session. It is kept across reconnects of the same session.
The action runs when the server ends the session: when the viewer closes it, when
it is closed from the website, or when an unreachable viewer reaches the session
idle timeout. It does not run while the viewer is reconnecting to the same session. The action applies to the console or RDP user session viewed last.
If no user is signed in, or the session was in background mode, nothing happens.
Logout does not save open work. Updating or restarting the Agent service skips
the action.

**Clear clipboard on session close** (**When the session ends → Clear remote
clipboard** in the macOS gear menu; Troubleshooting settings on Windows) empties the clipboard of that same Windows session when the
remote session ends, at the same points as **On session close** and before any
Lock. Administrators set its default for new sessions under **Settings → Remote
sessions**, and choose whether users may change it per session. When allowed, the
viewer's choice applies to the current session only and is kept across reconnects;
otherwise the toggle is shown as company managed and the Agent enforces the
default. Logout skips it because signing out discards the clipboard. Windows
clipboard history (Win+V) is left unchanged.

**Disconnect when idle** (macOS gear menu **Session** section; Windows
**Settings → Advanced**) ends the remote session after the technician has been
idle in the viewer for 5, 10, 15 or 30 minutes, or 1, 2, 4 or 8 hours, or
**Never**. Keyboard, mouse and wheel input over the remote display, viewer
controls that act on the session, and sent chat messages count as activity.
Time spent connecting or reconnecting does not count, and the idle time
restarts once the remote display is back. When it runs out, the viewer ends the
session as if the technician had disconnected (any **On session close**
action runs) and says why. Administrators choose the default under
**Settings → Remote sessions** (**Never** on a new server) and whether
users may choose another time. A user's choice lasts only for that session,
including its reconnects; the next session starts with the company default.

System audio from the Windows default playback device is forwarded to both native
viewers over a separate, bounded WebRTC audio channel, including while muted.
Each new session starts muted. On macOS, check **Play remote audio** in the gear
menu; on Windows, clear **Mute audio** under **Settings → Troubleshooting**.
The choice survives reconnects within the same session. Muting discards buffered
sound. Capture includes the system output mix, not the microphone, and follows
default playback-device changes. Audio uses PCM16 at the source sample rate
(about 1.5 Mbps for 48 kHz stereo); congested queues drop audio to keep it live.

To record the remote display locally, choose **Record video to Downloads** in
the macOS gear menu's **Session** section or Windows **Settings → Troubleshooting**.
A red **REC** item stays in the toolbar while recording; click it, or choose
**Stop recording and save**, to finish. Video-only
Matroska (`.mkv`) files are saved under `Downloads/MeshRMM Recordings/session-…`
on the viewer's computer, and the saved location is shown when stopped. Open
these files in a player supporting H.264/HEVC in MKV, such as VLC.
Recording begins at the next keyframe and creates a new part when the stream
changes (for example, selecting another display or quality). Disconnecting saves
captured video and stops recording; start it again after reconnecting. Audio,
chat, and the viewer's own controls are not recorded. Recording writes to disk
as video arrives on a bounded background writer; stopping releases the recording
control immediately while the writer finishes its remaining queued frames. No
whole-session buffer or final conversion is needed. A disk error or full recording queue stops recording and shows
an error without ending the remote session.

The native viewer sends mouse, wheel, physical keyboard input, and bidirectional
clipboard updates over the reliable control channel: plain text, HTML rich text
with a plain-text alternative, and images. The viewer's current clipboard is
copied to the Agent when the session connects; later copies on either computer
are detected every 250 ms. Rich text and images require updated peers on both
ends. Payloads are capped at 32 MiB (uncompressed RGBA pixels for images) and
sent in paced 60 KiB chunks; larger copies take longer to arrive. File lists
continue to use the file-transfer path. RTF-only formatting is not synchronized.
Turn off **Sync clipboard** (gear menu on macOS; Troubleshooting settings on
Windows) to stop this exchange, including copied files, in both directions. The
choice is saved for the current viewer user and defaults to on. When it is turned
back on, only later copies are sent; content copied while sync was off stays local.
**Type clipboard** (clipboard icon), **Send**/**Receive**, and drag-and-drop are unaffected.

The **pen icon** turns on annotating, which works in **View only** sessions too:
drag on the remote view to draw and right-click to erase. While annotating, the
mouse draws instead of reaching the device, and the keyboard still does (unless
the session is view-only). The Agent draws the red strokes in a click-through
window above every other window on the shown monitor, so the remote user sees
them. Screen capture includes them, so they appear in the video and in
recordings. Clicking the pen again, switching displays, or ending the session
erases the drawing. Annotations are unavailable on the background desktop.

The **folder icon** offers **Send files…** and **Receive files…** using native multi-file/folder
pickers on the source computer. Transfers preserve nested and empty folders and
land in the signed-in user’s `Documents/MeshRMM Transferred Files` folder.
Existing names are preserved by assigning a unique name to incoming duplicates.
The viewer saves files from the device only after **Receive**, one transfer per
request within 15 minutes; it ignores the device's own pick requests and never
accepts pasted or dropped files. Clipboard file copies are limited to 512 MiB
(use **Send** for larger files) and other transfers to 64 GiB, and the receiving
computer must have enough free disk space. Interrupted transfers are removed
after 24 hours, and cached clipboard/drop copies after an hour once they are no
longer on the clipboard.
Received files are tagged like browser downloads on both computers: the macOS
quarantine attribute (with MeshRMM as the downloading app) on files and folders,
and the Internet zone's Mark of the Web on Windows files. Opening a received app
or installer therefore gets the Gatekeeper or SmartScreen check; use **Open
Anyway** (macOS) or **Unblock** in the file's properties (Windows) to trust it.
Drag files from Finder or Explorer onto the remote view to deliver a native
Windows drop at that position (Explorer, desktop, or a browser drop target);
if the target declines the drop, files go to the same Documents folder.
File clipboard changes also synchronize in both directions while **Sync
clipboard** is on. Copy files/folders in Finder or Explorer, then paste into the
destination folder or desktop.
The receiver publishes files after all chunks and SHA-256 checks complete;
a remote paste shortcut waits for the transfer before pasting.
A native progress window appears on the receiving computer: on the Agent for
Send, or on the client for Receive. It shows the current filename, overall
percentage, transferred size, and item count, and closes when the transfer ends.
Files use bounded, acknowledged chunks over the encrypted reliable channel.
The Windows file helper runs as the signed-in user, separately from the
privileged capture/input helper, so pickers and shell actions use that user’s
profile. Both endpoints must run a file-transfer-capable build.

When both computers run a chat-capable version, click the **chat bubble** in the
viewer's top bar to open an attached chat popup. On the Windows agent, click the
MeshRMM Agent icon in the taskbar's notification area (or select it with the
keyboard); its tooltip says when chat is available. The agent popup opens beside
the icon, whether or not the connection banner is shown. Type a message and
choose **Send** or press Enter. Incoming viewer messages automatically open the
agent's chat popup. Incoming agent messages leave the viewer popup closed and
show an unread indicator on its chat icon; opening it clears the indicator. Click the icon again,
click outside the popup, or press Escape to close it. History and unsent drafts
survive closing the popup and switching displays.

Chat uses the reliable, encrypted peer-to-peer control channel; no server update
is needed. Typing in the popup does not send remote keystrokes. Each message is
limited to 4 KiB of UTF-8 text, with the latest 200 messages held in memory and no
disk history. Chat ends with the connection. Switching Windows desktops (for
example, a UAC prompt) recreates the endpoint chat popup and clears its local
history. Older peers can still connect, but chat requires updated builds.

Viewer diagnostics are written to `%LOCALAPPDATA%\MeshRMM\remote.log` on
Windows and `~/Library/Logs/MeshRMM/remote.log` on macOS, and the Agent's to
`%ProgramData%\MeshRMM\Agent\agent.log`. A log is rotated at 10 MiB, keeping
the three previous files (`remote.1.log`, newest, to `remote.3.log`). Periodic
connection and video statistics are logged every 30 seconds; the diagnostics
overlay updates every two.
Pointer coordinates are normalized to the display
currently being streamed and every event carries that display ID, so the Agent
rejects input left over from a previous display after a switch. On Windows,
press **F8** in the viewer to cycle displays. On macOS, use
**Control-Option-Left/Right Arrow**. **F12** shows the diagnostics overlay.
Either shortcut can use F8–F12 or be turned off, which sends the key to the
device: on Windows under **Settings → Keyboard**, on macOS (diagnostics only)
under the gear menu's **Diagnostics shortcut**. While the Windows viewer has
keyboard focus it also sends the Windows key, Alt+Tab, Alt+Esc and Ctrl+Esc to
the device instead of acting on them locally; turn this off under
**Settings → Keyboard**. Ctrl+Alt+Del and Windows+L always stay local. When the
device is a Mac, the macOS viewer's Command key is always Command, and the
Windows viewer's Windows key is Command too. On endpoints with multiple monitors,
select **All displays** from the viewer's monitor menu (also included in
keyboard cycling) to view and control the complete desktop in one window.
The combined view preserves monitor positions, including negative coordinates,
with black space between monitors. Select an individual monitor to return to
its full-resolution view. This option requires an updated Windows Agent and
appears automatically in both native viewers; the primary monitor remains the
default. Combined capture uses GDI with a CPU-to-GPU upload and the usual video
encoder, so frame rate can be lower than individual-monitor GPU capture.
The combined resolution must be supported by the agent's encoder and the
viewer's decoder. The active display name is shown in the
viewer title. The viewer sends input only while its remote-desktop window is in
the foreground. The click that activates an inactive macOS viewer is not
forwarded. Unfocusing the viewer, switching displays, or ending a session
releases held buttons and keys, and any unsent pointer movement is discarded.
Mouse movement, clicks, and wheel input are forwarded only while the pointer is
inside the displayed video. Letterbox bars and positions outside the client
area do not control the Agent, so the local pointer remains free to leave the
remote view. Releasing a drag outside the video still releases the held remote
button without moving the remote pointer.

Endpoint and remote input are collaborative: neither side locks out the other.
The newest local or remote action takes effect. Remote clicks and wheel actions
include their intended pointer position and are injected atomically so physical
endpoint activity cannot split a remote action across two cursor positions.

Closing the viewer or pressing Ctrl+C tears down its peer connection. Capture,
encoder, decoder, and presentation failures terminate only the remote session;
the Agent returns to its signaling loop and remains available.

## Verify

```powershell
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

Push-Location dashboard
npm ci
npm run verify
Pop-Location
```

The server's integration tests run against SQLite, and also against
PostgreSQL when `MESHRMM_TEST_POSTGRES_URL` names a server where they may
create databases, for example
`postgres://postgres:postgres@localhost:5432/postgres`. CI runs both.

The macOS target can be checked on macOS with:

```sh
cargo check --manifest-path remote/Cargo.toml
cargo clippy --manifest-path remote/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path remote/Cargo.toml
```

Compilation verifies API integration, not the physical GPU, two-machine ICE,
or TURN paths. Those require a running server and real devices; do not infer
performance measurements from a successful build.

## Intentional MVP limits

The Agent/capture implementation remains Windows-only; the viewer supports
Windows and macOS. The selected display and captured cursor are streamed.
Clipboard synchronization supports plain text, HTML rich text, images, and files/folders; there is no
recording, browser client, or concurrent
viewer. Windows secure-attention sequences
such as Ctrl+Alt+Delete cannot be synthesized by a normal user-mode Agent. H.265
or H.264 is sent over a purpose-built unreliable WebRTC DataChannel rather than
an RTP track. Encoded video necessarily crosses CPU memory for packetization and
decoder input. Single-monitor Windows capture and decoding keep full-size
images in D3D11 textures; macOS hands compressed samples to AVSampleBufferDisplayLayer and does
not create a CPU RGBA frame in application code.


Rotating a device's credential stages a new one on the Agent. Rotation returns HTTP 202
with `rotation_pending`; the agent commits the credential by reconnecting with it.
The Agent must be online: an offline Agent gets HTTP 409 and nothing is staged.
Rotating again before the Agent reconnects resends the same pending credential,
and the coordinator deletes its copy of the credential once the Agent has
authenticated with it.

An agent accepts one active remote session. A second viewer receives a busy response;
closing the current viewer releases the session.

### Experimental Session 0 background GUI

With an updated Windows agent installed, choose **Connect → Connect to background**
in the website to open the private workspace directly, without first capturing or changing the
user's desktop. You can also connect normally and select
**Background (Session 0 · experimental)** in the viewer's display selector.
The black workspace has a charcoal bottom taskbar with icon launchers and hover labels.
Open windows appear as icons after a gap, each with a persistent underline and
a name on hover. As on the Windows taskbar, this includes dialogs owned by hidden
windows, such as System Properties and Run, but not dialogs owned by a visible
window, such as Find. Minimized windows remain listed; select an icon to restore
its window and bring it to the front. Closing a window removes its icon.
It pins Command Prompt, PowerShell, Registry Editor, Services,
Event Viewer, Resource Monitor, Task Manager, Computer Management, Device Manager,
Windows Firewall, File Explorer, Disk Management, System Properties, Notepad, and
Run. With 15 pins, 11 open windows fit at full width; more share the rest of the
taskbar, and past 16 their icons are cut off. Task Manager, File Explorer, and Run are built-in
MeshRMM tools. Run, also opened with Win+R, takes a program with its arguments, a
document, or a folder. It looks them up like Windows' Run, on the system path and
with `PATHEXT` extensions, and opens documents with their associated program
(`diskmgmt.msc`, `sysdm.cpl`). Folders, `explorer`, and `taskmgr` open the built-in
tools. Programs it starts stay in the workspace. Windows' own Run dialog
(`rundll32 shell32.dll,#61`) isn't used: rundll32 passes it its own entry-point
arguments, and the dialog then silently ignores full paths. The process manager lists processes and supports confirmed End Task;
the file browser navigates folders, previews text, creates folders, renames entries,
and copies individual files without overwriting existing destinations. It opens administrative applications in a private Windows desktop in
Session 0 under SYSTEM. Select a physical monitor to return to the console.
Leaving background mode or closing the session terminates applications launched
in that workspace; save any work first. Changing video quality keeps the workspace
open. The existing Windows and macOS viewers can use this mode without a protocol
update.

The workspace uses a fixed 1280×800 canvas, up to 20 FPS, window capture, and
real mouse and keyboard input from a separate helper. While it is open, the
workspace makes its desktop Session 0's input desktop, which real input needs,
sets Session 0's display mode to 1280×800 (it idles at 1024×768), sets the work
area to end above the 48-pixel taskbar, so maximized and newly opened windows fit
the canvas, and has the wheel scroll the window under the pointer. None of these
changes is saved; closing the workspace restores the previous desktop, mode, work
area, and wheel setting. The workspace makes one lasting change to the machine:
it creates SYSTEM's Desktop folder, `%SystemRoot%\System32\config\systemprofile\Desktop`,
and the matching folder under `SysWOW64` on 64-bit Windows, if they're missing.
Without them, every Open and Save dialog first reports that the Desktop is
unavailable. If Windows refuses the mode, applications keep laying out
for Session 0's smaller screen, and the pointer can't reach past it.
Applications get the same input as from a local mouse and keyboard: menus open and
run from clicks, double-clicks open items, right-click menus appear at the pointer,
and scrollbars, caption drags, and resizing are Windows' own. A program started
from the taskbar or Run comes to the front when its first window appears. Disk
Management's disk pane doesn't handle the wheel itself; its scrollbar works.
Window images are retained between captures so slow or failed repaints do not
make already-captured windows disappear when the refresh budget expires.
Application-specific rendering limitations can still cause flicker. Session 0 has no Windows shell, so apart from
Win+R, Windows-key shortcuts do nothing: the Windows key, and keys pressed while it
is held, never reach applications. The Apps key opens the selected item's context menu. It does not switch the console desktop or move the
console pointer. Console audio, clipboard synchronization, file-transfer UI, chat,
blackout, input blocking, and Ctrl+Alt+Del are unavailable in background mode;
the toolbox's scripts and files work there.
Operations performed inside the workspace still affect the same machine, and
SYSTEM has a different profile and network credentials from the signed-in user.

This is a prototype for traditional Win32 administration tools, not a complete
Explorer login session. Applications that depend on the user's shell or modern
GPU-composited UI may not render or respond correctly. Session 0 has no desktop compositor, so the workspace makes each
window layered and copies the image Windows keeps for it. Printing windows
instead copied them before their controls finished painting. Composited windows,
and windows their application already layers itself, are still printed. The
built-in tools avoid the shell dependencies of Windows Task Manager and Explorer. Text previews are read-only
and limited to 1 MiB; recursive folder copying and shell file associations are not
supported. Background mode is currently entered after a normal connection; it
does not yet provide a separate background-only connection from the website.

The ignored native test `remote::background::tests::session_zero_gui` exercises
the launcher, text input, PowerShell keyboard input, Registry Editor rendering, H.264 output, Session 0
placement, and application cleanup. The ignored tests in
`remote::background::input_tests` check menus, right-click menus, double-clicks, scrollbars, and the
wheel on test windows, Registry Editor, Services, and Disk Management. Run each in
a dedicated SYSTEM process in Session 0 with no active background workspace. It writes
`meshrmm-background-gui.bmp` to that process's temporary directory. The capture
helper is disposable: a ten-second frame watchdog restarts it if a window stalls
Windows' synchronous capture API, while input remains in a separate helper.

### Sending Ctrl+Alt+Del

Use the keyboard icon (**Send Ctrl+Alt+Del**) in the Windows or macOS remote client's toolbar to send the
secure attention sequence to the Windows agent. Macs have no such sequence, so
the viewer leaves the button out when the device is a Mac. Update both the client and agent
for this command. The agent must run through its installed Windows service.

When Windows policy blocks service-generated secure attention, the agent temporarily
allows it locally, calls `SendSAS`, and restores the previous registry value (or
removes it if it was absent). This also applies to settings delivered by domain GPO;
the domain GPO itself is not changed. Already-permitted settings are left untouched.
Registry access or restoration failures are reported in the client. A different
value written by a concurrent policy refresh is preserved. Forced process termination
or a machine crash during the override can prevent restoration; `SendSAS` itself
returns no delivery status.
See [Microsoft's SendSAS documentation](https://learn.microsoft.com/en-us/windows/win32/api/sas/nf-sas-sendsas).
