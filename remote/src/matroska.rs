//! Streaming video-only Matroska: finite clusters, unknown segment size, no
//! accumulated frame/index buffer and no end-of-recording rewrite.
use anyhow::{Context, ensure};
use meshrmm_protocol::{Codec, EncodedFrame, VideoFormat};
use std::io::Write;

pub(crate) struct Matroska<W> {
    pub output: W,
    origin_us: u64,
}

fn element(id: u32, data: &[u8]) -> Vec<u8> {
    let bytes = id.to_be_bytes();
    let mut result = bytes[bytes.iter().position(|b| *b != 0).unwrap()..].to_vec();
    let length = data.len() as u64;
    let width = (1..=8).find(|w| length < (1u64 << (7 * w)) - 1).unwrap();
    let size = (length | (1u64 << (7 * width))).to_be_bytes();
    result.extend_from_slice(&size[8 - width..]);
    result.extend_from_slice(data);
    result
}

fn uint(id: u32, value: u64) -> Vec<u8> {
    let bytes = value.to_be_bytes();
    element(
        id,
        &bytes[bytes.iter().position(|b| *b != 0).unwrap_or(7)..],
    )
}

impl<W: Write> Matroska<W> {
    pub fn new(mut output: W, format: VideoFormat, first: &EncodedFrame) -> anyhow::Result<Self> {
        let private = codec_private(format.codec, &first.data)?;
        let header = [
            uint(0x4286, 1),
            uint(0x42f7, 1),
            uint(0x42f2, 4),
            uint(0x42f3, 8),
            element(0x4282, b"matroska"),
            uint(0x4287, 4),
            uint(0x4285, 2),
        ]
        .concat();
        output.write_all(&element(0x1a45dfa3, &header))?;
        output.write_all(&[
            0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ])?;
        output.write_all(&element(
            0x1549a966,
            &[
                uint(0x2ad7b1, 1_000_000),
                element(0x4d80, b"MeshRMM"),
                element(0x5741, b"MeshRMM"),
            ]
            .concat(),
        ))?;
        let codec = match format.codec {
            Codec::H264 => "V_MPEG4/ISO/AVC",
            Codec::H265 => "V_MPEGH/ISO/HEVC",
        };
        let track = [
            uint(0xd7, 1),
            uint(0x73c5, 1),
            uint(0x83, 1),
            uint(0x9c, 0),
            element(0x86, codec.as_bytes()),
            element(0x63a2, &private),
            element(
                0xe0,
                &[
                    uint(0xb0, u64::from(format.width)),
                    uint(0xba, u64::from(format.height)),
                ]
                .concat(),
            ),
        ]
        .concat();
        output.write_all(&element(0x1654ae6b, &element(0xae, &track)))?;
        output.flush()?;
        Ok(Self {
            output,
            origin_us: first.capture_timestamp_us,
        })
    }

    pub fn frame(&mut self, frame: &EncodedFrame) -> anyhow::Result<()> {
        let mut block = vec![0x81, 0, 0, if frame.keyframe { 0x80 } else { 0 }];
        let units = crate::h264::annex_b_units(&frame.data);
        ensure!(!units.is_empty(), "Video frame has no Annex-B NAL units");
        for unit in units {
            block.extend_from_slice(&u32::try_from(unit.len())?.to_be_bytes());
            block.extend_from_slice(unit);
        }
        // One complete cluster per access unit keeps memory bounded and allows
        // recovery up to the last written frame after an unexpected exit.
        let timestamp = frame.capture_timestamp_us.saturating_sub(self.origin_us) / 1000;
        let cluster = [uint(0xe7, timestamp), element(0xa3, &block)].concat();
        self.output.write_all(&element(0x1f43b675, &cluster))?;
        self.output.flush()?;
        Ok(())
    }
}

fn codec_private(codec: Codec, data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let units = crate::h264::annex_b_units(data);
    let parameter = |kind| {
        units
            .iter()
            .copied()
            .find(|n| match codec {
                Codec::H264 => n[0] & 31 == kind,
                Codec::H265 => (n[0] >> 1) & 63 == kind,
            })
            .context("Keyframe is missing a video parameter set")
    };
    let append = |out: &mut Vec<u8>, nal: &[u8]| -> anyhow::Result<()> {
        out.extend_from_slice(&u16::try_from(nal.len())?.to_be_bytes());
        out.extend_from_slice(nal);
        Ok(())
    };
    match codec {
        Codec::H264 => {
            let sps = parameter(7)?;
            let pps = parameter(8)?;
            ensure!(sps.len() >= 4, "Truncated AVC SPS");
            let mut out = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
            append(&mut out, sps)?;
            out.push(1);
            append(&mut out, pps)?;
            if matches!(
                sps[1],
                100 | 110 | 122 | 144 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
            ) {
                let mut bits = Bits::new(&sps[4..]);
                bits.ue()?; // seq_parameter_set_id
                let chroma = bits.ue()?;
                ensure!(chroma <= 3, "Invalid AVC chroma format");
                if chroma == 3 {
                    bits.read(1)?;
                }
                let luma = bits.ue()?;
                let chroma_depth = bits.ue()?;
                ensure!(luma <= 7 && chroma_depth <= 7, "Invalid AVC bit depth");
                out.extend_from_slice(&[
                    0xfc | chroma as u8,
                    0xf8 | luma as u8,
                    0xf8 | chroma_depth as u8,
                    0,
                ]);
            }
            Ok(out)
        }
        Codec::H265 => {
            let vps = parameter(32)?;
            let sps = parameter(33)?;
            let pps = parameter(34)?;
            ensure!(sps.len() >= 3, "Truncated HEVC SPS");
            let mut bits = Bits::new(&sps[2..]);
            bits.read(4)?;
            let sublayers = bits.read(3)? as usize;
            let nested = bits.read(1)? as u8;
            let mut out = vec![1];
            // general_profile_space through general_level_idc (96 bits).
            for _ in 0..12 {
                out.push(bits.read(8)? as u8);
            }
            let mut flags = Vec::new();
            for _ in 0..sublayers {
                flags.push((bits.read(1)?, bits.read(1)?));
            }
            if sublayers > 0 {
                for _ in sublayers..8 {
                    bits.read(2)?;
                }
            }
            for (profile, level) in flags {
                if profile != 0 {
                    for _ in 0..11 {
                        bits.read(8)?;
                    }
                }
                if level != 0 {
                    bits.read(8)?;
                }
            }
            bits.ue()?; // sps_seq_parameter_set_id
            let chroma = bits.ue()?;
            ensure!(chroma <= 3, "Invalid HEVC chroma format");
            if chroma == 3 {
                bits.read(1)?;
            }
            bits.ue()?;
            bits.ue()?; // dimensions
            if bits.read(1)? != 0 {
                for _ in 0..4 {
                    bits.ue()?;
                }
            }
            let luma = bits.ue()?;
            let chroma_depth = bits.ue()?;
            ensure!(luma <= 7 && chroma_depth <= 7, "Invalid HEVC bit depth");
            out.extend_from_slice(&[
                0xf0,
                0,
                0xfc,
                0xfc | chroma as u8,
                0xf8 | luma as u8,
                0xf8 | chroma_depth as u8,
                0,
                0,
                ((sublayers as u8 + 1) << 3) | (nested << 2) | 3,
                3,
            ]);
            for (kind, nal) in [(32, vps), (33, sps), (34, pps)] {
                out.extend_from_slice(&[0x80 | kind, 0, 1]);
                append(&mut out, nal)?;
            }
            Ok(out)
        }
    }
}

struct Bits {
    bytes: Vec<u8>,
    position: usize,
}
impl Bits {
    fn new(escaped: &[u8]) -> Self {
        let mut bytes = Vec::new();
        let mut zeros = 0;
        for &byte in escaped {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            bytes.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        Self { bytes, position: 0 }
    }
    fn read(&mut self, count: usize) -> anyhow::Result<u32> {
        ensure!(
            self.position + count <= self.bytes.len() * 8,
            "Truncated video parameter set"
        );
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1)
                | u32::from((self.bytes[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }
    fn ue(&mut self) -> anyhow::Result<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            ensure!(zeros < 32, "Invalid Exp-Golomb value");
        }
        Ok(((1u32 << zeros) - 1) + self.read(zeros)?)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use meshrmm_protocol::{PixelFormat, VideoStreamId};

    pub fn sample(codec: Codec, chroma444: bool) -> (VideoFormat, EncodedFrame) {
        let data: &[u8] = match (codec, chroma444) {
            (Codec::H264, false) => include_bytes!("../tests/fixtures/yuv420p.h264"),
            (Codec::H264, true) => include_bytes!("../tests/fixtures/yuv444p.h264"),
            (Codec::H265, false) => include_bytes!("../tests/fixtures/yuv420p.hevc"),
            (Codec::H265, true) => include_bytes!("../tests/fixtures/yuv444p.hevc"),
        };
        (
            VideoFormat {
                width: 64,
                height: 48,
                frames_per_second: 1,
                codec,
                pixel_format: if chroma444 {
                    PixelFormat::Ayuv
                } else {
                    PixelFormat::Nv12
                },
                bitrate_bits_per_second: 100_000,
            },
            EncodedFrame {
                stream_id: VideoStreamId(1),
                frame_id: 1,
                capture_timestamp_us: 1_000_000,
                encode_complete_timestamp_us: 1_000_001,
                send_timestamp_us: 1_000_002,
                keyframe: true,
                data: data.to_vec(),
            },
        )
    }

    #[test]
    fn streams_complete_clusters_with_sparse_timestamps_for_both_codecs() {
        for codec in [Codec::H264, Codec::H265] {
            for chroma444 in [false, true] {
                let (format, mut frame) = sample(codec, chroma444);
                let mut writer = Matroska::new(Vec::new(), format, &frame).unwrap();
                assert!(writer.output.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]));
                for elapsed in [0, 771_000, 788_000, 90_000_000] {
                    frame.capture_timestamp_us = 1_000_000 + elapsed;
                    let before = writer.output.len();
                    writer.frame(&frame).unwrap();
                    let cluster = &writer.output[before..];
                    assert!(cluster.starts_with(&[0x1f, 0x43, 0xb6, 0x75]));
                    let timestamp = uint(0xe7, elapsed / 1000);
                    assert!(cluster.windows(timestamp.len()).any(|w| w == timestamp));
                    for nal in crate::h264::annex_b_units(&frame.data) {
                        assert!(cluster.windows(nal.len()).any(|w| w == nal));
                    }
                }
            }
        }
    }

    #[test]
    fn rejects_missing_or_truncated_parameter_sets_and_disk_errors() {
        for codec in [Codec::H264, Codec::H265] {
            let (format, mut frame) = sample(codec, false);
            for size in 0..20 {
                assert!(codec_private(codec, &frame.data[..size]).is_err());
            }
            struct FullDisk;
            impl Write for FullDisk {
                fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                    Err(std::io::Error::other("disk full"))
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            assert!(Matroska::new(FullDisk, format, &frame).is_err());
            let mut writer = Matroska {
                output: FullDisk,
                origin_us: 0,
            };
            assert!(writer.frame(&frame).is_err());
            frame.data.clear();
            assert!(Matroska::new(Vec::new(), format, &frame).is_err());
        }
    }

    // Independent demux/decode validation, explicitly run with FFMPEG set.
    #[test]
    #[ignore = "requires FFMPEG executable"]
    fn ffmpeg_decodes_files_while_writer_is_still_open() {
        let ffmpeg = std::env::var_os("FFMPEG").expect("set FFMPEG");
        for codec in [Codec::H264, Codec::H265] {
            for chroma444 in [false, true] {
                let path = std::env::temp_dir().join(format!(
                    "meshrmm-mkv-{}-{codec:?}-{chroma444}.mkv",
                    std::process::id()
                ));
                let (format, mut frame) = sample(codec, chroma444);
                let mut writer = Matroska::new(
                    std::io::BufWriter::new(std::fs::File::create(&path).unwrap()),
                    format,
                    &frame,
                )
                .unwrap();
                for elapsed in [0, 771_000, 788_000, 90_000_000] {
                    frame.capture_timestamp_us = 1_000_000 + elapsed;
                    writer.frame(&frame).unwrap();
                }
                let result = std::process::Command::new(&ffmpeg)
                    .args(["-v", "error", "-xerror", "-i"])
                    .arg(&path)
                    .args([
                        "-fps_mode",
                        "passthrough",
                        "-enc_time_base",
                        "1:1000",
                        "-f",
                        "framemd5",
                        "-",
                    ])
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "{}",
                    String::from_utf8_lossy(&result.stderr)
                );
                let stdout = String::from_utf8(result.stdout).unwrap();
                let frames: Vec<_> = stdout
                    .lines()
                    .filter(|l| !l.starts_with('#') && !l.is_empty())
                    .collect();
                assert_eq!(frames.len(), 4, "{stdout}");
                let pts: Vec<i64> = frames
                    .iter()
                    .map(|l| l.split(',').nth(2).unwrap().trim().parse().unwrap())
                    .collect();
                assert_eq!(
                    pts,
                    [0, 771, 788, 90_000],
                    "sparse timestamps lost: {stdout}"
                );
                drop(writer);
                std::fs::remove_file(path).unwrap();
            }
        }
    }
}
