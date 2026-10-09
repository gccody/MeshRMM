# Background mode: real input in Session 0

Task 1 of the background-mode plan. Tested on September 30, 2026 on `DESKTOP-85R6S28`
(Windows 11 25H2, build 26200), as SYSTEM in Session 0, with the manual driver
harness plus a set of real-input commands. The harness is not in the repo. A copy is
at `~\bg-input-20260930\agent\src\remote\background\driver.rs` on the endpoint.

The sketch below is implemented (Tasks 7 and 8 of that plan). Two things it
didn't foresee: the thread that sends input can't own a window with a modal
loop, so Run has its own thread; and a newly started program doesn't take the
foreground, so the workspace brings its first window forward.

## Result: go

Once `MeshRMMBackground` is Session 0's input desktop, `SetCursorPos` and
`SendInput` drive Session 0 apps exactly like a local mouse and keyboard. Every
input bug that Task 1 was meant to check disappears. The console session is not
affected.

## What was needed

- **`SwitchDesktop`** with a handle opened with `DESKTOP_SWITCHDESKTOP` (0x100).
  No window has to be focused first. It succeeded straight away from the helper.
- **`DESKTOP_JOURNALPLAYBACK` (0x20) on the injecting thread's desktop handle.**
  Without it, `SendInput` fails with `ERROR_ACCESS_DENIED`, even when the desktop is
  the input desktop. The current `DESKTOP_RIGHTS` (0xC7) lack it: a thread bound
  with 0xC7 was refused, and one bound with 0xE7 worked. `SetThreadDesktop` can't
  rebind a thread that already owns windows ("The requested resource is in use"),
  so the right has to be in the handle that `Desktop::bind` uses before the
  workspace creates its windows.
- Before the switch, `SetCursorPos` also fails with access denied. It needs no
  desktop right beyond the input desktop matching the thread's desktop.

## Checks

| Check | Real input | Evidence |
|---|---|---|
| regedit menu-bar click opens the menu | pass | View opened. Items highlight on hover. The splitter did not move. |
| Clicking a menu item runs its command | pass | View → Font opened the Font dialog. |
| Double-click a value opens its editor | pass | `BuildBranch` opened Edit String. Services and Device Manager open properties the same way. |
| Right-click opens the right menu at the pointer | pass | Modify/Delete/Rename appeared at (470,219), not the "New" menu at (512,384). |
| Apps key opens the context menu | pass | The same value menu appeared at the selected item. |
| Disk Management pane: scrollbar arrows and thumb | pass | The arrows scrolled to Disk 2, and dragging the thumb scrolled back. |
| Disk Management pane: wheel | **fail (the app's own behaviour)** | See "Wheel routing" below. |
| Resource Monitor section arrows and ▶ graph toggle | pass | CPU collapsed, and the graph pane collapsed. |
| Firewall New Rule wizard: WinForms radio, then Cancel | pass | Port selected. Cancel closed the wizard with no snap-in error. |
| MMC Action menu (Device Manager) | pass | Opened on click. |
| Caption double-click | pass | Maximized. The bounds are still 1024×768 (Task 2). |
| Caption drag, caption X, Alt+F4 | pass | All native behaviour, with no emulation code. |
| Typing: scan codes and `KEYEVENTF_UNICODE` | pass | Typed into the Font dialog, Notepad, and a plain cmd window started **without** the console-input helper. |
| Click immediately followed by typing | pass | No lost keys. Real input goes through one ordered queue. |
| Win+R | still types "r" | There is no shell in Session 0 to consume Win combos (Task 5, item 1). |

### Foreground and focus

A real click activates the window under it, as it would on the console. Newly
launched windows don't always take the foreground (one Notepad did, and later
MMC windows did not), but the first click activates them. `SetForegroundWindow`
from the helper succeeds, since the helper produced the last input, so the taskbar's
restore can make the restored window the keyboard target.

### Wheel routing

Session 0 starts with `SPI_GETMOUSEWHEELROUTING` = 0, so the wheel goes to the
focused window rather than the window under the pointer. The SYSTEM profile has no
`MouseWheelRouting` value, while the signed-in user has 2. With 0, wheeling over
the Services list while the tree had focus did nothing. After
`SystemParametersInfo(SPI_SETMOUSEWHEELROUTING, 0, 2, 0)`, which changes only the
session and doesn't persist, the list under the pointer scrolled.

Disk Management's graphical pane still ignored the wheel, both hovered and focused.
The pane is `DMDiskView`, an `AfxWnd42u` window with its own `WS_VSCROLL` (style
`0x50200000`). It isn't a separate scrollbar control: its arrows and thumb work,
but it doesn't handle `WM_MOUSEWHEEL`. This wasn't compared on the console,
because that would mean injecting input into the signed-in session.

### Screen size

`SetCursorPos(1200, 700)` landed at (1023, 700). The cursor is clamped to Session
0's display mode, which is 1024×768. The canvas's right 256 px and bottom 32 px,
including most of the taskbar, can't be reached by real input at that size. Session
0 lists 1280×800 among its modes. `ChangeDisplaySettingsEx(1280×800, CDS_TEST)`
returned -1 while `Winlogon` was the input desktop, and 0 (success) once
`MeshRMMBackground` was. The mode itself was not changed.

### Session 0's own desktops and the console

- WinSta0 in Session 0 holds `MeshRMMBackground`, `Default`, `Disconnect` and
  `Winlogon`. Before the test, the input desktop was `Winlogon`.
- Switching back to `Winlogon` on exit works.
- When the helper exited *without* switching back (a simulated crash), the
  background desktop was destroyed and Session 0 fell back to `Default`, not
  `Winlogon`. A SYSTEM probe on WinSta0 confirmed this. Nothing visible depends on
  it, but the helper should restore the desktop it found.
- A probe in the console session (Session 1) logged the cursor position, input
  desktop and foreground window every 100 ms for the whole run (00:04–00:16). Its
  first sample never changed. Session 0 input never reached the console.

## Sketch of the change

1. `agent/windows/remote-screen/src/background.rs`: add `DESKTOP_SWITCHDESKTOP`
   and `DESKTOP_JOURNALPLAYBACK` to `DESKTOP_RIGHTS`. Update the module comment:
   the console's input desktop is still never switched, but Session 0's now is.
2. At the start of the input helper (`run_background_input_child`, or
   `Workspace::new`): record the Session 0 input desktop's name, call
   `SwitchDesktop`, and set wheel routing to 2 for the session. On drop, switch back
   to the recorded desktop. If a `SendInput` or `SetCursorPos` call is denied later,
   switch again and retry once.
3. `Workspace::apply`: send pointer moves, buttons, wheel, keys and text as
   `SendInput` batches normalized to Session 0's screen, reusing the builders in
   `agent/src/remote/input.rs` (`pointer_move_input`, `mouse_button_input`,
   `send_key`, `text_inputs`). Keep handling taskbar-strip presses in the workspace
   (launch and restore) instead of injecting them. After `restore_task` and
   `launch` of a built-in, call `SetForegroundWindow`. `release()` sends key and
   button releases, like `WindowsInputController::release_all`.
4. Delete the `PostMessage` emulation: `post`, `client_point`, the `WM_NCHITTEST`
   caption/border/button handling and `drag`, `pressed`, `focus`,
   `keyboard_target`, `AttachThreadInput`/`SetKeyboardState`, the `ToUnicodeEx`
   logic, and probably `background_console.rs`, since conhost took real keys
   directly. Also recheck whether the built-in Task Manager's and File Explorer's
   frame-resize special case is still needed.
5. Drop Win-key combos (or map Win+R to Task 6's built-in Run), because no shell
   consumes them.

### What this means for the other tasks

- **Task 2 depends on this change.** Changing the display mode only works once the
  background desktop is the input desktop, and real input needs a 1280×800 mode
  (or a 1024×768 canvas) to reach the whole canvas.
- **Tasks 7, 8, 9 and 11** become checks against the change, except Disk
  Management's wheel, which is the app's own behaviour and should be documented.
- **Task 5:** items 2 (Apps key) and 3 (click/typing race) are resolved by real
  input. Item 1 (Win key) still needs doing, in the new input path.
- **Task 3** is unaffected: regedit's address bar still went black after a menu
  closed.
