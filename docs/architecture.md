# Architecture

MeshRMM has four programs: the server, the website it serves, the Agent on
each managed computer, and the viewer on each technician's computer. One
server serves one company.

```text
                          ┌──────────────────────── meshrmm-server ────────────────────────┐
browser ── https ──────►  │ website (embedded, prerendered)   /v1 API (axum)               │
viewer  ── https/wss ──►  │ realtime: Agent coordinators · session actors · presence       │
Agent   ── https/wss ──►  │ auth: passwords, TOTP, passkeys, OIDC, SCIM, roles, audit      │
viewer/Agent ── udp ───►  │ STUN/TURN (3478 + relay port range)                            │
                          │ downloads: installers, update manifest, macOS install script   │
                          │ TLS: ACME | certificate files | proxy       maintenance task   │
                          └─────────┬───────────────────────────┬──────────────────────────┘
                                SQLite | PostgreSQL         data dir (files, ACME, keys)

viewer ◄──────────── WebRTC data channels (DTLS), direct or through TURN ────────────► Agent
```

The server is not in the video path. It keeps each Agent's control
connection, and for each remote session forwards bounded JSON SDP and ICE
messages between exactly one Agent and one viewer. Everything else in a
session goes over the peers' WebRTC connection.

## Repository layout

One Cargo workspace. `cargo build` builds the Agent and the viewer
(`default-members`); the server is built by name.

| Path | What it is |
|---|---|
| `agent/` | `meshrmm-agent`: the Windows service and macOS daemon, their installers and updaters, and everything a remote session does on the device (`src/remote/`). |
| `agent/protocol/` | `meshrmm-protocol`: the binary control, input and video protocol the Agent and viewer speak over WebRTC. |
| `agent/windows/remote-screen/` | `meshrmm-remote-screen`: Windows capture, colour conversion and encoding, and the background desktop's window capture. |
| `remote/` | `meshrmm-remote`: the viewer. `src/platform/windows` uses Media Foundation and D3D11; `src/platform/macos` uses AVFoundation, CoreMedia, Core Animation and AppKit. |
| `server/` | `meshrmm-server`: the self-hosted server. Embeds the website. |
| `dashboard/` | The website: Vite, React and React Router, prerendered at build time. See [its README](../dashboard/README.md). |
| `site/` | The static marketing page, which no server serves. See [its README](../site/README.md). |
| `crates/protocol-types/` | JSON types shared by the server, Agent and viewer: IDs, signaling, settings, metrics, toolbox. |
| `crates/signaling-client/` | The WebSocket client for the server's Agent and session sockets, with the shared TLS settings. |
| `crates/session-transport/` | WebRTC data-channel plumbing shared by both peers: service channels, backpressure, and the persistent DTLS identity. |
| `crates/audio/`, `chat/`, `clipboard/`, `file-transfer/` | One session service each, with its native parts for Windows and macOS. |
| `crates/self-update/` | Update manifest checks, release signatures and the platform updaters. |
| `crates/log-file/` | The rotating log file writer. |
| `vendor/dtls/`, `vendor/turn/` | Patched copies of two webrtc-rs crates. Each has a `MESHRMM.md` that says what changed and when to drop the patch. |
| `packaging/` | The Dockerfile, the systemd unit and the tarball's `install.sh`. |
| `scripts/` | Build, packaging and local install scripts. |

## How a session is set up

1. **Handoff.** A signed-in user chooses **Connect**. The website asks the
   server for a handoff: a random token that works once, for 60 seconds. It
   opens `meshrmm://connect?handoff=…&server=…`, which starts the viewer. The
   browser never holds a longer-lived credential.
2. **Redeem.** The viewer redeems the handoff over HTTPS. The server creates
   the session record, with the company's policy and the user's name resolved
   at that moment, and returns a session token, the ICE servers and
   per-session TURN credentials. The viewer redeems before it checks for an
   update, and carries the session through an update relaunch.
3. **Request.** The server sends the session request to the Agent over the
   control WebSocket the Agent already holds. An Agent accepts one session at
   a time; a second viewer gets a busy response.
4. **Signaling.** Both peers connect to the session's signaling WebSocket and
   exchange SDP and ICE candidates through the server.
5. **WebRTC.** The peers connect directly when they can, and through the
   server's TURN relay otherwise. Each checks the other's DTLS certificate
   against the fingerprint signaling delivered. See
   [transport security](transport-security.md).

A session that has not seen its first viewer has at least 15 minutes to
start, long enough for the computer's user to answer an approval prompt.
After that it has a sliding deadline, `remote.idle_timeout_seconds` (900 by
default), which a connected viewer renews every 30 seconds. A viewer that
loses its connection resumes the same session; the session record is in the
database, so this works across a server restart. When the viewer disconnects
on purpose it posts to the session's `/end` endpoint, which is idempotent, so
the Agent is free at once. TURN credentials work only while the session
lasts.

### Channels

| Channel | Delivery | Carries |
|---|---|---|
| Control (`meshrmm-control-v5`) | Reliable, ordered | Input, display and quality selection, maintenance commands, cursor and status updates |
| Video | Unordered, at most one retransmission | Encoded frames in 12 KiB fragments; the viewer reassembles only the latest frame |
| Audio | Ordered, no retransmission | 48 kHz stereo Opus, or PCM16 when a peer has no Opus |
| Chat, clipboard, files | Reliable, one channel each | The matching service |

Each service picks its channel once, after the peers exchange capabilities,
and never switches mid-transfer. The channels share the connection's
bandwidth but not each other's queues: a stalled file transfer does not delay
input.

## The server

`meshrmm-server` is a tokio and axum program. Its modules, under
`server/src/`:

- `config`: the TOML file with environment overrides.
- `db`: the connection pool, migrations and data access for both database
  backends. Queries are written once with `sea-query` and run through `sqlx`,
  because the backend is chosen at run time. Each backend has its own
  migrations directory (`migrations/sqlite`, `migrations/postgres`), kept in
  step by `tests/schema_parity.rs`. The server applies migrations when it
  starts.
- `http`: the router, client IP resolution, same-origin checks and security
  headers. `website`: the embedded pages.
- `auth`: passwords (Argon2id), TOTP and recovery codes, passkeys, OIDC and
  server-side sessions. `scim`: SCIM 2.0 users and groups. `rbac`: the
  permission list and roles. `audit`, `mail`.
- `api`: the `/v1` handlers, one file per area.
- `realtime`: see below.
- `turn`: the built-in STUN/TURN server and its per-session credentials.
- `downloads`: the release's Agent and viewer builds, the update manifest,
  and Developer ID signing of the macOS builds.
- `secrets`: the instance key, which encrypts secrets in the database and
  signs TURN credentials.
- `storage`: files in the data directory (thumbnails, toolbox library),
  written atomically.
- `maintenance`: the periodic task that purges expired tokens, old runs and
  leftover partial files.
- `admin`: the `meshrmm-server admin` commands for locked-out operators.

### Realtime

Each connected thing is an actor: a tokio task that owns its state and
sockets and takes messages over a channel, so one ID is always handled by one
task.

- **Coordinator**, one per connected Agent (`/v1/agents/{id}/connect`):
  authentication, superseding an older connection of the same Agent,
  revocation, commands (session requests, scripts, file deliveries,
  uninstall), credential rotation, the session lease and update grace.
- **Session actor**, one per remote session
  (`/v1/remote/sessions/{id}/signal`): the signaling relay, the idle
  deadline, resume, end and expiry. Its record is persisted in
  `remote_sessions`.
- **Presence**: a broadcast channel. Coordinators publish Agents going
  online, offline and updating; the enrollment and deletion handlers publish
  catalog changes. Website sockets (`/v1/events`, authenticated by the
  session cookie and `Origin`) get a snapshot, then revision-numbered changes.
  Disabling a user or removing their `devices.view` permission closes their
  sockets.
- **Metrics**: the latest resource readings of each Agent, held in memory.
  See [devices](devices.md#resource-monitoring).

### Sign-in and permissions

The website and the API share one origin, so the website uses an HttpOnly
`__Host-meshrmm-session` cookie backed by server-side sessions. Requests that
change something must carry the server's own `Origin` and an
`X-MeshRMM-Request` header. Agents authenticate with a bearer credential the
server stores only as a SHA-256 hash; viewers with handoff and session tokens.

Roles are built from these permissions (`server/src/rbac.rs`):

`devices.view`, `devices.enroll`, `devices.delete`,
`devices.rotate_credentials`, `sessions.connect`,
`sessions.connect_background`, `sessions.close_any`, `scripts.run`,
`scripts.manage_shared`, `files.deliver`, `files.manage_shared`,
`users.manage`, `roles.manage`, `settings.manage`, `authentication.manage`,
`audit.view`.

Administrator holds every permission and can't be deleted or emptied;
Technician is an editable default. Single sign-on, SCIM and email settings
are limited to administrators, because whoever controls them can sign in as
anyone.

When no account exists, the server logs a one-time setup link. Without it
nobody can create the first administrator.

## The Windows Agent

The Agent is one executable that runs in several roles, chosen by its
arguments:

- **The service** (`MeshRMMAgent`, LocalSystem, Session 0) keeps the control
  connection, supervises everything else, runs scripts and file deliveries,
  and checks for updates.
- **A worker** with the same LocalSystem token runs in the active console
  session, so Windows.Graphics.Capture and `SendInput` reach the interactive
  desktop without the Agent running as the signed-in user. When nobody is
  logged on the service stays available and starts a worker when a console
  session appears.
- **Desktop helpers**, one process per job: capture, input, clipboard, chat
  and banner, file transfer (as the signed-in user, so pickers and shell
  actions use their profile), the connection notification and approval
  prompts, and the tray icon. When the technician switches to another
  Windows session the helpers move together.
- **Background helpers** in Session 0 for [background mode](background-mode.md).

`--console` runs everything in the current session for development.

### Isolation rules

A slow or hung part of the Agent must not stall the rest. The code keeps to
these rules:

- Each WebSocket has its own network task. Ping, pong and liveness detection
  keep running while command handling waits. Application messages use bounded
  queues; a full queue closes the connection instead of blocking the network
  task. Socket and control-channel writes have a five-second deadline.
- Capture start, display changes, desktop recovery and stop run on a
  cancellable native thread with a bounded command queue, never on a tokio
  worker. Dropping it does not join the thread.
- Input callbacks only enqueue. A dedicated input worker keeps their order
  and releases pressed keys when cancelled. An input queue overflow ends the
  session, so a key-up is never silently lost.
- Clipboard, chat, files and maintenance commands each have their own native
  worker. Each parent-to-helper pipe has its own writer thread, a bounded
  queue and a byte budget, so a full or stalled pipe cannot block its caller.
- Services are event driven. Producers notify consumers when they queue
  output, and consumers register for a channel's open, close and buffer-low
  events before checking its state, so no wakeup is missed. A channel that
  stays full for five seconds fails the send. Native clipboard detection is
  the exception: it polls every 250 ms.
- The file log has a bounded writer queue. A stalled disk drops log records,
  and counts them, instead of stalling the threads that log.
- Native helpers are dispatched before the Agent enters its main tokio
  runtime, so a helper never starts a runtime inside another.

`scripts/test-helper-isolation.ps1` suspends one installed helper for a while,
to check on a real device that the others keep working.

## The macOS Agent

A launchd daemon runs the root coordinator, and a launchd agent runs a
session helper in each graphical session. See [macOS Agent](macos-agent.md).

## The viewer

The viewer takes its server from each link, so one install works with any
server. It registers the `meshrmm` URL scheme (on Windows when first opened,
on macOS through the bundle's `Info.plist`). Network work, decoding and each
session service run off the UI thread, with their own send queues, so local
clipboard work cannot hold up input.

Before the remote display first appears, a failed connection is retried at
most three times within a minute, then ends with an explanation. Afterwards
the viewer keeps reconnecting until the user gives up, and shows why, for how
long, and when the next attempt starts (`remote/src/reconnect.rs`).

## Updates

A release is one version of the server, the Agent and the viewer. The server
serves its release's builds and writes their update manifest itself, so
upgrading a server upgrades its Agents and viewers. Every updater checks the
release signature and the SHA-256 before replacing anything, and rolls back
if the new build does not start. See [releases](releases.md) for the signing
scheme and [devices](devices.md#updates) for what the Agent does.
