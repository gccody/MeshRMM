# Background taskbar validation

## Compact taskbar and capture stability follow-up

Validated September 16, 2026 on `DESKTOP-85R6S28`, using the installed macOS
viewer for the live service check. The taskbar is now 48 pixels high (previously
96), with 32-pixel icons in 48-pixel-wide buttons. Removing the status line
returns 48 rows to the application workspace. All ten launchers and hover labels
remain available.

The launcher uses `WS_EX_COMPOSITED` so its child buttons paint into a complete
buffer before capture. A cross-thread regression reproduced the missing/partial
firewall icon without this style and passed with it. GDI copies are also flushed
before and after `PrintWindow` when updating retained window images. Capture
budgets, window retention, and the full-content capture fallback are preserved.

Final source was synchronized to a dedicated Windows directory and checked
against a SHA-256 manifest. Validation passed:

- Windows Clippy: `cargo clippy -p meshrmm-agent -p meshrmm-remote-screen --all-targets -- -D warnings`.
- Windows tests: `cargo test -p meshrmm-agent -p meshrmm-remote-screen` (65 passed, 21 ignored).
- Explicit SYSTEM Session 0 GUI checks: retained-window budget/close regression,
  launcher/input/H.264/cleanup integration, and
  `stable_taskbar_and_management_caption`. The latter compares 60 cross-thread
  captures of the idle taskbar, then 60 more captures of the taskbar and Computer
  Management caption. All passed. Earlier same-thread-only checks missed the
  missing icon; the final regression uses a separate capture thread.
- Windows release build: `cargo build --locked --release -p meshrmm-agent`.
- macOS: `cargo fmt --all -- --check` and `git diff --check`.

The supported local installer installed working-tree release 0.2.7, SHA-256
`1C6951AE7BA4156D9468B8BAE8A686A2D8B777D2D55592F80BC0F129D5D508B3`.
The service started and reconnected with its configuration unchanged. In the live
viewer, all ten compact icons rendered, the status line was absent, Computer
Management launched and dragged successfully, and hover labels remained usable.
Firewall also launched and closed successfully; the underlying Computer
Management window remained visible. The firewall icon remained present across
those observations. After disconnect, logs confirmed session cleanup with zero
dropped encoded frames, no Session 0 MMC process remained, and the service was
still running with the installed hash above. Existing WebRTC shutdown warnings
were logged on disconnect. The automated caption comparisons passed; these checks
do not establish flicker-free rendering for every application or GPU surface.
Some MMC menu text was absent while inactive and reappeared after interaction;
that pre-existing application-rendering limitation remains.

## Earlier validation (before the compact taskbar follow-up)

Validated September 16, 2026 on Windows endpoint `DESKTOP-85R6S28`, with the
installed macOS viewer used for the service session.

The background canvas is black. The charcoal taskbar has 48-pixel application
icons, icon-only buttons, white hover labels, a cyan hover underline, and a
SYSTEM/background-session status line. Administrative snap-ins use distinct
icons. The existing ten launchers and application isolation are preserved.

The capture session retains bounded per-window GDI images. Refreshes rotate
through windows within the existing 100 ms budget; compositing includes all
cached windows even when that budget expires or a window is hung. A failed
PrintWindow call cannot overwrite the last retained image. Closed, hidden,
resized, or off-canvas windows invalidate their cached images. This addresses
whole-window disappearance caused by stopping the old compositing loop early.
It does not guarantee flicker-free application rendering: PrintWindow can report
success with blank application content, and a brief blank Registry Editor address
bar was observed during dragging before it repainted normally.

Checks against the synchronized working tree (SHA-256 source manifest verified):

- Windows: `cargo clippy -p meshrmm-agent -p meshrmm-remote-screen --all-targets -- -D warnings` passed.
- Windows: `cargo test -p meshrmm-agent -p meshrmm-remote-screen` passed: 65 tests, 20 ignored.
- Windows: `cargo build --locked --release -p meshrmm-agent` passed.
- Windows SYSTEM Session 0: explicitly ran the ignored
  `background::tests::retained_windows_survive_refresh_budget_and_close` test.
  Two deliberately slow windows remained visible across budget exhaustion;
  closing one cleared its pixels while preserving the other. Passed.
- Windows SYSTEM Session 0: explicitly ran the ignored
  `remote::background::tests::session_zero_gui` test. Verified black empty-desktop
  pixels, taskbar launching, rendering, H.264 encoding, PowerShell mixed-case
  keyboard input, and application cleanup. Passed. The captured bitmap was
  visually inspected, including tooltip text and application icons.
- macOS: `cargo fmt --all -- --check` and `git diff --check` passed.

The supported `scripts/install-agent-local.ps1 -SkipBuild` workflow installed
release 0.2.7 from the modified working tree, SHA-256
`3A1162B66C9F9DBEB6E0533621586DF6FD49E34FE2A3A9C9D0C85BC11D18C573`.
Configuration was preserved, and the service started and connected. Its installed
hash was verified again after live testing. The earlier validation's automatic
replacement problem did not recur with this checkout's 0.2.7 release metadata.

In a live connection to that installed service, selected Background, verified the
black canvas and taskbar, launched Registry Editor and Command Prompt, dragged
Registry Editor, observed overlapping windows, closed Command Prompt, and verified
that the underlying application remained visible. Hover labels and highlights
rendered correctly. Disconnected the viewer; logs confirmed session cleanup and
no Session 0 Registry Editor remained. The service was left running.

No capture-helper crash or restart was observed during this service session;
logs reported zero dropped encoded frames. Existing WebRTC shutdown warnings
appeared on disconnect. This is functional validation, not a quantitative flicker
benchmark or proof of compatibility with all GPU-rendered applications.
No changes were pushed and no release was published.
