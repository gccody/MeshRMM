//! Where the remote desktop appears inside the Windows viewer's client area.
//! The renderer and the pointer mapping use the same rectangle, so a click
//! lands on the remote pixel drawn under the local pointer.

/// A rectangle in client pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

impl VideoRect {
    pub fn right(self) -> i32 {
        self.left + self.width
    }

    pub fn bottom(self) -> i32 {
        self.top + self.height
    }
}

/// Fits the video into `area` without distorting it, centered with black
/// bars on the two sides that have spare room.
pub fn letterbox(area: VideoRect, video_width: u32, video_height: u32) -> VideoRect {
    let area_width = i64::from(area.width.max(1));
    let area_height = i64::from(area.height.max(1));
    let video_width = i64::from(video_width.max(1));
    let video_height = i64::from(video_height.max(1));
    let (width, height) = if area_width * video_height <= area_height * video_width {
        let height = (area_width * video_height + video_width / 2) / video_width;
        (area_width, height.clamp(1, area_height))
    } else {
        let width = (area_height * video_width + video_height / 2) / video_height;
        (width.clamp(1, area_width), area_height)
    };
    VideoRect {
        left: area.left + ((area_width - width) / 2) as i32,
        top: area.top + ((area_height - height) / 2) as i32,
        width: width as i32,
        height: height as i32,
    }
}

/// Maps a client pixel inside `video` to the protocol's 0..=65535 range.
/// Points outside the video return `None`.
pub fn normalize(video: VideoRect, x: i32, y: i32) -> Option<(u16, u16)> {
    if x < video.left || x >= video.right() || y < video.top || y >= video.bottom() {
        return None;
    }
    Some(normalize_clamped(video, x, y))
}

/// Like [`normalize`], but moves a point outside the video to its nearest
/// edge. Used while a drag that started on the video holds mouse capture.
pub fn normalize_clamped(video: VideoRect, x: i32, y: i32) -> (u16, u16) {
    let axis = |position: i32, start: i32, length: i32| {
        let last = i64::from(length.max(1) - 1);
        let offset = i64::from(position.saturating_sub(start)).clamp(0, last);
        (offset * 65_535 / last.max(1)) as u16
    };
    (
        axis(x, video.left, video.width),
        axis(y, video.top, video.height),
    )
}

/// Scales a video of `video_width` × `video_height` pixels down, never up, so
/// it fits in `max_width` × `max_height`. Returns the displayed size.
pub fn fit_within(
    video_width: u32,
    video_height: u32,
    max_width: i32,
    max_height: i32,
) -> (i32, i32) {
    let video_width = i32::try_from(video_width.max(1)).unwrap_or(i32::MAX);
    let video_height = i32::try_from(video_height.max(1)).unwrap_or(i32::MAX);
    if video_width <= max_width && video_height <= max_height {
        return (video_width, video_height);
    }
    let fitted = letterbox(
        VideoRect {
            left: 0,
            top: 0,
            width: max_width.max(1),
            height: max_height.max(1),
        },
        video_width as u32,
        video_height as u32,
    );
    (fitted.width, fitted.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(left: i32, top: i32, width: i32, height: i32) -> VideoRect {
        VideoRect {
            left,
            top,
            width,
            height,
        }
    }

    #[test]
    fn letterbox_keeps_the_aspect_ratio_and_centers_the_video() {
        // A 16:9 video in a taller area gets bars above and below.
        assert_eq!(
            letterbox(area(0, 68, 1600, 1000), 1920, 1080),
            area(0, 118, 1600, 900)
        );
        // A 16:9 video in a wider area gets bars on the sides.
        assert_eq!(
            letterbox(area(0, 68, 2000, 900), 1920, 1080),
            area(200, 68, 1600, 900)
        );
        // An exact fit fills the area.
        assert_eq!(
            letterbox(area(0, 68, 1920, 1080), 1920, 1080),
            area(0, 68, 1920, 1080)
        );
        // A degenerate area still produces a usable rectangle.
        assert_eq!(letterbox(area(0, 0, 0, 0), 1920, 1080), area(0, 0, 1, 1));
    }

    #[test]
    fn pointer_mapping_uses_the_letterboxed_video() {
        let video = letterbox(area(0, 68, 2000, 900), 1920, 1080);
        assert_eq!(normalize(video, 200, 68), Some((0, 0)));
        assert_eq!(normalize(video, 1799, 967), Some((65_535, 65_535)));
        assert_eq!(normalize(video, 1000, 518), Some((32_787, 32_803)));
        // The side bars and the toolbar are outside the video.
        assert_eq!(normalize(video, 199, 500), None);
        assert_eq!(normalize(video, 1800, 500), None);
        assert_eq!(normalize(video, 1000, 67), None);
        assert_eq!(normalize(video, 1000, 968), None);
    }

    #[test]
    fn a_scaled_window_maps_to_the_whole_remote_display() {
        // A 2560×1440 remote shown at half size, below a 102 px toolbar (150% DPI).
        let video = letterbox(area(0, 102, 1280, 720), 2560, 1440);
        assert_eq!(video, area(0, 102, 1280, 720));
        assert_eq!(normalize(video, 0, 102), Some((0, 0)));
        assert_eq!(normalize(video, 1279, 821), Some((65_535, 65_535)));
        assert_eq!(normalize(video, 640, 462), Some((32_793, 32_813)));
    }

    #[test]
    fn clamped_mapping_pins_points_outside_the_video_to_its_edge() {
        let video = area(200, 68, 1600, 900);
        assert_eq!(normalize_clamped(video, 0, 0), (0, 0));
        assert_eq!(normalize_clamped(video, 5000, 5000), (65_535, 65_535));
        assert_eq!(normalize_clamped(video, 1000, -40), (32_787, 0));
    }

    #[test]
    fn fitting_only_shrinks_videos_larger_than_the_space() {
        assert_eq!(fit_within(1280, 720, 1800, 900), (1280, 720));
        assert_eq!(fit_within(3840, 2160, 1728, 900), (1600, 900));
        assert_eq!(fit_within(1080, 1920, 1728, 900), (506, 900));
    }
}
