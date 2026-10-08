# Plan: pivot MeshRMM to a single-company, fully self-hosted product

Status: draft for review (2026-10-07).

## Goal

A company downloads one MeshRMM server release, runs it on its own Linux machine, and
gets everything: the website, the API, Agent and viewer connections, NAT traversal,
sign-in, and the Agent/viewer downloads and updates. Nothing calls Cloudflare, WorkOS,
or any other hosted service at runtime. One install serves one company; multi-tenancy,
the platform admin console, and cost tracking are removed.

All backend code is Rust. The website stays TypeScript/React.

## Decisions

These were settled with the maintainer and are not reopened by this plan.

| Area | Decision |
|---|---|
| Tenancy | One company per install. No `companies` table, tenant subdomains, platform admin, or cost/usage metering. |
| Server | One Rust binary (`meshrmm-server`) serving the HTTP API, WebSockets, STUN/TURN, the website, and downloads. Linux only (x86_64, arm64), shipped as a Docker image and a tarball with a systemd unit. |
| Database | SQLite or PostgreSQL, chosen in the config file at install. |
| Sign-in | Built-in accounts (password, TOTP, recovery codes, passkeys) plus optional OIDC SSO, SCIM provisioning, custom roles from a permission list, and an audit log. Optional SMTP; without it, invitations and resets are one-time links an admin copies. |
| Website | Vite + React + TypeScript, prerendered to HTML at build time, served by the Rust server. No Node at runtime. |
| NAT traversal | STUN/TURN built into the server. |
| TLS | All three: automatic Let's Encrypt, operator-supplied certificate (e.g. a Cloudflare origin certificate), or plain HTTP behind a reverse proxy. No CA pinning. |
| Agent/viewer builds | Bundled in each server release. The server serves installers and update manifests itself; upgrading the server rolls Agents forward. |
| Realtime scale | A single node. Multi-region WebSocket nodes are out of scope. |
| Marketing | A separate static site in the repo (`site/`), no backend. |
| Rollout | Integration branch `pivot/self-hosted`; one reviewed PR per area; merge to `main` once the whole system works end to end. |
| Live services | Nothing in this plan touches the live Cloudflare or WorkOS resources. The maintainer tears those down. |

Per `AGENTS.md`, nothing is kept for compatibility: the schema starts fresh (no data
migration from D1), and wire paths, config formats, and client behavior change freely.

## What exists today

- `server/`: a `workers-rs` Wasm Worker (~14k lines). It uses D1, two R2 buckets,
  Analytics Engine, a 30-minute cron, Cloudflare TURN, and three Durable Objects:
  `AgentCoordinator` (one per device, the Agent's control socket), `RemoteSession`
  (one per session, relays WebRTC signaling between viewer and Agent), and
  `CompanyPresence` (one per company, pushes Agent online/offline and catalog changes to
  dashboards). WorkOS supplies sign-in JWTs, roles, invitations, organizations, SSO, and
  directory sync.
- `dashboard/`: Next.js on vinext, deployed as a Cloudflare Worker. The Worker runs the
  WorkOS PKCE flow with sealed cookies, routes by host (marketing, tenant subdomains,
  `admin.`, `auth.`), queries D1 for tenant lookup, and proxies `/v1/*` to the server.
  WorkOS widgets provide user management, SSO setup, and domain verification.
- `agent/`, `remote/`, `crates/`: no hardcoded hosts and no pinning. They take the
  server URL from enrollment, deep links, or config, and trust the OS certificate store.
  Two constraints matter: both require **TLS 1.3**, and the update manifest URL defaults to
  a build-time value derived from `release.json`.
- Releases: CI builds the Windows Agent and viewer (unsigned) and the macOS ones
  (Developer ID, notarized), then deploys them as static assets of the Cloudflare
  dashboard under `meshrmm.com/downloads/`.

## Target architecture

```
                          ┌──────────────────────── meshrmm-server ────────────────────────┐
browser ── https ──────►  │ website (embedded, prerendered)   /v1 API (axum)               │
viewer  ── https/wss ──►  │ realtime: agent actors · session actors · presence bus          │
agent   ── https/wss ──►  │ auth: local, passkeys, TOTP, OIDC, SCIM, roles, audit           │
viewer/agent ── udp ───►  │ STUN/TURN (3478 udp/tcp + relay port range)                     │
                          │ downloads: installers, update manifest, macOS install script    │
                          │ TLS: ACME | cert files | proxy mode       maintenance tasks     │
                          └─────────┬───────────────────────────┬──────────────────────────┘
                                SQLite | PostgreSQL         data dir (blobs, ACME cache, keys)
```

### Crate layout

- `server/` becomes a native binary crate, `meshrmm-server` (tokio + axum). The Wasm
  build, `worker`/`worker-macros`, `wrangler.jsonc`, and the Miniflare tests are deleted.
- Modules:
  - `config`: TOML file (`/etc/meshrmm/server.toml`) with environment overrides.
  - `db`: connection pool, migrations, and data access for both backends.
  - `http`: router, cookie sessions, CSRF checks, security headers, static files.
  - `auth`: passwords, TOTP, passkeys, OIDC, invitations, resets, sessions.
  - `rbac`, `audit`, `scim`, `mail`.
  - `agents`, `enrollment`, `toolbox`, `thumbnails`, `handoffs`.
  - `realtime`: `coordinator`, `session`, `presence`.
  - `turn`, `tls`, `downloads`, `maintenance`.
- Pure logic carries over from today's server unchanged except for the error type:
  `meshrmm-protocol-types`, the validators, token hashing, enrollment key derivation,
  lease/rotation matching, the presence state machine, the toolbox helpers, thumbnail
  checks, and health assessment.

### Database (SQLite and PostgreSQL)

- Data access with `sqlx` and `sea-query`. `sea-query` renders the right SQL dialect for
  each backend, so each query is written once. That matters because the backend is chosen
  at runtime, and `sqlx`'s compile-time checking only covers one backend per build.
- Migrations: one directory per backend (`migrations/sqlite`, `migrations/postgres`),
  written by hand and kept in step. CI applies both and runs every test suite against
  both.
- `database.url` in the config selects the backend (`sqlite:///var/lib/meshrmm/meshrmm.db`
  or `postgres://…`). SQLite runs in WAL mode.

New schema (fresh; every `company_id` column is gone):

| Table | Notes |
|---|---|
| `settings` | Single row: today's company policy columns (idle timeout, blackout message, display border, banner, notifications, approval, idle disconnect, clipboard), plus instance name, password policy, "require 2FA", session lifetime. |
| `users` | email, display name, Argon2id password hash (nullable for SSO-only), disabled flag, `oidc_subject`, `scim_external_id`, timestamps. |
| `user_totp`, `user_recovery_codes`, `user_passkeys` | Second factors. TOTP secrets encrypted with the instance key. |
| `user_sessions` | Server-side sessions: token hash, user, created/last-seen/expiry, IP, user agent, step-up timestamp. |
| `invitations`, `password_resets` | One-time token hashes with expiry. |
| `roles`, `role_permissions`, `user_roles` | Custom roles. Built-in "Administrator" (all permissions, can't be deleted or emptied) and "Technician" (editable default). |
| `oidc_provider` | Issuer, client ID, encrypted client secret, scopes, claim-to-role mapping, auto-provision flag. One provider per install. |
| `scim_tokens` | Hashed bearer tokens for the IdP. |
| `audit_events` | actor, action, target, metadata JSON, IP, time. Covers sign-ins, settings, role changes, enrollments, sessions, script runs, file deliveries. |
| `agents`, `agent_install_tokens`, `remote_handoffs` | As today without tenancy. |
| `remote_sessions` | Persisted session records, so a viewer and Agent can resume after a server restart. Replaces Durable Object storage. |
| `toolbox_scripts`, `toolbox_files`, `script_runs`, `file_deliveries` | As today without tenancy. |

Dropped: `companies`, `company_domains`, `company_provisioning_operations`,
`platform_audit_events`, `presence_catalog_outbox` (replaced by an in-process event bus),
`usage_object_owners`, `company_active_users`, `agent_event_subscriptions` (dashboard
sockets authenticate with the session cookie instead).

Files (thumbnails and toolbox library) go to the data directory, written atomically and
checked against their SHA-256 as today. The 95 MB toolbox limit came from Cloudflare's
body limit; it becomes a config setting.

### Permissions (custom roles pick from this list)

`devices.view`, `devices.enroll`, `devices.delete`, `devices.rotate_credentials`,
`sessions.connect`, `sessions.connect_background`, `sessions.close_any`,
`scripts.run`, `scripts.manage_shared`, `files.deliver`, `files.manage_shared`,
`users.manage`, `roles.manage`, `settings.manage`, `authentication.manage`
(OIDC, SCIM, password policy), `audit.view`.

### Authentication

- The website and API share one origin, so the website uses an HttpOnly, Secure,
  `SameSite=Lax` session cookie (`__Host-meshrmm-session`) backed by `user_sessions`. That
  replaces WorkOS JWTs, JWKS caching, and the PKCE cookie sealing. State-changing requests
  require a matching `Origin` and an `X-MeshRMM-Request` header.
- Sign-in: email and password, then TOTP, a recovery code, or a passkey if the user has
  one. Passwordless passkey sign-in, and "Sign in with <IdP>" (OIDC authorization code
  flow with PKCE) when configured. The admin can require 2FA for local accounts.
- Libraries: `argon2`, `totp-rs`, `webauthn-rs`, `openidconnect`, `lettre` (SMTP).
- SCIM 2.0 (`/scim/v2/Users`, `/scim/v2/Groups`) with bearer tokens. Groups map to roles;
  deprovisioning disables the user and ends their sessions and remote sessions.
- First run: if no users exist, the server prints a one-time setup URL to its log. The
  setup page creates the first administrator and the instance name. Without that token,
  whoever reached the server first could take it over.
- Agents keep bearer tokens; viewers keep handoff and session tokens. Their flows don't
  change apart from losing tenancy checks.
- Handoffs read the viewer's display name from `users` instead of WorkOS.
- A CLI escape hatch: `meshrmm-server admin reset-password <email>` and
  `meshrmm-server admin create-user`, for locked-out operators.

### Realtime (replacing the Durable Objects)

Each Durable Object becomes an in-process actor: a tokio task that owns its state and
sockets and receives messages over a channel, so one ID is always handled by one task, as
with Durable Objects. Alarms become tokio timers. The internal `https://*.internal/...`
calls become typed method calls on actor handles.

- **Coordinator actor** (one per connected Agent, at `/v1/agents/{id}/connect`): auth,
  supersede on reconnect (close code 4000), revoke (4001), commands, token rotation, the
  session lease, update grace, uninstall acknowledgment.
- **Session actor** (one per remote session, at `/v1/remote/sessions/{id}/signal`): signaling
  relay, idle deadline, resume, end, expiry. Its record is persisted in `remote_sessions`.
- **Presence bus**: a broadcast channel. Coordinators publish connect/disconnect/updating;
  the agent, enrollment, and deletion handlers publish catalog changes directly. Dashboard
  sockets (`/v1/events`, authenticated by session cookie and `Origin`) get a snapshot,
  then deltas, using today's wire format (`snapshot`, `agent_upsert`, `agent_deleted`,
  revision numbers). Disabling a user or removing their `devices.view` permission closes
  their sockets.
- The periodic jobs from `maintenance.rs` (purging expired tokens and old runs) become a
  tokio interval task.

### STUN/TURN

- Built on the `turn` crate from webrtc-rs, which the Agent and viewer already use for
  WebRTC.
- Listeners: UDP and TCP on 3478, with a configurable UDP relay port range (default
  49160–49200). Config: `turn.public_ip` (auto-detected if unset) and `turn.host`.
- Credentials: short-lived TURN REST-style username/password (expiry plus an HMAC with
  the instance key), issued per session at handoff redeem and resume. This replaces the
  Cloudflare TURN API. The `supported_ice_url` allowlist goes away; the server hands out
  its own STUN/TURN URLs.
- TURN over TLS (for networks that block UDP and port 3478) is a follow-up spike, not part
  of the first cut.

### TLS and networking

- `tls.mode = "acme"`: Let's Encrypt with TLS-ALPN-01 on port 443 (`rustls-acme`), so port
  80 isn't needed. The account and certificates are cached in the data directory. The
  ACME directory URL is configurable (staging, or an internal ACME server).
- `tls.mode = "files"`: certificate and key paths, reloaded on change (works with a
  Cloudflare origin certificate, an internal CA, or certbot).
- `tls.mode = "proxy"`: plain HTTP on a local port. Trusted proxy CIDRs make the server
  honor `X-Forwarded-For` and `X-Forwarded-Proto`.
- Documented constraints:
  - Agents and viewers require TLS 1.3, so the proxy or certificate path must offer it.
  - TURN is UDP and can't go through an HTTP proxy or Cloudflare's orange cloud. With
    Cloudflare in front, `turn.host` must be a DNS-only record pointing at the server.
  - A Cloudflare origin certificate is only trusted by Cloudflare's edge. It works only
    while the record is proxied.
  - Cloudflare limits proxied request bodies (100 MB on the free plan), which caps toolbox
    uploads.
- The server sets a strict CSP, HSTS (outside proxy mode), `frame-ancestors 'none'`,
  `nosniff`, and `Referrer-Policy`. Today's dashboard sets none of these.

### Website

- Replace vinext/Next.js with Vite + React + React Router.
  - `next/link` and `next/navigation` become router equivalents.
  - `next/dynamic` becomes `React.lazy`.
  - `next/font` is replaced by bundled Geist files.
  - The `next/headers` surface and host logic go away.
- A build step prerenders each route's shell with `react-dom/server`: `/`, `/toolbox`,
  `/users`, `/roles`, `/authentication`, `/settings`, `/audit`, `/account`, `/login`,
  `/setup`, `/invite`, `/reset`. Pages then hydrate and load data from `/v1`.
- The built files are embedded in the server binary with `rust-embed`, with an SPA
  fallback for unknown routes.
- Runtime configuration comes from `GET /v1/instance` (instance name, setup required,
  enabled sign-in methods), not build-time host headers.
- Kept with small changes: devices, inventory stream, thumbnails, remote session and
  handoff, enrollment, toolbox, settings, and their unit tests. `authorizedFetch` drops
  bearer tokens and relies on the cookie. `useAuth` keeps its shape.
- New screens replacing WorkOS:
  - login (password, second factor, passkey, SSO button)
  - first-run setup
  - accept invitation
  - password reset
  - account security (password, TOTP, passkeys, recovery codes, active sessions)
  - Users (invite, disable, assign roles, reset 2FA)
  - Roles (permission editor)
  - Authentication (OIDC provider, SCIM tokens, 2FA policy, SMTP settings plus a test email)
  - Audit log
- Removed: platform console, cost reports, host/tenant routing, the WorkOS widgets and
  Radix Themes, `worker/`, `wrangler.jsonc`, and the Cloudflare/vinext dependencies.
- The marketing page moves to `site/` as plain static HTML/CSS. It's built separately and
  isn't part of the server.

### Releases, installers, and updates

- A server release is versioned together with the Agent and viewer. CI builds:
  - the server for `x86_64` and `aarch64` Linux (musl, static);
  - the website, embedded in the server binary;
  - the Agent and viewer for Windows (unsigned, as today) and macOS (Developer ID +
    notarized, as today);
  - a tarball per architecture (`meshrmm-server`, `share/meshrmm/downloads/*`,
    `artifacts.json` with versions and SHA-256s, a systemd unit, an example config) and a
    multi-arch Docker image.
- Publishing goes to GitHub Releases and GHCR. Those are the channels operators download
  from; no running server ever contacts them.
- The server generates `/downloads/update-manifest.json` at runtime from `artifacts.json`
  and its configured public URL, so every manifest points at the operator's own server.
  It also serves the installers and `/install-agent-macos.sh`.
- Client changes (small):
  - Drop the build-time default manifest URL (`crates/self-update/build.rs`) and the
    `download_origin`/`viewer_server` fields of `release.json`.
  - Enrollment already returns `update_manifest_url`, so it stays required.
  - The viewer derives its manifest URL from the server in its deep link or config.
  - Releases are signed (decided while building PR 8). A deep link can name any
    server, so a viewer that takes updates from it would run whatever an
    attacker's server offered. CI signs each build's target, version and SHA-256
    with an Ed25519 release key. Agents and viewers embed the public key from
    `release.json` and install only signed updates, whichever server offers them.
    See `docs/releases.md`.
  - The macOS viewer bundle no longer embeds a `remote.json` with a server URL.
  - Update the doc comments and error messages that mention Cloudflare.
- Enrollment is unchanged: the Windows installer still gets the bootstrap trailer appended
  in the browser, and macOS still uses the `curl | sudo sh` script. Both point at the
  instance's own URL.
- Removed: `scripts/deploy-*.mjs`, `provision-cloudflare.ps1`,
  `verify-dashboard-deploy.mjs`, the Cloudflare publish job, and the `server-wasm` CI job.
  `release-config.mjs`, `update-release-manifest.mjs`, and `verify-release-assets.mjs` are
  rewritten for `artifacts.json`.

## PR sequence on `pivot/self-hosted`

Each step is one PR into the integration branch, with tests, reviewed before the next one
that depends on it.

1. **Server foundation.**
   - Native `meshrmm-server` crate: config, the three TLS modes, both database backends
     with the new schema, `/healthz`, static file and download serving, maintenance task,
     structured logging, graceful shutdown.
   - Delete the Worker build, wrangler config, and Miniflare tests.
   - CI: a Linux server job with a PostgreSQL service container.
2. **Accounts and access.**
   - Users, sessions, passwords, TOTP, recovery codes.
   - First-run setup, custom roles and permission checks, audit log, invitations and
     resets, SMTP and copyable links, admin CLI.
3. **Devices and toolbox.**
   - Enrollment, Agent auth and rotation, deletion, thumbnails, toolbox scripts and
     files, script runs, file deliveries, handoff creation.
   - File storage on disk.
4. **Realtime.**
   - Coordinator and session actors, presence bus, dashboard event socket, handoff
     redeem, resume and end.
   - Port the Miniflare scenarios (presence, session cleanup, token rotation, request
     path, thumbnails, toolbox) to Rust integration tests.
5. **STUN/TURN.** Built-in server, credentials, ICE config at redeem and resume.
6. **Website.** Vite + React Router migration, prerendering, cookie auth, the new
   account/users/roles/audit/setup screens, embedding in the binary, platform and tenant
   code removed.
7. **Passkeys, OIDC, SCIM** and their website screens.
8. **Releases and packaging.**
   - `artifacts.json`, runtime manifest, Docker image, tarballs, systemd unit, and the
     GitHub Releases/GHCR workflow.
   - The client changes above.
   - Remove the Cloudflare scripts and workflows.
9. **Marketing site** in `site/`.
10. **Operator docs and end-to-end validation, then merge to `main`.**
    - Docs: install with Docker or the tarball, each TLS mode, ports and firewall, backups
      for SQLite and PostgreSQL, upgrades.

Dependencies:
- PR 1 comes first.
- PRs 2, 3, 5, and 6 can run in parallel after it. The website can work against the API
  contract while 2 and 3 land.
- PR 4 needs 3. PR 7 needs 2 and 6. PR 8 needs 6. PR 10 needs everything.

## Testing

- **Rust unit tests** for the ported pure logic, kept from today's suite.
- **Rust integration tests:**
  - start the server in-process against SQLite and PostgreSQL;
  - drive the HTTP and WebSocket APIs with fake Agents and viewers;
  - cover auth flows (password, TOTP, passkeys with a software authenticator, OIDC
    against a mock provider, SCIM), permission checks for every route, presence, sessions,
    rotation, toolbox, TURN credential checks, and restart and resume.
- **Schema parity:** a test that the SQLite and PostgreSQL migrations produce equivalent
  tables and columns. This replaces `sql_regressions.py`.
- **Website:** unit tests kept and updated; a test of the prerendered HTML replaces
  `rendered-html.test.mjs`; `typecheck` and `lint` stay in CI.
- **End to end, before merging to `main`:**
  - the Docker image on a Linux host in each TLS mode (ACME against Let's Encrypt staging);
  - enroll the Windows endpoint's Agent and a macOS Agent from the website;
  - a remote session from the macOS and Windows viewers, including one that has to go
    through TURN;
  - a toolbox script and a file delivery;
  - presence updates in the website;
  - an Agent and viewer update served by the server.
  - Windows validation follows the maintainer's private endpoint instructions.

## Risks and open items

- **Two database backends** roughly double the database testing. `sea-query` keeps the
  queries single-source, but every migration is written twice.
- **The website rewrite** is the largest single piece: about nine new screens, plus
  replacing everything WorkOS rendered.
- **TURN without TLS** won't get through networks that only allow 443. TURN over TLS is a
  follow-up spike.
- **Windows code signing** remains absent. Appending the bootstrap trailer would break an
  Authenticode signature, so adding signing later needs a different way to pass the
  bootstrap (for example a signed stub plus a separate config download).
- **The macOS viewer updater** checks only SHA-256, not the code signature. Release
  signatures (PR 8) now cover it and every other updater, independent of the server.
- **Single node:** the server is a single point of failure. That's acceptable for now
  (decided above); HA would need shared session state and is out of scope.
- **Secrets at rest:** TOTP secrets, the OIDC client secret, and the TURN HMAC key are
  encrypted with an instance key generated at first start in the data directory. Backups
  must include that key, and the operator docs must say so.
