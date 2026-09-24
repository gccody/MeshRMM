# MeshRMM Dashboard

The MeshRMM dashboard is a vinext/React application hosted on Cloudflare. It
uses WorkOS for user and organization identity and calls the Rust control-plane
Worker for all tenant-scoped Agent operations. It serves marketing on
`meshrmm.com`, the owner console on `admin.meshrmm.com`, and company dashboards
on immutable `<slug>.meshrmm.com` hostnames.

## Prerequisites

- Node.js `>=22.13.0`

## Local development

`npm run dev` serves the dashboard at `http://localhost:3000`. With
`MESHRMM_DEV_ROOT_DOMAIN=localhost`, it serves the production hosts under
`localhost`: marketing at `localhost:3000`, the owner console at
`admin.localhost:3000`, and each company at `<slug>.localhost:3000`. Browsers
resolve every `*.localhost` name to this computer, and the setting lives only in
the ignored `.dev.vars`, so it never affects a deployed Worker. Chrome, Edge and
Firefox treat `http://*.localhost` as secure, so the `__Host-` session cookies
work without HTTPS; Safari does not.

1. Build the control-plane Worker once, and after server changes. When
   `server/build/` exists, `npm run dev` runs it beside the dashboard, sharing
   the local D1 database, and `/v1/` requests reach it:

   ```bash
   cd ../server && worker-build --profile server-release
   ```

2. Copy `.dev.vars.example` to `.dev.vars` and `../server/.dev.vars.example` to
   `../server/.dev.vars`, then fill them in. Use a WorkOS **staging**
   environment: set its client ID in both files, and in WorkOS add
   `http://<slug>.localhost:3000` (and `http://admin.localhost:3000` for the owner
   console) as redirect URIs and CORS origins.

3. Create the local database and a fixture company. The organization ID is a
   WorkOS staging organization that your staging user belongs to:

   ```bash
   npm install
   npm run dev:seed -- acme org_01...
   npm run dev
   ```

   Then open `http://acme.localhost:3000`. The seed only writes local state
   under `.wrangler/`; run it again after new migrations.

The Agent list does not load locally. It arrives over a WebSocket that the
control plane advertises as `wss://<slug>.<root>/v1/agents/events`, without the
dev server's port or scheme. Sign-in, account and settings, and the owner
console work; remote sessions, enrollment and installer downloads need the
production configuration.

## Deployment

Deploy the dashboard only through the **Publish native release** GitHub Actions
workflow. A deployment replaces every published Agent and viewer download, and
`public/downloads/` is not committed, so a deployment from a normal checkout
would remove them or publish local builds. `npm run deploy` refuses to run unless
the workflow set `MESHRMM_RELEASE_DEPLOY=1` and `public/downloads/` holds the
complete release for `release.json`'s version. To publish a dashboard change
without a new native version, run the workflow manually. Use
`npm run deploy:dry-run` to check a build without deploying. See
[automated native releases](../docs/native-releases.md).

Application code is organized by responsibility:

- `app/` contains route composition and providers.
- `features/agents/` owns Agent models, event-stream synchronization, and UI.
- `features/enrollment/` owns installer enrollment UI.
- `features/session/` owns organization-scoped dashboard inactivity handling.
- `features/platform/` owns invite-only company provisioning for the platform owner.
- `features/marketing/` owns the public root-domain experience.
- `lib/` contains shared browser HTTP behavior.
- `wrangler.jsonc` owns the production Worker, domain, and runtime settings.

## Session policy

The dashboard Worker manages the public WorkOS PKCE flow and stores refresh
tokens in an encrypted, host-only `Secure; HttpOnly; SameSite=Lax` cookie.
Short-lived access tokens are held only in browser memory for API requests and
WorkOS Widgets. No paid WorkOS custom domain is required. Reloading restores the
session through a same-origin endpoint; sign-out clears the cookie and revokes
the WorkOS session. Existing users must sign in once after this migration.

Before deploying, configure `DASHBOARD_SESSION_KEY` as a dashboard Worker secret:

```sh
openssl rand -base64 32 | npx wrangler secret put DASHBOARD_SESSION_KEY
```

Keep this key stable across deployments. Rotating it invalidates all dashboard
sessions. Never put it in `wrangler.jsonc` or browser environment variables. Local
development can set it in an ignored `.dev.vars` file; use an HTTPS local origin
for secure session cookies. The existing WorkOS root callback URLs, CORS origins,
and `https://meshrmm.com` sign-out URL remain valid. The Worker uses WorkOS's
public PKCE code and refresh grants without an API key. Session endpoints reject
cross-origin requests, including other tenant subdomains; transient WorkOS
failures preserve the cookie for retry. Browser Web Locks serialize refresh and
sign-out across tabs where supported.

MeshRMM enforces a per-organization dashboard inactivity timeout. New
organizations default to four hours, and organization administrators can change
the value under **Settings → Dashboard security**. Expiry locks the dashboard and ends the
WorkOS session without navigating away; the user deliberately resumes through
WorkOS when they return.

WorkOS also has an application-wide inactivity timeout based on token refreshes.
Set it in the WorkOS Dashboard under **Applications → Sessions** to at least the
largest MeshRMM organization timeout (24 hours). If it remains at five minutes,
WorkOS can expire a suspended browser tab before MeshRMM's tenant policy does.
MeshRMM automatically refreshes WorkOS tokens while the page is in the background,
so an open dashboard remains active whenever the browser is still running it.

See [company domains and provisioning](../docs/company-domains.md) for wildcard
DNS, WorkOS redirect/CORS configuration, owner identity, and deployment order.

## Verification

- `npm run typecheck`: validate browser and Cloudflare types.
- `npm run lint`: run the TypeScript, React, accessibility, and Next rules.
- `npm test`: build and run session-security, rendered-shell, and Agent model tests.
- `npm run verify`: run the complete dashboard verification sequence.

## Learn More

- [vinext Documentation](https://github.com/cloudflare/vinext)
