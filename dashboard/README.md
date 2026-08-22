# MeshRMM Dashboard

The MeshRMM dashboard is a vinext/React application hosted on Cloudflare. It
uses WorkOS for user and organization identity and calls the Rust control-plane
Worker for all tenant-scoped Agent operations.

## Prerequisites

- Node.js `>=22.13.0`

## Local development

```bash
npm install
npm run dev
npm run build
npm run deploy
```

Application code is organized by responsibility:

- `app/` contains route composition and providers.
- `features/agents/` owns Agent models, event-stream synchronization, and UI.
- `features/enrollment/` owns installer enrollment UI.
- `features/session/` owns organization-scoped dashboard inactivity handling.
- `lib/` contains shared browser HTTP behavior.
- `wrangler.jsonc` owns the production Worker, domain, and runtime settings.

## Session policy

MeshRMM enforces a per-organization dashboard inactivity timeout. New
organizations default to four hours, and organization administrators can change
the value under **Profile & session**. Expiry locks the dashboard and ends the
WorkOS session without navigating away; the user deliberately resumes through
WorkOS when they return.

WorkOS also has an application-wide inactivity timeout based on token refreshes.
Set it in the WorkOS Dashboard under **Applications → Sessions** to at least the
largest MeshRMM organization timeout (24 hours). If it remains at five minutes,
WorkOS can expire a suspended browser tab before MeshRMM's tenant policy does.
MeshRMM permits AuthKit's automatic refresh while the page is in the background,
so an open dashboard remains active whenever the browser is still running it.

## Verification

- `npm run typecheck`: validate browser and Cloudflare types.
- `npm run lint`: run the TypeScript, React, accessibility, and Next rules.
- `npm test`: build and run rendered-shell and Agent model tests.
- `npm run verify`: run the complete dashboard verification sequence.

## Learn More

- [vinext Documentation](https://github.com/cloudflare/vinext)
