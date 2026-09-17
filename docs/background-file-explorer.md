# Background File Explorer

The background File Explorer pin opens a native Windows 10-style browser inside
MeshRMM's isolated SYSTEM Session 0 desktop. It restores an existing browser when
possible. File > Open new window creates another browser in the same workspace.
Closing the workspace terminates its browsers and their child applications.

The window uses a flat light frame, File/Home/View controls, grouped ribbon
commands, a navigation pane, clickable path breadcrumbs, an editable address bar,
a search box, native file icons, sortable/resizable details columns, and a selection
status bar. View offers details, small icons, and large icons, plus hidden-item and
filename-extension toggles. Dates follow the endpoint's locale. The frame supports
resizing, maximizing within the background workspace, minimizing, and restoring
from the File Explorer pin. Common-controls v6 is activated only for this helper.

## File management

- This PC lists drives and public folders. Quick access uses public folders because
  this desktop runs as SYSTEM; it does not impersonate the signed-in user.
- Navigate with breadcrumbs, an absolute drive/UNC address, Back, Forward, or Up.
  Failed navigation keeps the current folder and history intact.
- Search recursively by case-insensitive filename or Windows `*`/`?` wildcards.
  Inaccessible folders are counted in the result status. Cancel interrupts search
  between directory reads. F5 refreshes the current search; clearing the search box
  and pressing Enter returns to the folder.
- Select multiple items, select all/none, invert selection, and sort naturally by
  name, modification time, type, or size. Folders remain ahead of files.
- Create folders and text documents, then rename inline. Enter commits and Escape
  cancels the edit. Refresh/sort preserve selected paths.
- Copy/cut/paste files and directory trees. Native CF_HDROP and Preferred DropEffect
  formats allow copying between background browser windows and desktop applications.
  Moving clears successfully moved entries from the clipboard without replacing a
  newer clipboard selection. Copying within the same folder chooses a unique name.
- Drag files onto a folder or a navigation destination to move within a volume;
  Ctrl-drag copies. Cross-volume drag defaults to copying. Dropping keeps the source
  folder open and does not replace the clipboard.
- Ctrl+Z / File > Undo reverses up to 20 recent supported copy, create, rename, or
  move operations. Undo checks file identities and verifies copied/created trees
  have not changed; it refuses to remove modified copies or overwrite new files.
  Permanent deletion cannot be undone.
- Existing destinations are never silently overwritten or merged. Operation errors
  are reported, including partial completion. Cancellation preserves completed work.
  Recursive copy refuses reparse points and copying a folder into itself. Deleting
  a directory junction removes the junction rather than its target.
- Delete captures the selected paths and requires an inline confirmation. It is
  explicitly **permanent deletion**, not a Recycle Bin operation. Cancel does not
  delete anything.
- Open starts the associated desktop executable with an explicit background desktop,
  the current SYSTEM token, no inherited handles, and the workspace's inherited job.
  It does not use ShellExecute/DDE to redirect activation into an interactive session.
  Preview provides bounded, read-only UTF-8/BOM-marked UTF-16 text viewing. Properties
  displays file metadata; File > Copy path publishes quoted paths to the clipboard.

Directory enumeration, search, copying, moving, and deletion run off the UI thread.
The original 20,000-entry and 1 MiB text-preview bounds remain in place. An active
operation can be cancelled; a currently blocked filesystem call must return before
cancellation can take effect.

## Keyboard

| Shortcut | Action |
| --- | --- |
| Alt+Left / Alt+Right | Back / Forward |
| Alt+Up / Backspace | Parent folder |
| Ctrl+L / Alt+D / F6 | Edit address |
| Ctrl+F / Ctrl+E / F3 | Search |
| Enter | Navigate, open, search, or commit rename according to focus |
| F2 | Rename the selected item |
| F5 | Refresh |
| Ctrl+C / Ctrl+X / Ctrl+V | Copy / Cut / Paste |
| Ctrl+Z | Undo the last recorded file operation |
| Ctrl+A | Select all items or all text in the active edit control |
| Ctrl+Shift+N | New folder |
| Delete | Request permanent deletion, with confirmation |
| Alt+Enter | Properties |
| Escape | Cancel pending deletion/drag or inline rename; clear a focused search |

## Differences from the Windows shell

This is not the Windows Explorer executable or a complete shell namespace host.
Native Explorer failed to create a folder window on the validation endpoint's
isolated desktop, as recorded in [background tools](background-tools-validation.md).
The browser does not provide a Recycle Bin, shell extensions, Share/OneDrive
integration, indexed/AQS searches, thumbnail providers, editable ACL/property sheets,
archive namespaces, or OLE dragging into other applications. Properties is read-only.
Desktop associations that depend on UWP activation, DDE, or an interactive user
session may be unavailable. Folder moves across volumes can fail; use copy followed
by explicitly confirmed deletion instead. These differences are not represented by
inert ribbon buttons.

## Validation

Validation uses the SHA-256-verified working source in
`C:\Users\gccody\meshrmm-explorer-20260917` on `DESKTOP-85R6S28`.
The ignored `remote::background::tests::file_browser_opens_in_background` test runs
as SYSTEM in Session 0, launches the real Agent helper through the workspace, and
exercises disposable fixtures. It verifies browsing, inline rename, file/folder
copy, cut/paste, deletion confirmation and cancellation, recursive search, selection,
view switching, pointer drag/move and Ctrl-drag/copy, guarded Undo, resizing, maximization,
minimization, pin restoration, job membership, and cleanup. It captures Home, View,
and deletion-confirmation images.

Unit regressions cover unsafe paths/names, bounded text decoding, preserving existing
destinations, natural sorting, hidden metadata, unique copy names, history,
cancellation, junction handling, wildcard matching, guarded Undo, and quoted association arguments.
Final validation on 2026-09-17:

- Windows native workspace Clippy with warnings denied passed. Workspace tests:
  **201 passed, 0 failed, 25 ignored**. The release Agent build passed.
- Three ignored scenarios were then explicitly run as SYSTEM against the installed
  executable: File Explorer (17.72 s), Task Manager (30.01 s), and Session 0 GUI /
  keyboard / H.264 capture (3.02 s). All passed.
- The supported `scripts/install-agent-local.ps1 -SkipBuild` procedure installed
  the working-tree build based on `a346d16`, preserved configuration, and confirmed
  signaling reconnection. Installed SHA-256:
  `E6837EA14C13F263DB1B708EC3C803BFC7D616FCB3435CCACF61F195E92541CF`.
- The macOS viewer connected through the dashboard to the installed Windows service.
  Live checks passed for address/breadcrumb navigation, folder creation and inline
  rename, copy/paste and Undo, recursive `*.txt` search (84 results), large icons,
  details, and scrollbar page movement. Input was checked after asynchronous UI
  transitions completed.
- After disconnect, `MeshRMMAgent` remained running (service PID 656), only its
  service and worker processes remained, and the background helpers were gone.
  Reviewed logs contained no Explorer errors or panics; transport shutdown warnings
  accompanied normal session teardown. The temporary scheduled test task was removed.
- Formatting and `git diff --check` passed on macOS. An optional Windows cross-build
  on macOS was blocked by missing Windows SDK headers; native Windows checks above
  provide the platform validation. No server or dashboard implementation changed.

Native logs and captured screenshots are retained in the endpoint's `proof` folder
and locally in the ignored `dist/explorer-validation/` directory. The latter contains
`explorer-home.png`, `explorer-view.png`, `explorer-delete.png`, workspace check logs,
and the three `installed-*.log` scenario results.
