# Screen thumbnails

The Devices page shows a small image of each device's main display. Choosing
it opens a larger preview. Online devices refresh it every five minutes. An
offline device keeps the last image it sent, shown in grayscale.

## Data path

```text
Agent coordinator (Session 0)
  -> one-shot LocalSystem desktop helper on the console's input desktop
       GDI halftone scaling of the primary display to at most 640x400
       Windows Imaging Component JPEG, quality 0.7 (normally 20–80 KiB)
  -> PUT /v1/agents/{device_id}/thumbnail   (Agent credential, HTTPS)
  -> Worker -> R2 thumbnails/{company_id}/{device_id}.jpg
Dashboard row on screen
  -> GET /v1/agents/{device_id}/thumbnail  (WorkOS token, If-None-Match)
  -> Worker -> R2 conditional get -> 200 with the image, 304, or 204 if none
```

Durable Objects are not involved. Images never cross the coordinator's
signaling WebSocket or the company presence stream, and no event announces
a new image. R2 keeps only the latest image of each device.

Bandwidth is kept down at each step:

- The Agent captures only while it is connected to the control plane, and one
  capture runs at a time. It does not upload an image that is identical to the
  last one the server accepted.
- The dashboard loads images only for rows on or near the screen, only while
  the tab is visible, and only for devices that are online, apart from one
  load of an offline device's last image. It revalidates with the image's
  ETag, so an unchanged image costs a 304 without a body. Checks follow the
  Agent's schedule: shortly after the next upload is due, based on the
  image's `Last-Modified`.
- Images are kept in memory for the dashboard session, so changing pages or
  filters does not download them again. Signing out or the idle lock discards
  them.

The capture helper follows the input desktop, so the sign-in screen, the lock
screen and UAC prompts are captured as the person at the device sees them.
Deleting a device removes its image, and a device being deleted cannot upload
another.

## Setup

Create the bucket once before deploying the server:

```sh
cd server
npx wrangler r2 bucket create meshrmm-thumbnails
```

`server/wrangler.jsonc` binds it as `THUMBNAILS`. Then deploy the server with
`node scripts/deploy-server.mjs` and publish a native release so Agents start
uploading.

## Costs

Each online device makes one Worker request and one R2 Class A write every
five minutes when its screen changes, about 8,600 a month. Each visible
dashboard row makes one Worker request and one R2 Class B read per check. The
platform cost report does not yet include R2.
