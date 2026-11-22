//! The video picture, drawn by a view of its own.
//!
//! A new video frame used to re-render the whole window: every element of
//! the menus, control bar, overlays and dialogs was laid out and painted
//! again 60 times a second, most of the UI thread's time during playback.
//! Now a frame notifies only this view, which paints the picture beneath the
//! rest of the window ([`Chrome`]). GPUI re-renders the root (an ancestor),
//! which only holds the two, and replays the cached chrome as it was. The
//! chrome is redrawn when the player notifies (state changes, the 200 ms
//! tick while playing, subtitle changes; see `on_engine_wake`).

use super::player::{Aspect, PlayerView};
use super::render::video_rect;
use gpui::{
    Bounds, ContentMask, Context, Corners, IntoElement, Pixels, Render, RenderImage, Styled, WeakEntity, Window,
    canvas, div, point, px, size,
};
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

pub struct Picture {
    pub frame: Option<Arc<RenderImage>>,
    /// The frame's size in pixels, and the border of repeated edge pixels
    /// around it (see `engine::present`).
    pub size: (u32, u32),
    pub pad: u32,
    pub aspect: Aspect,
    pub zoom: f32,
    /// The video area, as the chrome laid it out. Its prepaint stores it, and
    /// every prepaint runs before any paint, so painting reads this frame's.
    pub area: Rc<Cell<Bounds<Pixels>>>,
}

impl Render for Picture {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let (frame, fsize, pad, aspect, zoom) =
            (self.frame.clone(), self.size, self.pad as f32, self.aspect, self.zoom);
        let area = self.area.clone();
        canvas(
            |_, _, _| (),
            move |_, _, window, _| {
                let b = area.get();
                if b.size.width <= px(0.) || b.size.height <= px(0.) {
                    return;
                }
                window.paint_quad(gpui::fill(b, gpui::black()));
                let Some(img) = frame else { return };
                let r = picture_rect(b, fsize, aspect, zoom, window.scale_factor());
                // The image carries a border of repeated edge pixels so the
                // GPU's filtering never reaches past the picture: draw it
                // that much larger, clipped to the picture (and the area,
                // when zoomed in).
                let (px_w, px_h) =
                    (f32::from(r.size.width) / fsize.0.max(1) as f32, f32::from(r.size.height) / fsize.1.max(1) as f32);
                let padded = Bounds::new(
                    point(r.origin.x - px(pad * px_w), r.origin.y - px(pad * px_h)),
                    size(r.size.width + px(2. * pad * px_w), r.size.height + px(2. * pad * px_h)),
                );
                window.with_content_mask(Some(ContentMask { bounds: r.intersect(&b) }), |window| {
                    let _ = window.paint_image(padded, Corners::default(), img, 0, false);
                });
            },
        )
        .absolute()
        .size_full()
    }
}

/// Where a `fsize` picture is drawn in `area`: [`video_rect`], except that a
/// rect within a pixel of the picture's own size (its width is rounded to
/// even, see `engine::present::output_size`) becomes exactly that size, as
/// stretching it by one pixel blurs it.
pub fn picture_rect(area: Bounds<Pixels>, fsize: (u32, u32), aspect: Aspect, zoom: f32, scale: f32) -> Bounds<Pixels> {
    let r = video_rect(area, fsize, aspect, zoom);
    let (iw, ih) = (fsize.0 as f32 / scale, fsize.1 as f32 / scale);
    let (rw, rh) = (f32::from(r.size.width), f32::from(r.size.height));
    if (rw - iw).abs() <= 1.0 / scale + 0.01 && (rh - ih).abs() <= 1.0 / scale + 0.01 {
        let x = f32::from(r.origin.x) + (rw - iw) / 2.0;
        let y = f32::from(r.origin.y) + (rh - ih) / 2.0;
        Bounds::new(point(px((x * scale).round() / scale), px((y * scale).round() / scale)), size(px(iw), px(ih)))
    } else {
        r
    }
}

/// Everything in the window but the picture: rendered by `PlayerView` and
/// cached by GPUI between player notifications.
pub struct Chrome {
    pub player: WeakEntity<PlayerView>,
}

impl Render for Chrome {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.player.upgrade() {
            Some(p) => p.update(cx, |p, cx| p.render_chrome(window, cx)),
            None => div().into_any_element(),
        }
    }
}
