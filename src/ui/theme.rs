//! Colours. Follows the system light/dark preference unless overridden.
//!
//! The palette follows the app icon: neutral charcoal like its disc, and its
//! orange-to-lavender gradient for the accent.

use crate::settings::ThemeMode;
use gpui::{Background, Hsla, Window, WindowAppearance, linear_color_stop, linear_gradient, rgb, rgba};

#[derive(Clone, Copy)]
pub struct Theme {
    /// Window chrome (menu bar, control bar).
    pub chrome: Hsla,
    pub chrome_border: Hsla,
    pub surface: Hsla,
    pub surface_alt: Hsla,
    pub popup: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub text_dim: Hsla,
    pub text_disabled: Hsla,
    /// The icon's orange: solid accents (toggles, checks, focus rings).
    pub accent: Hsla,
    /// The icon's lavender, where the brand gradient ends.
    pub accent_end: Hsla,
    pub accent_soft: Hsla,
    /// A menu list's chosen item: `accent_soft` over `surface_alt`, as a solid
    /// colour, so it matches the current Preferences category on menus' darker background.
    pub chosen: Hsla,
    pub on_accent: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub track: Hsla,
    pub backdrop: Hsla,
    pub osd_bg: Hsla,
    pub danger: Hsla,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            chrome: rgb(0x1c1b1c).into(),
            chrome_border: rgb(0x2d2b2d).into(),
            surface: rgb(0x242324).into(),
            surface_alt: rgb(0x2e2c2e).into(),
            popup: rgb(0x292729).into(),
            border: rgb(0x3e3b3e).into(),
            text: rgb(0xefeced).into(),
            text_dim: rgb(0xaba6a9).into(),
            text_disabled: rgb(0x6f6a6d).into(),
            accent: rgb(0xf4682a).into(),
            accent_end: rgb(0xa08cff).into(),
            accent_soft: rgba(0xf4682a33).into(),
            chosen: rgb(0x56382d).into(),
            on_accent: rgb(0xffffff).into(),
            hover: rgba(0xffffff14).into(),
            pressed: rgba(0xffffff22).into(),
            track: rgba(0xffffff2e).into(),
            backdrop: rgba(0x0000008c).into(),
            osd_bg: rgba(0x141314d0).into(),
            danger: rgb(0xff5d6e).into(),
        }
    }

    pub fn light() -> Self {
        Self {
            chrome: rgb(0xf6f3f2).into(),
            chrome_border: rgb(0xe3dedc).into(),
            surface: rgb(0xffffff).into(),
            surface_alt: rgb(0xf2eeed).into(),
            popup: rgb(0xffffff).into(),
            border: rgb(0xd9d3d1).into(),
            text: rgb(0x221e20).into(),
            text_dim: rgb(0x625b5e).into(),
            text_disabled: rgb(0xa9a2a5).into(),
            accent: rgb(0xdf520e).into(),
            accent_end: rgb(0x8670ee).into(),
            accent_soft: rgba(0xdf520e24).into(),
            chosen: rgb(0xefd8ce).into(),
            on_accent: rgb(0xffffff).into(),
            hover: rgba(0x0000000f).into(),
            pressed: rgba(0x0000001c).into(),
            track: rgba(0x00000024).into(),
            backdrop: rgba(0x00000066).into(),
            osd_bg: rgba(0x141314d0).into(),
            danger: rgb(0xd42f45).into(),
        }
    }

    /// The icon's orange → lavender gradient. `angle` follows CSS: 180° runs
    /// top to bottom (as on the icon), 90° left to right.
    pub fn brand(&self, angle: f32) -> Background {
        linear_gradient(angle, linear_color_stop(self.accent, 0.), linear_color_stop(self.accent_end, 1.))
    }

    pub fn resolve(mode: ThemeMode, window: &Window) -> Self {
        let dark = match mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::System => matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark),
        };
        if dark { Self::dark() } else { Self::light() }
    }
}

/// Colours used for controls drawn on top of video (always light-on-dark).
pub fn overlay_theme(base: &Theme) -> Theme {
    let mut t = Theme::dark();
    t.accent = base.accent;
    t.accent_end = base.accent_end;
    t.accent_soft = base.accent_soft;
    t.chrome = rgba(0x00000000).into();
    t
}
