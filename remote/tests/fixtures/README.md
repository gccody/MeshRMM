One-frame, 64×48 blue video fixtures generated with FFmpeg (libx264/libx265),
covering 8-bit 4:2:0 and 4:4:4 parameter sets. No external media is used.

Generation (substitute codec, pixel format, and output path):

```sh
ffmpeg -f lavfi -i color=c=blue:s=64x48:r=1 -frames:v 1 \
  -pix_fmt yuv420p -c:v libx264 -threads 1 -f h264 yuv420p.h264
ffmpeg -f lavfi -i color=c=blue:s=64x48:r=1 -frames:v 1 \
  -pix_fmt yuv420p -c:v libx265 -threads 1 \
  -x265-params pools=none:log-level=error -f hevc yuv420p.hevc
```

Run independent MKV demux/decode and timestamp verification with:

```sh
FFMPEG=/path/to/ffmpeg cargo test -p meshrmm-remote ffmpeg_decodes -- --ignored
```

`yuv420p-software.h264` is one 64×48 blue frame from the Agent's software
H.264 encoder (Microsoft's encoder transform, low latency, no B-frames), so a
decoder outputs it without waiting for more input. It was written by
encoding a blue texture with `SoftwareH264Encoder` on Windows and saving the
access unit with its sequence header.
