//! Layout arithmetic for the bottom control bar: how wide each control is,
//! the window width the enabled controls need, and which controls to leave
//! out when the window is narrower than that anyway.
//!
//! The widths here must match what `control_bar` in `render.rs` builds.

use crate::settings::BarControl;

/// The bar's horizontal padding (`px_3` on both sides).
const PADDING: f32 = 2.0 * 12.0;
/// Gap between the left group, the title and the right group (`gap_2`).
const GROUP_GAP: f32 = 8.0;
/// Gap between controls inside a group (`gap_1`).
const GAP: f32 = 4.0;
/// The round Play/Pause button.
pub const PLAY: f32 = 38.0;
/// Room kept for the title, including its padding.
const TITLE_MIN: f32 = 40.0;
/// An icon button.
pub const BUTTON: f32 = 32.0;
/// Speed: the value label and the slider.
pub const SPEED_LABEL: f32 = 44.0;
pub const SPEED_SLIDER: f32 = 84.0;
/// Volume: space before it, the mute button, the slider and the percentage
/// (with a little space before it).
pub const VOLUME_LEAD: f32 = 6.0;
pub const VOLUME_SLIDER: f32 = 96.0;
pub const VOLUME_TEXT_LEAD: f32 = 4.0;
pub const VOLUME_TEXT: f32 = 44.0;
/// Narrowest window allowed even with hardly any controls.
const MIN_WINDOW_WIDTH: f32 = 420.0;

/// Width of one control in the bar.
pub fn width(c: BarControl) -> f32 {
    match c {
        BarControl::Speed => SPEED_LABEL + SPEED_SLIDER,
        BarControl::Volume => VOLUME_LEAD + BUTTON + GAP + VOLUME_SLIDER + GAP + VOLUME_TEXT_LEAD + VOLUME_TEXT,
        _ => BUTTON,
    }
}

/// Width of the whole bar row showing `controls`.
pub fn bar_width(controls: &[BarControl]) -> f32 {
    let (left, right): (Vec<BarControl>, Vec<BarControl>) = controls.iter().partition(|c| c.on_left());
    let left_w = PLAY + left.iter().map(|&c| GAP + width(c)).sum::<f32>();
    let right_w = right.iter().map(|&c| width(c)).sum::<f32>() + GAP * right.len().saturating_sub(1) as f32;
    PADDING + left_w + GROUP_GAP + TITLE_MIN + GROUP_GAP + right_w
}

/// The window's minimum width: everything enabled fits, including the
/// subtitle and audio track buttons that only appear for some media.
pub fn min_window_width(controls: &[BarControl]) -> f32 {
    bar_width(controls).ceil().max(MIN_WINDOW_WIDTH)
}

/// The order controls are left out in when the bar doesn't fit: least
/// needed (or reachable from the keyboard and menus anyway) first.
const DROP_ORDER: [BarControl; 12] = [
    BarControl::Speed,
    BarControl::Settings,
    BarControl::Shuffle,
    BarControl::Repeat,
    BarControl::Stop,
    BarControl::Previous,
    BarControl::Playlist,
    BarControl::AudioTrack,
    BarControl::Subtitles,
    BarControl::Next,
    BarControl::Volume,
    BarControl::Fullscreen,
];

/// The controls from `controls` (kept in order) that fit in `available`
/// pixels, leaving out the least important ones first.
pub fn fit(controls: &[BarControl], available: f32) -> Vec<BarControl> {
    let mut shown = controls.to_vec();
    for drop in DROP_ORDER {
        if bar_width(&shown) <= available {
            break;
        }
        shown.retain(|&c| c != drop);
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_fits_at_the_minimum_width() {
        let all = BarControl::ALL.to_vec();
        let min = min_window_width(&all);
        assert!(min > MIN_WINDOW_WIDTH);
        assert_eq!(fit(&all, min), all);
        // Fewer controls, narrower window, never below the floor.
        let few = vec![BarControl::Fullscreen];
        assert!(min_window_width(&few) < min);
        assert_eq!(min_window_width(&[]), MIN_WINDOW_WIDTH);
    }

    #[test]
    fn narrow_bars_drop_the_least_needed_controls_first() {
        let all = BarControl::ALL.to_vec();
        let min = min_window_width(&all);
        let shown = fit(&all, min - 1.0);
        assert!(!shown.contains(&BarControl::Speed));
        assert_eq!(shown.len(), all.len() - 1);
        // Order is kept, and very narrow windows keep fullscreen to the end.
        let tiny = fit(&all, 100.0);
        assert_eq!(tiny, vec![]);
        let small = fit(&all, bar_width(&[BarControl::Fullscreen]));
        assert_eq!(small, vec![BarControl::Fullscreen]);
        let mid = fit(&all, bar_width(&[BarControl::Next, BarControl::Fullscreen, BarControl::Volume]));
        assert_eq!(mid, vec![BarControl::Next, BarControl::Fullscreen, BarControl::Volume]);
    }
}
