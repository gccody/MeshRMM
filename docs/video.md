# Video

How a remote screen gets from the device to the viewer. The code is in
`agent/windows/remote-screen` (Windows capture and encoding),
`agent/src/remote/macos` (macOS capture and encoding), `agent/src/remote`
(sending and rate control) and `remote/src/platform` (decoding and
presentation).

## Data path

```text
Capture (BGRA D3D11 texture)
  -> D3D11 video processor (pooled NV12 4:2:0 or AYUV 4:4:4 texture)
  -> Media Foundation hardware H.265 or H.264 encoder
     (final fallback: CPU NV12 conversion -> Microsoft's software H.264 encoder)
  -> latest encoded frame slot
  -> paced 12 KiB fragments on the video data channel
  -> latest-only reassembly
  -> presentation
       Windows: Media Foundation decoder (DXGI surface) -> D3D11
       macOS:   AVSampleBufferDisplayLayer -> Core Animation/AppKit window
```

Every stage keeps only the newest frame. A slow network or decoder costs
frames, never latency.

Encoded video has to cross CPU memory to be packetized and fed to the
decoder. Apart from that, single-monitor Windows capture and decoding keep
full-size images in D3D11 textures, and the macOS viewer hands compressed
samples to `AVSampleBufferDisplayLayer` without making a CPU image.

A Mac Agent captures with ScreenCaptureKit and encodes with VideoToolbox; see
[macOS Agent](macos-agent.md).

## Capture on Windows

- **One unrotated monitor:** DXGI Desktop Duplication. Unlike
  Windows.Graphics.Capture it reports when Windows switches the visible
  desktop, so the LocalSystem capture helper can follow the user to the
  sign-in screen, the lock screen and UAC prompts without dropping the
  session.
- **A rotated monitor, or All displays:** GDI, with one CPU-to-GPU upload per
  frame, because rotated monitors and monitors on different GPUs then share
  one desktop coordinate space. Its frame rate can be lower.
- **The background desktop:** window capture; see
  [background mode](background-mode.md#capture).
- **A console-mode Agent** (`--console`, for development) uses
  Windows.Graphics.Capture.

Capture and the asynchronous Media Foundation encoder share a protected
D3D11 device. A blocking `AcquireNextFrame` can hold the driver's device lock
while it waits for the screen to change, which starves the encoder: at
2560×1440 this once cut HEVC output to 10–14 FPS while capture ran at 60. So
capture polls with a zero timeout and sleeps 1 ms outside the graphics
driver. Keep it that way.

Three things that compilation does not catch, found on real hardware:

- A portrait monitor's DXGI texture keeps its native orientation. Don't
  compare its height with the rotated desktop height.
- GDI upload textures need a render-target binding as well as a
  shader-resource binding, or some GPUs reject them as video-processor
  input.
- Some encoders reject the `IsModifiable` probe for the one-shot keyframe
  command. Set the value directly, as a `VT_UI4`.

The Agent streams the display's real resolution, cropped by at most one row
or column for NV12, at 60 FPS by default.

## Codecs and negotiation

The viewer advertises the codec and chroma profiles it can decode: those with
a hardware decoder, plus H.264 4:2:0, which it can always decode in software.
The Agent prefers H.265 within the chosen chroma mode, then falls back
through the profiles both ends support if an encoder or playback fails to
start. H.264 4:2:0 comes last.

- **4:2:0** is the bandwidth-efficient default.
- **4:4:4** keeps text crisp. It needs a GPU driver with an AYUV and
  High 4:4:4 or RExt hardware path at both ends; the viewer disables the
  choice otherwise. The macOS viewer advertises 4:2:0 only, and Mac Agents
  encode 4:2:0 only.

The hardware encoders run a streaming configuration: CBR, real-time and
low-latency modes, no B-frames or reordering, a short recovery GOP, a buffer
of about one frame with a 16 KiB floor, a speed-biased quality setting and an
optional maximum-QP guard. Sequence headers go with every keyframe, so a
viewer can start decoding at any of them.

When the device replaces its stream (another display, codec or size), the
Windows viewer keeps its window and rebuilds only what the new stream needs
(`remote/src/stream_reset.rs`).

## Quality and rate control

Quality is chosen in the viewer, separately from chroma:

| Preset | Ceiling | Notes |
|---|---|---|
| Ultra data saver | 1 Mbps | Grayscale, up to 24 FPS |
| Data saver | 3 Mbps | |
| Balanced | 6 Mbps | |
| Best quality | 12 Mbps | The Agent's configured bitrate, never above 12 Mbps |

Below the ceiling the Agent adapts (`agent/src/remote/bitrate.rs`):

- **Pacing.** An encoder's CBR is a target, not a limit, so fragments are
  paced at the ceiling less the audio being sent.
- **Congestion** is judged from how long the video channel's buffer would
  take to drain at the current bitrate (50 ms to drain, 150 ms congested),
  not from byte counts, so every preset responds alike.
- **H.264** changes bitrate live, with additive increase and multiplicative
  decrease.
- **H.265** changes bitrate by restarting the encoder one step down or up
  (100, 70, 50, 35 or 25% of the ceiling). Several hardware HEVC encoders
  accept a live bitrate change and then die, so they are never asked for
  one. A restart costs a keyframe, so steps need sustained congestion and are
  spaced apart.

## Software fallback

Software H.264 4:2:0 is the last resort at both ends.

- **Agent.** With no GPU, or one that cannot convert or encode the stream,
  the Agent copies frames to the CPU, converts them to NV12 and encodes with
  Microsoft's software H.264 encoder transform, at up to 30 FPS. Safe Mode
  always uses this path: Windows loads no GPU vendor driver and disables
  Media Foundation there, so the Agent creates the transform directly through
  COM. See
  [restart and Safe Mode](maintenance-controls.md#restart-and-safe-mode).
- **Windows viewer.** Without an H.264 hardware decoder transform it uses
  Microsoft's H.264 decoder, which decodes with DXVA when the GPU can and in
  software otherwise. NVIDIA drivers register no hardware decoder transform,
  so this is their GPU path. Without a D3D11 hardware video device the viewer
  renders with WARP.
- **macOS viewer.** VideoToolbox decodes H.264 in software when there is no
  hardware decoder.

Startup still fails, with an error that says why, when neither path exists,
for example on a Windows N edition without the Media Feature Pack.

## Computers without a monitor

Windows has no desktop to capture when no monitor is connected. When a
session finds the console in that state, the Agent adds a virtual monitor
through [SudoVDA](https://github.com/SudoMaker/SudoVDA), an Indirect Display
Driver bundled in the Agent executable (`agent/assets/sudovda`).

- The Agent installs the driver the first time a computer needs it, trusting
  SudoMaker's code-signing certificate in the machine's `Root` and
  `TrustedPublisher` stores. If another application already installed
  SudoVDA, the Agent uses that copy.
- Uninstalling the Agent removes only what the Agent installed.
- The virtual monitor lasts until the session ends. Its size is a viewer
  preference, 1280 × 720 by default, under **Display** in the Windows
  viewer's settings or **Remote computer** in the macOS viewer's session
  menu. Changing it resizes a connected computer's virtual monitor at once.
- The setting does nothing while a real monitor is connected.

## Measuring

Periodic Agent capture statistics include capture FPS, stream FPS,
rate-limited frames, `frames_encoder_busy`, `mean_encode_us`, and `switch_ms`
for a capture reconfiguration. The hardware tests and the capture benchmark
are listed under [development](development.md#hardware-and-desktop-tests).

Compiling proves the API calls fit together. It says nothing about a real
GPU, a two-machine ICE path or TURN; don't infer performance from a build.
