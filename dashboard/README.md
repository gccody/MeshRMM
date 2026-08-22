# PulseRMM Dashboard

The PulseRMM dashboard is a vinext/React application hosted on Cloudflare. It
uses WorkOS for user and organization identity and calls the Rust control-plane
Worker for all tenant-scoped Agent operations.

## Prerequisites

- Node.js `>=22.13.0`

## Local development

```bash
npm install
npm run dev
npm run build
```

Application code is organized by responsibility:

- `app/` contains route composition and providers.
- `features/agents/` owns Agent models, event-stream synchronization, and UI.
- `features/enrollment/` owns installer enrollment UI.
- `features/session/` owns organization-scoped dashboard inactivity handling.
- `lib/` contains shared browser HTTP behavior.
- `worker/` contains the Cloudflare hosting entry point.

## Session policy

PulseRMM enforces a per-organization dashboard inactivity timeout. New
organizations default to four hours, and organization administrators can change
the value under **Profile & session**. Expiry locks the dashboard and ends the
WorkOS session without navigating away; the user deliberately resumes through
WorkOS when they return.

WorkOS also has an application-wide inactivity timeout based on token refreshes.
Set it in the WorkOS Dashboard under **Applications → Sessions** to at least the
largest PulseRMM organization timeout (24 hours). If it remains at five minutes,
WorkOS can expire a background tab before PulseRMM's tenant policy does.

## Verification

- `npm run typecheck`: validate browser and Cloudflare types.
- `npm run lint`: run the TypeScript, React, accessibility, and Next rules.
- `npm test`: build and run rendered-shell and Agent model tests.
- `npm run verify`: run the complete dashboard verification sequence.

## Learn More

- [vinext Documentation](https://github.com/cloudflare/vinext)
