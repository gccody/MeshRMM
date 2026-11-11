# Screen thumbnails

The Devices page shows a small image of each device's main display. Choosing
it opens a larger preview. An offline device keeps the last image it sent,
shown in grayscale.

An online device refreshes its image on demand:

- While a website tab shows the device (its tile is in view, in a tab that
  is in front), the Agent captures at once, unless it captured in the last
  minute, and then every 5 minutes. The tab shows the new image within
  seconds of the upload.
- Otherwise it captures about once an hour, at a random point of the hour.

So an image can be up to about an hour old (more for a screen that has not
changed, see below) when a tile first appears, and is replaced within
seconds. A capture that looks like the last image sent is not uploaded.

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

Images never cross the Agent's control connection or the presence stream.
The server keeps only the latest image of each device, replacing it in one
step, and reading it needs `devices.view`. When the image of a device some
website tab watches is replaced, the server sends
`{"type":"thumbnails","device_ids":[...]}` on the event sockets watching that
device (see [the device list](devices.md#the-device-list)), and only those;
the upload doesn't wait for it. An upload of a device no tab watches costs
the event sockets nothing.

How often to capture comes from the server with how often to report
resource usage, from the same record of which devices tabs watch
(`server/src/realtime/metrics/interval.rs`): when the Agent connects it is
sent `set_thumbnail_interval` with 300 seconds if a tab watches the device
and 3,600 otherwise, then 300 at once when a tab starts watching it, and
3,600 once no tab has watched it for 25 seconds, so a reloading tab or a
wall scrolled back and forth changes nothing. Each command goes with the
matching `set_metrics_interval`. The Agent keeps what it is told between 5
minutes and 3 hours, and a new connection starts at an hour until told.

A fleet of 10,000 devices uploads at most about three images a second while
nobody watches them, plus one every 5 minutes for each device a tab shows,
and the server keeps the cost of each one small:

- An image is renamed into place without waiting for the disk to confirm it.
  A power cut can lose the latest images, or leave an empty one; each
  Agent's first capture after it reconnects is sent even when the screen
  looks the same, so it puts its image back. A crash of the server alone
  cannot lose one.
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
  window, a dialog or the lock screen does not. A capture is sent anyway
  when it is the first on the Agent's connection, so a server that lost the
  image, as after restoring a backup, gets it back when the Agent
  reconnects, and once the last image sent is three hours old, so a subtle
  change shows within three or four hours.
- Captures are spread out across a fleet. The first comes at a random point
  in the Agent's first minute. A capture that falls due while the Agent is
  offline moves to a random point of the hour after it reconnects, and an
  Agent told to go back to hourly, as every watched device is when its tabs
  close, captures next at a random point of the hour, so Agents told the
  same in the same second, as when the server restarts, don't upload
  together. Each interval after that is the one set, give or take 5%.
- Uploads share one HTTPS client for the life of the Agent process, so each
  resumes the previous TLS 1.3 session instead of a full handshake when the
  server still has it. The client closes each connection once its upload is
  done; no idle connection is kept between uploads.
- The website loads images only for tiles on or near the screen and only
  while the tab is visible: a tile's image once when it appears, and again
  when the event socket says it was replaced. It does not poll. When its
  socket starts watching a device it was not watching, as after the socket
  reconnects or the tab is shown again, it checks the images it already
  holds of those devices once, since it may have missed their
  announcements. A failed load is tried again a minute later. Checks
  revalidate with the image's ETag, so an unchanged image costs a 304
  without a body.
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
Back it up with the rest of the data directory, or don't; each device
uploads its image again with its first capture after it reconnects, within
the hour, or within seconds of a tab showing it.
