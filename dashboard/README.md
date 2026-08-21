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
- `lib/` contains shared browser HTTP behavior.
- `worker/` contains the Cloudflare hosting entry point.

## Verification

- `npm run typecheck`: validate browser and Cloudflare types.
- `npm run lint`: run the TypeScript, React, accessibility, and Next rules.
- `npm test`: build and run rendered-shell and Agent model tests.
- `npm run verify`: run the complete dashboard verification sequence.

## Learn More

- [vinext Documentation](https://github.com/cloudflare/vinext)
