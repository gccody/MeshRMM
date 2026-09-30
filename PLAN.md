# Plan: fix the background-mode app issues

The plan has 11 tasks. Task 1 is a short experiment that decides how Tasks 7–9 get built. Six tasks don't depend on it and can start right away in their own threads.

## Things every thread needs

- **Test machine.** Validate on DESKTOP-85R6S28 (`ssh 192.168.1.152`), following `~/.config/meshrmm/AGENTS.private.md`. Every item in the previous report comes with a reproduction step.
- **Test harness.** The interactive tool used for the original testing lives at `~\bg-apps-20260929\agent\src\remote\background\driver.rs` on the endpoint. It isn't in the repo. The memory note `background-mode-app-driver` explains how to run it. Test through it, because it sends the same `RemoteInput` events the viewer sends.
- **One test at a time on the endpoint.** Every workspace uses the same desktop name (`MeshRMMBackground`), so two harnesses running at once share one desktop and spoil each other's results. Before launching one, create `C:\bg-validation.lock` containing your thread name, and delete it afterwards.
- **Shared file.** Tasks 7, 8, 9 and 5 all edit `Workspace::button`/`apply` in `agent/src/remote/background.rs`. Put new logic in new modules (for example `agent/src/remote/background/menus.rs`) and keep edits inside `button()` small, so merges stay easy.
- **No compatibility work needed.** Per `AGENTS.md`, changing the protocol, display size, or constants is fine.

## Order and dependencies

| Start now, in parallel | Wait for the real-input change (Task 1 result: go) |
|---|---|
| 3, 4, 6, 10, and 5 item 1 | 2, 7, 8, 9, 11 |

**Task 1 is done: "go".** See `docs/background-real-input.md` for the evidence and the sketch. The next step is implementing that sketch. Task 2 now depends on it too, because Session 0's display mode can only be changed while the background desktop is the input desktop.

---

## Task 1 — Experiment: real mouse and keyboard input inside Session 0 (time limit: 1 day)

**Status: done, "go"** (2026-09-30). With `SwitchDesktop` plus `DESKTOP_JOURNALPLAYBACK` on the injecting thread's desktop handle, every check below passes except Disk Management's wheel, which the app itself ignores. The console is unaffected. Details and the implementation sketch are in `docs/background-real-input.md`.

**Why.** Most of the input bugs happen because the workspace *posts* window messages instead of producing real input. As a result, Windows never generates double-clicks, the cursor position apps read is wrong, menu loops and scrollbars never start tracking, and WinForms and custom buttons misbehave.

- Session 0 isn't the console (the console is Session 1), so switching Session 0's active desktop to `MeshRMMBackground` wouldn't show anything to the signed-in user.
- Session 0's cursor currently sits at the centre of its screen, (512,384). That matches exactly where regedit opened its wrong context menu.

**What to do.** In the Session 0 helper:
1. Call `SwitchDesktop` to make the background desktop Session 0's active desktop.
2. Deliver a click with `SetCursorPos` plus `SendInput`, and a keystroke with `SendInput`.
3. Check each of these:
   - regedit menu-bar click opens the menu, and clicking a menu item runs its command
   - double-clicking a value opens its edit dialog
   - right-click opens the correct menu at the pointer
   - the Disk Management graphical pane scrolls by wheel and scrollbar
   - Resource Monitor's section arrows respond
   - Cancel in the Firewall New Rule wizard doesn't crash

Also check:
- whether any window has to be focused or foreground first
- the desktop access rights required
- what happens to Session 0's own Winlogon desktop
- that the console session is completely unaffected

**Result.** Write up go/no-go with evidence. On "go", sketch the change: `Workspace::apply` maps pointer and key events to real input, and the `PostMessage` emulation is deleted. Tasks 7, 8, 9 and 11 then mostly become checks against that change. On "no-go", those tasks proceed as written below.

**Files:** `agent/src/remote/background.rs`, `agent/src/remote/capture_helper/child.rs` (`run_background_input_child`), and `agent/windows/remote-screen/src/background.rs` (`Desktop`).

---

## Task 2 — Apps think the screen is 1024×768, not the 1280×800 canvas

**Status: done** (2026-09-30). `Workspace::new` claims Session 0's screen (`agent/src/remote/background/screen.rs`): it makes `MeshRMMBackground` the input desktop, sets the mode to 1280×800, and sets the work area to end above the taskbar. It restores all three on drop. The canvas stays 1280×800, so step 3 wasn't needed. Maximized native windows now come out as (−8,−8)→(1288,760), Firewall opens at (40,0)→(1101,752), and Services and Event Viewer open above the taskbar. Windows forces the standard 8 px frame overhang onto any maximized sizable window that fits the work area exactly, overriding `WM_GETMINMAXINFO` and later `SetWindowPos` calls. So the built-in Task Manager and File Explorer frames now clip their non-client area to the work area while maximized.

**Evidence.**
- Maximized regedit, Event Viewer and cmd come out as (−8,−8)→(1032,776): a 256 px black strip is left on the right and the bottom sits under the taskbar.
- Firewall opens at exactly 1024×768.
- Services and Event Viewer open with their bottom edge under the taskbar.

**What to do.**
1. First, read Session 0's real screen metrics from the helper (`GetSystemMetrics`, `EnumDisplaySettings`) and try changing its display mode to 1280×800.
2. If the mode can be changed, set it at workspace start.
3. If it can't, make the canvas size a runtime value taken from Session 0's metrics instead of the `WIDTH`/`HEIGHT` constants.
4. Either way, call `SystemParametersInfo(SPI_SETWORKAREA)` so that the usable area excludes the 48 px taskbar. Restore the old work area when the workspace closes.

**Code.**
- `agent/windows/remote-screen/src/background.rs:11-13` (constants, `display()`, the renderer, `snapshot_bmp`)
- `agent/src/remote/background.rs` (taskbar geometry, `launch` window rect, pointer scaling in `move_pointer`)
- `agent/src/remote/background_files/window.rs:501` and `agent/src/remote/background_tasks/window.rs:310` (maximize bounds)
- `agent/windows/remote-screen/src/desktop.rs` and `duplication.rs:276`

**Done when:** maximized native apps fill the canvas above the taskbar, and MMC and Firewall windows open fully visible. Update the canvas size in `README.md`, and update the existing ignored tests, which assume 1280×800.

**Conflicts:** `PINS` and taskbar layout with Task 6, if the canvas shrinks.

---

## Task 3 — Some push buttons never draw, others start black

**Status: done** (2026-09-30). The leading hypothesis was wrong: the controls do paint. Session 0 has no DWM, so `PrintWindow` repaints the window into a temporary surface. It then copies that surface before the app's thread has painted the last child controls. Repeated captures of one idle Environment Variables dialog returned four variants: Cancel missing (about 75%), complete, Cancel's frame without its label, and all black. Both flags behaved the same, and neither a prior `RedrawWindow` nor retrying helped. `Renderer` now makes each window layered (`SetLayeredWindowAttributes`, alpha 255), fills the new surface once, and copies it with `BitBlt` from the window DC, so capture no longer asks the app to paint. Composited windows (the built-in tools and the taskbar, whose children stop reaching a layered surface) and windows the app layers itself still use `PrintWindow`. `paint_mmc_toolbars` was the same race and is gone; the MMC menu-label test passes without it. On `DESKTOP-85R6S28` the four dialogs and every pinned app render in their first frames, and the new `dialog_buttons_render_in_every_frame` test fails on the old renderer and passes on the new one. Through the installed service's capture helper, the old build dropped Cancel in 36 of 108 frames and Run's Browse… entirely; the new build dropped neither.

**Evidence.**
- Environment Variables' Cancel button is invisible but clickable.
- The MMC "error in a snap-in" dialog's OK button didn't draw, then later drew as an empty rectangle.
- The Run dialog's Browse… button disappeared after OK was clicked.
- regedit's address bar and a button in Event Properties start solid black and only fix themselves after a repaint.

**What to do.** Work out why the capture step doesn't pick up these controls in `Renderer::paint`, which calls `PrintWindow` with flag 2, then falls back to 0.
- Leading hypothesis: controls that have never received a real paint have nothing for the full-content capture to copy.
- Candidate fixes:
  - force a synchronous `RedrawWindow` on each newly seen top-level window, including all its children, before its first capture
  - composite child windows separately, the way `paint_mmc_toolbars` already does
  - detect captures that come back all black and retry with the other flag

**Code:** `agent/windows/remote-screen/src/background.rs` (`Renderer::paint`, `paint_mmc_toolbars`).

**Done when:** those four dialogs render correctly in their first frames. Add a native regression test covering a dialog with several buttons.

---

## Task 4 — Some dialogs have no taskbar button and can be lost

**Status: done** (2026-09-30). `task_windows` now follows the taskbar rules below. On `DESKTOP-85R6S28`, the real System Properties and Run dialogs are `WS_EX_APPWINDOW` windows owned by hidden `rundll32` windows. Both now get buttons, and regedit's Find dialog doesn't. Restoring needed a second fix: Session 0 now has a foreground window, and `SetWindowPos(HWND_TOP)` from the workspace can't raise another process's window above it. The dialogs open as the foreground window, so they stayed on top of every window restored after them, and a covered dialog stayed buried. `restore_task`, and re-showing a running built-in tool, now call `SetForegroundWindow`, which Task 1's sketch already planned. The extended test fails on the old filter, and also on the new filter with the old `HWND_TOP` restore. The taskbar's hover label also repaints now when it moves between adjacent buttons. Capture copies each window's retained surface, and changing the label didn't repaint it, so it kept showing the previous button's name.

**Evidence.** System Properties (`sysdm.cpl`) and the Run dialog are owned by hidden windows, so `task_windows` skips them (`agent/src/remote/background.rs:1047`). Once another window covers one, it can only be recovered by minimizing the covering window.

**What to do.** Use Windows' own taskbar rules:
- show a window that is visible, isn't a tool window, and either has no owner, has an owner that isn't visible, or has the `WS_EX_APPWINDOW` style
- still exclude menus (`#32768`), tooltips and the workspace's own windows

**Done when:** System Properties and the Run dialog get taskbar buttons that restore them. Normal owned dialogs (Find, Properties) still don't get buttons. Extend `running_window_taskbar_restores_minimized_window` to cover this.

**Conflicts:** none. The change is limited to `task_windows` and `refresh_tasks`.

---

## Task 5 — Keyboard fixes (independent of Task 1 unless Task 1 replaces key posting)

**Status: done** (2026-09-30), in the current window-message input path. `background/keyboard.rs` drops the Windows keys, every key pressed while one is held, and those keys' releases. It runs before events split between GUI windows and the console helper, so it covers both paths and still applies after the real-input change. Windows opens a context menu only from a real Apps key-up, not from a posted one, so the workspace now posts `WM_CONTEXTMENU` (lParam −1) to the keyboard target after the Apps key-up, unless Ctrl or Alt is held. On `DESKTOP-85R6S28` the Apps key opened regedit's Modify/Delete/Rename menu and Services' Start/Stop/…/Properties menu at the selected row, with no second menu after Escape. Item 3 didn't reproduce: typing straight after clicking regedit's address bar arrived intact in the old and new builds (the new test runs it 5 times, and the installed-service run once). The new `windows_and_apps_keys` and `apps_key_opens_native_context_menus` tests, and `console_exit_closes_window_task_and_input_helper` with an added Win+R, fail on the old code and pass on the new. The installed service's helper passed the same checks from its stdin, and the previous build failed them.

**Evidence and what to do.**
1. **Win-key shortcuts type their letter.** Win+R typed "r" into the console. Treat `VK_LWIN`/`VK_RWIN` as modifiers so they suppress typed characters (the `literal` check at `agent/src/remote/background.rs:893`) and are left out of `ToUnicodeEx`. In the console-input path, `background_console.rs` needs the same treatment.
2. **The Apps/Menu key does nothing, while Shift+F10 works.** Make sure the key-up for `VK_APPS` reaches the app's default window procedure, or post `WM_CONTEXTMENU` with lParam −1 to the keyboard target. Check this in regedit and MMC.
3. **Possible race between a click and typing that follows.** Once, typing straight after clicking regedit's address bar was lost. Try to reproduce it with zero delay between the click and the key events; if it reproduces, make keyboard focus changes settle before the following keys go out.

**Done when:** Win+R types nothing, and the Apps key opens the selected item's context menu. Add unit or native tests.

**Conflicts:** the key branch of `Workspace::apply`. This doesn't overlap the `button()` edits in Tasks 7 and 8.

---

## Task 6 — Pins for the missing apps, a working Run dialog, and the taskbar icon

**Status: done** (2026-09-30). Disk Management, System Properties (`SystemPropertiesAdvanced.exe`), Notepad and Run are pinned after the existing pins, so the old pin indices still hold. Each pin names its icon; Device Manager's is `devmgr.dll` index 5, since index 4 is a generic gear document. Run is a small dialog on the workspace thread (`background/run.rs`), opened by its pin or by Win+R, which now has a keyboard route of its own. Keyboard input moves to Run as soon as it opens, and Escape or Cancel hands it back. `background::launch::resolve` finds what was typed the way Windows' Run does, without ShellExecute: by path or with `SearchPath`, with `PATHEXT` extensions tried, and unquoted paths with spaces allowed. Documents open through their association (`.msc` in MMC, `.cpl` through `control.exe`), and folders, `explorer` and `taskmgr` open the built-in tools. Real `taskmgr.exe` exits at once in Session 0 with no window. What Run starts is assigned to the workspace job, and console programs get console input. Task Manager's "Run as SYSTEM" and File Explorer's Open use the same resolver. On `DESKTOP-85R6S28`, the installed service's helper ran `notepad`, `diskmgmt.msc` and `sysdm.cpl` from Run, opened the three new pins as its own children, and left no process behind when it stopped.

The shell's own Run dialog does work in Session 0 as SYSTEM, but not when hosted by rundll32. Driven with `WM_COMMAND IDOK`, `rundll32 shell32.dll,#61` ran `notepad` and `cmd /c …`. It silently ignored `C:\Windows\System32\notepad.exe`: the dialog stayed open, nothing started, and no error appeared. `RunFileDlg` called from a PowerShell thread with valid arguments ran the same full path whether COM was STA, MTA, or left to the runtime. So the cause is rundll32 calling ordinal 61 with its own `(hwnd, hinstance, command line, show)` entry-point arguments, which `RunFileDlg` takes as its icon, directory, title and flags. The workspace's posted clicks also can't press the dialog's OK (`BM_CLICK` didn't), which would explain why short names failed in the original report too.

With 15 pins the running-window buttons start at x = 740, so 11 fit at full width on the 1280-pixel canvas. More share the 532 remaining pixels, and past 16 their 32-pixel icons are cut off.

`apps_key_opens_native_context_menus` now fails on this endpoint before and after this change, identically: regedit's address bar never takes the typed key. The other ignored background tests pass.

**Evidence.**
- Disk Management, System Properties, Notepad and Run aren't pinned.
- `rundll32 shell32.dll,#61` shows the Run dialog, but it launches nothing, even `C:\Windows\System32\notepad.exe`, and reports no error.
- Win+R does nothing (there's no shell to handle it).
- The Device Manager pin shows a generic document icon (the `devmgr.dll`, index 4 entry near `background.rs:229`).

**What to do.**
1. Add pins:
   - Disk Management: `mmc.exe diskmgmt.msc`
   - System Properties: launch `SystemPropertiesAdvanced.exe` (or `...ComputerName.exe`) directly, not through `control`/`rundll32`
   - Notepad
   - Run
2. Implement Run as a small built-in dialog. Reuse Task Manager's "Run as SYSTEM" launch code (`background_tasks.rs:1570`, via `background::launch::launch`, so it stays on this desktop and in the job).
3. Investigate briefly why the shell's own Run dialog fails. Most likely it depends on the Explorer shell's window or its launch mechanism. Document the result; don't try to fix Windows.
4. Fix the Device Manager pin icon, and check every pin's icon.
5. Recheck how many running-window buttons still fit with 15 pins (≈720 px). This depends on Task 2's final canvas width.

**Done when:** each new pin launches its app in Session 0 inside the workspace job, and the built-in Run launches `notepad`, `diskmgmt.msc` and `sysdm.cpl`. Update the pin list in `README.md` and the hard-coded pin indices in the tests (`tests.rs` uses 2, 7, 8 and 11; `task_icon` uses 7 and 11).

**Conflicts:** `PINS` and the icon match in `background.rs`.

---

## Task 7 — Menus: menu-bar clicks, menu-item clicks, and focus after a menu closes

**Evidence.**
- Clicking regedit's menu bar doesn't open a menu. The catch-all `_ => {}` at `agent/src/remote/background.rs:743` hands the click on as an ordinary client click, and regedit moves its splitter to that position.
- Resource Monitor and the built-in Task Manager menus don't open on click either.
- In an open popup menu (regedit and MMC), clicking an item closes the menu without running the command.
- After a menu closes, `focus` still points at the destroyed menu window, so `keyboard_target` returns 0 and typing is lost until the next click.

**What to do if Task 1 is "no-go".**
- **Menu-bar click:** find which item was hit with `GetMenuBarInfo` and `MenuItemFromPoint`, then open that menu the keyboard way (`WM_SYSCOMMAND` with `SC_KEYMENU`, then arrow to the item). Never forward a menu-bar click as a client click.
- **Popup-menu window (`#32768`):** get the menu with `MN_GETHMENU`, find the item at the pointer with `MenuItemFromPoint`, then select and run it with `MN_SELECTITEM` and `MN_BUTTONUP` (undocumented, but these are what accessibility tools use). Fall back to arrow keys plus Enter. Also highlight items on hover.
- **Focus:** when the focus window has gone, fall back to the focused control of the top workspace window.

**Done when:**
- regedit's File and Edit menus open on click and Find… runs from a click, and the same works in MMC's Action menu
- regedit's splitter doesn't move on a menu-bar click
- typing works straight after a menu closes

Add native tests using a simple test window with a menu.

**Conflicts:** `button()` and `keyboard_target()`. Coordinate with Task 8.

---

## Task 8 — Double-click, scrollbars and mouse wheel

**Evidence.**
- Double-click never registers in native apps (no regedit value editor, service or device properties). Only the built-in File Explorer detects double-clicks itself.
- The graphical pane in Disk Management won't scroll by wheel, arrow buttons or thumb drag. Arrow keys do scroll it. Device Manager's and Task Manager's scrollbars do work.

**What to do if Task 1 is "no-go".**
- **Double-click:** keep the previous mouse-down (window, button, time, position). If a second press comes within `GetDoubleClickTime()` and the system double-click distance, on a window class that accepts double-clicks, send the double-click message for that button (left, right or middle). A double-click on a window caption should toggle maximize.
- **Scrollbars:** hit-test the child window under the pointer, not just the top-level window. If the click lands on a scrollbar, translate it into scroll messages using the scrollbar's layout: arrows scroll by a line, the track by a page, and a thumb drag tracks then sets the position. Figure out why the Disk Management pane differs from lists that already work: it may use separate scrollbar controls, or it may only handle the wheel when it has focus.
- **Wheel:** follow Windows' behaviour and send the wheel to the window under the pointer, falling back to the focused window.

**Done when:**
- double-click opens the editor or properties in regedit, Services, Event Viewer and Device Manager
- double-clicking a caption maximizes the window
- Disk Management's graphical pane scrolls all three ways

Add native tests.

**Conflicts:** `button()` and `wheel()`. Coordinate with Task 7, and put the new logic in its own module.

---

## Task 9 — Right-click menus appear in the wrong place with the wrong items

**Evidence.** Right-clicking a registry value showed regedit's empty-area "New" menu at (512,384), which is Session 0's screen centre, instead of Modify/Delete/Rename at the pointer. MMC apps put the menu in the right place.

**What to do.** Apps that read the real cursor position (`GetCursorPos`/`GetMessagePos`) need Session 0's cursor to match the workspace pointer. That is Task 1's `SetCursorPos`. If Task 1 fails, test whether `SetCursorPos` alone works while the background desktop isn't Session 0's active desktop. If it doesn't, document regedit as limited to Shift+F10.

**Done when:** right-clicking a value in regedit opens the value's menu at the pointer.

---

## Task 10 — Open/Save dialogs report the SYSTEM Desktop folder as unavailable

**Evidence.** Every common Open/Save dialog first shows "`C:\WINDOWS\system32\config\systemprofile\Desktop` is unavailable". Notepad's Save As worked once that was dismissed.

**What to do.** Before launching apps, create `%SystemRoot%\System32\config\systemprofile\Desktop` if it's missing, and the matching folder under `SysWOW64` on 64-bit Windows. This is a small, deliberate change to the machine; mention it in `README.md`.

**Done when:** Notepad's Save As, regedit's Export, and Event Viewer's Save All Events As open without the error.

**Conflicts:** none. Use a helper called from `run_background_input_child` or `Workspace::new`.

---

## Task 11 — Firewall New Rule wizard crashes when Cancel is clicked, and Resource Monitor's arrow buttons ignore clicks

**Evidence.**
- Clicking Cancel in the New Inbound Rule wizard gives "MMC has detected an error in a snap-in", with `ObjectDisposedException: Cannot access a disposed object 'Button'`. The stack trace runs `Button.OnMouseUp → PointToScreen`.
- It reproduces every time; Esc cancels cleanly.
- Resource Monitor's section arrows and ▶ graph-pane toggle take focus but never act. Clicking the section header works.

**What to do.**
1. Confirm both work with real input: on the console desktop while it's idle, or in a VM. If they work there, the problem comes from the workspace's input emulation.
2. After Task 1:
   - If it's "go", retest both; they're likely fixed.
   - If it's "no-go", compare the message sequence a real click produces against the workspace's (Spy++ or a message hook). Suspects:
     - mouse capture during the press
     - the order of `SetFocus` and the button press in `button()`
     - `WindowFromPoint` using the real cursor position
3. Fix the difference in the input code, working with whichever of Tasks 7 and 8 owns `button()`.

**Done when:** clicking Cancel closes the wizard without an error, and Resource Monitor's arrows expand and collapse sections.

---

### Out of scope, but deserves a decision

Real `explorer.exe` starts in Session 0 and never shows a window. It sits hidden in the workspace job until the workspace closes. `README.md` already says Explorer isn't supported. Decide whether to leave that as documented, or to detect hidden `explorer.exe` processes in the job, end them, and show a notice pointing to the built-in File Explorer.
