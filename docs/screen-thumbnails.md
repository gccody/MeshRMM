# Screen thumbnails

The Devices page shows a small image of each device's main display. Choosing
it opens a larger preview. Online devices refresh it about every five minutes
when the screen has changed, and at least once an hour. An offline device
keeps the last image it sent, shown in grayscale.

## Data path

```text
Agent coordinator (Session 0)
  -> one-shot LocalSystem desktop helper on the console's input desktop
       GDI halftone scaling of the primary display to at most 640x400
       Windows Imaging Component JPEG, quality 0.7 (normally 20–80 KiB)
  -> PUT /v1/agents/{device_id}/thumbnail   (Agent credential, HTTPS)
  -> server -> {data_dir}/thumbnails/{first two digits of device_id}/{device_id}.jpg
Website tile on screen
  -> GET /v1/agents/{device_id}/thumbnail  (session cookie, If-None-Match)
  -> server -> 200 with the image, 304, or 204 if none
```

Images never cross the Agent's control connection or the presence stream,
and no event announces a new image. The server keeps only the latest image of
each device, replacing it in one step, and reading it needs `devices.view`.

A large fleet sends hundreds of images a second, so the server keeps the
cost of each one small:

- An image is renamed into place without waiting for the disk to confirm it.
  A power cut can lose the latest images, or leave an empty one, until each
  Agent sends its next; a crash of the server alone cannot.
- Images are spread over 256 folders named by the first two hex digits of
  the device ID, so 100,000 devices make folders of about 400 files.
- The ETag comes from the file's modification time, size and inode rather
  than its contents, so a 304 costs one look at the file's metadata and no
  read. Every upload changes it, even one of the same image.

Bandwidth is kept down at each step:

- The Agent captures only while it is connected to the control plane, and one
  capture runs at a time. Whoever captures (the desktop helper, the session
  helper, or a console Agent itself) first reduces the scaled pixels to a
  32x20 grid of average brightness and compares it with the grid of the last
  image the server accepted. When fewer than five cells moved by more than
  10 of 255 levels, the screen looks the same: nothing is encoded or
  uploaded. A clock ticking or a cursor blinking stays under that; a new
  window, a dialog or the lock screen does not. After eleven skipped captures,
  about an hour, the next one is sent anyway, so a subtle change or an image
  the server lost, as after restoring a backup, is put right within the hour.
- Captures are spread out across a fleet. The first comes at a random point
  in the Agent's first minute. A capture that falls due while the Agent is
  offline moves to a random point of the five minutes after it reconnects, so
  Agents that reconnect together after an outage don't upload together. Each
  interval after that is 5 minutes, give or take 15 seconds.
- Uploads share one HTTPS client for the life of the Agent process, so each
  resumes the previous TLS 1.3 session instead of a full handshake when the
  server still has it. The client closes each connection once its upload is
  done; no idle connection is kept between uploads.
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

A Linux computer works the same way, with the helper of the console's
graphical session, which captures one frame through its display server; see
[Linux Agent](linux-agent.md). A server without a graphical session has no
thumbnail.

On a Mac, the root coordinator asks the session helper on the console for the
image: the signed-in user's, or the login window's. The helper captures one
frame of the main display with ScreenCaptureKit, already scaled to at most
640x400, and AppKit encodes it as a JPEG at quality 0.7. A capture or upload
that fails, as when the coordinator starts before the session helper
connects, is tried again after 30 seconds, up to four times, before waiting
for the next refresh; Windows and Linux Agents retry the same way. An image
that failed to upload is not taken as the server's, so the next capture is
compared with the image the server has.

## Setup

Nothing to set up: the server creates `thumbnails/` in its data directory,
and each of its folders as the first image for it arrives.
Back it up with the rest of the data directory, or don't; each online device
uploads its image again within the hour.
