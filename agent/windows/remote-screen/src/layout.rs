/// Bounding rectangle in physical desktop pixels, padded to even codec dimensions.
pub(crate) fn desktop_bounds(
    displays: impl IntoIterator<Item = (i32, i32, u32, u32)>,
) -> Option<(i32, i32, u32, u32)> {
    let mut displays = displays.into_iter();
    let (x, y, width, height) = displays.next()?;
    if width == 0 || height == 0 {
        return None;
    }
    let (mut left, mut top) = (i64::from(x), i64::from(y));
    let (mut right, mut bottom) = (left + i64::from(width), top + i64::from(height));
    for (x, y, width, height) in displays {
        if width == 0 || height == 0 {
            return None;
        }
        left = left.min(i64::from(x));
        top = top.min(i64::from(y));
        right = right.max(i64::from(x) + i64::from(width));
        bottom = bottom.max(i64::from(y) + i64::from(height));
    }
    let width = u32::try_from((right - left + 1) & !1).ok()?;
    let height = u32::try_from((bottom - top + 1) & !1).ok()?;
    Some((left as i32, top as i32, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_negative_origins_and_gaps() {
        assert_eq!(
            desktop_bounds([(-1920, 0, 1920, 1080), (0, -1440, 2560, 1440)]),
            Some((-1920, -1440, 4480, 2520))
        );
    }
    #[test]
    fn pads_odd_edges_and_handles_mirrors() {
        assert_eq!(
            desktop_bounds([(0, 0, 1921, 1081), (0, 0, 1921, 1081)]),
            Some((0, 0, 1922, 1082))
        );
    }
    #[test]
    fn rejects_empty_invalid_or_overflowing_layouts() {
        assert_eq!(desktop_bounds([]), None);
        assert_eq!(desktop_bounds([(0, 0, 0, 1080)]), None);
        assert_eq!(desktop_bounds([(i32::MIN, 0, u32::MAX, 1080)]), None);
    }
}
