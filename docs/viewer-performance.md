# Viewer frame-rate regression

The September 13, 2026 logs showed desktop capture reaching 60–78 FPS while
HEVC output fell to 10–14 FPS at 2560×1440. The Mac submitted incoming frames
in under 1 ms, with zero reassembly or presenter drops in that session.

Desktop Duplication and the asynchronous Media Foundation encoder share a
protected D3D11 device. Waiting inside `AcquireNextFrame(10)` interfered with
encoder progress. Capture now uses a zero-timeout acquisition and sleeps for
1 ms outside the graphics driver when there is no new frame. This also keeps
encoder output and recovery requests serviced on static desktops.

On the remote RTX 3080 machine, a 10-second animated desktop benchmark measured:

| Configuration | Encoded FPS | Mean capture-to-encoded latency |
| --- | ---: | ---: |
| Original capture loop, 15 ms animation timer | 12.87 | 62.8 ms |
| Fixed capture loop, identical animation | 48.02 | 9.5 ms |
| Fixed capture loop, 1 ms animation timer | 55.01 | 8.5 ms |
| Final benchmark script, primary monitor | 58.17 | 8.4 ms |

A separate synthetic GPU conversion/encoder benchmark sustained 60 FPS for
both H.264 and HEVC even before the fix, isolating the regression to capture
and encoder interaction. These are capture/encode results, not a measurement
of end-to-end displayed FPS. Static desktops intentionally produce fewer frames.
The change requires an updated Windows agent; the Mac renderer is unchanged.

## Reproduce on Windows

From an interactive desktop with a hardware HEVC encoder, run:

```powershell
.\scripts\benchmark-capture.ps1
```

This builds a release test executable, opens an animated window on the primary
monitor, runs the real capture pipeline for 10 seconds, prints FPS and latency,
and closes the window. The test requires at least 30 FPS. Animation cadence,
monitor refresh, resolution, and other GPU workloads affect the result.

To isolate conversion and hardware encoding from desktop capture:

```powershell
cargo test -p meshrmm-remote-screen --release hardware_encode_1440p60 -- --ignored --nocapture
```

Periodic agent capture statistics now include `frames_encoder_busy` and
`mean_encode_us` alongside capture FPS, stream FPS, and rate-limited frames.

## All-monitors capture

The **All monitors** display choice combines the Windows desktop into one video
stream. It uses GDI capture and one CPU-to-GPU upload per frame, followed by the
existing hardware encoder. Unrotated individual monitors use DXGI; rotated monitors use the same GDI
region capture path with their physical desktop bounds. GDI
capture has different performance characteristics; the measurements above do
not apply to it. Large desktop bounds are subject to GPU encoder/decoder limits.

On Windows with two or more monitors, verify startup and switching with:

```powershell
cargo test -p meshrmm-remote-screen all_monitors_stream_and_switch_back -- --ignored --nocapture
```

For the end-to-end smoke test, select **All monitors** in each native viewer and
check that both displays update in the same window, the cursor and clicks align
on each display, and individual-monitor selection still works. Include a monitor
left of or above the primary, mixed DPI, portrait orientation, a monitor layout
change, and lock/unlock. Reconnect the session while All monitors is selected to
check selection restoration. These hardware checks cannot run on the macOS
build host.

## Monitor-switch regression (September 13, 2026)

Live testing found three failures that compilation did not catch:

- Portrait DXGI textures retain their native orientation. Comparing their height
  with the rotated desktop height triggered continuous capture restarts.
- GDI upload textures were created with only a shader-resource binding, which
  this GPU rejected as video-processor input. They now include a render-target
  binding.
- The encoder rejected the `IsModifiable` probe for the one-shot keyframe
  command, so the command was never sent. Issue `SetValue` directly with the
  documented `VT_UI4` value. Repeated keyframes now work on a static desktop.

The existing helper processes and Mac viewer window are reused during monitor
selection. The local Mac installer sends SIGINT so the viewer releases its
server session before a new handoff; abrupt termination leaves the previous
session lease active until its idle deadline.

Run the hardware regression from the logged-in Windows desktop (an SSH session
alone can expose a different, non-capturable desktop):

```powershell
cargo test -p meshrmm-remote-screen every_display_produces_keyframes -- --ignored --nocapture
```

The test requires a portrait display, checks every display including the combined
view in both codecs, and requests eight additional keyframes per view while
crossing the periodic layout check. On the test RTX 3080 machine:

| View | H.264 first encoded frame | HEVC first encoded frame |
| --- | ---: | ---: |
| LG ULTRAGEAR (landscape) | 286 ms | 233 ms |
| AW2521HF (portrait) | 252 ms | 260 ms |
| All monitors | 314 ms | 306 ms |

These measure capture startup to first encoded frame, excluding transport and
presentation. The agent logs `switch_ms` for capture reconfiguration; use the
native viewer to verify the final displayed image and input mapping as well.

Live Mac viewer checks on the same machine passed after installing both local
builds: landscape → portrait → combined → landscape, reverse keyboard cycling,
and correct Start-menu clicks on both sides of the combined desktop. The window
and video layer stayed in place, with no capture restart loop. Agent-side switch
setup measured 320–522 ms in the initial live cycle. Reinstalling the active Mac
viewer then released its server lease cleanly; a fresh dashboard handoff connected
immediately and portrait switching still rendered correctly.
