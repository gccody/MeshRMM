# MeshRMM

MeshRMM is a self-hosted remote monitoring and management tool with
low-latency remote desktop. A company runs one server, installs the Agent on
its computers, and its technicians connect to them with a native viewer. The
server is a single Linux program that serves the website, the API, the Agents'
and viewers' connections, STUN/TURN, and the Agent and viewer downloads.
Nothing calls a hosted service.

MeshRMM is in heavy development and not production ready.

## Features

**Remote desktop**

- Native viewers for Windows and macOS, opened from the website with a
  one-time link. Video goes peer to peer over WebRTC, and through the
  server's built-in TURN relay only when no direct path exists.
- Hardware H.265 or H.264 at up to 60 FPS, in 4:2:0 or crisp-text 4:4:4, with
  software H.264 as the final fallback at both ends. Four quality presets
  from 1 to 12 Mbps, with the bitrate adapting to the network.
- Keyboard, mouse and wheel input, Ctrl+Alt+Del, and every monitor separately
  or all of them in one view. Console, Remote Desktop and background sessions
  are listed separately.
- Clipboard sync for text, rich text, images and files.
- File transfer in both directions: pickers, drag and drop, and copy and
  paste, with folders, checksums and a progress window.
- Chat between the technician and the person at the computer.
- System audio, local session recording to MKV, and on-screen annotations.
- Sessions reconnect by themselves, survive a restart of the remote computer,
  and can restart it into Safe Mode with Networking and back.
- A virtual monitor for Windows computers that have none connected.

**Unattended and background work**

- Background mode: a private desktop in Windows Session 0 with its own
  taskbar, Task Manager, File Explorer, Run dialog and the usual
  administration tools, which the signed-in user never sees.
- A toolbox of PowerShell, Command Prompt and zsh scripts and a file library,
  private or shared, run and delivered from the website or the viewer as the
  signed-in user or as SYSTEM/root.
- Saved Windows credentials, encrypted on the device, that fill sign-in and
  UAC prompts with one click.

**Devices**

- A live device wall: online state, a screen thumbnail, and CPU and memory
  gauges on every tile, updated over a WebSocket.
- Resource monitoring: CPU, memory, network, uptime and disk space, live and
  as charts for the last hour, day or week.
- Enrollment with a single-use installer (Windows) or install command
  (macOS). Agents and viewers update themselves from the server and check a
  release signature first.

**Control for the company and the user**

- A session banner, a connection notification, and optional approval by the
  computer's user before a technician connects.
- Blocking the user's input, blacking out the screens with a message, a
  border on the viewed monitor, and locking, signing out or clearing the
  clipboard when a session ends.
- Company policy for all of these, set by administrators in the website.

**Accounts and access**

- Built-in accounts with passwords, authenticator apps, recovery codes and
  passkeys, plus optional single sign-on (OIDC) and SCIM provisioning.
- Custom roles built from a list of permissions, and an audit log of
  sign-ins, changes, sessions, script runs and file deliveries.
- Email for invitations and resets is optional; without it the website gives
  one-time links to copy.

**Self-hosting**

- One static binary or Docker image for x86_64 and arm64 Linux.
- SQLite or PostgreSQL.
- TLS from Let's Encrypt, from certificate files, or from a reverse proxy.
- Optional signing and notarization of the macOS builds with the company's
  own Developer ID.

## Supported platforms

| Component | Runs on |
|---|---|
| Server | Linux, x86_64 or arm64 |
| Agent | Windows 10 version 1903 or newer; macOS 12.3 or newer (in development) |
| Viewer | Windows 10 version 1903 or newer; macOS 12 or newer (releases ship an Apple silicon build) |
| Website | A current browser |

## Set up

You need a Linux machine, a DNS name with an A record to it, and TCP 443,
UDP 3478 and UDP 49160–49200 open from the internet.

1. Start the server. It gets its certificate from Let's Encrypt:

   ```sh
   docker volume create meshrmm-data
   docker run -d --name meshrmm --restart unless-stopped \
     -p 443:443/tcp -p 3478:3478/udp -p 49160-49200:49160-49200/udp \
     -v meshrmm-data:/var/lib/meshrmm \
     -e MESHRMM_PUBLIC_URL=https://rmm.example.com \
     -e MESHRMM_TLS__MODE=acme \
     -e MESHRMM_TLS__CONTACT_EMAIL=admin@example.com \
     ghcr.io/gccody/meshrmm-server:latest
   ```

2. Open the one-time setup link from `docker logs meshrmm` and create the
   first administrator.
3. Invite technicians under **Users**.
4. On each computer to manage, sign in to the website, choose **Add device**,
   and run the Windows installer or the macOS command it gives you within 30
   minutes.
5. On each technician's computer, download the viewer from the website's
   **Remote app** menu and open it once. **Connect** on a device then opens
   the session.

[Running a MeshRMM server](docs/self-hosting.md) covers the rest: installing
from the tarball with systemd, the other TLS modes, PostgreSQL, TURN, signing
the macOS builds, backups, upgrades and troubleshooting.

## Build from source

You need rustup, CMake and a C compiler, and Node.js 22.13 or newer. See
[development](docs/development.md) for the details and for running a local
server.

```sh
cargo build --release            # the Agent and viewer for this platform
cargo build -p meshrmm-server    # the server; build the website first
cd dashboard && npm ci && npm run verify
```

## Documentation

For operators:

- [Running a MeshRMM server](docs/self-hosting.md)
- [Releases](docs/releases.md): what a release contains, signatures, and
  how to publish one

For developers:

- [Architecture](docs/architecture.md): the components, how a session is
  set up, and how the server and the Agent are built
- [Development](docs/development.md): building, running, testing and
  installing local builds
- [Devices](docs/devices.md): enrollment, the device list, resource
  monitoring, updates and removal
- [Remote sessions](docs/remote-sessions.md): what the viewer does and how
  each feature behaves
- [Video](docs/video.md): capture, encoding, negotiation and presentation
- [Maintenance controls](docs/maintenance-controls.md): input blocking,
  blackout, notification, approval, restart and Safe Mode
- [Background mode](docs/background-mode.md): the Session 0 workspace and its
  built-in tools
- [Toolbox](docs/toolbox.md): scripts and files
- [Credential autofill](docs/credential-autofill.md)
- [Screen thumbnails](docs/screen-thumbnails.md)
- [macOS Agent](docs/macos-agent.md)
- [Transport security](docs/transport-security.md)
- [The website](dashboard/README.md) and [the marketing page](site/README.md)
