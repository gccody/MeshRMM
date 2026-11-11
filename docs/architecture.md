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
                          │ downloads: installers, update manifest, install scripts        │
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
| `agent/` | `meshrmm-agent`: the Windows service, the macOS daemon and the Linux systemd service, their installers and updaters, and everything a remote session does on the device (`src/remote/`; the Unix session helpers in `src/remote/helper/`). |
| `agent/protocol/` | `meshrmm-protocol`: the binary control, input and video protocol the Agent and viewer speak over WebRTC. |
| `agent/windows/remote-screen/` | `meshrmm-remote-screen`: Windows capture, colour conversion and encoding, and the background desktop's window capture. |
| `remote/` | `meshrmm-remote`: the viewer. `src/platform/windows` uses Media Foundation and D3D11; `src/platform/macos` uses AVFoundation, CoreMedia, Core Animation and AppKit. |
| `server/` | `meshrmm-server`: the self-hosted server. Embeds the website. |
| `dashboard/` | The website: Vite, React and React Router, prerendered at build time. See [its README](../dashboard/README.md). |
| `site/` | The static marketing page, which no server serves. See [its README](../site/README.md). |
| `crates/protocol-types/` | JSON types shared by the server, Agent and viewer: IDs, signaling, settings, metrics, toolbox. |
| `crates/signaling-client/` | The WebSocket client for the server's Agent and session sockets, with the shared TLS settings. |
| `crates/session-transport/` | WebRTC data-channel plumbing shared by both peers: service channels, backpressure, and the persistent DTLS identity. |
| `crates/audio/`, `chat/`, `clipboard/`, `file-transfer/` | One session service each, with its native parts for Windows, macOS and Linux. |
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
  starts. On SQLite every transaction and every statement other than a
  `SELECT` goes through one writer connection, so writes queue in the server
  instead of contending for SQLite's lock; `SELECT`s go to a pool of
  `query_only` connections. A transaction holds the writer until it ends, so
  it must not wait on a write outside itself or on slow work: what only
  reads comes first, outside it (creating a job on many devices, for one,
  reads them all before a transaction that only inserts). A pool that
  stays exhausted for its acquire timeout fails the request with `503` and
  `Retry-After`.
- `http`: the router, client IP resolution, same-origin checks, security
  headers and compression. The website's pages and the `/v1` and SCIM
  responses that are JSON of 1 KiB or more are compressed (brotli or gzip, at
  level 1 for the API); sockets, images and file downloads are sent as they
  are. `website`: the embedded pages.
- `serve`: the listener, on axum-server, in each TLS mode: socket options,
  the timeouts for a TLS handshake and for HTTP to start, hyper's
  header-read timeout and HTTP/2 pings, TLS session tickets, and certificate
  file reloads. See [connections](self-hosting.md#connections).
- `auth`: passwords (Argon2id), TOTP and recovery codes, passkeys, OIDC and
  server-side sessions. `scim`: SCIM 2.0 users and groups; the lookups an
  identity provider sends for each user and group on every sync (`userName`,
  `externalId`, `id`, `displayName` or a member equal to a value) run and
  page in the database, and any other filter is evaluated on the rows they
  select. `rbac`: the
  permission list and roles. `audit`, `mail`.
- `api`: the `/v1` handlers, one file per area.
- `realtime`: see below.
- `turn`: the built-in STUN/TURN server and its per-session credentials.
- `downloads`: the release's Agent and viewer builds, the update manifest,
  and Developer ID signing of the macOS builds.
- `secrets`: the instance key, which encrypts secrets in the database and
  signs TURN credentials.
- `storage`: files in the data directory (thumbnails, toolbox library),
  written atomically. Library files are flushed to disk before they take
  their name; thumbnails, which Agents send again, are not.
- `system_info`: devices' hardware and software snapshots, the application
  change log and their searches. See
  [hardware and software](hardware-and-software.md).
- `alerts`: alert rules, the engine that judges them and the alerts they
  raise. `notifications`: email and webhook channels, each with a bounded
  queue and one worker that delivers it in order. See
  [alerts](alerts.md).
- `monitors`: monitor rules' settings, sending each Agent the monitors that
  apply to it, taking in their results, and self-healing (`remediation`).
  See [monitors and self-healing](monitors.md).
- `automation`: jobs (a script, file, update scan, update install,
  application install, update or uninstall, or wake on many devices), the dispatcher that starts them a few at a time, and policies
  that start jobs on a schedule or on enrollment. See
  [automation](automation.md).
- `updates`: each device's pending operating system updates, update scans
  and installs and their history. See [patch management](patch-management.md).
- `applications`: the application library, each device's third-party
  packages, application runs and their history, and what application jobs
  and policies do. See [application deployment](application-deployment.md).
- `wake`: Wake-on-LAN requests, choosing the online devices that relay
  them (looked up by index among the devices on the woken devices'
  subnets, not read from every online device), and their reports. See
  [Wake-on-LAN](wake-on-lan.md).
- `terminals`: remote terminals that are being opened or are open, and the
  relay between each one's browser and Agent sockets. See
  [remote terminal](terminal.md).
- `files`: the file browser's requests to Agents, its downloads and uploads,
  and the relay between each transfer's HTTP request and the socket its
  Agent opened. See [file browser](files.md).
- `maintenance`: the periodic task that purges expired tokens, old runs,
  resolved alerts, old application changes, old update runs, old wake
  requests and leftover partial files. It deletes each table's due rows in
  batches of 2,000 (100 for script runs and update installs), each its own
  short statement with a pause after it, so other writers never wait long
  for the database. It runs until nothing is due. A table that fails is
  logged and tried again at the next run, and the other tables are still
  purged.
- `admin`: the `meshrmm-server admin` commands for locked-out operators.

### Realtime

Each connected thing is an actor: a tokio task that owns its state and
sockets and takes messages over a channel, so one ID is always handled by one
task.

- **Coordinator**, one per connected Agent (`/v1/agents/{id}/connect`):
  authentication, superseding an older connection of the same Agent,
  revocation, commands (session requests, scripts, file deliveries,
  uninstall, system information, the update scan schedule, update scans and
  installs, terminals,
  wake packets, file requests and transfers, monitors and self-healing
  actions),
  credential rotation, the session lease and update grace.
- **Session actor**, one per remote session
  (`/v1/remote/sessions/{id}/signal`): the signaling relay, the idle
  deadline, resume, end and expiry. Its record is persisted in
  `remote_sessions`.
- **Presence**: a broadcast channel. Coordinators publish Agents going
  online, offline and updating; the enrollment, deletion, company, tag and
  notes handlers publish catalog changes, including companies and tags
  themselves. Website sockets (`/v1/events`, authenticated by the
  session cookie and `Origin`) get a snapshot, then revision-numbered changes.
  Presence reads the database outside the lock that numbers changes, so
  publishers never queue behind each other's reads, and sockets that ask for
  a snapshot at the same time share one read of the fleet.
  Disabling a user or removing their `devices.view` permission closes their
  sockets at once (see [access changes](#access-changes)).
  What every socket sends alike is serialised once for all of them: each
  change and snapshot, alert event and update event is broadcast as a
  `Shared` event (`server/src/realtime/shared.rs`), whose JSON the first
  socket to send it makes and the others send as the same bytes. Only job
  events, shaped for each viewer, and each socket's message of the readings
  it watches are made per socket.
- **Metrics**: the latest resource readings of each Agent, held in memory.
  Its writer task stores the finished minutes of every device together
  every 5 seconds, in one transaction. Each event socket watches the
  devices its tab shows, and the `Watched` registry
  (`server/src/realtime/metrics/watched.rs`) counts the sockets watching
  each device, dropping a device when its last socket stops watching it or
  closes, however it closes. A task can follow one device being watched
  with `Watched::listen`, a `tokio::sync::watch` channel set on each change
  between watched and not; nothing listens yet. While any event socket is
  open, one feed task (`server/src/realtime/metrics/feed.rs`) scans every 5
  seconds for the watched devices' readings since its last scan, looking
  through the watched devices or the online ones, whichever are fewer, and
  broadcasts them. Each socket picks out its own devices' readings from the
  smaller of the tick and its set, so a tick costs a socket what it
  watches, not the fleet; each reading's JSON is made once, by the first
  socket that sends it, and copied into every socket's message. A socket
  that starts watching a device sends its latest reading at once, and one
  that refreshes or falls behind sends every watched device's again; each
  socket keeps the number of the last reading it sent per device, so none
  is sent twice. See [devices](devices.md#resource-monitoring).
- **Alerts**: the engine's task reads presence and the metrics, keeping each
  device's readings only as far back as the enabled CPU and memory rules
  look, raises and resolves alerts, and broadcasts them; event sockets of users who may see
  alerts pass them on. See [alerts](alerts.md).
- **Monitors**: the engine's task sends each connected Agent its monitors
  when they change, comparing what each was last sent: for the devices a
  presence change names, and for every connected device on rule changes,
  deleted tags and every minute, at most once per 5 seconds. The set sent on
  connecting says what the server holds of each monitor's latest result, and
  the Agent answers with only the results that differ. The Agent sends
  failures and changes at once and passes of passing monitors every 15
  minutes; each message's results are matched against what the device was
  sent, kept in one statement and judged against one read of its open
  alerts. Self-healing attempts are made on the alert engine's passes. See
  [monitors](monitors.md).
- **Automation**: the engine's task fires due policies, starts jobs' runs on
  connected devices and broadcasts jobs whose counts changed; event sockets
  pass on the jobs each user may see. See [automation](automation.md).
- **Updates**: the engine's task marks update runs that never reported as
  lost and asks connected devices with old scans for new ones; scans,
  progress and installs are broadcast to event sockets of users with
  `updates.view`. See [patch management](patch-management.md).
- **Applications**: the engine's task marks application runs that never
  reported as lost and removes runs older than a year. Agents report scans,
  progress and results on the control connection; the website reads them
  again while a run is in progress. See
  [application deployment](application-deployment.md).
- **Terminals**: each open terminal is the task of its browser socket
  (`/v1/terminals/{id}`), which relays to the socket the Agent opened for it
  (`/v1/agents/{id}/terminals/{terminal_id}`), so terminal traffic never
  shares an Agent's control connection. See [remote terminal](terminal.md).
- **File transfers**: each download or upload is the HTTP request that
  carries it, relaying to the socket the Agent opened for it
  (`/v1/agents/{id}/file-transfers/{transfer_id}`), one piece at a time.
  Listings and changes are answered over the control connection, file
  contents never. See [file browser](files.md).

An Agent that loses its control connection, or cannot open it, tries again
after 1 second, doubling the wait to at most 2 minutes, and starts over only
once a connection has lasted 20 seconds, so an Agent the server accepts and
then drops (superseded by another machine with the same device ID, say)
does not reconnect every second. Each wait is cut by up to half at random,
so a fleet that lost a restarting server together spreads its reconnects
out. A server that answers the handshake with 401, 403 or 404 does not know
the device's credential; the Agent keeps asking, in case the device is
restored, from every 2 minutes growing to once an hour
(`agent/src/remote/signaling.rs`).

On the server, connecting a device with nothing pending only reads: a dozen
statements about that device, none of them a write, so a fleet reconnecting
after a restart doesn't queue for SQLite's write lock. Each step that may
record something (the device was woken, it connects from a new address, an
application run was interrupted) looks first and writes only then. The update
scan schedule the Agent is sent only reads the device's last scan: Agents
keep the schedule themselves (see [patch management](patch-management.md)). The instance settings are kept in memory until they are
saved (`settings::Cache` on `AppState`), the monitor rules for 15 seconds,
and a device's update status is read only while a website listens for it.
`Database::counts` counts statements, writes and transactions, which
`server/tests/realtime.rs` holds the connect to.

### Sign-in and permissions

The website and the API share one origin, so the website uses an HttpOnly
`__Host-meshrmm-session` cookie backed by server-side sessions. Requests that
change something must carry the server's own `Origin` and an
`X-MeshRMM-Request` header. Agents authenticate with a bearer credential the
server stores only as a SHA-256 hash; viewers with handoff and session tokens.

Wrong passwords are limited in memory (`AuthState` in
`server/src/auth/mod.rs`), and only failures count: 30 per client address
(an IPv6 /64 is one address) and 10 per account and address in 15 minutes,
so a guesser uses up only their own attempts. Across addresses an account
takes 50 wrong passwords in 15 minutes, the bound on guessing from many
addresses. A full sign-in leaves a signed, HttpOnly
`__Host-meshrmm-device` cookie naming the account
(`server/src/auth/devices.rs`); attempts from a browser presenting it get 10
of their own and skip the account-wide limit, so a stranger's failures can't
keep the owner out of a browser they have signed in on. Wrong second-factor
codes are limited to 10 per user in 15 minutes, across challenges; unknown
emails are counted like known ones.

Scripts and integrations use personal [API tokens](api.md) (`Authorization:
Bearer mrmm_pat_…`, also stored as SHA-256 hashes). The `Authorized` extractor
(`server/src/auth/session.rs`) accepts a session or a token
(`server/src/api_tokens.rs`); a token's permissions are its scope
intersected with its user's roles at each request, and its requests skip the
CSRF check, since a browser can't attach the header cross-site. `SignedIn`
(account routes) takes sessions only, and `website_only` in
`server/src/api/mod.rs` refuses tokens on account security, credential and
sign-in settings, invitations, email settings and SSO links, changes to
users and roles, and the website's sockets.

Authenticating a request reads the session's row (or the token's) every
time, so an ended session or a revoked token stops working on the next
request. The settings come from `settings::Cache`, and the user, their roles
with permissions and their second factors from `auth::accounts::Accounts`
on `AppState`, so a warm request runs that one statement (plus the
`last_seen_at` or `last_used_at` write at most once a minute), where it ran
eight. An account is kept only for the access generation it was loaded in
(see below): every change that can alter it bumps the generation before
its response is sent, so the next request reads the database again, and a
load that raced a change isn't kept. Entries also last at most 30 seconds,
for changes made outside the server; past 1,000 users the oldest go.
`server/tests/request_authentication.rs` holds a warm request to one
statement.

Roles are built from these permissions (`server/src/rbac.rs`):

`devices.view`, `devices.edit`, `devices.enroll`, `devices.delete`,
`devices.rotate_credentials`, `devices.wake`, `companies.manage`,
`tags.manage`, `sessions.connect`,
`sessions.connect_background`, `sessions.close_any`, `terminal.connect`,
`terminal.connect_system`, `scripts.run`,
`scripts.manage_shared`, `files.deliver`, `files.manage_shared`,
`files.browse`, `files.manage`, `files.system`,
`jobs.run_wide`, `automation.manage`, `updates.view`, `updates.install`,
`applications.view`, `applications.deploy`, `applications.manage`,
`alerts.view`, `alerts.respond`, `alerts.manage`, `users.manage`, `roles.manage`, `settings.manage`, `authentication.manage`,
`api_tokens.manage`, `audit.view`.

Administrator holds every permission and can't be deleted or emptied;
Technician is an editable default. Single sign-on, SCIM and email settings
are limited to administrators, because whoever controls them can sign in as
anyone.

The server keeps an administrator who can sign in.
`rbac::remaining_administrators` counts an enabled user holding the Administrator role (directly, or through
a SCIM or SSO group) who has a password or a passkey, or whose account is
linked to SSO while SSO is turned on. An account SSO or SCIM created has no
password, so it counts only while SSO is on and after its first SSO sign-in.
Every change that could leave nobody counted is refused inside its
transaction: taking the role away, disabling or deleting a user (from the
website, the API or SCIM), unlinking a user's SSO identity, and turning SSO
off, removing it or moving it to another issuer (which unlinks everyone). A
sign-in through SSO keeps the groups that hold the last such administrator's
role. The `meshrmm-server admin` commands are not limited by this; they are
the way back in.

When no account exists, the server logs a one-time setup link. Without it
nobody can create the first administrator.

#### Access changes

Open event sockets, terminals and remote sessions keep checking that their
user may still use them, and close when the user is signed out, disabled or
loses the permission they need. Each checks when presence's access
generation is bumped (`Presence::recheck_access`), and event sockets and
terminals also every 30 seconds (`auth::accounts::ACCESS_RECHECK`). The
same generation is what keeps the accounts authentication holds valid, so
the bump that wakes a socket has already made its check read the user
afresh.

The generation is bumped by changes to a user, their roles or their second
factors, and by anything else that can take access away, after they are
committed:

- any successful write to the routes `server/src/api/mod.rs` groups as
  changing access: signing in and out, setup, accepting an invitation and
  completing a password reset; the account's own security (profile,
  password, second factors, passkeys, sessions); API tokens; and
  administration (users, invitations, roles, instance, sign-in, SMTP, SCIM
  and SSO settings). A new route in those groups signals by default. A write
  there that changed nothing, such as signing out a browser without a
  session, says so with `AccessUnchanged`;
- an SSO sign-in (`GET /v1/auth/sso/callback`), which reads the user's
  groups again;
- SCIM requests that change or delete a user, or change the members of, or
  delete, a group that grants a role.

Everything else (devices, tags, notes, companies, scripts, files, alerts,
automation, applications) changes no one's access and doesn't signal, so a
busy server doesn't make every open socket check its access for nothing.
Deleting a device ends its remote session directly. A change made outside
the server process, such as `meshrmm-server admin`, can't signal: it applies
within 30 seconds, when the cached account expires and the sockets' periodic
check runs.

## The Windows Agent

The Agent is one executable that runs in several roles, chosen by its
arguments:

- **The service** (`MeshRMMAgent`, LocalSystem, Session 0) keeps the control
  connection, supervises everything else, runs scripts and file deliveries,
  runs [monitors](monitors.md) on their own scheduler thread, and checks for
  updates.
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
  keep running while command handling waits, and reading goes on while a
  large message is written. Application messages use bounded queues; a full
  queue closes the connection instead of blocking the network task. A
  signaling socket write fails once the socket has taken nothing for 30
  seconds, with no limit in total, so a large report still gets through a
  slow uplink; while the server takes such a write in, the connection counts
  as alive though its pongs wait behind it. Control-channel writes have a
  five-second deadline.
- An update or application report the control connection fails to send goes
  first on the next connection, but only until three connections in a row
  have failed to send it. It is then dropped, and the server counts the runs
  it answers as lost, so one report can't keep the device reconnecting.
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

## The Linux Agent

A systemd service runs the root coordinator, which asks logind for the
graphical sessions and starts a session helper in each, as the session's
user, over a socket pair. The helpers speak the macOS helpers' protocol
(`agent/src/remote/helper/`). See [Linux Agent](linux-agent.md).

## The viewer

The viewer takes its server from each link, so one install works with any
server. Opened from anywhere but its install location, such as the
Downloads folder, it installs itself for the current user first: on Windows
in `%LOCALAPPDATA%\Programs\MeshRMM Remote`, where it registers the
`meshrmm` URL scheme; on macOS in `~/Applications` (a viewer in
`/Applications` stays there), where the bundle's `Info.plist` registers the
scheme. See [remote sessions](remote-sessions.md#installing-the-viewer).
Network work, decoding and each session service run off the UI thread, with
their own send queues, so local clipboard work cannot hold up input.

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
scheme and [devices](devices.md#agent-updates) for what the Agent does.
