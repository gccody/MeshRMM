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
  usage metering. None of that carries over. A single long-running process with ordinary WebSockets
  and in-memory state replaces it.
- **The platform cost report is dropped** (`GET /v1/platform/costs`, `server/src/routes/costs.rs`,
  `server/src/usage.rs`, the dashboard costs page, `docs/cost-tracking.md`). It may come back later
  as VPS-side numbers.
- **The dashboard stays on Cloudflare.** The dashboard Worker keeps serving the app and its auth flow,
  but forwards API traffic to the VPS instead of a service binding, and stops reading D1.
- **TURN stays on Cloudflare Realtime.** The server keeps calling the TURN credential API. Moving to
  `coturn` is out of scope.
- **One process, one SQLite file.** Scaling past one VPS is out of scope. If it is ever needed, the
  in-memory registries described below need a shared routing layer.

## Features that must survive

| Feature | Today | After |
| --- | --- | --- |
| Health check | `GET /healthz`, checks the applied D1 migration | Same route, checks the SQLite schema |
| WorkOS sign-in, invitation resolve | `GET /v1/auth/invitations/resolve` | Same |
| Account, company settings | `GET /v1/account`, `PUT /v1/company/settings` | Same |
| Platform company admin | list/create/retry/domain/suspend/activate under `/v1/platform/companies` | Same, minus `/v1/platform/costs` |
| Agent inventory | `GET /v1/agents` | Same |
| Live presence (online/offline/updating) | subscription token + renew + `GET /v1/agents/events` socket | One dashboard socket, see [Dashboard presence](#t5-dashboard-presence-socket) |
| Installers | `POST /v1/agent-installers`, `POST /v1/agent-installers/redeem` | Same |
| Agent delete/uninstall, close session, rotate token | `DELETE /v1/agents/{id}`, `POST .../close-session`, `POST .../rotate-token` | Same |
| Agent control connection | `GET /v1/agents/{id}/connect` → `AgentCoordinator` | Same route, handled by an in-memory Agent hub |
| Remote handoff and session | handoffs + redeem, sessions `end`/`resume`/`signal` → `RemoteSession` | Same routes, in-memory session table |
| Session idle timeout, activity, TURN credentials | alarms + stored leases | Timers inside the session task |
| Expired token cleanup | Cron trigger every 30 minutes | `tokio::time::interval` |
| Audit events | D1 tables | Same tables in SQLite |

## Architecture

```text
Browser ──> dashboard Worker (Cloudflare) ──fetch──> ┐
Agent  ─────────────── wss://api.meshrmm.com ──────> ├─ Caddy (TLS) ─> meshrmm-server (axum) ─> SQLite
Viewer ─────────────── https://api.meshrmm.com ────> ┘                         │
                                                                            WorkOS, Cloudflare TURN API
```

`meshrmm-server` becomes an ordinary binary crate in the workspace, using `axum` (WebSockets),
`tokio`, `sqlx` with SQLite (WAL mode), `reqwest` (WorkOS, JWKS, TURN), `tower-http` (CORS,
tracing) and `tracing`. Configuration comes from environment variables loaded from an env file:
`DATABASE_PATH`, `LISTEN_ADDR`, `PUBLIC_API_URL`, `DASHBOARD_ORIGIN`, `TENANT_ROOT_DOMAIN`,
`PLATFORM_OWNER_USER_IDS`, `WORKOS_CLIENT_ID`, `WORKOS_ISSUER`, `WORKOS_API_KEY`, `TURN_KEY_ID`,
`TURN_KEY_API_TOKEN`, `REMOTE_SESSION_IDLE_TIMEOUT_SECONDS`. `CLOUDFLARE_ACCOUNT_ID` and
`CLOUDFLARE_ANALYTICS_API_TOKEN` are only used by the cost report and go away.

Shared state:

```rust
struct AppState {
    db: SqlitePool,
    config: Config,
    http: reqwest::Client,
    jwks: JwksCache,
    agents: Mutex<HashMap<DeviceId, AgentHandle>>,        // connected Agents
    sessions: Mutex<HashMap<SessionId, Arc<Session>>>,    // active remote sessions
    presence: Mutex<HashMap<CompanyId, broadcast::Sender<PresenceEvent>>>,
    updating: Mutex<HashMap<DeviceId, (String, Instant)>>,// update-grace entries
}
```

Each WebSocket is served by one task that `select!`s over the socket, a bounded command channel, and
its timers. Registries hold channel senders, never sockets. Plain `std::sync::Mutex` or `DashMap`
is fine; no lock is held across an `.await`.

**A restart drops every connection.** Agents and viewers already reconnect with backoff
(`crates/signaling-client`, `agent/src/remote/mod.rs` `run`). Presence rebuilds itself as Agents
reconnect. Active remote sessions do not survive a restart: `resume` returns 410 and the user starts
a new session from the dashboard. This is acceptable for now and should be noted in the README.

### Tenancy

Today the API derives the tenant from the request hostname (`request_tenant_company`,
`is_legacy_control_plane_request` in `server/src/infrastructure.rs`) because tenant hostnames reach
the API through the dashboard Worker's service binding. After the move, **the server derives the
company only from credentials**: the WorkOS `org_id` claim, the Agent token, a handoff token, or a
session token. Hostnames no longer matter to the server, which removes the "legacy control plane"
branches.

- Native clients (Agent, viewer) talk to `PUBLIC_API_URL` (`https://api.meshrmm.com`) directly.
  `company_url()` is no longer used for Agent `server` values or handoff `api_url`.
- The dashboard keeps calling same-origin `/v1/*`; its Worker forwards to the VPS.
- CORS allows the dashboard origin and `https://*.{TENANT_ROOT_DOMAIN}`.

### Signaling stays on a per-session socket

Earlier discussion considered moving the Agent's side of session signaling onto its control socket.
**This plan keeps the separate `/v1/remote/sessions/{id}/signal?role=agent` socket.** The Agent's
sender (`agent/src/remote/transport.rs`, `run_sender`) is built around its own `SignalingConnection`,
and relaying between two sockets is ~100 lines in axum. Multiplexing would force an Agent refactor
without simplifying the server. Both peers keep using `SignalMessage` unchanged.

## Removed

- `server/wrangler.jsonc`, `worker`/`worker-macros` dependencies, the wasm32 target for the server,
  `worker-build`, `getrandom`/`uuid` `js` features.
- `server/src/agent_coordinator.rs`, `remote_session.rs`, `company_presence.rs` (replaced, not ported),
  `usage.rs`, `routes/costs.rs`, `maintenance.rs` (becomes a few lines in `main.rs`).
- Tables: `agent_event_subscriptions`, `presence_catalog_outbox` and its triggers,
  `usage_object_owners` and its trigger, `company_active_users`.
- Endpoints: `GET /v1/platform/costs`, `POST /v1/agents/events/subscriptions`,
  `POST /v1/agents/events/subscriptions/renew`, `GET /v1/agents/events`.
- Dashboard: `features/agents/subscription-renewal.ts`, `features/platform/costs.ts` and its UI,
  `worker/usage.ts`, the `DB`, `USAGE` and `MESHRMM_API` bindings in `dashboard/wrangler.jsonc`.
- `server/tests/*.mjs` (Miniflare), `scripts/deploy-server.mjs` and its test, the cost-related parts
  of `scripts/provision-cloudflare.ps1`, `docs/cost-tracking.md`.

## Status

| Task | Area | Wave | Depends on | Status |
| --- | --- | --- | --- | --- |
| T1 Server skeleton, config, schema, health | Server | 1 | — | Not started |
| T2 HTTP routes and WorkOS auth | Server | 2 | T1 | Not started |
| T3 Agent hub (`/v1/agents/{id}/connect`) | Server | 2 | T1 | Not started |
| T4 Remote sessions and signaling relay | Server | 3 | T2, T3 | Not started |
| T5 Dashboard presence socket | Server, dashboard | 3 | T2, T3 | Not started |
| T6 Dashboard Worker forwards to the VPS | Dashboard | 3 | T2 | Not started |
| T7 Native clients point at the API host | Agent, viewer, installer | 3 | T2 | Not started |
| T8 Tests and CI | Server, CI | 4 | T2–T5 | Not started |
| T9 Deployment | Ops | 4 | T1 | Not started |
| T10 Cutover and endpoint validation | All | 5 | T1–T9 | Not started |

## Tasks

### T1 Server skeleton, config, schema, health

- Convert `server/Cargo.toml` to a `[[bin]]` crate (`meshrmm-server`). Drop `cdylib`.
- `main.rs`: load config, open the SQLite pool (`journal_mode=WAL`, `foreign_keys=ON`,
  `busy_timeout`), run migrations with `sqlx::migrate!`, build the router, spawn the maintenance
  interval (the purge in `server/src/maintenance.rs`), serve with graceful shutdown.
- Replace `server/migrations/*` with a single `0001_initial.sql` holding the current end state of
  `companies`, `company_domains`, `company_provisioning_operations`, `agents`,
  `agent_install_tokens`, `remote_handoffs`, `audit_events`, `platform_audit_events`. Drop the
  tables listed under [Removed](#removed).
- Keep `deserialize_sql_bool` behavior or map booleans with `sqlx` directly; API JSON must still
  expose booleans.
- `GET /healthz` reports the applied migration version as today (`server/src/health.rs`).
- Error type: one `ApiError` that renders `{"error": "..."}` with a status, matching
  `meshrmm_protocol_types::ApiError`.

### T2 HTTP routes and WorkOS auth

Port the handlers in `server/src/routes/*.rs`, `auth.rs`, and `infrastructure.rs` to axum
extractors. Keep behavior; drop metering and hostname-tenant logic.

- `auth.rs`: keep the JWKS cache semantics (cache until expiry, refetch unknown `kid` at most once per
  interval, keep known keys through a WorkOS outage for a day) and their unit tests. Store the cache
  in `AppState` instead of a global.
- Identity extractor: bearer JWT → `Identity`; company from `org_id` via
  `SELECT … FROM companies WHERE workos_organization_id = ?`. Platform-owner extractor from
  `PLATFORM_OWNER_USER_IDS`.
- Agent auth (`authorize_agent` in `server/src/lib.rs`): same hash / pending-hash promotion logic, no
  hostname check.
- Suspending a company (`suspend_platform_company`) and deleting an Agent call into the Agent hub
  directly (T3) instead of `revoke_agents` fanning out to Durable Objects.
- Add `GET /v1/tenants/{slug}` returning `{ organization_id, status }` for active/awaiting-admin
  companies only. The dashboard Worker needs it (T6).
- Keep the SQL; `query!` macro calls become `sqlx::query`/`query_as`. Batches become transactions.

### T3 Agent hub

`GET /v1/agents/{id}/connect` upgrades after Agent auth and runs one task per Agent:

- **Superseding:** registering a device closes any previous connection for it (close code 4000, as
  today).
- **On connect:** clear any update-grace entry; if `deletion_requested_at` is set, send
  `AgentCommand::Uninstall`; otherwise publish `connected=true`, replay the active session request if
  one exists (T4), and resend a staged rotation.
- **Messages:** text `ping` → `pong`; `AgentStatusMessage::UninstallScheduled` → close the socket
  with 4001 (the row stays soft-deleted via `deletion_requested_at`, as today; the dashboard already
  got `agent_deleted` when the admin deleted it); `AgentStatusMessage::Updating { version }` → record the update-grace entry
  (`UPDATE_GRACE_MS` = 10 minutes) so the disconnect publishes `updating_to`.
- **Commands** arrive on the task's channel: session request, end session, rotate token, uninstall,
  revoke (company suspended), close.
- **Token rotation:** generate a token, store only its hash as `pending_auth_token_hash`, keep the
  plaintext in the connection task, and send `AgentCommand::RotateToken`. Agent auth already
  promotes the pending hash on the next connect. If the Agent reconnects before promotion, the new
  task resends the plaintext it received from the rotate handler; if the server restarted, the
  plaintext is gone and the rotation is simply abandoned (the old credential still works, and the
  admin can rotate again). No plaintext is stored anywhere.
- **Disconnect:** publish `connected=false` (with `updating_to` if in the grace window) only if the
  closing task is still the registered one, so a superseded socket cannot mark a live Agent offline.

The Agent side of this protocol does not change.

### T4 Remote sessions and signaling relay

A `Session` holds: id, company, device, viewer name, policy (`start_in_background`, `idle_policy`,
`display_border`, `blackout_message`), `client_token`, `agent_token`, `idle_timeout`, deadline, the
current client and agent senders, and at most one pending terminal signal per side.

- `POST /v1/remote/handoffs` and `…/redeem`: same SQL and WorkOS name lookup
  (`server/src/routes/handoffs.rs`). Redeem creates the session, generates TURN credentials, asks the
  Agent hub to deliver `AgentSessionRequest` (409 if the Agent is offline or already busy), and
  returns `SessionBootstrap`.
- `GET /v1/remote/sessions/{id}/signal?role=client|agent`: bearer token must match the role's token
  (constant-time). A new socket for a role replaces the old one (close 4000). Deliver a pending
  terminal signal for that role on connect.
- Relay rules from `server/src/remote_session.rs` `handle_websocket_message`: max 64 KiB text, JSON
  must parse as `SignalMessage`, only the client may send `Activity` (refreshes the deadline) or
  `EndSession` (expires the session), `Error` is held as a pending terminal signal until delivered,
  and socket close is advisory (no `PeerLeft` from stale sockets).
- `POST …/resume`: client token, regenerate TURN credentials, extend the deadline, re-send the
  session request to the Agent (the Agent's `replays_session` check makes an identical request a
  no-op), return a fresh `SessionBootstrap`.
- `POST …/end`: client token, expire. Idempotent: an unknown session returns success.
- Expiry (idle deadline, end, close-session, Agent revoked or deleted): close both sockets with 4001
  and the reason, tell the Agent `AgentCommand::EndSession`, remove from the table. A single
  deadline timer per session replaces alarms and leases.
- `POST /v1/agents/{id}/close-session` expires the Agent's active session and returns
  `{ "closed": bool }` as today.

### T5 Dashboard presence socket

Replaces the subscription/renew/events flow with `GET /v1/dashboard/events`.

- Browsers cannot set `Authorization` on a WebSocket, so the first client message is
  `{ "type": "auth", "token": "<WorkOS access token>" }`. The server validates it, checks `Origin`,
  and sends a snapshot. Before the token expires the dashboard sends another `auth` message with a
  fresh token; the server closes the socket when the current token expires without one.
- Server messages keep today's shapes so the UI changes stay small: a `snapshot` with `revision`,
  `agents` (`id`, `name`, `connected`, `updating_to`) and `generated_at_unix_ms`, then
  `agent_upsert` / `agent_deleted` events with increasing `revision`. The revision is a per-company
  counter in memory; a reconnect always starts with a snapshot, so no persisted revision is needed.
- Sources of events: the Agent hub (connect/disconnect/updating), Agent create (installer redeem),
  and delete (`deletion_requested_at` set). These replace the `presence_catalog_outbox` triggers. Each goes through the company's `broadcast` channel. A lagging receiver closes its
  socket and the dashboard reconnects for a fresh snapshot.
- Dashboard: update `features/agents/use-agent-inventory.ts` to open the socket, send `auth`, and
  re-authenticate on token refresh; delete `subscription-renewal.ts` and its tests.

### T6 Dashboard Worker forwards to the VPS

`dashboard/worker/index.ts`:

- Replace `env.MESHRMM_API.fetch(...)` with `fetch` against `MESHRMM_SERVER_URL` (already a var,
  currently empty), preserving method, headers, body and WebSocket upgrades. The `auth` surface's
  `/login` → `/v1/auth/invitations/resolve` rewrite keeps `redirect: "manual"`.
- Replace the D1 tenant lookup with `GET /v1/tenants/{slug}` on the server, cached briefly with the
  Cache API.
- Remove the `DB`, `USAGE` and `MESHRMM_API` bindings, `worker/usage.ts`, and the costs page.

### T7 Native clients point at the API host

- Installer bootstrap (`AgentInstallerBootstrap.server`) and redeemed `AgentConfig.server` become
  `PUBLIC_API_URL`. Handoff `api_url` becomes `PUBLIC_API_URL`.
- Replace "Cloudflare" in client log and error strings (for example
  `agent/src/remote/mod.rs` "connecting Agent to Cloudflare signaling",
  `remote/src/signaling.rs` "Cloudflare session API request failed").
- Protocol types in `crates/protocol-types/src/signaling.rs` stay as they are; the server keeps the
  same URLs for Agent connect, handoff redeem, and session end/resume/signal.
- Check TLS settings in `crates/signaling-client/src/tls.rs` still accept the VPS certificate chain
  (Let's Encrypt via Caddy).

### T8 Tests and CI

- Rust integration tests in `server/tests/` start the server on an ephemeral port with a temp SQLite
  file, a local JWKS stub (sign tokens with a test RSA key, as `server/tests/presence.mjs` does),
  and a stub TURN endpoint. Drive them with `tokio-tungstenite` and `reqwest`.
- Port the scenarios covered by `presence.mjs`, `session_cleanup.mjs`, `token_rotation.mjs` and
  `request_path.mjs`: superseded Agent sockets, presence on connect/disconnect/update grace, session
  delivery and 409 when busy/offline, relay rules, idle expiry, resume, end idempotency,
  close-session, rotation promotion and abandoned rotation, suspension revoking Agents and sessions.
- Keep or retarget `server/tests/sql_regressions.py` to the new schema and SQL locations; drop it if
  every query is covered by the integration tests.
- CI (`.github/workflows/ci.yml`): remove the `server-wasm` job's wasm check, `worker-build` and
  `node server/tests/*.mjs`; the server's tests run with the normal workspace `cargo test`.

### T9 Deployment

Add `deploy/` with:

- A systemd unit running `meshrmm-server` as an unprivileged user with an `EnvironmentFile`.
- A Caddyfile terminating TLS for `api.meshrmm.com` and proxying to `LISTEN_ADDR` (WebSockets work
  without extra config).
- A backup job: `sqlite3 … ".backup …"` on a timer, copied off the host.
- A short `deploy/README.md`: build (`cargo build --release -p meshrmm-server`, target the VPS's
  architecture), copy, restart, check `/healthz`.

Open question for the user: VPS provider, OS and architecture, and whether they prefer Docker over
systemd.

### T10 Cutover and endpoint validation

- DNS: point `api.meshrmm.com` at the VPS. Keep the old Worker until the new server is validated,
  then delete it and its Durable Object namespaces.
- Data: start from a fresh database and re-enroll test Agents, unless the user wants companies and
  Agents copied from D1 (`wrangler d1 export`, then import the kept tables).
- Validate on DESKTOP-85R6S28 per [AGENTS.md](../AGENTS.md): install the updated Agent service,
  confirm it connects to the VPS, then exercise presence in the dashboard, a remote session from the
  macOS and Windows viewers, resume after a network drop, idle timeout, close-session, token
  rotation, uninstall, update grace, and a server restart with reconnection. Leave a working
  service installed.

## Working rules

- Don't deploy, change DNS, delete Cloudflare resources, or push to `main` unless the user asks.
- Before changing code, read the code paths involved. If something in this plan turns out to be
  wrong, record why in the task's section instead of silently deviating.
- Match the surrounding style and comment density. Add tests for the behavior you change.
- Checks on the Mac: `cargo fmt --all -- --check`,
  `cargo clippy --locked --workspace --all-targets -- -D warnings`, `cargo test --locked --workspace`,
  and `cd dashboard && npm run verify`. Windows-only Agent and viewer code is checked on the endpoint
  per [AGENTS.md](../AGENTS.md).

## Estimate

About 4–6 focused days. The server should shrink from ~7,400 lines to roughly 3,000–4,000. Most of
the effort is T2 (mechanical but broad), T4 (the relay rules), T8, and T10.
