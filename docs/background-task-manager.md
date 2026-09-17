# Background Task Manager

The taskbar launches a native Windows 10-style Task Manager on MeshRMM's private
SYSTEM desktop. Native `taskmgr.exe` cannot create its window on the validated
Windows 11 Session 0 desktop; this implementation keeps desktop isolation and
uses Win32 controls and GDI so the existing background capture/input path works.

## Views and actions

- **Processes:** grouped, expandable applications/background/Windows processes;
  sortable CPU, working-set memory, disk and network rates; resource heat colors;
  identity-preserving selection and scrolling; compact application view.
  Apps are windows on the isolated desktop and their descendants. Processes in
  other sessions remain visible in the process inventory and Details.
- **Performance:** selectable CPU, memory, physical disk and hardware-network
  graphs, 60 samples, process/thread/handle counts, uptime and commit statistics.
- **App history:** CPU and I/O accumulated while this Task Manager is open, with
  a reset action. This is not Windows' persisted per-user Store-app history.
- **Startup:** native Run/Run32 and Startup-folder entries for the machine and
  loaded user profiles. Enable/disable changes the Windows startup approval
  value, preserving the command/file and rejecting stale confirmation state.
  Packaged-app startup entries and Windows' boot-impact history are not exposed.
- **Users:** signed-in sessions, aggregate CPU/memory, expandable processes and
  confirmed disconnection. Disconnection leaves the user's applications running.
- **Details:** PID, owner, session, CPU, working set, threads, handles, I/O,
  priority and executable path; process/tree termination and priority changes.
- **Services:** names, PIDs, descriptions and real states; confirmed start, stop,
  restart and navigation to the owning process. Restart waits for the service to
  stop before starting it. The MeshRMM connection service is protected.

File > Run new task launches under the existing SYSTEM token on the isolated
background desktop and inherits the workspace cleanup job. View controls refresh
speed, pause/manual refresh and grouping; Options controls always-on-top. F5,
keyboard selection, context menus and Ctrl+Tab are supported.
The taskbar pin restores the existing Task Manager after minimization. Maximizing
uses the background viewport above its taskbar, and the frame supports border
resizing through the isolated input path.

Process termination retains validated process handles across confirmation and
checks creation times, self-termination and Windows' critical-process flag.
Sampling, service commands and other potentially slow actions run off the UI
thread. Inventory refreshes retain selection by identity, not just PID.
Unavailable or access-denied measurements are shown explicitly rather than as
invented zeroes. Working-set memory and I/O definitions are labeled; aggregate
network excludes virtual/loopback interfaces, while per-process ETW counts TCP/UDP
traffic including loopback. These totals need not match.

The helper activates version 6 common controls only for its own UI thread.
Masked, padded icons and scrollbar painting address Session 0 capture omissions.
The paint fallback uses native scrollbar geometry and states. A list-specific
adapter translates background client mouse messages into native scroll commands
for arrows, page clicks, hold-to-repeat and thumb dragging. Other applications
keep their existing input routing. No global theme or desktop setting changes.

## Telemetry

A private, bounded, real-time ETW system session measures process disk and TCP/UDP
bytes. It neither attaches to nor modifies an existing trace session. Normal
window closure stops it; the background workspace retains the helper identity and
stops its trace after forced job cleanup. Event loss makes process rates
unavailable. Physical disk activity uses PDH; CPU and memory use native APIs.

The implementation follows Microsoft's [private system trace session guidance](https://learn.microsoft.com/en-us/windows/win32/etw/configuring-and-starting-a-systemtraceprovider-session)
and [disk](https://learn.microsoft.com/en-us/windows/win32/etw/diskio-typegroup1)
and [TCP event layouts](https://learn.microsoft.com/en-us/windows/win32/etw/tcpip-typegroup1).
GPU graphs, Windows' persisted application/boot history, and shell-dependent
property/dump/wait-chain dialogs are not implemented. This is a functional
background replacement, not a claim of complete native Task Manager parity.

## Validation

Native tests cover sampling, numeric sorting, expansion, tabs/compact/pause,
selection/scroll preservation and PID reuse, real disposable-child termination,
startup toggling/stale-state rejection, and protecting the connection service.
Ignored endpoint tests explicitly exercise real ETW disk/network traffic and
trace cleanup, disposable service start/restart/stop, and routed background GUI
input across all tabs, Run new task, confirmation/cancel and termination.

Validated on **2026-09-17**, natively on `DESKTOP-85R6S28` (Windows 11), using
Rust 1.97.1 through rustup. The dedicated checkout
`C:\Users\gccody\meshrmm-task-20260917` was checked against a SHA-256 manifest
including uncommitted and new source files.

- `cargo clippy -p meshrmm-agent -p meshrmm-remote-screen --all-targets -- -D warnings`: passed.
- `cargo test -p meshrmm-agent -p meshrmm-remote-screen`: 78 passed; 25 explicitly
  platform-specific tests ignored by the normal suites.
- Three relevant ignored tests were explicitly run as SYSTEM: live disk/network
  ETW traffic and trace cleanup; disposable service start/restart/stop; and the
  background GUI test using the optimized release binary. All passed.
- The GUI scenario exercises all seven tabs, both scrollbar arrows and thumb
  dragging through routed input, minimize/pin restore without a duplicate helper,
  exact 1280×752 maximization and restore, border resizing, compact mode, pause
  and manual refresh, Run new task, and cancellation/confirmation of termination
  against an identified disposable process.
- `cargo build --locked --release -p meshrmm-agent`: passed.
- `cargo fmt --all -- --check` and `git diff --check`: passed on macOS.

Installed with `scripts/install-agent-local.ps1 -SkipBuild`. The supported updater
confirmed startup, signaling reconnection, and unchanged protected configuration.
Final installed executable SHA-256:
`614101472859F6741158E24C7B157EAC5CC558104B4F63AAB4A6F087776F3003`.

The installed macOS viewer connected through the dashboard handoff and opened the
Task Manager from the background taskbar. Live checks confirmed process sorting,
resource graphs, startup/service inventory, horizontal scrolling, compact/detail
switching, minimize/pin restore, full-viewport maximization and normal closure.
After disconnect, the service remained running; no Task Manager helper, disposable
fixture service, or Task Manager ETW session remained. Transport logs reported
zero dropped encoded frames. Existing helper-stderr logging of INFO statistics
and WebRTC shutdown warnings were still present; no Task Manager failure was
observed in the final session.

Local evidence is retained under `dist/task-manager-validation/`: PNG captures of
all seven tabs, compact mode and termination confirmation, native-check logs,
SYSTEM scenario logs, installer results, and installed-service health output.
These generated validation artifacts are ignored by Git. No server, dashboard,
or macOS viewer source changed; their builds were outside this validation scope.
