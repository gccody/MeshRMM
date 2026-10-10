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
  -> server -> {data_dir}/thumbnails/{device_id}.jpg
Website tile on screen
  -> GET /v1/agents/{device_id}/thumbnail  (session cookie, If-None-Match)
  -> server -> 200 with the image, 304, or 204 if none
```

Images never cross the Agent's control connection or the presence stream,
and no event announces a new image. The server keeps only the latest image of
each device, replacing it in one step, and reading it needs `devices.view`.

Bandwidth is kept down at each step:

- The Agent captures only while it is connected to the control plane, and one
  capture runs at a time. It does not upload an image that is identical to the
  last one the server accepted.
- The website loads images only for tiles on or near the screen, only while
  the tab is visible, and only for devices that are online, apart from one
  load of an offline device's last image. It revalidates with the image's
  ETag, so an unchanged image costs a 304 without a body. Checks follow the
  Agent's schedule: shortly after the next upload is due, based on the
  image's `Last-Modified`.
- Images are kept in memory for the website session, so changing pages or
  filters does not download them again. Signing out or the idle lock discards
  them.

The capture helper follows the input desktop, so the sign-in screen, the lock
screen and UAC prompts are captured as the person at the device sees them.
Deleting a device removes its image, and a device being deleted cannot upload
another.

On a Mac, the root coordinator asks the session helper on the console for the
image: the signed-in user's, or the login window's. The helper captures one
frame of the main display with ScreenCaptureKit, already scaled to at most
640x400, and AppKit encodes it as a JPEG at quality 0.7. A capture that
fails, as when the coordinator starts before the session helper connects, is
tried again after 30 seconds, up to four times, before waiting for the next
five-minute refresh; Windows Agents retry the same way.

## Setup

Nothing to set up: the server creates `thumbnails/` in its data directory.
Back it up with the rest of the data directory, or don't; the next upload
replaces a lost image.
