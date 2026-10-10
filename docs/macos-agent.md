# macOS Agent

The Agent runs on macOS 12.3 or newer. It is still being ported: it covers
most of what the Windows Agent does, and this page says where it differs.

## What works

- **Video.** ScreenCaptureKit captures 4:2:0 frames and VideoToolbox encodes
  them as H.265 or H.264, in hardware where the Mac has it. VideoToolbox has
  no 4:4:4 encoder, so Mac Agents always stream 4:2:0.
- **Input.** Quartz events carry the viewer's keyboard and pointer input. The
  macOS viewer's Command key is Command, and so is the Windows viewer's
  Windows key.
- **Audio.** ScreenCaptureKit captures the Mac's system audio.
- **Clipboard, file transfer and chat.** A menu bar item opens the chat during
  a session, and it opens by itself for the technician's messages.
- **Company policy.** The session banner, the connection notification and the
  connection approval prompt work as on Windows. The viewed-display border is
  left out of the capture, and the technician's annotations are drawn over
  the shared display.
- **[Maintenance controls](maintenance-controls.md).** Blocking the user's
  keyboard and mouse uses an event tap that lets only the technician's tagged
  input through. The blackout shows the company's message on every screen
  while the capture leaves it out. Prevent idle lock and Hide wallpaper work
  too.
- **Session close actions.** Locking the screen, logging out and clearing the
  clipboard apply to the sign-in the technician saw.
- **Restart.** The viewer offers only **Restart…**: Apple silicon Macs enter
  Safe Mode only from the power button at startup. Macs have no Ctrl+Alt+Del,
  so the viewer leaves that button out.
- **[Screen thumbnails](screen-thumbnails.md)** and
  **[resource monitoring](devices.md#resource-monitoring)**.
- **[Toolbox](toolbox.md).** Shell (zsh) scripts run as the console user or
  as root, and files go to the user's Documents transfer folder.

There is no background mode, credential autofill or virtual monitor on a Mac.

## Install

The website's **Add device** dialog creates a one-time Terminal command for
Macs. It runs `install-agent-macos.sh` from the server, which downloads the
server's universal (Apple silicon and Intel) Agent, checks its code
signature, and runs `sudo meshrmm-agent --install <authorization>`. That
enrolls the Mac with the hex-encoded installer authorization and installs the
app bundle in `/Library/Application Support/MeshRMM`.

`sudo meshrmm-agent --uninstall` removes everything. The configuration and
the WebRTC identity live in `/Library/Application Support/MeshRMM/Agent`,
which only root can read.

## Processes

- A launchd daemon runs the **root coordinator**, which keeps the signaling
  connection and the WebRTC session.
- Capture and input need a graphical session, so a launchd agent runs a
  **session helper** in each one, the login window's included. Helpers
  connect to the coordinator's socket at `/var/run/com.meshrmm.agent.sock`,
  which admits only processes running the Agent's own executable.

A session follows the console: when another user takes it, capture moves to
that user's helper.

## Privacy permissions

macOS lets only the user grant the Screen & System Audio Recording,
Accessibility and Input Monitoring permissions the helpers need. The first
helper asks for them.

macOS ties those permissions to the app's code signature. An Agent signed
with a Developer ID keeps them across updates. An ad-hoc signed Agent does
not, so after each update the Agent resets them and the Mac's user grants
them again. Operators avoid this by having their server
[sign the macOS builds](self-hosting.md#sign-the-macos-builds).

## Updates

The coordinator updates the Agent as the Windows service does: every six
hours, and postponed while a remote session is live, it stages a newer
`agent-macos` release once its SHA-256 and signature check out.

- An Agent signed with a Developer ID takes only builds signed by the same
  team.
- An ad-hoc signed Agent takes only builds that carry the release signature.

The new Agent waits for the coordinator to stop, swaps the app bundle,
restarts the launchd jobs, and puts the previous bundle back if the new
coordinator does not keep running. See
[releases](releases.md#release-signatures).

## Development

`meshrmm-agent --console --config agent.json` runs the Agent in the current
session. `scripts/build-agent-macos.sh` builds the universal app bundle; see
[development](development.md#macos).
