use meshrmm_protocol::Codec;

use super::capture_control::profile_candidates;
use super::*;

#[test]
fn profile_negotiation_prefers_hevc_and_falls_back_to_420() {
    let profiles = [
        VideoProfile {
            codec: Codec::H264,
            chroma: ChromaMode::Yuv420,
        },
        VideoProfile {
            codec: Codec::H265,
            chroma: ChromaMode::Yuv420,
        },
        VideoProfile {
            codec: Codec::H264,
            chroma: ChromaMode::Yuv444,
        },
    ];

    assert_eq!(
        profile_candidates(&profiles, ChromaMode::Yuv444, &[]),
        vec![
            VideoProfile {
                codec: Codec::H264,
                chroma: ChromaMode::Yuv444,
            },
            VideoProfile {
                codec: Codec::H265,
                chroma: ChromaMode::Yuv420,
            },
            VideoProfile {
                codec: Codec::H264,
                chroma: ChromaMode::Yuv420,
            },
        ]
    );
}

#[test]
fn rejected_video_profiles_are_not_retried() {
    let h265_444 = VideoProfile {
        codec: Codec::H265,
        chroma: ChromaMode::Yuv444,
    };
    let h264_444 = VideoProfile {
        codec: Codec::H264,
        chroma: ChromaMode::Yuv444,
    };
    let h264_420 = VideoProfile {
        codec: Codec::H264,
        chroma: ChromaMode::Yuv420,
    };

    assert_eq!(
        profile_candidates(
            &[h265_444, h264_444, h264_420],
            ChromaMode::Yuv444,
            &[h265_444],
        ),
        vec![h264_444, h264_420]
    );
}
