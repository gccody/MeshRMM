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
existing hardware encoder. The single-monitor DXGI path is unchanged. Combined
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
