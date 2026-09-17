# Background tools

## Task Manager

The Windows 10-style replacement and its current validation are documented in
[Background Task Manager](background-task-manager.md). The notes below record
the earlier implementation and the native Task Manager compatibility investigation.

The background taskbar's Task Manager button launches MeshRMM Task Manager,
a native Win32 process manager inside the existing isolated Session 0 desktop.
It lists processes across sessions with PID, session ID, and working-set memory,
refreshes asynchronously, and supports confirmed End Task. It is a maintenance
tool, not a replica of every Windows Task Manager tab.

The original `System32\\taskmgr.exe` launch reproduced a missing window on
Windows 11 Pro build 26200. Both the `-d` classic UI option and the SysWOW64
executable also failed to produce a window on this desktop. A zero process exit
code did not mean a usable window existed. No Windows executable, policy, user
token, or interactive desktop is modified to work around this limitation.

The built-in helper uses the installed Agent executable, with no inherited
handles or console window. It is assigned to the workspace's kill-on-close job
before it resumes. It requires Session 0 and binds to the background desktop.
End Task holds a process handle across confirmation and validates its creation
time, preventing a stale row from terminating a recycled PID. Critical processes
and the process manager itself cannot be ended through this tool. Access errors
are displayed rather than silently ignored.

Validation on `DESKTOP-85R6S28`:

- Windows Clippy and tests for `meshrmm-agent` and `meshrmm-remote-screen`.
- A native process snapshot test, actual termination of a disposable child, and
  refusal to terminate a process with a mismatched creation time.
- Explicit SYSTEM Session 0 `task_manager_opens_in_background`: launch through
  the workspace, require populated rows, capture the window, verify job
  membership, and require process exit after workspace cleanup.
- Locked Windows release build and installation through
  `scripts/install-agent-local.ps1 -SkipBuild`.

The source was synchronized to `C:\Users\gccody\meshrmm-launch-20260916` and
verified against a SHA-256 manifest before the native checks. The installed
service started and reconnected with its configuration preserved.

Final installed Task Manager build SHA-256:
`F7F7404AD4E3A7C5F86BE0620E02A4AACFF498BFB53469211BA6F90D85BC9F08`.
The final native suites passed (68 tests; 22 platform-specific tests ignored),
plus the explicitly executed Session 0 test. A list-view regression verifies that
inserting a process before the visible rows preserves the selected PID and scroll
anchor. Formatting and `git diff --check` passed on macOS.

The installed macOS viewer opened the tool from the background taskbar, displayed
processes, accepted keyboard selection and scrolling, and kept the list anchored
across refreshes. Inline confirmation displayed the name and PID of the disposable
`MeshRMM-EndTask-Test.exe`; confirming ended only that test process and displayed
"Process ended." Cancel was also exercised without ending the selected process.
The session was disconnected and the service remained running. Existing WebRTC
shutdown warnings remain outside this change. Standard Windows message boxes
were unsuitable because their text was missing in background capture; this tool
uses an inline confirmation instead. It does not guarantee artifact-free capture
of every Windows control.

## File Explorer

The current Windows 10-style replacement and its validation are documented in
[Background File Explorer](background-file-explorer.md). The notes below describe
the original minimal browser and its earlier validation.

The File Explorer pin launches MeshRMM File Browser on the same private desktop.
Native Explorer with `/separate,C:\` failed to create a folder window on the
validation endpoint. The built-in browser uses asynchronous directory enumeration
and file operations, supports absolute paths, parent navigation, folder creation,
rename, individual file copy/paste, and read-only UTF-8/UTF-16 text previews.
Existing destinations are never overwritten. Unsafe child names and device paths
are rejected. Previews are limited to 1 MiB and listings to 20,000 entries.
Shell associations, recursive copying, and deletion are not implemented.

Native Windows Clippy, 71 tests, and a locked release build passed against the
SHA-256-verified final source (23 platform-specific tests ignored in the default
suites). Three ignored tests were then explicitly executed as SYSTEM in Session 0:
Task Manager launch/cleanup, File Browser launch/cleanup, and `session_zero_gui`.
The last also verifies ordered address editing, Registry Editor capture, H.264,
and mixed-case PowerShell input. Printable window input now uses the same queue
as navigation keys so it cannot overtake Home/Delete or disappear on a 20 ms
synchronous-send timeout.

Installed agent SHA-256:
`F111EE03CFF56E4CC808DCA51E84CC900D8A69FF26F2B9BA79DD0D3AC3E36A07`.
The supported installer preserved configuration and confirmed service startup
and signaling. Through the installed macOS viewer, validation exercised Ctrl+A,
a full typed path, folder navigation, a visible text preview, folder creation,
rename, file copy/paste, and the visible duplicate-destination error. Only fixtures
inside the dedicated checkout were changed. Remote hashes confirmed identical
source/destination files. Disconnect removed the browser helper, left the service
running, and logged zero dropped encoded frames; existing WebRTC shutdown warnings
were observed. Formatting and diff whitespace checks passed on macOS.
