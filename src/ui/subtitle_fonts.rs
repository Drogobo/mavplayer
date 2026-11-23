//! Subtitle font selection for native GPUI rendering only.
//! The embedded fallback is registered lazily, on the first subtitle that needs it.

use super::super::player::PlayerView;
use crate::settings::DEFAULT_SUB_FONT_FAMILY;
use gpui::Context;
use std::sync::atomic::{AtomicBool, Ordering};

const EMBEDDED_SUB_FONT: &[u8] = include_bytes!("../../assets/fonts/NotoSans.ttf");

// GPUI does not expose the origin of a loaded face. Record when our own
// embedded fallback is registered so the Preferences label remains accurate
// after that registration makes has_family() return true.
static BUNDLED_SUB_FONT_REGISTERED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefaultFontSource {
    System,
    Bundled,
    Unavailable,
}

/// Report the actual source of the default font, not merely whether GPUI
/// resolves its family name (which can be a substitute on Windows).
pub(super) fn default_font_source(cx: &mut Context<PlayerView>) -> DefaultFontSource {
    if BUNDLED_SUB_FONT_REGISTERED.load(Ordering::Relaxed) {
        return DefaultFontSource::Bundled;
    }
    if system_has_default() && has_family(DEFAULT_SUB_FONT_FAMILY, cx) {
        return DefaultFontSource::System;
    }
    let _ = resolve_family("", cx);
    if BUNDLED_SUB_FONT_REGISTERED.load(Ordering::Relaxed) {
        DefaultFontSource::Bundled
    } else {
        DefaultFontSource::Unavailable
    }
}

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
    if BUNDLED_SUB_FONT_REGISTERED.load(Ordering::Relaxed)
        || (system_has_default() && has_family(DEFAULT_SUB_FONT_FAMILY, cx))
    {
        return DEFAULT_SUB_FONT_FAMILY.to_owned();
    }
    // No verified system fallback: make sure the executable always has a font available.
    // After successful registration has_family() will succeed, so subsequent
    // subtitle frames will not reload the same bytes.
    use std::borrow::Cow;
    match cx.text_system().add_fonts(vec![Cow::Borrowed(EMBEDDED_SUB_FONT)]) {
        Ok(()) => {
            if has_family(DEFAULT_SUB_FONT_FAMILY, cx) {
                BUNDLED_SUB_FONT_REGISTERED.store(true, Ordering::Relaxed);
            } else {
                log::warn!("embedded subtitle font registered but GPUI cannot resolve it");
            }
        }
        Err(err) => log::warn!("could not register embedded subtitle font: {err}"),
    }
    DEFAULT_SUB_FONT_FAMILY.to_owned()
}

/// The system snapshot must be inspected BEFORE adding the bundled face.
/// Keep it together with the existing lazy font catalog to avoid a second
/// system-font scan. Nothing is scanned at application startup unless a
/// subtitle (or the font preferences) actually requests this information.
struct FontCatalog {
    db: fontdb::Database,
    system_has_default: bool,
}

fn font_catalog() -> &'static FontCatalog {
    static CATALOG: std::sync::OnceLock<FontCatalog> = std::sync::OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let system_has_default = db
            .faces()
            .any(|face| face.families.iter().any(|(name, _)| name.eq_ignore_ascii_case(DEFAULT_SUB_FONT_FAMILY)));
        // Bundled font is for weight metadata only; it must never be counted
        // as an installed system font.
        db.load_font_data(EMBEDDED_SUB_FONT.to_vec());
        FontCatalog { db, system_has_default }
    })
}

fn system_has_default() -> bool {
    font_catalog().system_has_default
}

/// Declared upright weights for the selected family. Variable fonts contribute
/// the complete 'wght' range, rounded to integer CSS weight values.
/// This is metadata, not a guarantee that GPUI synthesizes every weight.
pub(super) fn available_weights(family: &str) -> Vec<u16> {
    use std::collections::BTreeSet;
    let family = if family.trim().is_empty() { DEFAULT_SUB_FONT_FAMILY } else { family.trim() };
    let db = &font_catalog().db;
    let mut weights = BTreeSet::new();
    for face in db.faces() {
        if face.style != fontdb::Style::Normal
            || !face.families.iter().any(|(name, _)| name.eq_ignore_ascii_case(family))
        {
            continue;
        }
        // For variable fonts, the default face weight alone is misleading.
        // Read the OpenType 'wght' axis from the actual face bytes.
        let range = db
            .with_face_data(face.id, |bytes, index| {
                ttf_parser::Face::parse(bytes, index).ok().and_then(|font| {
                    font.variation_axes()
                        .into_iter()
                        .find(|axis| axis.tag == ttf_parser::Tag::from_bytes(b"wght"))
                        .map(|axis| (axis.min_value, axis.max_value))
                })
            })
            .flatten();
        if let Some((min, max)) = range {
            let lo = min.ceil().clamp(1.0, 1000.0) as u16;
            let hi = max.floor().clamp(1.0, 1000.0) as u16;
            for weight in lo..=hi {
                weights.insert(weight);
            }
        } else {
            weights.insert(face.weight.0);
        }
    }
    weights.into_iter().collect()
}

/// Preserve a selected weight if available; otherwise prefer Regular (400),
/// then the closest actually declared upright weight.
pub(super) fn supported_weight(family: &str, current: u16) -> u16 {
    let weights = available_weights(family);
    if weights.contains(&current) {
        return current;
    }
    if weights.contains(&400) {
        return 400;
    }
    weights.into_iter().min_by_key(|&w| (w.abs_diff(current), w)).unwrap_or(current)
}

/// Installed font families, from the same lazy catalog used for weight lookup.
/// Collect once, sort case-insensitively, and remove localized-name duplicates.
pub(super) fn installed_families() -> &'static [String] {
    static FAMILIES: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    FAMILIES.get_or_init(|| {
        let mut families: Vec<String> =
            font_catalog().db.faces().flat_map(|face| face.families.iter().map(|(name, _)| name.clone())).collect();
        families.sort_unstable_by_key(|name| name.to_lowercase());
        families.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        families
    })
}
