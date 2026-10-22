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
- A remote terminal in the browser: PowerShell, Command Prompt, or a login
  shell, bash, zsh or sh, as SYSTEM/root or the signed-in user, in tabs, with
  no remote session needed, so headless servers get a shell too.
- A toolbox of PowerShell, Command Prompt and shell (zsh or bash) scripts and a file library,
  private or shared, run and delivered from the website or the viewer as the
  signed-in user or as SYSTEM/root.
- Jobs that run a script or send a file on chosen devices, whole companies,
  tagged devices or every device, skip devices without the script's interpreter, wait for
  offline ones, and show each device's result and output live.
- Automation policies that run shared scripts, scan for and install
  updates, or wake offline devices, every few minutes or hours, daily or weekly at a time in any time
  zone, once, or on each newly enrolled device, and notify when their jobs
  fail.
- Patch management: Agents scan for pending Windows Update, macOS Software
  Update, APT, DNF and pacman updates, with their category, severity and
  whether they need a restart; install chosen updates, categories such as
  security only, or everything, on one device or many, and restart never,
  if required or always after a warning; per-update results and a history,
  a fleet overview of pending security updates, failed installs and pending
  restarts, and alerts when updates or restarts stay pending.
- Saved Windows credentials, encrypted on the device, that fill sign-in and
  UAC prompts with one click.

**Devices**

- A live device wall: online state, a screen thumbnail, and CPU and memory
  gauges on every tile, updated over a WebSocket.
- Companies to group devices by, such as each customer an MSP supports:
  filter the wall by company and enroll new devices straight into one.
- Tags in nine colors for every other grouping: tag devices one at a time
  or in bulk, filter the wall by any or all of some tags, and target jobs,
  policies, alert rules and the Updates and Software pages at them.
- Notes on each device, with who last edited them, a short history, and a
  warning instead of a silent overwrite when two people edit at once.
- Resource monitoring: CPU, memory, network, uptime and disk space, live and
  as charts for the last hour, day or week.
- Hardware and software: operating system, model, serial number, processor,
  memory, disks, network adapters and signed-in users, and the installed
  applications with a change log, on each device's page; a fleet-wide search
  for which devices have an application in which version.
- Alerts: rules for devices going offline, low disk space, sustained CPU or
  memory load, failed scripts, updates or restarts pending too long, and
  failed update installs, for every device, chosen companies or tags.
  Alerts are acknowledged and resolved in the website, resolve themselves
  when the condition passes, and notify by email or signed webhook.
- Wake-on-LAN: wake offline devices, one or many, from the website or a
  policy before a patch window. Online Agents on the same network send the
  magic packet, and the website shows which ones did and when the device
  came online.
- Enrollment with a single-use installer (Windows) or install command
  (macOS and Linux). Agents and viewers update themselves from the server and
  check a release signature first.
- Linux computers with or without a desktop: servers get monitoring, a
  terminal, scripts and files, and X11, GNOME, KDE Plasma and wlroots
  desktops (sway, Hyprland) remote sessions too.

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
  sign-ins, changes, sessions, terminals, script runs, file deliveries,
  update installs, wakes and jobs.
- Personal API tokens for scripts and integrations: each acts as its user,
  optionally limited to some permissions, expires, and is named in the audit
  log for everything done with it. Administrators can see and revoke every
  token, cap their lifetime, or turn them off.
- Email for invitations, resets and alerts is optional; without it the
  website gives one-time links to copy, and alerts go to webhooks.

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
| Agent | Windows 10 version 1903 or newer; macOS 12.3 or newer (in development); Linux, x86_64 or arm64, with systemd and glibc 2.28 or newer |
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
   and run the Windows installer or the macOS or Linux command it gives you
   within 30 minutes.
5. On each technician's computer, download the viewer from the website's
   **Remote app** menu and open it once to install it. **Connect** on a
   device then opens the session.

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

For operators and integrators:

- [Running a MeshRMM server](docs/self-hosting.md)
- [The API](docs/api.md): API tokens, errors, lists, and examples with curl
- [Releases](docs/releases.md): what a release contains, signatures, and
  how to publish one

For developers:

- [Architecture](docs/architecture.md): the components, how a session is
  set up, and how the server and the Agent are built
- [Development](docs/development.md): building, running, testing and
  installing local builds
- [Devices](docs/devices.md): enrollment, companies, tags, notes, the
  device list, resource monitoring, updates and removal
- [Wake-on-LAN](docs/wake-on-lan.md): waking offline devices through online
  ones on their network, and what the sleeping computer needs
- [Remote sessions](docs/remote-sessions.md): what the viewer does and how
  each feature behaves
- [Video](docs/video.md): capture, encoding, negotiation and presentation
- [Maintenance controls](docs/maintenance-controls.md): input blocking,
  blackout, notification, approval, restart and Safe Mode
- [Background mode](docs/background-mode.md): the Session 0 workspace and its
  built-in tools
- [Remote terminal](docs/terminal.md): shells in the browser, and how
  their traffic and flow control work
- [Toolbox](docs/toolbox.md): scripts and files
- [Patch management](docs/patch-management.md): scanning for and installing
  operating system updates
- [Automation](docs/automation.md): jobs on many devices and scheduled
  policies
- [Hardware and software](docs/hardware-and-software.md): what the Agents
  collect, how the server stores and searches it, and its API
- [Alerts](docs/alerts.md): rules, alerts and notification channels
- [Credential autofill](docs/credential-autofill.md)
- [Screen thumbnails](docs/screen-thumbnails.md)
- [macOS Agent](docs/macos-agent.md)
- [Linux Agent](docs/linux-agent.md)
- [Platform parity](docs/platform-parity.md): what the Agent does on
  Windows, macOS and Linux, and why the gaps exist
- [Transport security](docs/transport-security.md)
- [The website](dashboard/README.md) and [the marketing page](site/README.md)
