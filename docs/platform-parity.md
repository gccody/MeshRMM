# Platform parity

What the Agent does on each operating system, what is missing where, and
why. The Windows Agent is the reference: every feature started there, and the
[macOS Agent](macos-agent.md) and [Linux Agent](linux-agent.md) pages
describe how they do the same things. Update this page when a feature is
added to, or removed from, any platform.

**Yes** means the feature works as it does on Windows. **Partly** means it
works with a limit given in the cell or below the table. **No** means it is
missing. Each table is followed by the reason for every gap. "Not
implemented" means there is no technical barrier: nobody has written the code
yet.

## Platforms and installation

| | Windows | macOS | Linux |
|---|---|---|---|
| Minimum version | Windows 10 1903 | macOS 12.3 | systemd, glibc 2.28 |
| Architectures | x64 | Universal (Apple silicon and Intel) | x64, arm64 |
| Install | Setup executable with the authorization appended | One-time Terminal command (`install-agent-macos.sh`) | One-time shell command (`install-agent-linux.sh`) |
| Runs as | LocalSystem service, plus helpers per session | launchd daemon (root), plus a launchd agent per graphical session | systemd service (root), plus a helper per graphical session |
| Re-running the installer repairs and keeps the identity | Yes | Yes | Yes |
| Local uninstall | Yes | Yes | Yes |
| Uninstall when the device is deleted on the website | Yes | Yes | Yes |
| Credential rotation | Yes | Yes | Yes |
| Self-update with rollback | Yes | Yes | Yes |
| Console mode for development (`--console`) | Yes | Yes | Yes |
| Privacy permissions the user must grant | None | Screen & System Audio Recording, Accessibility, Input Monitoring | None |

- **Windows is x64 only.** `scripts/build-agent.ps1` builds for the host and
  there is no ARM64 release target (`scripts/release-artifacts.mjs`). Not
  implemented.
- **macOS 12.3** is the first version with ScreenCaptureKit, which the Agent
  captures video and thumbnails with. System audio, though, uses
  `SCStreamConfiguration.capturesAudio`, a macOS 13 API. On macOS 12 the Agent
  finds the method missing and sessions go without audio
  (`crates/audio/src/capture_macos.rs`).
- **macOS privacy permissions.** macOS lets only the user grant them. An
  ad-hoc signed Agent loses them on every update, because macOS ties them to
  the code signature; a Developer ID signed Agent keeps them. See
  [macOS Agent](macos-agent.md#privacy-permissions).
- **Linux needs systemd** for the service, for logind (which finds the
  graphical sessions) and for `systemd-run`, which runs updates and the
  uninstall outside the service.

## Devices

| | Windows | macOS | Linux |
|---|---|---|---|
| Online state on the device wall | Yes | Yes | Yes |
| CPU and memory | Yes | Yes | Yes |
| Network throughput, physical adapters only | Yes | Yes | Yes |
| Uptime | Yes | Yes | Yes |
| Volumes | Fixed drives with a letter | Local writable volumes and `/` | Local block devices, once each |
| Screen thumbnail | Yes | Yes | Partly: none without a graphical session, none of GNOME's Wayland login or lock screen |
| Thumbnail of the sign-in and lock screens | Yes, UAC prompts too | Login window yes; lock screen not verified | X11 yes; GNOME Wayland no |

- **Linux thumbnails** come from the console's graphical session, so a
  server without one has no thumbnail. GNOME Shell refuses screen capture at
  its Wayland login screen and while the screen is locked
  (`agent/src/remote/linux/capture/mutter.rs`). Live sessions get around the
  lock by unlocking it through logind (see below), but thumbnails don't.
- Each platform counts memory the way its own task manager does: Windows
  leaves out the standby cache, macOS counts app, wired and compressed
  memory, Linux counts the total less `MemAvailable`.

## Toolbox

| | Windows | macOS | Linux |
|---|---|---|---|
| Interpreters | PowerShell, Command Prompt | Shell (zsh) | Shell (bash, or `sh` without bash) |
| Run as the signed-in user | Yes, with their unelevated token, on their desktop | Yes, in their login session | Yes, with their groups and session bus |
| Run as the system account | SYSTEM | root | root |
| Nobody signed in | Runs as SYSTEM | Runs as root | Runs as root |
| Timeout stops everything the script started | Yes | Yes (process group) | Yes (process group) |
| File delivery, user's folder | `Documents\MeshRMM Transferred Files` | `~/Documents/MeshRMM Transferred Files` | `~/Documents/MeshRMM Transferred Files` |
| File delivery, shared folder | `C:\Users\Public\Documents\…` | `/Users/Shared/…` | `/srv/…` |

- The Agent reports its operating system when it enrolls, and the server
  refuses a script for another platform's interpreter. The website and the
  viewer offer each device only the scripts it can run.

## Remote session video

| | Windows | macOS | Linux |
|---|---|---|---|
| Capture | DXGI Desktop Duplication; GDI for rotated monitors and All displays | ScreenCaptureKit | X11 MIT-SHM; Mutter or KWin screencast over PipeWire; wlr-screencopy |
| Hardware encoding | Yes (Media Foundation) | Yes (VideoToolbox, software where the Mac has none) | No |
| H.265 | Yes | Yes | No |
| 4:4:4 chroma | Yes, where the GPU can | No | No |
| Software fallback | H.264 4:2:0, 30 FPS | VideoToolbox's own | Always: OpenH264 H.264 4:2:0 |
| Frame rate | 60 FPS | 60 FPS | 30 FPS |
| Largest picture | What the encoder accepts | What the encoder accepts | 3840 × 2160, cropped |
| Live bitrate changes | H.264 live; H.265 restarts the encoder | H.264 live; H.265 restarts the encoder | Live (H.264 only) |
| Quality presets, grayscale Ultra data saver | Yes | Yes | Yes |
| Choose a monitor | Yes | Yes | Yes |
| All displays in one view | Yes | No | No |
| Sessions offered | Console, each active Remote Desktop session, Background | Console | Console |
| Follows the user to sign-in, lock and UAC screens | Yes | Login window yes; lock screen not verified | X11 yes; GNOME Wayland by unlocking; KDE and wlroots greeters not handled |
| Follows a switch of user at the console | Yes | Yes | Yes |
| Virtual monitor when none is connected | Yes (SudoVDA) | No | No |
| Cursor shape shown in the viewer | Yes | Partly: standard AppKit cursors | Partly: X11 only |
| User's own pointer drawn into the video while they move it | Yes | Yes | No |
| Display menu marks the monitor with the user's pointer (`➤`) | Yes | Yes | No |

- **Linux encodes in software.** The Agent bundles OpenH264, which encodes
  H.264 4:2:0 only, so H.265 and 4:4:4 are unavailable and
  `set_codec`/`set_chroma` do nothing (`agent/src/remote/linux/mod.rs`).
  Software encoding is held to 30 FPS, as the Windows software fallback is
  (`agent/src/remote/linux/encoder.rs`). A hardware path through VA-API or
  NVENC is not implemented.
- **Linux crops displays larger than 3840 × 2160**, OpenH264's largest
  picture, instead of scaling them, while the stream still announces the
  display's full size (`agent/src/remote/linux/encoder.rs`,
  `agent/src/remote/linux/local.rs`).
- **4:4:4 on macOS.** VideoToolbox has no 4:4:4 hardware encoder, so Mac
  Agents encode 4:2:0 only.
- **H.265 bitrate on macOS** restarts the encoder at every step, because the
  shared rate control never changes H.265 bitrate live, a rule written for
  Windows hardware encoders that accept the change and then fail
  (`agent/src/remote/bitrate.rs`). VideoToolbox may not need it.
- **All displays** is built on Windows' GDI capture of the whole virtual
  desktop. Not implemented on macOS or Linux.
- **Sessions.** Remote Desktop sessions and the background desktop are
  Windows concepts. A Mac or Linux Agent starts helpers in every graphical
  session but offers only the console's (on Linux, logind's active session
  on `seat0`). Other users' sessions, such as fast-user-switched Macs or a
  second X server, are not offered.
- **GNOME on Wayland** refuses capture while locked and at its login screen.
  A locked session is unlocked through logind for the session and locked
  again when it ends, and the banner tells the user. The login screen has no
  session to unlock and can't be viewed. Other Wayland compositors without
  Mutter's, KWin's or wlroots' interfaces can't be captured at all without
  asking the user each time.
- **Virtual monitor** relies on SudoVDA, a Windows Indirect Display Driver.
  macOS and Linux have no equivalent bundled, so a headless Mac or a Linux
  computer without a graphical session can't be viewed. The viewers offer
  the size setting only for Windows devices.
- **Cursors on Linux.** Only X11 (through XFixes) names the current cursor;
  the Wayland backends always report the default arrow. A Linux Agent also
  always reports that the technician controls input, because no Linux
  backend tracks the local user's pointer
  (`agent/src/remote/linux/local.rs`). So the user's own pointer is drawn
  into the video only while the technician records, and the display menu
  never marks the user's monitor.
- **Cursors on macOS** are matched to AppKit's standard cursors by image;
  any other cursor shows as the arrow (`agent/src/remote/macos/cursor.rs`).

## Input

| | Windows | macOS | Linux |
|---|---|---|---|
| Keyboard, mouse and wheel | Yes (`SendInput`) | Yes (Quartz events) | Yes (uinput virtual devices) |
| Input at the sign-in and lock screens | Yes | Yes | Yes |
| Windows key | Windows | Command | Super |
| Type clipboard | Yes, any character | Yes, any character | Partly: US keyboard characters only |
| Send Ctrl+Alt+Del | Yes | No | No |

- **Type clipboard on Linux** presses the keys of a US layout through
  uinput, which has no way to send an arbitrary character; anything else is
  skipped and logged (`agent/src/remote/linux/input.rs`). Windows and macOS
  send Unicode characters directly.
- **Ctrl+Alt+Del** is Windows' secure attention sequence. macOS's login
  window needs none, and on Linux the keys would only open a desktop's log
  out dialog, so the viewer leaves the button out for both
  (`agent/src/remote/secure_attention.rs`).

## Audio, clipboard, files and chat

| | Windows | macOS | Linux |
|---|---|---|---|
| System audio | Yes | Partly: needs macOS 13 | Yes, through PulseAudio or `pipewire-pulse` |
| Clipboard: text, HTML, images | Yes | Yes | Yes |
| Clipboard: copied files | Yes | Yes | Yes |
| Device-side picker for **Receive files** | Files and folders | Files and folders | Files or folders, with zenity or kdialog |
| Dropped files land at the drop position | Yes (OLE drag and drop) | No: transfer folder | No: transfer folder |
| Progress window on the device | Yes | Yes | No |
| Received files marked as downloaded | Mark of the Web | Quarantine attribute | No |
| Chat on the device | Notification-area icon and chat window | Menu bar item and popover | Notifications with **Reply** |

- **macOS audio** uses a ScreenCaptureKit option added in macOS 13; see
  [Platforms and installation](#platforms-and-installation).
- **Linux audio** needs `libpulse-simple` and a sound server; without them
  sessions have no audio.
- **Drops** on Windows are a real OLE drag at the pointer's position
  (`crates/file-transfer/src/windows.rs`). The macOS and Linux
  `drop_files` return `false`, so dropped files are saved to the transfer
  folder. Not implemented.
- **The Linux picker** uses zenity or kdialog, neither of which picks files
  and folders in one dialog, so it first asks which to send. kdialog picks
  one folder at a time.
- **No Linux progress window.** The viewer shows the transfer's progress
  instead (`crates/file-transfer/src/linux.rs`). Not implemented.
- **No Linux download marking.** Linux has no quarantine mechanism for
  downloads that file managers or the shell honour.
- **Chat on Linux** has no window of its own: messages are desktop
  notifications, and **Reply** asks for the answer in a zenity or kdialog
  dialog. Without a notification daemon the user sees no messages.

## Company policy and maintenance controls

| | Windows | macOS | Linux |
|---|---|---|---|
| Session banner | Yes | Yes | Partly: a persistent notification |
| Connection notification | Yes | Yes | Yes, as a notification |
| Connection approval | Yes | Yes | Partly: needs zenity or kdialog |
| Approval accepts at once when locked and idle | Yes, the login screen counts as locked | Yes | Yes, the login screen and an empty console count as locked |
| Border on the viewed monitor | Yes | Yes | No |
| Annotations | Yes | Yes | No |
| Black out the screens | Yes, Windows 10 2004 or newer | Yes | No |
| Block the user's keyboard and mouse | Yes | Yes, needs Accessibility | Yes |
| Prevent idle lock | Yes | Yes | Yes |
| Hide wallpaper | Yes | Yes | Partly: GNOME, Unity and Xfce |
| On close: lock | Yes | Yes | Partly: needs the desktop's screen locker |
| On close: log out | Yes | Yes, apps with unsaved work can still ask | Yes |
| On close: clear the clipboard | Yes | Yes | Yes |

- **Border, annotations and blackout** each need a window the user sees but
  the capture leaves out. Windows has `WDA_EXCLUDEFROMCAPTURE`, and macOS
  windows can be left out of a ScreenCaptureKit stream; X11 and Wayland
  display servers have no such window (`agent/src/remote/linux/local.rs`).
  The Agent refuses blackout and annotations with an error, but accepts the
  border silently, so the viewer shows the border as on.
- **Blackout on Windows** needs `WDA_EXCLUDEFROMCAPTURE`, added in Windows
  10 2004 (`agent/src/remote/blackout.rs`).
- **Linux approval** with no zenity or kdialog, or no helper on the console,
  can't show the prompt, so it waits out the timeout and then accepts. A
  login screen counts as locked, and seat0 with no session at all counts as
  locked and idle. Greeters report no idle time to logind, so X11 sessions
  use the X server's; a Wayland greeter that sets no idle hint keeps asking
  until the timeout (`agent/src/remote/linux/approval.rs`).
- **Hide wallpaper on Linux** is set through `gsettings` (GNOME, Unity) or
  `xfconf-query` (Xfce). KDE Plasma and the wlroots compositors are not
  implemented, and the Agent reports an error there.
- **Lock on close on Linux** asks logind, which asks the desktop's screen
  locker; a desktop without one, such as Xfce without light-locker or
  xfce4-screensaver, stays unlocked.
- **Block input on macOS** uses an event tap, which can block only with
  Accessibility; with Input Monitoring alone it can listen but not block.

## Restart and Windows-only features

| | Windows | macOS | Linux |
|---|---|---|---|
| Restart from the viewer | Yes | Yes | Yes |
| Restart into Safe Mode with Networking | Yes | No | No |
| Session survives the restart | Yes | Yes | Yes |
| Approval answer kept across the restart | Yes | Yes | Yes |
| [Background mode](background-mode.md) | Yes | No | No |
| [Credential autofill](credential-autofill.md) | Yes | No | No |
| Tray icon while the Agent runs | Yes | No | No |

- **Safe Mode.** Apple silicon Macs enter Safe Mode only from the startup
  options a person holds the power button for, and the Agent would not run
  there. Linux has no Safe Mode the Agent could start in. Both refuse the
  request, and the viewer leaves the item out (`agent/src/power.rs`).
- **Background mode** is a private desktop in Windows Session 0. A Mac or
  Linux computer has no desktop the technician could work on without the user
  seeing it, so the Agent refuses a background session
  (`agent/src/remote/session.rs`), the server refuses to start one, and the
  website offers **Connect** > **Background** only on Windows devices.
- **Credential autofill** fills Windows sign-in and UAC prompts through UI
  Automation and keeps the credential with DPAPI. The macOS login window and
  `sudo` or authorization prompts would each need their own mechanism. Not
  implemented, so the viewers show the key button only for Windows devices.
- **Tray icon.** The Windows Agent runs a notification-area icon for each
  signed-in user. The Mac Agent shows a menu bar item only during a session,
  for chat, and the Linux Agent shows only notifications. Not implemented.

## Features only some platforms have

- **Windows and macOS:** after a keyframe, a static desktop is re-encoded
  for a bounded number of frames to sharpen it
  (`agent/windows/remote-screen/src/refinement.rs`, which the Mac Agent
  shares). A Linux Agent re-encodes the last frame only to answer a keyframe
  request, so a static Linux screen keeps the keyframe's quality until
  something moves.
- **Linux:** a locked GNOME session is unlocked for the session and locked
  again afterwards; copied files are offered in GNOME's clipboard format, the
  only one Thunar, Caja and Nemo paste; servers without a desktop are
  supported for monitoring, scripts and files.
