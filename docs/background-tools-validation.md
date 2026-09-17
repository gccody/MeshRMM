# Background tools

## Task Manager

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
