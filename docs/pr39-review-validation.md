# PR 39 review follow-up validation

Validated on 2026-09-27, on top of `7105dee1433498f659b0d3887321fe1f4d8f1ba3`,
including the uncommitted review fixes. These results supplement the historical
checks in [the implementation plan](ux-improvements-2026-09-26-plan.md).

## Fixes

- Startup retry limits now remain active until native presentation confirms a
  decoded frame. Enqueuing an encoded frame no longer counts as presentation.
  Windows records a successful renderer presentation; macOS observes
  `AVSampleBufferDisplayLayer.isReadyForDisplay`. The receiver also times out
  presenters that exist but never display a frame.
- Audio playback resets Opus sequence/codec history together with resampling
  history when output is unavailable, output reopens, or playback generation
  changes. Regression tests cover 40,000 skipped packets (13m20s), nonzero
  resumed audio, duplicate rejection, and equality with fresh decoding/resampling.

## Source and build provenance

The Windows test directory is `C:\Users\gccody\pr39-validation-20260927`.
All 452 source files were verified against a SHA-256 manifest before testing and
again before the final live tests; the development working tree matched too.
Manifest SHA-256:
`1e410016ee77c306cd65a481286bd54c9ec3b8bcb39cffac43b4aa94ee44fe6e`.
Documentation added after testing does not change the tested code.

Installed test binaries:

| Binary | SHA-256 |
| --- | --- |
| Windows Agent | `14ff2fa4330cada6198303cf2a733dc51c414efd4c7abefce4d5e4f9e59efc17` |
| Windows viewer (built) | `61145d6921ac82a5b2441dd77e5f52abfb7b75e6cd5354ccde85c68e852a8824` |
| macOS viewer | `b3e13f258891957ce1df19ed1e1ea98f304f940099914f741de4d1459919c0ff` |

## Automated and native checks

- macOS development host: pinned Rust 1.97.1 formatting, workspace Clippy with
  `--all-targets -- -D warnings`, workspace tests, and release viewer build passed.
  Viewer: 100 passed, 1 ignored. Audio: 10 passed.
- Windows `DESKTOP-85R6S28`: native formatting, workspace Clippy, workspace tests,
  and release Agent/viewer builds passed. Viewer: 89 passed, 3 ignored; audio:
  10 passed. CMake was configured through `scripts/use-cmake.ps1`.
- The initial Windows workspace run hit a self-update executable-identity test
  failure through a target-directory junction. Using the canonical target path
  resolved it; the complete workspace rerun passed.
- Interactive Windows `reset_probe` and `reconnect_probe` passed twice. These
  exercised retained HWNDs, controls, chat draft/transcript, focus, portrait and
  chroma resets, minimize/restore, pointer corners, and the real Retry button.
- Dashboard verification (100 tests), server request-path tests and SQL
  regressions (28 checks) passed during review on macOS, not CI's Linux host.
  No dashboard/server code changed in this follow-up.

## Live macOS viewer to installed Windows service

Sessions were launched from the user's signed-in Chrome dashboard. The test
Agent was installed with `scripts/install-agent-local.ps1 -SkipBuild`; its service
started and connected. Computer use verified the rendered desktop and native UI.

- HEVC streamed an animated test canvas. A 2 Mbit/s temporary Windows QoS limit
  caused bitrate steps 6 → 4.2 → 3 Mbit/s, with requests more than five seconds
  apart. Removing the limit allowed 3 → 4.2 → 6 Mbit/s recovery, with roughly
  twenty seconds between upward requests. The macOS presenter/window was retained.
- Service stop/restart showed the offline reason and elapsed reconnect time,
  then recovered live video automatically. Presentation was recognized by the
  retry policy (`startup_failures=0`).
- Clicking the enabled macOS Retry now button at 22:56:20 UTC interrupted a
  15-second backoff. Logs confirmed the user request immediately; the new
  presenter was visible within one second and reported decoded frames within
  three seconds. The disabled state during an active attempt was also checked.
- All displays, the portrait second display, and keyboard display switching
  rendered correctly. Best quality (12 Mbit/s), Ultra data saver (1 Mbit/s,
  grayscale), and Balanced changes reset the stream in the same macOS window.
- Ctrl+Alt+Del displayed the Windows secure screen; Cancel returned to the
  desktop and streaming recovered.
- An actual UAC prompt for Notepad rendered over the session. Clicking No
  through the viewer dismissed it, and the animated desktop continued rendering.
- Mute stopped agent audio capture; unmute restarted capture in about 13 ms.
  Opus traffic resumed at roughly 97 kbit/s with no reported audio drops. Initial
  local audio-device opening took about 2.8 seconds. Human audibility was not
  assessed; the regression tests verify decoded nonzero samples.

## Remaining coverage limits

The endpoint contains an RTX 3080, but its viewer probe reports no registered
Media Foundation hardware decoder for H.264 or HEVC, for either NV12 or AYUV.
This was reconfirmed from the interactive desktop. No second Windows machine
was supplied. Full Windows-viewer video session/reset testing remains incomplete;
the successful synthetic window probes do not replace it.

Lock/unlock with credentials and a physical audio-output device switch were
not exercised. The long audio outage was covered by a
deterministic sequence-gap regression rather than waiting for a physical outage.

## Final machine state

The test session ended normally. Test audio/animation stopped, the test-only
Windows browser window closed, and all three temporary scheduled tasks were
removed. No test QoS policy remained. Original macOS viewer preferences were
restored. The validated local Agent remains installed and its service is Running;
the validated macOS viewer remains installed. Previous binaries are retained in
the validation backup directories for restoration if needed.

Evidence is retained locally in `/tmp/pr39-validation/live-viewer.log` and in the
Windows test directory (`live-agent.log`, `results.log`, `test.log`,
`reset-probe.log`, `reconnect-probe.log`, `probe-results.log`, and screenshots).
