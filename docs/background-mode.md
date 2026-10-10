# Background mode

Background mode gives the technician a private Windows desktop on the device,
in Session 0 under SYSTEM, without capturing or changing the signed-in user's
desktop. It is experimental, and meant for traditional Win32 administration
tools, not a complete Explorer session.

Open it with **Connect in background** in a device's menu on the website
(`sessions.connect_background`), or connect normally and choose
**Background** in the viewer's session menu. Choosing a physical monitor goes
back to the console.

Leaving background mode or closing the session ends every application started
in the workspace, so save work first. Changing the video quality keeps the
workspace open. What is done there still happens on the same computer, and
SYSTEM has a different profile and different network credentials from the
signed-in user.

## The workspace

A black 1280×800 canvas at up to 20 FPS, with a 48-pixel charcoal taskbar at
the bottom: icon launchers with hover labels, then, after a gap, an icon for
each open window.

The pins are Command Prompt, PowerShell, Registry Editor, Services, Event
Viewer, Resource Monitor, Task Manager, Computer Management, Device Manager,
Windows Firewall, File Explorer, Disk Management, System Properties, Notepad
and Run. Task Manager, File Explorer and Run are MeshRMM's own tools,
described below; the rest are Windows' programs.

Open windows are listed as on the Windows taskbar: dialogs owned by a hidden
window, such as System Properties and Run, are included, and dialogs owned by
a visible window, such as Find, are not. A minimized window stays listed;
its icon restores it and brings it forward. With 15 pins, 11 open windows fit
at full width, more share the rest, and past 16 their icons are cut off.

Unavailable in background mode: console audio, clipboard sync, the viewer's
file transfer, chat, annotations, blackout, input blocking and Ctrl+Alt+Del.
The [toolbox](toolbox.md)'s scripts and files work, and so does
[credential](credential-autofill.md) forgetting.

## Input

Applications get real input, exactly as from a local mouse and keyboard:
`SetCursorPos` and `SendInput` from a helper separate from capture. Menus
open and run from clicks, double-clicks open items, right-click menus appear
at the pointer, and scrollbars, caption drags and resizing are Windows' own.
There is no emulation layer.

For that to work, while the workspace is open it:

- makes its desktop (`MeshRMMBackground`) Session 0's input desktop with
  `SwitchDesktop`. The handle the helper's threads bind to needs
  `DESKTOP_SWITCHDESKTOP` and `DESKTOP_JOURNALPLAYBACK`; without the latter
  `SendInput` fails with access denied even on the input desktop. A thread
  that already owns windows can't be rebound, so the rights must be in the
  handle before the workspace creates its windows.
- sets Session 0's display mode to 1280×800. Session 0 idles at 1024×768,
  and the cursor is clamped to the display mode, so the rest of the canvas
  would be out of reach. Windows accepts the mode change only once the
  workspace's desktop is the input desktop.
- sets the work area to end above the taskbar, so maximized and new windows
  fit the canvas.
- sets wheel routing to the window under the pointer. Session 0 starts with
  the wheel going to the focused window.

None of this is saved. Closing the workspace restores the previous desktop,
mode, work area and wheel setting. The console session is never affected:
Session 0 input does not reach it, and its input desktop is never switched.

The workspace makes one lasting change. It creates SYSTEM's Desktop folder,
`%SystemRoot%\System32\config\systemprofile\Desktop`, and the matching folder
under `SysWOW64`, if they are missing. Without them every Open and Save
dialog first reports that the Desktop is unavailable.

Things to know when changing this code:

- The thread that sends input can't own a window with a modal loop, so the
  Run dialog has its own thread.
- A newly started program does not take the foreground by itself. The
  workspace brings its first window forward.
- Session 0 has no shell, so Windows-key shortcuts do nothing: the Windows
  key, and keys pressed while it is held, never reach applications. Win+R is
  handled by the workspace and opens Run. The Apps key opens the selected
  item's context menu.
- Disk Management's disk pane (`DMDiskView`) doesn't handle the wheel
  itself. Its scrollbar works. That is the application's behaviour.
- If Windows refuses the display mode, applications keep laying out for the
  smaller screen and the pointer can't reach past it.

## Capture

Session 0 has no desktop compositor, so there is no desktop image to grab.
The workspace composes one from its windows:

- Each window is made layered, and the image Windows keeps for it is copied.
  Printing windows with `PrintWindow` copied them before their controls had
  finished painting. Composited windows, and windows their application
  already layers itself, are still printed.
- Window images are kept between captures. Refreshes rotate through the
  windows within a 100 ms budget, and composing uses every kept image, so a
  slow or hung window does not make the others disappear. A failed capture
  never overwrites the last good image. Closing, hiding or resizing a window
  drops its image.
- MMC draws its File/Action/View/Help labels in `ToolbarWindow32` children
  of `MMCMainFrame`, which a window capture can miss. They are captured
  separately and laid over the window.
- The taskbar uses `WS_EX_COMPOSITED`, so its buttons paint into a complete
  buffer before a capture.
- The capture helper is disposable. A ten-second frame watchdog restarts it
  if a window stalls Windows' synchronous capture call. Input lives in
  another helper and keeps working.

Rendering quirks of individual applications can still flicker. Applications
that depend on the user's shell or on GPU-composited UI may not render or
respond properly.

## Run

Run, also opened with Win+R, takes a program with its arguments, a document
or a folder. It looks them up as Windows' Run does, on the system path and
with `PATHEXT` extensions, and opens documents with their associated program
(`diskmgmt.msc`, `sysdm.cpl`). Folders, `explorer` and `taskmgr` open the
built-in tools. Programs it starts stay in the workspace.

Windows' own Run dialog (`rundll32 shell32.dll,#61`) isn't used: rundll32
passes it its own entry-point arguments, and the dialog then silently ignores
full paths.

## Task Manager

Windows' `taskmgr.exe` cannot create its window on an isolated Session 0
desktop, with or without `-d`, and from `SysWOW64` too. The pin opens a
Windows 10-style replacement built from Win32 controls and GDI, so the
workspace's capture and input work on it. It is not a claim of full parity.

- **Processes:** grouped, expandable apps, background and Windows processes;
  sortable CPU, working-set memory, disk and network rates; a compact view.
  Apps are windows on the background desktop and their descendants.
- **Performance:** CPU, memory, physical disk and network graphs over 60
  samples, with process, thread and handle counts, uptime and commit.
- **App history:** CPU and I/O accumulated while this Task Manager is open.
- **Startup:** Run, Run32 and Startup-folder entries for the machine and
  loaded user profiles, with enable and disable.
- **Users:** signed-in sessions with their processes, and confirmed
  disconnection, which leaves the user's applications running.
- **Details:** PID, owner, session, CPU, memory, threads, handles, I/O,
  priority and path; ending a process or its tree; priority changes.
- **Services:** names, PIDs, descriptions and states; confirmed start, stop
  and restart. The MeshRMM service is protected.

**File → Run new task** starts a program under SYSTEM on the background
desktop, in the workspace's cleanup job.

Implementation notes:

- Ending a process holds a process handle across the confirmation and checks
  its creation time, so a stale row can't end a recycled PID. It refuses
  itself and processes Windows marks critical.
- Confirmations are inline. Standard message boxes lost their text in
  background capture.
- Sampling, service commands and other slow actions run off the UI thread.
  Refreshes keep the selection by identity, not PID.
- Measurements that are unavailable or denied say so instead of showing
  zero.
- Per-process disk and network rates come from a private, bounded, real-time
  ETW system session, following Microsoft's
  [private system trace session guidance](https://learn.microsoft.com/en-us/windows/win32/etw/configuring-and-starting-a-systemtraceprovider-session).
  It attaches to no existing session. Closing the window stops it, and the
  workspace stops it after a forced cleanup. Event loss makes the rates
  unavailable. Physical disk activity uses PDH.
- Aggregate network excludes virtual and loopback interfaces, while the
  per-process ETW counts include loopback, so the totals need not match.
- A list-specific adapter turns background mouse messages into scroll
  commands for the scrollbars it paints itself.

Not implemented: GPU graphs, Windows' persisted app and boot history,
packaged-app startup entries, and the shell's property, dump and wait-chain
dialogs.

## File Explorer

Windows Explorer does not create a folder window on the background desktop.
The pin opens a Windows 10-style file browser instead, and restores the one
already open. **File → Open new window** opens another.

- **Places.** This PC lists drives and public folders. Quick access uses the
  public folders, because the desktop runs as SYSTEM and does not
  impersonate the signed-in user.
- **Navigation.** Breadcrumbs, an editable address bar taking drive and UNC
  paths, Back, Forward and Up. A navigation that fails keeps the current
  folder and history.
- **Views.** Details, small icons and large icons; hidden items and file
  extensions; sortable, resizable columns. Folders sort ahead of files.
- **Search.** Recursive, by case-insensitive name or `*` and `?` wildcards.
  Folders it can't read are counted in the status.
- **File operations.** New folder and text document, inline rename, copy,
  cut and paste of files and folder trees, drag to move and Ctrl-drag to
  copy, and delete. Copying into the same folder picks a new name. An
  existing destination is never overwritten or merged.
- **Delete is permanent.** There is no Recycle Bin here. It asks for
  confirmation inline.
- **Undo.** Ctrl+Z reverses up to 20 recent copies, creations, renames and
  moves. It checks file identities first and refuses to remove a copy that
  has changed since.
- **Open** starts the file's associated program on the background desktop
  under SYSTEM, in the workspace's job. It does not use `ShellExecute` or
  DDE, which could redirect the launch into an interactive session.
- **Preview** shows text read-only (UTF-8, or UTF-16 with a byte-order
  mark), up to 1 MiB. **Properties** is read-only. **File → Copy path**
  copies quoted paths.

Directory listing, search, copying, moving and deleting run off the UI
thread and can be cancelled between file system calls. A folder lists at most
20,000 entries. Recursive copy refuses reparse points and copying a folder
into itself, and deleting a directory junction removes the junction, not its
target.

| Shortcut | Action |
| --- | --- |
| Alt+Left / Alt+Right | Back / Forward |
| Alt+Up / Backspace | Parent folder |
| Ctrl+L / Alt+D / F6 | Edit the address |
| Ctrl+F / Ctrl+E / F3 | Search |
| F2 | Rename |
| F5 | Refresh |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / Cut / Paste |
| Ctrl+Z | Undo |
| Ctrl+A | Select all |
| Ctrl+Shift+N | New folder |
| Delete | Delete permanently, after confirming |
| Alt+Enter | Properties |
| Escape | Cancel a pending delete, drag or rename; clear the search |

Not provided: the Recycle Bin, shell extensions, indexed search, thumbnails,
editable ACLs and property sheets, archive folders, and OLE dragging into
other applications. Associations that need UWP activation, DDE or an
interactive user session may not work. Moving a folder across volumes can
fail; copy it and delete the original.

## Processes and cleanup

Every program the workspace starts, the built-in tools included, runs with
the SYSTEM token on the background desktop, with no inherited handles, and is
assigned to a kill-on-close job before it resumes. Ending the workspace
closes the job, which ends them all. The built-in tools are the Agent
executable in another role, and turn on version 6 common controls only for
their own UI thread; nothing changes a global theme or desktop setting.

## Tests

The Session 0 tests are ignored by default. Each must run in its own SYSTEM
process in Session 0 with no background workspace open; see
[development](development.md#session-0-tests).
