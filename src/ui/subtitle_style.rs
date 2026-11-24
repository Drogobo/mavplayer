//! Shared subtitle typography settings for the GPUI subtitle renderer.

/// The base size is relative to video height, then adjusted by Settings::sub_scale.
pub const BASE_FONT_HEIGHT_RATIO: f32 = 0.052;
pub const MIN_FONT_SIZE: f32 = 12.0;
pub const MAX_FONT_SIZE: f32 = 96.0;
pub const LINE_HEIGHT_FACTOR: f32 = 1.22;
pub const BOX_HORIZONTAL_PADDING_FACTOR: f32 = 0.35;
pub const BOX_VERTICAL_PADDING_FACTOR: f32 = 0.10;
/// Visible separation between successive subtitle rows, in font-size units.
pub const LINE_GAP_FACTOR: f32 = 0.09;

/// Additional inter-row gap for outlined text; the normal line height already
/// provides natural spacing. Box rows retain their own padding and gap.
pub const OUTLINE_LINE_GAP_FACTOR: f32 = 0.0;

pub fn font_size(video_height: f32, scale: f32) -> f32 {
    (video_height.max(100.0) * BASE_FONT_HEIGHT_RATIO * scale).clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}

pub fn line_height(font_size: f32) -> f32 {
    font_size * LINE_HEIGHT_FACTOR
}

/// Bottom inset (relative to the player area) for a complete subtitle block.
/// `picture_bottom` is relative to the area's top, and `distance` is computed
/// from the *video picture's* height for every position mode.
pub fn bottom_margin(
    mode: crate::settings::SubPosition,
    picture_bottom: f32,
    area_height: f32,
    distance: f32,
    block_height: f32,
) -> f32 {
    use crate::settings::SubPosition;
    match mode {
        SubPosition::VideoPicture => area_height - picture_bottom + distance,
        SubPosition::PlayerWindow => distance,
        SubPosition::Automatic => {
            // Prefer placing the first row below the picture, while keeping
            // the last row above the player area's lower safety margin.
            let preferred_top = picture_bottom + distance;
            let latest_top = area_height - distance - block_height;
            let top = preferred_top.min(latest_top);
            area_height - top - block_height
        }
    }
}

/// Ensure a subtitle block is at least `distance` above a fullscreen overlay.
/// `control_bar_height` is zero when there is no overlay.
pub fn margin_above_controls(margin: f32, distance: f32, control_bar_height: f32) -> f32 {
    if control_bar_height > 0.0 { margin.max(control_bar_height + distance) } else { margin }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::SubPosition;

    #[test]
    fn visible_controls_push_subtitles_above_them() {
        assert_eq!(margin_above_controls(30.0, 40.0, 74.0), 114.0);
        // Already high enough: do not move anything.
        assert_eq!(margin_above_controls(180.0, 40.0, 74.0), 180.0);
        // A non-overlay control bar does not affect the position.
        assert_eq!(margin_above_controls(30.0, 40.0, 0.0), 30.0);
    }

    #[test]
    fn automatic_uses_letterbox_when_it_fits() {
        // Bottom bar: y=700..900; block: 60px high; margin: 40px.
        assert_eq!(bottom_margin(SubPosition::Automatic, 700.0, 900.0, 40.0, 60.0), 100.0);
    }

    #[test]
    fn automatic_moves_up_when_letterbox_is_too_short() {
        // Bottom bar: 30px; last row must stay at least 40px from bottom.
        assert_eq!(bottom_margin(SubPosition::Automatic, 870.0, 900.0, 40.0, 60.0), 40.0);
    }

    #[test]
    fn picture_and_window_use_different_bottom_edges() {
        assert_eq!(bottom_margin(SubPosition::VideoPicture, 700.0, 900.0, 40.0, 60.0), 240.0);
        assert_eq!(bottom_margin(SubPosition::PlayerWindow, 700.0, 900.0, 40.0, 60.0), 40.0);
    }
}
