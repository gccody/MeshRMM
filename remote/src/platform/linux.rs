//! The Linux viewer. winit opens its windows on X11 and Wayland, wgpu draws
//! them with Vulkan or OpenGL, and egui draws the controls, the toolbar
//! among them from the model every platform shares. The video is H.264
//! decoded in software by the bundled OpenH264.

mod app;
mod decoder;
mod dialog;
mod gpu;
mod install;
mod instance;
mod keymap;
mod menu;
mod presenter;
mod session;
mod style;
mod toolbar;
mod video;

use std::sync::OnceLock;
use std::time::Instant;

use meshrmm_protocol::{ChromaMode, Codec, VideoFormat, VideoProfile};

pub use app::{run_application, show_launch_status, show_notice};
pub use install::{install_if_needed, is_uninstall_invocation, register, uninstall};
pub use instance::claim as claim_instance;
pub use presenter::{Presenter, refresh_open_controls};

/// What the viewer decodes: H.264 in 4:2:0, in software.
pub fn supported_video_profiles(_format: VideoFormat) -> Vec<VideoProfile> {
    vec![VideoProfile {
        codec: Codec::H264,
        chroma: ChromaMode::Yuv420,
    }]
}

pub fn monotonic_timestamp_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START
        .get_or_init(Instant::now)
        .elapsed()
        .as_micros()
        .try_into()
        .unwrap_or(u64::MAX)
}
