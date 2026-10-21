# Linux Agent

The Agent runs on 64-bit x86 and Arm Linux with systemd and glibc 2.28 or
newer, which covers the distributions released since 2018. It runs on servers
without a desktop as well as on desktops, and this page says where it
differs from the Windows and macOS Agents.

## What works

On every Linux computer, servers included:

- **Devices.** Enrollment, online state,
  [resource monitoring](devices.md#resource-monitoring), automatic updates,
  restart and uninstall from the website.
- **[Toolbox](toolbox.md).** Shell scripts run with bash (or `sh` where
  there is no bash), as root or as the user at the console, and files go to
  that user's Documents transfer folder, or to `/srv` when nobody is signed
  in.

With a graphical session on the computer's screen, remote sessions too:

- **Video.** The session's display server is captured: X11 directly, GNOME
  on Wayland through Mutter's screencast interface, KDE Plasma on Wayland
  through KWin's, and wlroots compositors such as sway and Hyprland through
  wlr-screencopy. OpenH264 encodes the frames in software as H.264 4:2:0, at
  up to 30 frames a second, scaling a display larger than 3840 × 2160 down
  to fit. GNOME on Wayland refuses screen capture at its
  login screen and while its screen is locked. When the console's session
  is locked, the Agent unlocks it through logind, so the technician sees the
  desktop, and locks it again when the remote session ends; the session
  banner tells the user. GNOME's Wayland login screen can't be viewed. X11
  sessions, GNOME's included, show their login and lock screens. Other Wayland compositors offer no capture
  interface the Agent can use without asking the user each time.
- **Input.** A virtual keyboard and pointer from uinput carry the viewer's
  keys and pointer. The display server reads them like real devices, so
  input reaches the lock and login screens too, and the session's own
  keyboard layout applies. **Type clipboard** types the characters a US
  keyboard has. In X11 sessions, the Agent tells the user's own
  keyboard and mouse from the viewer's, so the viewer shows the user's
  pointer while they use it and marks the monitor it is on.
- **Audio.** The default output's sound is captured through the PulseAudio
  protocol, which PulseAudio and PipeWire's `pipewire-pulse` both serve.
  Without `libpulse-simple` or a sound server, sessions have no audio.
- **Clipboard and file transfer.** Text, rich text, images and copied files.
  Wayland compositors with a data-control protocol (KDE, wlroots) are read
  directly, and GNOME through Xwayland. Copied files go on an X11 clipboard
  as both `text/uri-list` and GNOME's `x-special/gnome-copied-files`, the
  only format Thunar, Caja and Nemo paste. **Send files** asks with zenity or
  kdialog. Files dropped on the remote view go to the transfer folder.
- **Chat.** The technician's messages show as desktop notifications with a
  **Reply** button, which asks for the answer in a dialog.
- **Company policy.** The session banner is a notification that stays until
  the session ends. The connection notification is a notification too, and
  the approval prompt a zenity or kdialog dialog. logind tells the Agent
  whether the session is locked or at the login screen, and an X11 session's
  X server how long it has had no input (otherwise logind's idle hint). A
  console with no session at all, as on a server, counts as locked and idle.
- **[Maintenance controls](maintenance-controls.md).** Blocking the user's
  keyboard and mouse grabs their input devices. Prevent idle lock inhibits
  the session's idle timer. Hide wallpaper works on GNOME, KDE Plasma and
  Xfce. Linux display servers can't show the user a window that capture
  leaves out, so there is no blackout, display border or annotation drawing.
- **Session close actions.** Locking goes through logind, which asks the
  session's screen locker; a desktop without one, such as Xfce without
  light-locker or xfce4-screensaver, stays unlocked. Logging out asks
  the desktop (GNOME, KDE, Xfce) to log out without its confirmation dialog,
  and ends the logind session elsewhere. Clearing the clipboard runs in the
  user's session.
- **Restart.** The viewer offers only **Restart…**; Linux has no Safe Mode
  for the Agent, and no Ctrl+Alt+Del button.
- **[Screen thumbnails](screen-thumbnails.md)** of the console's session.
  GNOME and KDE show their screen-sharing indicator for the moment a
  thumbnail is captured.

There is no background mode, credential autofill or virtual monitor on Linux.

| Session | Capture | Needs |
|---|---|---|
| X11 | MIT-SHM, polled at the frame rate | the X server's cookie, from the session |
| GNOME on Wayland | Mutter's `org.gnome.Mutter.ScreenCast` D-Bus API and PipeWire | `libpipewire-0.3` |
| KDE Plasma on Wayland | KWin's `zkde_screencast_unstable_v1` and PipeWire | `libpipewire-0.3`, the Agent's desktop file |
| sway, Hyprland and other wlroots compositors | `zwlr_screencopy_manager_v1` | nothing more |

KWin lends its screencast interface only to programs a desktop file names,
so the installer adds `/usr/share/applications/meshrmm-agent.desktop`. KWin
renders the frames with OpenGL; where it renders with llvmpipe and the
computer has no DRM render node, as in Hyper-V virtual machines, its frames
come back blank, and KDE's own screenshot tool shows the same.

## Install

The website's **Add device** dialog creates a one-time command for Linux:

```sh
curl -fsSL https://rmm.example.com/install-agent-linux.sh | sudo sh -s -- https://rmm.example.com <authorization>
```

`install-agent-linux.sh` downloads the server's build for the computer's
architecture (`meshrmm-agent-linux-x64` or `-arm64`) and runs
`meshrmm-agent --install <authorization>` as root. That enrolls the computer
with the hex-encoded installer authorization, using its pretty host name or
host name as the device name, and installs:

- the executable, `/opt/meshrmm/meshrmm-agent`;
- the `meshrmm-agent.service` systemd service, enabled and started;
- KWin's desktop file, `/usr/share/applications/meshrmm-agent.desktop`;
- the configuration, WebRTC identity, logs and update state in
  `/var/lib/meshrmm-agent`, which only root can read.

Running the command on an enrolled computer repairs the install and keeps
its device identity. `sudo /opt/meshrmm/meshrmm-agent --uninstall` removes
everything.

## Processes

- systemd runs the **root coordinator**, which keeps the signaling
  connection and the WebRTC session.
- The coordinator asks logind for the computer's sessions every second, and
  starts a **session helper** in each X11 or Wayland session, login screens
  included, as the session's user. A helper gets the session's display
  variables from the session's own processes (GNOME and KDE keep them in the
  user's systemd services), one end of a socket pair to talk to the
  coordinator on, and two `/dev/uinput` descriptors, which only root can
  open. A helper that stops starts again after five seconds, and every
  helper ends with the coordinator.
- The console is the active session on `seat0`. Remote sessions and
  thumbnails use its helper, and a session moves to another helper when the
  console changes.
- Blocking the user's input needs root, so the coordinator grabs the
  devices itself.

The coordinator logs to `/var/lib/meshrmm-agent/agent.log`, and each helper
to its user's `~/.local/state/meshrmm/agent-session-helper.log`.

## Updates

The coordinator updates the Agent as the other platforms do: every six
hours, and postponed while a remote session is live, it downloads a newer
`agent-linux-x64` or `agent-linux-arm64` release and checks its SHA-256 and
[release signature](releases.md#release-signatures). It stages the release
beside the installed executable and starts its `--apply-update` in a
transient systemd unit, which outlives the service: the new Agent waits for
the coordinator to stop, swaps the executable, restarts the service, and puts
the previous executable back if the new coordinator does not keep running.

## Development

`meshrmm-agent --console --config agent.json`, run in a desktop session,
streams that session. Input needs root or write access to `/dev/uinput`.

`scripts/build-agent-linux.sh [x64|arm64]` builds the Agent with
cargo-zigbuild against glibc 2.28, so it builds the same on Linux and macOS
hosts; it needs zig and CMake. The builds load PipeWire and PulseAudio only
when a session uses them, and need nothing beyond glibc otherwise.

The ignored tests in `agent/src/remote/linux/snapshot.rs` capture and encode
the session they run in, and save the thumbnail as
`/tmp/meshrmm-snapshot.jpg`; run them in a desktop session, with its display
variables set. `input::tests::creates_the_virtual_devices` needs root.
