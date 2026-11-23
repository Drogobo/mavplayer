//! Subtitle font selection for native GPUI rendering only.
//! The embedded fallback is registered lazily, on the first subtitle that needs it.

use super::super::player::PlayerView;
use crate::settings::DEFAULT_SUB_FONT_FAMILY;
use gpui::Context;

const EMBEDDED_SUB_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoSans.ttf");

/// `resolve_font` may silently substitute another family, so compare the
/// resolved face with the requested family before declaring it available.
pub(super) fn has_family(family: &str, cx: &mut Context<PlayerView>) -> bool {
    let family = family.trim();
    if family.is_empty() {
        return false;
    }
    let ts = cx.text_system();
    let id = ts.resolve_font(&gpui::font(family.to_owned()));
    ts.get_font_for_id(id).is_some_and(|face| face.family.eq_ignore_ascii_case(family))
}

pub(super) fn resolve_family(requested: &str, cx: &mut Context<PlayerView>) -> String {
    let requested = requested.trim();
    if !requested.is_empty() && has_family(requested, cx) {
        return requested.to_owned();
    }
    if has_family(DEFAULT_SUB_FONT_FAMILY, cx) {
        return DEFAULT_SUB_FONT_FAMILY.to_owned();
    }
    // No system fallback: make sure the executable always has a font available.
    // After successful registration has_family() will succeed, so subsequent
    // subtitle frames will not reload the same bytes.
    use std::borrow::Cow;
    if let Err(err) = cx.text_system().add_fonts(vec![Cow::Borrowed(EMBEDDED_SUB_FONT)]) {
        log::warn!("could not register embedded subtitle font: {err}");
    }
    DEFAULT_SUB_FONT_FAMILY.to_owned()
}
