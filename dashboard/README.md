# MeshRMM website

The website people use to manage a MeshRMM server: devices with their hardware, software, updates, terminals and files, the toolbox, jobs and automation policies, alerts, users,
roles, sign-in policy, settings, the audit log and their own account. It's a
Vite + React + React Router app. The build prerenders every page to HTML, and
the server (`meshrmm-server`) embeds the result and serves it from the same
origin as its API. Nothing runs on Node in production.

## How it fits together

- **Prerendered pages.** `npm run build` builds the browser bundle, then renders
  each page in `src/pages.ts` with `react-dom/server` into `dist/` (`/` is
  `index.html`, `/users` is `users/index.html`, and unknown paths get `404.html`).
  Nobody is signed in at build time, so workspace pages render their heading over
  a loading state, and the browser hydrates them and loads everything from `/v1`.
  Render nothing in the first pass that depends on the browser (the URL's hash,
  storage, the session) or hydration won't match.
- **Content-Security-Policy.** The server sends `default-src 'self'` without
  `'unsafe-inline'`, so pages can't use inline scripts, `<style>` elements or
  `style` attributes in their HTML. `tests/prerender.test.mjs` checks this.
- **Sessions.** Signing in sets an HttpOnly `__Host-meshrmm-session` cookie.
  Every request that changes something sends `X-MeshRMM-Request: 1` (see
  `lib/http.ts`), which the server requires along with its own `Origin`.
  `features/auth/session.tsx` holds who is signed in; a 401 locks the workspace,
  and other tabs hear about sign-in and sign-out through `localStorage`.
- **Permissions.** Pages and controls follow the account's permissions
  (`features/workspace/views.ts`, `features/toolbox/access.ts`); the server
  enforces them regardless.
- **Live devices.** `/v1/events` is a WebSocket authenticated by the cookie. It
  sends a snapshot, then revision-numbered changes (`features/agents/inventory-stream.ts`).
  Revisions restart with the server, so the page compares them only within one
  socket: each new socket's first snapshot applies whatever its revision.
  A lost socket reconnects after a random wait between half and all of a step
  that doubles from 1 s to 30 s, so tabs that lost a restarted server together
  don't all ask for their snapshot at once; the step restarts only once a
  socket delivers its snapshot. A refused socket checks whether the user is
  still signed in at once, then at most about once a minute while the server
  stays unreachable; a socket closed for lost access (4001) always checks.
  A background tab's socket keeps receiving changes, so showing the tab or
  the network returning asks the server for nothing: it only reconnects at
  once when the socket is down, or replaces one that is live but has heard
  nothing for 30 s and may be dead.
- **The device store.** The devices, companies, tags and each online
  device's latest resource reading live outside React
  (`features/agents/device-store.ts`). A change moves only its device in the
  sorted list, and the store tells its subscribers at most once a frame (a
  few times a second in a background tab), however many changes arrive, so
  a fleet reconnecting at once costs one render. Components subscribe to
  what they show (`features/agents/use-devices.ts`): one device, one
  device's reading, the list, the counts, the companies or the tags. A
  device's tile on the wall redraws for its own device and reading only.
- **One bundle, one lazy chunk.** Every page hydrates from a single entry
  script (kept under 800 kB by `tests/prerender.test.mjs`). Only the
  [terminal](../docs/terminal.md)'s xterm.js is split out, loaded with
  `React.lazy` when a terminal opens (`features/terminal/terminal-view.tsx`).
  xterm adds `<style>` elements, which the Content-Security-Policy refuses,
  so while a terminal is open they become inert elements whose rules go into
  constructed stylesheets.
- **Large lists without inline styles.** The [file browser](../docs/files.md),
  the many-device picker and the Devices wall draw only the rows on screen
  (`lib/use-windowed-rows.ts`) and set the space of the rest through the DOM,
  which the Content-Security-Policy allows, rather than with `style`
  attributes. The wall is a grid that scrolls with the page: it measures its
  columns and its tiles, which are all one height, and pads itself for the
  rows not drawn. Pickers that choose one device search the fleet and list the
  first matches (`features/device-picker/`), so neither puts every device in
  the page. Its uploads hash files with
  an incremental SHA-256 (`lib/sha256.ts`), since `crypto.subtle` needs a
  file whole.

## Local development

Requires Node.js 22.13 or newer.

Run a server in proxy mode, for example with this `server.toml`:

```toml
public_url = "https://localhost"
data_dir = "/tmp/meshrmm-dev"
tls.mode = "proxy"
turn.enabled = false
```

```bash
cargo run -p meshrmm-server -- -c server.toml   # prints the first-run setup link
cd dashboard
npm install
npm run dev                                      # http://localhost:5173
```

The dev server proxies `/v1`, `/downloads` and `/healthz` to the server
(`MESHRMM_SERVER`, default `http://127.0.0.1:8080`) and presents the server's
public origin (`MESHRMM_PUBLIC_URL`, default `https://localhost`) so its
same-origin checks pass. Open the setup link from the server's log with
`http://localhost:5173` in place of its origin. Chrome, Edge and Firefox treat
`localhost` as secure, so the `__Host-` cookie works over HTTP there; Safari
doesn't.

To try the embedded build instead, run `npm run build` and restart the server:
debug builds read `dist/` when they start, release builds embed it.

## Checks

```bash
npm run verify   # typecheck, lint, build, and the tests in tests/
```

The tests run on Node's test runner with type stripping, so modules they import
carry `.ts` extensions and avoid JSX.
