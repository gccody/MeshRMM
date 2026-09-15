# MeshRMM

MeshRMM is a multi-tenant remote monitoring and management project. It adds
low-latency Windows desktop streaming and remote control in a single Cargo
workspace with platform and transport responsibilities kept in focused crates:

- `agent/` — the native endpoint Agent, binary control/video protocol, and the
  Windows-specific `windows/remote-screen` capture/encoder package.
- `crates/` — lightweight shared JSON protocol types and signaling client code.
- `server/` — the Rust Cloudflare Worker and Durable Objects used only for
  authentication and WebRTC signaling.
- `remote/` — the native Windows/macOS viewer. Windows uses Media Foundation
  and D3D11; macOS uses AVFoundation, CoreMedia, Core Animation, and AppKit.

## MVP data path

```text
Windows.Graphics.Capture (BGRA ID3D11Texture2D)
  -> D3D11 video processor (pooled NV12 4:2:0 or AYUV 4:4:4 texture)
  -> Media Foundation hardware H.265/H.264 encoder
  -> latest encoded frame slot
  -> unordered/unreliable WebRTC DataChannel (12 KiB fragments)
  -> latest-only reassembly
  -> platform-native video presentation
       Windows: Media Foundation H.265/H.264 decoder (DXGI surface) -> D3D11
       macOS: AVSampleBufferDisplayLayer -> Core Animation/AppKit window
```

The viewer advertises only codec/chroma profiles for which it can initialize a
hardware decoder. The Agent prefers H.265/HEVC within the selected chroma mode,
then falls back through the mutually supported hardware profiles if encoder or
playback initialization fails. Windows viewers can select bandwidth-efficient
4:2:0 or crisp-text 4:4:4 when the GPU driver exposes the required AYUV and
High 4:4:4/RExt hardware path. Unsupported 4:4:4 controls are disabled and
macOS currently advertises 4:2:0 only. Quality remains independently
configurable as Data saver (3 Mbps), Balanced (6 Mbps), or Best quality (up to
12 Mbps).

The hardware encoders use a streaming-oriented CBR configuration: real-time
and low-latency modes, no B-frames/reordering, a short recovery GOP, an
approximately one-frame VBV with a 16 KiB detail floor, a speed-biased
quality-versus-speed setting, and an optional maximum-QP guard. The video data
channel drains and declares congestion using bitrate-relative time budgets
instead of fixed byte counts, keeping the three quality presets similarly
responsive.

Cloudflare is not in that data path. An Agent coordinator Durable Object keeps
the authenticated Agent reachable, and a temporary remote-session Durable
Object forwards bounded JSON SDP/ICE messages between exactly one Agent and one
client. The P2P connection itself is encrypted by WebRTC DTLS. Cloudflare TURN
credentials are created server-side with the configured idle-timeout lifetime
and are used by ICE only when a direct candidate pair cannot connect.

## Prerequisites

- Windows 10 version 1903 or newer for the Agent and Windows viewer.
- Local administrator approval to install the Agent as a Windows service.
- macOS 12 or newer for the macOS viewer.
- A current stable Rust MSVC toolchain.
- A GPU/driver exposing Media Foundation hardware H.264 encode and decode
  transforms plus D3D11 NV12 video processing. Hardware H.265/HEVC and AYUV
  4:4:4 support are optional and negotiated only when available at both ends.
- Node.js/npm and a Cloudflare account for the one-time server deployment.
- The `wasm32-unknown-unknown` Rust target.

```powershell
rustup target add wasm32-unknown-unknown
```

There is deliberately no software codec fallback. Startup fails with a
contextual error if the required hardware path is unavailable.

## Preconfigured deployment

The viewer loads sidecar JSON next to its executable. The installed Agent reads
its protected configuration from `%ProgramData%\MeshRMM\Agent\agent.json`.
No environment variables are required on Agent or viewer machines. The public
site is deployed at `https://meshrmm.com`; the owner console lives at
`https://admin.meshrmm.com`, and each company uses an immutable
`https://<slug>.meshrmm.com` dashboard and Agent control-plane endpoint. WorkOS
organizations and memberships define the identity boundary, Cloudflare D1
stores company-owned Agent records and audit events, and Durable Objects retain
live signaling state. See [company domains and provisioning](docs/company-domains.md)
for production setup and onboarding details.

### Automated native releases

`release.json` is the single source of truth for the deployed Agent and viewer
version. To publish an update, increase only its `version` field, commit the
change, and push it to `main`:

```json
{
  "version": "0.2.2",
  "download_origin": "https://meshrmm.com",
  "viewer_server": "https://api.meshrmm.com"
}
```

The **Publish native release** GitHub Actions workflow validates that the
version increased, builds the Windows Agent plus Windows and Apple Silicon
macOS viewers, generates one verified release manifest, and deploys the
dashboard containing all update assets. The only required GitHub Actions secret is
`CLOUDFLARE_API_TOKEN`; configure the optional Apple signing and notarization
secrets before distributing production macOS builds. See
[automated native releases](docs/native-releases.md) for the one-time setup and
recovery procedure.

For local builds, the following wrappers read the same `release.json`. The
Agent wrapper copies the finished generic executable into both `dist/agent/`
and the dashboard's ignored `public/downloads/` release directory, then writes
its SHA-256 file and release-manifest entry. The remote-client wrapper does the
same for the Windows client:

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

On a Mac, build an application bundle. The wrapper generates its non-secret
`dist/remote/remote.json` sidecar from `release.json` (or accepts an explicit
sidecar path as its first argument):

```sh
sh scripts/build-remote-macos.sh
open "dist/remote-macos/MeshRMM Remote.app"
```

To build and install this checkout locally on your Mac without publishing a
release, run:

```sh
sh scripts/install-remote-macos.sh
```

This builds a release-mode viewer with ad-hoc signing, closes any running viewer,
installs it in `~/Applications/MeshRMM Remote.app`, and registers dashboard links.
Use a fresh **Connect** link to start a session. Existing installs
are retained in a `.meshrmm-backup.*` folder under `~/Applications`.
An optional JSON configuration path can be passed as the first argument.
The installed configuration sets `auto_update` to `false` so production releases
cannot replace the local build. Normal builds default to automatic updates;
reinstall a normal release to restore that behavior. This script does not change
`release.json`, generate dashboard download assets, or deploy anything.
For a local bundle without installation, use `sh scripts/build-remote-macos.sh --local`.

The script builds for the Mac architecture it runs on, copies the Cloudflare
API URL into the app bundle, signs it, and publishes a zipped update artifact
plus the corresponding manifest entry. Run it on each macOS architecture that
you distribute. A browser deep link supplies a 60-second, single-use handoff
token. The viewer redeems it before checking for updates and carries the resulting
session through an update relaunch. A session awaiting its first viewer has a
15-minute startup window; connected sessions use the configured sliding timeout.
Developer ID signing and notarization are still required before distributing
the app through normal Gatekeeper-protected download channels.
Set `MESHRMM_CODESIGN_IDENTITY` to the Developer ID Application certificate
name before running the script for a production archive; the default is an
ad-hoc development signature.

The native update version is compiled from `release.json`; Cargo package
metadata is not used to decide whether an update is newer. Manifest and release
URLs must use HTTPS. Each updater verifies the downloaded artifact against the
manifest's SHA-256 digest before replacing anything.

Existing installations are repaired without replacing their device identity. New
installations persist a private recovery key and pending configuration so a failed
or interrupted enrollment can be retried. Installer authorization remains limited
to its original expiry; recovery is restricted to the endpoint holding that key.

Creating a new TURN key requires an explicit secure API token with Cloudflare
Calls Write permission. `scripts/provision-cloudflare.ps1` installs only those
TURN secrets. Agent credentials are created or rotated per company in the
control plane and are stored in D1 only as SHA-256 hashes.

Each Agent installer downloaded from the dashboard contains a random,
single-use company enrollment authorization that expires after 30 minutes.
During setup, the endpoint reads its Windows computer name and redeems that
authorization. The server generates the device ID and Agent credential, creates
the company-owned Agent record, and returns the protected runtime configuration.
Delete the downloaded installer after it succeeds.
The installed copy stores only the executable under Program Files and protects
the credential under ProgramData so only LocalSystem and administrators can
read it. `dist/remote/remote.json` contains no viewer credential. Browser
handoffs expire after 60 seconds. Remote sessions use the
sliding `REMOTE_SESSION_IDLE_TIMEOUT_SECONDS` idle timeout (900 seconds by
default): a connected viewer renews the deadline every 30 seconds, and the
session expires after the viewer stops reporting activity for the configured
interval. TURN credentials use the same configured lifetime when issued.

For local Worker development, bind a local D1 database, put the two TURN values
in an ignored `server/.dev.vars` file, and use `npx wrangler dev`. Cloudflare
TURN credential generation still needs real credentials and outbound access.
Apply the D1 migrations before deploying a Worker that exposes the installer
endpoints:

```powershell
Push-Location server
npx wrangler d1 migrations apply DB --remote
Pop-Location
```

## Run

### Web dashboard

The responsive dashboard at `https://<company>.meshrmm.com` lists only Agents
owned by the organization bound to that hostname and the current WorkOS token. It receives inventory
and connection changes from a company-scoped, hibernating WebSocket event
stream instead of polling. The browser obtains a 60-second, one-use
subscription token and reconnects automatically. Each inventory connection must
reauthorize at least every five minutes; **Refresh** requests a single
authoritative snapshot when needed. **Remote** requests a one-time server
handoff, then opens the native viewer with a
`meshrmm://connect?handoff=...&server=...` deep link. No service credential is
entered into or retained by the browser.

Company administrators can use **Close session** beside an Agent's **Remote**
button to disconnect its viewer and clear a stale session reservation. The Agent
stays online and can accept a fresh connection immediately. This action is safe
to repeat when no session exists and is recorded in the company audit log.

During desktop sharing, the Windows endpoint shows a translucent banner at the
top of the primary screen with the connected user's dashboard name (WorkOS
first and last name, or email when no name is set). Click the banner to collapse
it to a tiny arrow tab with no name; click again to expand it. Drag either state
sideways to move it out of the way; its horizontal position persists when toggling. It does not take keyboard
focus and disappears when capture stops. The server resolves the name from the
authenticated handoff owner and retains it across session reconnects. Deploy
both the server and updated Windows Agent to enable named banners; older
session records use “Remote user”.

Company administrators use the embedded WorkOS user-management, domain, and
SSO widgets to invite users, assign roles, verify domains, and configure a SAML
or OIDC identity provider. The application has no company switcher; a user with
multiple memberships signs into another company by visiting its URL. Company
administrators can also set an organization-specific dashboard
idle timeout under **Profile & session**; new organizations default to four
hours. WorkOS's application-wide inactivity timeout must be at least as long as
the largest permitted organization policy (24 hours) so WorkOS's session policy
does not expire sessions first. The dashboard allows AuthKit to renew tokens
while its page is in the background. **Add Agent** asks only for the
installer platform and downloads a company-authorized Windows setup executable.
The computer name comes from the endpoint, the device ID is generated by the
server, and no Agent record is created until setup redeems its authorization.
The dashboard never substitutes sample inventory or exposes a loose
`agent.json`.

The Worker exposes organization-scoped Agent, enrollment, and handoff APIs. On
Windows, opening the viewer once registers the `meshrmm` protocol for the
current user. The macOS application bundle declares the same protocol in its
`Info.plist`.

On the target endpoint, sign in to the dashboard as a company administrator,
choose **Add Agent**, select **Windows 10/11 (x64)**, and download the installer.
Run it within 30 minutes and approve the Windows User Account Control prompt.
Setup reads the Windows computer name, obtains a server-generated device ID and
credential, installs the binary under `%ProgramFiles%\MeshRMM\Agent`, registers
the automatic `MeshRMMAgent` LocalSystem service with recovery actions,
protects its configuration under `%ProgramData%\MeshRMM\Agent`, and starts it.

Deleting an Agent from the dashboard immediately removes it from inventory and
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

Copy `dist/remote/` to the authorized viewer computer and open
`meshrmm-remote.exe` once to register the protocol. Remote sessions should
then be launched from the dashboard. A viewer can also redeem a handoff from
the command line:

```powershell
.\meshrmm-remote.exe "meshrmm://connect?handoff=<one-time-token>&server=https%3A%2F%2Fapi.meshrmm.com"
```

For macOS, copy `MeshRMM Remote.app` from `dist/remote-macos/` to the
authorized Mac and open it. The app reads `remote.json` from its own
`Contents/MacOS` directory; it connects to the same preconfigured Cloudflare
deployment as the Windows viewer.

The Windows and macOS clients check for a newer release at the start of each
dashboard launch. When one is available, the client verifies it, replaces the
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
and all-monitor blackout. Company admins customize the blackout notice under
**Profile & session**. See [maintenance controls](docs/maintenance-controls.md)
for usage, Windows requirements, and cleanup behavior.

The native viewer sends mouse, wheel, physical keyboard input, and bidirectional
plain-text clipboard updates over the reliable control channel. The viewer's
current text clipboard is copied to the Agent when the session connects; later
text copies on either computer are mirrored within 250 ms. Clipboard payloads
are capped at 60 KiB to stay within the control channel's message limit, and
rich text and images remain local.

The **folder icon** offers **Send** and **Receive** using native multi-file/folder
pickers on the source computer. Transfers preserve nested and empty folders and
land in the signed-in user’s `Documents/MeshRMM Transferred Files` folder.
Existing names are preserved by assigning a unique name to incoming duplicates.
Drag files from Finder or Explorer onto the remote view to deliver a native
Windows drop at that position (Explorer, desktop, or a browser drop target);
if the target declines the drop, files go to the same Documents folder.
File clipboard changes also synchronize in both directions. Copy files/folders
in Finder or Explorer, then paste into the destination folder or desktop.
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
viewer's top bar or the Windows agent's connection banner to open an attached
chat popup. The banner keeps its chat button when collapsed. Type a message and
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
Windows and `~/Library/Logs/MeshRMM/remote.log` on macOS.
Pointer coordinates are normalized to the display
currently being streamed and every event carries that display ID, so the Agent
rejects input left over from a previous display after a switch. On Windows,
press **F8** in the viewer to cycle displays. On macOS, use
**Control-Option-Left/Right Arrow**. On endpoints with multiple monitors,
select **All monitors** from the viewer's display dropdown (also included in
keyboard cycling) to view and control the complete desktop in one window.
The combined view preserves monitor positions, including negative coordinates,
with black space between monitors. Select an individual monitor to return to
its full-resolution view. This option requires an updated Windows Agent and
appears automatically in both native viewers; the primary monitor remains the
default. Combined capture uses GDI with a CPU-to-GPU upload and hardware video
encoding, so frame rate can be lower than individual-monitor GPU capture.
The combined resolution must be supported by the agent's hardware encoder and
the viewer's decoder. The active display name is shown in the
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
cargo check -p meshrmm-server --target wasm32-unknown-unknown

Push-Location dashboard
npm ci
npm run verify
Pop-Location
```

The macOS target can be checked on macOS with:

```sh
cargo check --manifest-path remote/Cargo.toml
cargo clippy --manifest-path remote/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path remote/Cargo.toml
```

Compilation verifies API integration, not the physical GPU, two-machine ICE,
or TURN paths. Those require the deployment and hardware smoke test described
above; do not infer performance measurements from a successful build.

## Intentional MVP limits

The Agent/capture implementation remains Windows-only; the viewer supports
Windows and macOS. The selected display and captured cursor are streamed.
Clipboard synchronization supports plain text and files/folders; there is no audio,
recording, browser client, or concurrent
viewer. Windows secure-attention sequences
such as Ctrl+Alt+Delete cannot be synthesized by a normal user-mode Agent. H.265
or H.264 is sent over a purpose-built unreliable WebRTC DataChannel rather than
an RTP track. Encoded video necessarily crosses CPU memory for packetization and
decoder input. Single-monitor Windows capture and decoding keep full-size
images in D3D11 textures; macOS hands compressed samples to AVSampleBufferDisplayLayer and does
not create a CPU RGBA frame in application code.


Apply migration `0007_recoverable_enrollment.sql` before deploying the audit fixes.
The new enrollment and credential-rotation queries require its columns. Publish
updated native binaries with the server change to enable recoverable enrollment,
acknowledged credential rotation, and cancellation cleanup on endpoints. Older
installers retain one-shot enrollment, and older agents retain their current token
when they do not understand the staged rotation command. Rotation returns HTTP 202
with `rotation_pending`; the agent commits the credential by reconnecting with it.

An agent accepts one active remote session. A second viewer receives a busy response;
closing the current viewer releases the session. Suspending a company revokes its
live coordinator/session/inventory connections and blocks token redemption.
