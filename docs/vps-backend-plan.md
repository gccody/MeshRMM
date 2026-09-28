# VPS backend plan

Replaces the Cloudflare Worker and Durable Objects in `server/` with a plain Rust server hosted on a
VPS. Work happens on branch `vps-backend`, cut from `main` at `fb89beb`. Line numbers and file
contents were taken at `fb89beb`; search for the quoted code rather than trusting a number.

This document is only the plan. No code changes are part of the PR that introduces it.

## Goals and decisions

- **Rewrite, not a port.** MeshRMM is not in production, so old Agents, viewers and dashboard builds
  do not need to keep working. Server, Agent, viewer and dashboard changes land together.
- **As simple as possible without losing a user-facing feature.** The Durable Object design exists to
  minimize Cloudflare cost: hibernation, attachments, storage write-through, alarms, and per-object
  usage metering. None of that carries over. One long-running process with ordinary WebSockets
  replaces it.
- **Users still sign in at `company.meshrmm.com`.** The dashboard Worker keeps serving each company
  hostname and keeps binding the signed-in user to that company. See [Tenancy](#tenancy).
- **The database stays on D1 for now.** The VPS reaches D1 through a small gateway Worker. Moving the
  data to SQLite on the VPS is a later, separate change; the server's database layer is written so
  that swap only replaces one module. See [Database access](#database-access).
- **Remote sessions survive server restarts, and restarts are close to invisible.** See
  [Restarts and deployment](#restarts-and-deployment).
- **The Agent's session signaling moves onto its control connection.** The viewer keeps its own
  signaling socket. See [Signaling](#signaling).
- **The platform cost report is dropped** (`GET /v1/platform/costs`, `server/src/routes/costs.rs`,
  `server/src/usage.rs`, the dashboard costs page, `docs/cost-tracking.md`).
- **The dashboard stays on Cloudflare**, and **TURN stays on Cloudflare Realtime** (the server keeps
  calling the TURN credential API).
- **Hosting: a Contabo VPS in the US East (New York) region**, the closest to the D1 primary
  (`running_in_region: ENAM` in `wrangler d1 info pulsermm-production`, checked 2026-09-27).
- **One server process for now, with room to scale out.** Running several servers is out of scope
  for this rewrite, but nothing here should block it. See [Scaling out later](#scaling-out-later).
  Postgres was considered and deferred; D1 is already shared by any number of servers.

## Features that must survive

| Feature | Today | After |
| --- | --- | --- |
| Health check | `GET /healthz`, checks the applied D1 migration | Same, through the gateway |
| Company sign-in and invitation resolve | `company.meshrmm.com`, `auth.meshrmm.com/login` → `GET /v1/auth/invitations/resolve` | Same |
| Account, company settings | `GET /v1/account`, `PUT /v1/company/settings` | Same |
| Platform company admin on `admin.meshrmm.com` | list/create/retry/domain/suspend/activate under `/v1/platform/companies` | Same, minus `/v1/platform/costs` |
| Agent inventory | `GET /v1/agents` | Same |
| Live presence (online/offline/updating) | subscription token + renew + `GET /v1/agents/events` socket | One dashboard socket, see [T5](#t5-dashboard-presence-socket) |
| Installers | `POST /v1/agent-installers`, `POST /v1/agent-installers/redeem` | Same |
| Agent delete/uninstall, close session, rotate token | `DELETE /v1/agents/{id}`, `POST .../close-session`, `POST .../rotate-token` | Same |
| Agent control connection | `GET /v1/agents/{id}/connect` → `AgentCoordinator` | Same route, in-memory Agent hub, also carries session signaling |
| Remote handoff and session | handoffs + redeem, sessions `end`/`resume`/`signal` → `RemoteSession` | Same routes; sessions stored in D1 so they outlive restarts |
| Session idle timeout, activity, TURN credentials | alarms + stored leases | A deadline per session, checked by one timer |
| Expired token cleanup | Cron trigger every 30 minutes | `tokio::time::interval` |
| Audit events | D1 tables | Same |

## Architecture

```text
Browser ─> company.meshrmm.com ─> dashboard Worker ─┐  (adds tenant + edge secret)
Agent  ────────────────── wss://api.meshrmm.com ────┤
Viewer ────────────────── https://api.meshrmm.com ──┴─> Cloudflare proxy ─> Caddy ─> meshrmm-server
                                                                                        │
                                          D1 gateway Worker ─> D1  <─── HTTPS + secret ─┤
                                                                   WorkOS, TURN API  <──┘
```

`meshrmm-server` becomes an ordinary binary crate in the workspace, using `axum` (WebSockets),
`tokio`, `reqwest` (D1 gateway, WorkOS, JWKS, TURN), `tower-http` and `tracing`. Configuration comes
from an env file: `LISTEN_ADDR`, `PUBLIC_API_URL`, `DASHBOARD_ORIGIN`, `TENANT_ROOT_DOMAIN`,
`PLATFORM_OWNER_USER_IDS`, `WORKOS_CLIENT_ID`, `WORKOS_ISSUER`, `WORKOS_API_KEY`, `TURN_KEY_ID`,
`TURN_KEY_API_TOKEN`, `REMOTE_SESSION_IDLE_TIMEOUT_SECONDS`, `D1_GATEWAY_URL`, `D1_GATEWAY_TOKEN`,
`EDGE_TOKEN`. `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_ANALYTICS_API_TOKEN` only served the cost
report and go away.

Shared state:

```rust
struct AppState {
    db: Db,                                                  // D1 gateway client (SQLite later)
    config: Config,
    http: reqwest::Client,
    jwks: JwksCache,
    agents: Mutex<HashMap<DeviceId, AgentHandle>>,           // connected Agents
    sessions: Mutex<HashMap<SessionId, Session>>,            // cache of active D1 session rows
    presence: Mutex<HashMap<CompanyId, broadcast::Sender<PresenceEvent>>>,
}
```

Each WebSocket is served by one task that `select!`s over the socket, a bounded command channel, and
its timers. Registries hold channel senders, never sockets. No lock is held across an `.await`.

### Tenancy

Today the API derives the tenant from the request hostname (`request_tenant_company`,
`is_legacy_control_plane_request` in `server/src/infrastructure.rs`), because company hostnames
reach the API through the dashboard Worker's service binding. That stays true for people:

- The dashboard Worker (`dashboard/worker/index.ts`) keeps resolving `company.meshrmm.com` against D1
  and keeps its sign-in flow. For `/healthz` and `/v1/*` it forwards the request to the VPS with
  `fetch`, replacing the `MESHRMM_API` service binding. It adds `X-Mesh-Tenant-Host` (the original
  hostname) and `X-Mesh-Edge-Token` (a shared secret), and strips any client-supplied copies.
- The server trusts `X-Mesh-Tenant-Host` only when `X-Mesh-Edge-Token` matches (constant-time), and
  uses it where it uses the request hostname today. **WorkOS-authenticated routes are rejected
  without a trusted tenant host**, so the dashboard API cannot be used except through
  `company.meshrmm.com` (or `admin.meshrmm.com` / `auth.meshrmm.com` for their routes). The existing
  check that the token's organization matches the company hostname stays.
- Browser traffic is same-origin through the Worker, so the server needs no CORS for it.
- Machine credentials already identify the company, so the Agent and viewer talk to
  `https://api.meshrmm.com` directly: Agent connect, installer redeem, handoff redeem, and session
  end/resume/signal. The installer and handoff responses return `PUBLIC_API_URL` instead of the
  company URL. Keeping long-lived Agent sockets out of the dashboard Worker also means a dashboard
  deploy never disconnects Agents.

### Database access

Querying D1 from outside Workers through Cloudflare's REST API is meant for administration: it shares
the account's global API limit of 1,200 requests per 5 minutes, which reconnecting Agents alone could
exceed. Cloudflare's recommended pattern is a proxy Worker with a D1 binding.

- New Worker `d1-gateway/` (a few dozen lines of TypeScript) bound to `pulsermm-production`.
  `POST /query` takes `{ "statements": [{ "sql": "...", "params": [...] }] }`, runs them with
  `DB.batch()` (a transaction), and returns each result's rows and `meta.changes`. It requires
  `Authorization: Bearer <D1_GATEWAY_TOKEN>`, compared in constant time, and has no other routes.
  Enable Smart Placement so it runs near the database.
- The server's `Db` module exposes `first`, `all`, `run` and `batch` with the same SQL and `?N`
  parameters used today. Queries that run together go in one `batch` call to save round trips; for
  example Agent authentication, which is several lookups today.
- The VPS is in the region nearest the D1 primary (ENAM), since every query crosses the gateway.
- Migrations stay in `server/migrations/` and keep being applied with `wrangler d1 migrations apply`.
  This plan adds one **additive** migration (`remote_sessions`, and update-grace columns on `agents`;
  see T3 and T4). Tables that only the old design used (`agent_event_subscriptions`,
  `presence_catalog_outbox` and its triggers, `usage_object_owners` and its trigger,
  `company_active_users`) are left in place and unused until the move off D1, where they are simply
  not carried over.
- Moving to SQLite later means a new `Db` implementation and a data copy; no handler changes.

### Signaling

Today each remote session opens a second socket for signaling on both sides
(`/v1/remote/sessions/{id}/signal?role=agent|client`), and **both peers end the live stream when that
socket closes**, even though video flows peer-to-peer and never touches the server:
`agent/src/remote/transport.rs` breaks with "signaling connection closed", and
`remote/src/transport/receiver.rs` breaks with `FailureKind::SignalingLost`. Any server restart
would therefore drop every live session.

The separate Agent socket has no remaining benefit: the viewer needs a socket of its own regardless,
the Agent's control connection already reconnects with backoff, and a second per-session credential
(`AgentSessionRequest.signaling_token`) is one more thing to issue and check. So:

- **Agent:** session signaling rides the control connection as
  `AgentCommand::Signal { session_id, signal }` (server → Agent) and
  `AgentStatusMessage::Signal { session_id, signal }` (Agent → server). The Agent's control loop
  forwards them to and from the session task through channels. The session task no longer owns a
  socket, so a control reconnect does not touch the stream. `signaling_token` is removed from
  `AgentSessionRequest`.
- **Viewer:** keeps `GET /v1/remote/sessions/{id}/signal`. Once WebRTC is connected, losing that
  socket is no longer fatal: the viewer reconnects it in the background and keeps streaming. Before
  WebRTC connects, the existing failure and resume path stays.
- **Server:** relays viewer ↔ Agent by looking up the session's device in the Agent hub. No
  two-socket pairing and no per-role tokens.

### Restarts and deployment

**Platform: a Contabo VPS (US East) running the server under systemd, behind Caddy, behind the
Cloudflare proxy for `api.meshrmm.com`.** Contabo has no managed services and its disk and network
performance varies, which suits this design: the server keeps no data on disk (D1 holds it), so
the VPS can be rebuilt from `deploy/` at any time. Managed platforms with rolling deploys (Fly.io, Railway,
Render) were considered and rejected: a rolling deploy runs the old and new instance side by side,
which splits the in-memory registries (an Agent connected to the old instance, its viewer to the
new one) and would require a cross-instance routing layer. Their deploys also close WebSockets,
so they offer nothing over a local restart.

What a restart looks like:

1. The deploy script uploads the new binary to `releases/<commit>/`, runs it with `--check` (config
   loads, gateway reachable, D1 schema version expected), then points the `current` symlink at it.
2. `systemctl restart meshrmm-server` sends SIGTERM. The server stops accepting, writes any pending
   session deadlines to D1, closes every WebSocket with **1012 (Service Restart)**, and exits.
3. systemd starts the new binary. It loads active sessions from D1 and starts listening, typically
   in well under a second.
4. Caddy holds requests that arrive in that gap and retries the upstream (`lb_try_duration`), so no
   HTTP request fails.
5. Agents, viewers and dashboards treat 1012 as "reconnect now": no backoff, 0–2 s of random jitter
   to spread the reconnects.

Result: no failed HTTP requests; control, signaling and presence sockets are back within about
1–2 seconds; **live remote sessions keep streaming**, because the video path is peer-to-peer and the
session state is in D1. The dashboard waits a few seconds before showing an Agent as offline (T5),
so a restart does not flash every Agent offline. Rollback is the same procedure pointing
`current` at the previous release.

Holding WebSockets open across a process restart (socket handoff between processes) is not planned.
It would add real complexity to save the 1–2 second reconnect.

Crash restarts (not graceful) behave the same except that sockets close without 1012, so clients
use their normal backoff, and session deadlines may be up to one minute stale (T4).

### Scaling out later

Not built in this rewrite, but kept possible:

- **Database:** D1 is reached over the network, so several servers can share it through the gateway
  as they are. If D1 is replaced later, Postgres is the candidate that keeps this property; a local
  SQLite file would not.
- **Keep cross-connection calls behind one interface.** Handlers never touch another connection's
  socket; they call the Agent hub (`deliver(device_id, command)`) and the presence publisher
  (`publish(company_id, event)`). In this rewrite those are in-memory. With several servers they
  become "look up which server holds the Agent, and send it there" and "publish to every server",
  through a message bus.
- **Viewer signaling goes to the Agent's server.** With several servers, `SessionBootstrap` and
  `resume` would return the signaling URL of the server holding the Agent's connection, so SDP is
  never relayed between servers.
- **Deploys** could then start a new server, move connections over, and stop the old one, instead
  of restarting in place.

## Status

| Task | Area | Wave | Depends on | Status |
| --- | --- | --- | --- | --- |
| T1 Server skeleton, D1 gateway, `Db` layer, health | Server, gateway | 1 | — | Not started |
| T2 HTTP routes, WorkOS auth, edge tenant trust | Server | 2 | T1 | Not started |
| T3 Agent hub | Server, protocol | 2 | T1 | Not started |
| T4 Remote sessions and relay | Server | 3 | T2, T3 | Not started |
| T5 Dashboard presence socket | Server, dashboard | 3 | T2, T3 | Not started |
| T6 Dashboard Worker forwards to the VPS | Dashboard | 3 | T2 | Not started |
| T7 Agent: signaling over the control connection | Agent, protocol | 3 | T3 | Not started |
| T8 Viewer: signaling loss is not fatal | Viewer, signaling client | 3 | T4 | Not started |
| T9 Graceful restart and deployment | Server, ops | 4 | T1, T4 | Not started |
| T10 Tests and CI | Server, CI | 4 | T2–T5 | Not started |
| T11 Cutover and endpoint validation | All | 5 | T1–T10 | Not started |

## Tasks

### T1 Server skeleton, D1 gateway, `Db` layer, health

- Convert `server/Cargo.toml` to a `[[bin]]` crate (`meshrmm-server`). Drop `cdylib`, `worker`,
  `worker-macros`, and the `js` features of `getrandom` and `uuid`.
- Add the `d1-gateway/` Worker and its `wrangler.jsonc` ([Database access](#database-access)).
- `Db` module: `first`/`all`/`run`/`batch` over the gateway, typed with `serde`. Keep
  `deserialize_sql_bool` so API JSON still exposes booleans.
- `main.rs`: load config, build the router, spawn the maintenance interval (the purge in
  `server/src/maintenance.rs`), serve with graceful shutdown (T9 fills in the shutdown steps).
- `GET /healthz` reports the applied D1 migration as today (`server/src/health.rs`).
- One error type that renders `{"error": "..."}` with a status, matching
  `meshrmm_protocol_types::ApiError`.

### T2 HTTP routes, WorkOS auth, edge tenant trust

Port the handlers in `server/src/routes/*.rs`, `auth.rs`, and `infrastructure.rs` to axum
extractors. Keep the behavior and the SQL; drop metering.

- `auth.rs`: keep the JWKS cache semantics (cache until expiry, refetch an unknown `kid` at most once
  per interval, keep known keys through a WorkOS outage for a day) and their unit tests. Store the
  cache in `AppState`.
- Tenant extractor: `X-Mesh-Tenant-Host` when `X-Mesh-Edge-Token` is valid, otherwise none.
  `request_hostname()` callers use it. WorkOS routes return 404 "company hostname was not found"
  without it, matching today's response for unknown hosts.
- Agent auth (`authorize_agent` in `server/src/lib.rs`): same hash and pending-hash promotion, done
  in one gateway batch; the company-hostname comparison is dropped because Agents connect to the API
  host directly.
- Suspending a company and deleting an Agent call the Agent hub (T3) directly instead of
  `revoke_agents` fanning out to Durable Objects.

### T3 Agent hub

`GET /v1/agents/{id}/connect` upgrades after Agent authentication and runs one task per Agent.

- **Superseding:** registering a device closes any previous connection for it (close code 4000, as
  today).
- **Hello:** the Agent's first message is `AgentStatusMessage::Hello { active_session: Option<SessionId> }`.
  The server reconciles it with D1: an Agent session the server no longer has gets
  `AgentCommand::EndSession`; a server session the Agent no longer runs (for example after an Agent
  restart) gets a fresh `AgentSessionRequest` with new TURN credentials. This replaces replaying the
  stored request byte-for-byte, which `replays_session` in `agent/src/remote/mod.rs` exists to
  tolerate.
- **On connect:** if `deletion_requested_at` is set, send `AgentCommand::Uninstall`; otherwise
  publish `connected=true`, clear the update-grace columns, and resend a staged rotation.
- **Messages:** text `ping` → `pong`; `UninstallScheduled` → close with 4001 (the row stays
  soft-deleted, as today); `Updating { version }` → set `agents.updating_to` and
  `agents.updating_until` (now + 10 minutes, `UPDATE_GRACE_MS`) so the disconnect is shown as an
  update, even across a server restart; `Signal { session_id, signal }` → relay to the viewer (T4).
- **Commands** arrive on the task's channel: session request, signal, end session, rotate token,
  uninstall, revoke (company suspended).
- **Token rotation:** store only the new token's hash as `pending_auth_token_hash`, keep the plaintext
  in the connection task, send `AgentCommand::RotateToken`. Agent auth already promotes the pending
  hash on the next connect. After a server restart the plaintext is gone and the rotation is
  abandoned; the old credential still works and the admin can rotate again. No plaintext is stored.
- **Disconnect:** publish `connected=false` only if the closing task is still the registered one, so
  a superseded socket cannot mark a live Agent offline.

### T4 Remote sessions and relay

New D1 table `remote_sessions`: `id`, `company_id`, `device_id`, `viewer_name`, the policy
(`start_in_background`, `idle_policy`, `display_border`, `blackout_message`), `client_token_hash`,
`idle_timeout_ms`, `expires_at`, `created_at`. At most one row per device. The server caches active
rows in memory and loads them at startup.

- `POST /v1/remote/handoffs` and `…/redeem`: same SQL and WorkOS name lookup
  (`server/src/routes/handoffs.rs`). Redeem inserts the session row, generates TURN credentials,
  delivers `AgentSessionRequest` through the hub (409 if the Agent is offline or busy, removing the
  row), and returns `SessionBootstrap`.
- `GET /v1/remote/sessions/{id}/signal`: bearer token must hash to `client_token_hash`
  (constant-time). A new viewer socket replaces the old one (close 4000).
- Relay rules from `server/src/remote_session.rs` `handle_websocket_message`: text only, at most
  64 KiB, must parse as `SignalMessage`; `Activity` and `EndSession` are accepted only from the
  viewer; an `Error` from the Agent is held in memory until the viewer socket receives it; socket
  closes are not forwarded as `PeerLeft`.
- **Deadlines:** `Activity` moves the in-memory deadline. It is written to D1 only when it has moved
  by at least a minute since the last write, and on graceful shutdown, so D1 sees about one write per
  active session per minute. One timer task expires overdue sessions.
- `POST …/resume`: client token, regenerate TURN credentials, extend the deadline, re-send the
  session request to the Agent, return a fresh `SessionBootstrap`.
- `POST …/end`: client token, expire. An unknown session returns success (idempotent).
- **Expiry** (deadline, end, close-session, Agent revoked or deleted): close the viewer socket with
  4001 and the reason, send `AgentCommand::EndSession`, delete the row.
- `POST /v1/agents/{id}/close-session` expires the device's session and returns
  `{ "closed": bool }` as today.

### T5 Dashboard presence socket

Replaces the subscription/renew/events flow with `GET /v1/dashboard/events`, reached through the
dashboard Worker on the company hostname.

- Browsers cannot set `Authorization` on a WebSocket, so the first client message is
  `{ "type": "auth", "token": "<WorkOS access token>" }`. The server validates it against the
  trusted tenant host and sends a snapshot. The dashboard sends another `auth` message with a fresh
  token before the current one expires; the server closes the socket when a token expires without
  a replacement.
- Messages keep today's shapes: a `snapshot` (`revision`, `agents` with `id`, `name`, `connected`,
  `updating_to`, and `generated_at_unix_ms`), then `agent_upsert` / `agent_deleted` with increasing
  `revision`. The revision is an in-memory counter per company; every connection starts with a
  snapshot, so it never needs to persist.
- Events come from the Agent hub (connect, disconnect, updating), installer redeem (new Agent), and
  Agent delete. They replace the `presence_catalog_outbox` triggers. A lagging subscriber is closed
  and reconnects for a fresh snapshot.
- Dashboard: `features/agents/use-agent-inventory.ts` opens the socket, sends `auth`, re-sends it on
  token refresh, reconnects immediately on close code 1012, and shows an Agent as offline only after
  it has stayed disconnected for about 10 seconds. Delete `subscription-renewal.ts` and its tests.

### T6 Dashboard Worker forwards to the VPS

`dashboard/worker/index.ts`:

- Replace `env.MESHRMM_API.fetch(...)` with `fetch` to `MESHRMM_SERVER_URL` (already a var,
  currently empty), preserving method, headers, body and WebSocket upgrades, and adding the tenant
  and edge headers ([Tenancy](#tenancy)). The `auth` surface's `/login` rewrite keeps
  `redirect: "manual"`.
- Keep the `DB` binding and the tenant lookup. Remove the `USAGE` and `MESHRMM_API` bindings,
  `worker/usage.ts`, and the costs page and `features/platform/costs.ts`.
- Add `EDGE_TOKEN` as a Worker secret.

### T7 Agent: signaling over the control connection

- `crates/protocol-types/src/signaling.rs`: add `AgentCommand::Signal`, `AgentStatusMessage::Signal`
  and `AgentStatusMessage::Hello`; remove `AgentSessionRequest.signaling_token`.
- `agent/src/remote/transport.rs` `run_sender`: take a pair of channels instead of a signal URL and
  token. The loop around `signal.next()` reads from the channel; the channel only closes when the
  session ends.
- `agent/src/remote/mod.rs` `run`: send `Hello` after connecting; route `Signal` commands to the
  active session's channel and forward its outgoing signals over the socket. Signals produced while
  disconnected wait in a small bounded queue and are sent after reconnecting; if it fills, the oldest
  are dropped, and the viewer's negotiation retry recovers. The session task keeps running while the
  control connection reconnects.
- Close code 1012 reconnects after 0–2 s of jitter instead of the backoff.
- Drop `session_signal_url` and the Agent-side session socket. Replace "Cloudflare" in log strings
  (for example "connecting Agent to Cloudflare signaling").
- The installer (`agent/src/installer.rs`) already takes its server URL from the redeem response,
  which now returns `PUBLIC_API_URL`.

### T8 Viewer: signaling loss is not fatal

- `remote/src/transport/receiver.rs`: once the peer connection is connected, a closed signaling
  socket starts a background reconnect (1012: immediately with jitter; otherwise the
  `ReconnectBackoff` in `crates/signaling-client`) instead of ending with
  `FailureKind::SignalingLost`. `Activity` messages are skipped while disconnected. Before the peer
  connects, the current failure and resume behavior stays.
- Close codes 1008 and 4001 stay terminal (`signaling_close_error` in
  `crates/signaling-client/src/lib.rs`).
- Replace "Cloudflare" in error strings (`remote/src/signaling.rs` "Cloudflare session API request
  failed").

### T9 Graceful restart and deployment

- Server: on SIGTERM, stop accepting, flush session deadlines, close sockets with 1012, exit within
  10 seconds. `--check` validates config, gateway access and schema version, then exits.
- Add `deploy/`:
  - systemd unit running `current/meshrmm-server` as an unprivileged user with an
    `EnvironmentFile`, `KillSignal=SIGTERM`, `TimeoutStopSec=15`, `Restart=always`.
  - Caddyfile for `api.meshrmm.com` with a Cloudflare Origin CA certificate, proxying to
    `LISTEN_ADDR` with `lb_try_duration` of about 10 s. TLS 1.3 must stay available, since the
    native clients accept only TLS 1.3 (`crates/signaling-client/src/tls.rs`).
  - Firewall: accept 443 only from Cloudflare's IP ranges.
  - `deploy.sh`: build for the VPS's architecture, upload to `releases/<commit>/`, run `--check`,
    swap `current`, restart, poll `/healthz`, and roll back automatically if it fails. Keep the last
    few releases.
- Replace `scripts/deploy-server.mjs` and its test. Trim the server and cost parts of
  `scripts/provision-cloudflare.ps1`; add the gateway Worker's deploy.

### T10 Tests and CI

- Rust integration tests in `server/tests/` start the server on an ephemeral port against an
  in-memory fake of the `Db` interface backed by `rusqlite` with the real migrations applied, a
  local JWKS stub (sign tokens with a test RSA key, as `server/tests/presence.mjs` does), and a stub
  TURN endpoint. Drive them with `tokio-tungstenite` and `reqwest`.
- Port the scenarios in `presence.mjs`, `session_cleanup.mjs`, `token_rotation.mjs` and
  `request_path.mjs`, and add: edge-token enforcement, hello reconciliation, relay through the Agent
  hub, deadline persistence, and **a restart test** (stop the server with a live session, start a new
  one on the same database, and check the session, its deadline, presence and update grace).
- Agent and viewer tests for continuing a connected session across a signaling drop, and for 1012.
- Keep `server/tests/sql_regressions.py` pointed at the server's SQL (the SQL is unchanged).
- CI (`.github/workflows/ci.yml`): remove the server's wasm check, `worker-build` and
  `node server/tests/*.mjs`. The server's tests run with the workspace `cargo test`. Add a
  typecheck for `d1-gateway/`.

### T11 Cutover and endpoint validation

- Apply the additive D1 migration. Deploy the gateway Worker, the VPS, and the dashboard Worker
  with `MESHRMM_SERVER_URL` set. Point `api.meshrmm.com` at the VPS. Keep the old API Worker
  deployed until validation passes, then remove it and its Durable Object namespaces (ask first).
- Existing enrolled Agents keep their credentials (they are in D1), but need the updated Agent build,
  and their stored `server` value must be `https://api.meshrmm.com`.
- Validate on DESKTOP-85R6S28 per [AGENTS.md](../AGENTS.md): install the updated Agent service,
  confirm it connects, then exercise presence in the dashboard on a company hostname; a remote
  session from the macOS and Windows viewers; **a server restart during a live session** (the stream
  continues, signaling reconnects, the dashboard does not show the Agent offline); a restart during
  an Agent update; idle timeout; close-session; token rotation; uninstall; and a rollback. Leave a
  working service installed.

## Working rules

- Don't deploy, change DNS, apply remote D1 migrations, delete Cloudflare resources, or push to
  `main` unless the user asks.
- Before changing code, read the code paths involved. If something in this plan turns out to be
  wrong, record why in the task's section instead of silently deviating.
- Match the surrounding style and comment density. Add tests for the behavior you change.
- Checks on the Mac: `cargo fmt --all -- --check`,
  `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace`,
  and `cd dashboard && npm run verify`. Windows-only Agent and viewer code is checked on the endpoint
  per [AGENTS.md](../AGENTS.md).

## Estimate

About 6–8 focused days. That is more than the first estimate because restart survival adds the
Agent and viewer signaling changes (T7, T8), session persistence (T4), and the deploy tooling
(T9), and keeping D1 adds the gateway (T1). The server itself should shrink from ~7,400 lines to
roughly 3,500–4,500.
