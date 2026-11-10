use meshrmm_protocol::{Codec, QualityPreset};

use super::capture_control::{ProfileSelection, profile_candidates};
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

const H264_420: VideoProfile = VideoProfile {
    codec: Codec::H264,
    chroma: ChromaMode::Yuv420,
};
const H265_420: VideoProfile = VideoProfile {
    codec: Codec::H265,
    chroma: ChromaMode::Yuv420,
};
const H264_444: VideoProfile = VideoProfile {
    codec: Codec::H264,
    chroma: ChromaMode::Yuv444,
};

/// A session whose viewer rejected H.265 and now watches H.264.
fn fallen_back_to_h264() -> ProfileSelection {
    let mut selection = ProfileSelection::new(H264_420);
    selection.negotiate(
        vec![H264_420, H265_420, H264_444],
        QualityPreset::Balanced,
        ChromaMode::Yuv420,
    );
    assert_eq!(selection.candidates(), vec![H265_420, H264_420]);
    selection.reject(H265_420);
    assert_eq!(selection.candidates(), vec![H264_420]);
    selection
}

#[test]
fn the_same_chroma_or_quality_sent_again_keeps_a_rejection_and_restarts_nothing() {
    let mut selection = fallen_back_to_h264();
    // A new viewer window used to send both again; each restarted the
    // stream on the rejected profile.
    assert!(!selection.choose_chroma(ChromaMode::Yuv420));
    assert!(!selection.choose_quality(QualityPreset::Balanced));
    assert_eq!(selection.candidates(), vec![H264_420]);
}

#[test]
fn a_different_chroma_lets_a_rejected_profile_be_tried_again() {
    let mut selection = fallen_back_to_h264();
    assert!(selection.choose_chroma(ChromaMode::Yuv444));
    assert_eq!(selection.candidates(), vec![H264_444, H265_420, H264_420]);
    assert!(selection.choose_chroma(ChromaMode::Yuv420));
    assert_eq!(selection.candidates(), vec![H265_420, H264_420]);
}

#[test]
fn a_different_quality_lets_a_rejected_profile_be_tried_again() {
    let mut selection = fallen_back_to_h264();
    assert!(selection.choose_quality(QualityPreset::BestQuality));
    assert_eq!(selection.candidates(), vec![H265_420, H264_420]);
}
