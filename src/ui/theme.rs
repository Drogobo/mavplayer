//! Colours. Follows the system light/dark preference unless overridden.

use crate::settings::ThemeMode;
use gpui::{Hsla, Window, WindowAppearance, rgb, rgba};

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
    pub accent: Hsla,
    pub accent_soft: Hsla,
    pub on_accent: Hsla,
    pub hover: Hsla,
    pub pressed: Hsla,
    pub track: Hsla,
    pub backdrop: Hsla,
    pub video_bg: Hsla,
    pub osd_bg: Hsla,
    pub danger: Hsla,
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            chrome: rgb(0x1b1c20).into(),
            chrome_border: rgb(0x2a2c32).into(),
            surface: rgb(0x23252b).into(),
            surface_alt: rgb(0x2b2e35).into(),
            popup: rgb(0x26282e).into(),
            border: rgb(0x3a3d45).into(),
            text: rgb(0xe9eaee).into(),
            text_dim: rgb(0xa3a7b1).into(),
            text_disabled: rgb(0x6b6f78).into(),
            accent: rgb(0x6d7cff).into(),
            accent_soft: rgba(0x6d7cff33).into(),
            on_accent: rgb(0xffffff).into(),
            hover: rgba(0xffffff14).into(),
            pressed: rgba(0xffffff22).into(),
            track: rgba(0xffffff2e).into(),
            backdrop: rgba(0x0000008c).into(),
            video_bg: rgb(0x000000).into(),
            osd_bg: rgba(0x101114d0).into(),
            danger: rgb(0xff6b6b).into(),
        }
    }

    pub fn light() -> Self {
        Self {
            chrome: rgb(0xf3f4f7).into(),
            chrome_border: rgb(0xdcdfe5).into(),
            surface: rgb(0xffffff).into(),
            surface_alt: rgb(0xf0f1f4).into(),
            popup: rgb(0xffffff).into(),
            border: rgb(0xd3d6dd).into(),
            text: rgb(0x1d1f24).into(),
            text_dim: rgb(0x5d626d).into(),
            text_disabled: rgb(0xa2a6ae).into(),
            accent: rgb(0x4f5ee8).into(),
            accent_soft: rgba(0x4f5ee826).into(),
            on_accent: rgb(0xffffff).into(),
            hover: rgba(0x0000000f).into(),
            pressed: rgba(0x0000001c).into(),
            track: rgba(0x00000024).into(),
            backdrop: rgba(0x00000066).into(),
            video_bg: rgb(0x000000).into(),
            osd_bg: rgba(0x101114d0).into(),
            danger: rgb(0xd93636).into(),
        }
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
    t.accent_soft = base.accent_soft;
    t.chrome = rgba(0x00000000).into();
    t
}
