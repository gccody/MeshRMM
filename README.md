# PulseRMM

PulseRMM is a multi-tenant remote monitoring and management project. It adds
low-latency Windows desktop streaming and remote control while retaining the
repository's three independent Rust workspaces:

- `agent/` — the native endpoint Agent, shared remote protocol, and the
  Windows-specific `windows/remote-screen` capture/encoder package.
- `server/` — the Rust Cloudflare Worker and Durable Objects used only for
  authentication and WebRTC signaling.
- `remote/` — the native Windows/macOS viewer. Windows uses Media Foundation
  and D3D11; macOS uses AVFoundation, CoreMedia, Core Animation, and AppKit.

## MVP data path

```text
Windows.Graphics.Capture (BGRA ID3D11Texture2D)
  -> D3D11 video processor (pooled NV12 ID3D11Texture2D)
  -> Media Foundation hardware H.264 encoder
  -> latest encoded frame slot
  -> unordered/unreliable WebRTC DataChannel (12 KiB fragments)
  -> latest-only reassembly
  -> platform-native video presentation
       Windows: Media Foundation H.264 decoder (DXGI surface) -> D3D11
       macOS: AVSampleBufferDisplayLayer -> Core Animation/AppKit window
```

Cloudflare is not in that data path. An Agent coordinator Durable Object keeps
the authenticated Agent reachable, and a temporary remote-session Durable
Object forwards bounded JSON SDP/ICE messages between exactly one Agent and one
client. The P2P connection itself is encrypted by WebRTC DTLS. Cloudflare TURN
credentials are created server-side with the configured idle-timeout lifetime
and are used by ICE only when a direct candidate pair cannot connect.

## Prerequisites

- Windows 10 version 1903 or newer for the Agent and Windows viewer.
- macOS 12 or newer for the macOS viewer.
- A current stable Rust MSVC toolchain.
- A GPU/driver exposing Media Foundation hardware H.264 encode and decode
  transforms plus D3D11 NV12 video processing.
- Node.js/npm and a Cloudflare account for the one-time server deployment.
- The `wasm32-unknown-unknown` Rust target.

```powershell
rustup target add wasm32-unknown-unknown
```

There is deliberately no software codec fallback. Startup fails with a
contextual error if the required hardware path is unavailable.

## Preconfigured deployment

The applications load sidecar JSON next to each executable. No environment
variables are required on the Agent or viewer machines. The production control
plane is deployed at `https://pulsermm.gccody.dev`. WorkOS organizations and
memberships define the company boundary, Cloudflare D1 stores company-owned
Agent records and audit events, and Durable Objects retain live signaling state.

The following commands are only for rebuilding the portable binaries. The
Agent wrapper automatically copies the finished executable into `dist/agent/`
without changing its `agent.json`:

```powershell
& .\scripts\build-agent.ps1
cargo build --release --manifest-path remote/Cargo.toml
Copy-Item remote/target/release/pulsermm-remote.exe dist/remote/
```

On a Mac, build an application bundle using the `dist/remote/remote.json`
sidecar:

```sh
sh scripts/build-remote-macos.sh
open "dist/remote-macos/PulseRMM Remote.app"
```

The script builds for the Mac it runs on, copies the Cloudflare API URL into
the app bundle, and applies an ad-hoc signature. A browser deep link supplies a
60-second, single-use handoff token when a remote session starts.
Developer ID signing and notarization are still required before distributing
the app through normal Gatekeeper-protected download channels.

Creating a new TURN key requires an explicit secure API token with Cloudflare
Calls Write permission. `scripts/provision-cloudflare.ps1` installs only those
TURN secrets. Agent credentials are created or rotated per company in the
dashboard and are stored in D1 only as SHA-256 hashes.

Keep `dist/agent/agent.json` only with the target Agent. It contains the
one-time Agent credential. `dist/remote/remote.json` contains no viewer
credential. Browser handoffs expire after 60 seconds. Remote sessions use the
sliding `REMOTE_SESSION_IDLE_TIMEOUT_SECONDS` idle timeout (900 seconds by
default): a connected viewer renews the deadline every 30 seconds, and the
session expires after the viewer stops reporting activity for the configured
interval. TURN credentials use the same configured lifetime when issued.

For local Worker development, bind a local D1 database, put the two TURN values
in an ignored `server/.dev.vars` file, and use `npx wrangler dev`. Cloudflare
TURN credential generation still needs real credentials and outbound access.

## Run

### Web dashboard

The responsive dashboard at `https://pulsermm.gccody.dev` lists only Agents
owned by the organization in the current WorkOS token and refreshes connection
state every 15 seconds. **Remote** requests a one-time server handoff, then
opens the native viewer with a `pulsermm://connect?handoff=...&server=...` deep
link. No service credential is entered into or retained by the browser.

Company administrators use the embedded WorkOS user-management, domain, and
SSO widgets to invite users, assign roles, verify domains, and configure a SAML
or OIDC identity provider. **Add Agent** returns a real `agent.json` enrollment
configuration once; the dashboard never substitutes sample inventory.

The Worker exposes organization-scoped Agent, enrollment, and handoff APIs. On
Windows, opening the viewer once registers the `pulsermm` protocol for the
current user. The macOS application bundle declares the same protocol in its
`Info.plist`.

Copy `dist/agent/` to the target Windows computer, then double-click
`pulsermm-agent.exe` or run:

```powershell
.\pulsermm-agent.exe
```

Copy `dist/remote/` to the authorized viewer computer and open
`pulsermm-remote.exe` once to register the protocol. Remote sessions should
then be launched from the dashboard. A viewer can also redeem a handoff from
the command line:

```powershell
.\pulsermm-remote.exe "pulsermm://connect?handoff=<one-time-token>&server=https%3A%2F%2Fpulsermm-server.gccody2010.workers.dev"
```

For macOS, copy `PulseRMM Remote.app` from `dist/remote-macos/` to the
authorized Mac and open it. The app reads `remote.json` from its own
`Contents/MacOS` directory; it connects to the same preconfigured Cloudflare
deployment as the Windows viewer.

The Agent defaults to the primary display's actual resolution (cropped by at
most one row/column for NV12), 60 FPS, and 12 Mbps. These values can be edited
in `agent.json`. CLI flags remain available for development overrides. ICE
candidate-pair logs identify a `direct` or `turn` connection; periodic WebRTC
logs include measured RTT.

The native viewer sends mouse, wheel, and physical keyboard input over the
reliable control channel. Pointer coordinates are normalized to the display
currently being streamed and every event carries that display ID, so the Agent
rejects input left over from a previous display after a switch. On Windows,
press **F8** in the viewer to cycle displays. On macOS, use
**Control-Option-Left/Right Arrow**. The active display name is shown in the
viewer title. Unfocusing the viewer, switching displays, or ending a session
releases held buttons and keys.

Endpoint and remote input are collaborative: neither side locks out the other.
The newest local or remote action takes effect. Remote clicks and wheel actions
include their intended pointer position and are injected atomically so physical
endpoint activity cannot split a remote action across two cursor positions.

Closing the viewer or pressing Ctrl+C tears down its peer connection. Capture,
encoder, decoder, and presentation failures terminate only the remote session;
the Agent returns to its signaling loop and remains available.

## Verify

```powershell
cargo fmt --manifest-path agent/Cargo.toml --all -- --check
cargo check --manifest-path agent/Cargo.toml --workspace
cargo clippy --manifest-path agent/Cargo.toml --workspace --all-targets -- -D warnings
cargo test --manifest-path agent/Cargo.toml --workspace

cargo fmt --manifest-path remote/Cargo.toml --all -- --check
cargo check --manifest-path remote/Cargo.toml
cargo clippy --manifest-path remote/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path remote/Cargo.toml

cargo fmt --manifest-path server/Cargo.toml --all -- --check
cargo check --manifest-path server/Cargo.toml --target wasm32-unknown-unknown
cargo clippy --manifest-path server/Cargo.toml --all-targets -- -D warnings
cargo test --manifest-path server/Cargo.toml
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
Windows and macOS. The selected display and captured cursor are streamed; there
is no audio, clipboard, file transfer, recording, simultaneous multi-monitor
view, browser client, or concurrent viewer. Windows secure-attention sequences
such as Ctrl+Alt+Delete cannot be synthesized by a normal user-mode Agent. H.264
is sent over a purpose-built unreliable WebRTC DataChannel rather than an RTP
track. Encoded H.264 necessarily crosses CPU memory for packetization and decoder
input. Windows keeps full-size captured and decoded images in D3D11 textures;
macOS hands compressed samples to AVSampleBufferDisplayLayer and does not create
a CPU RGBA frame in application code.
