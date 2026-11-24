//! Lazy system font metadata and bundled Open Sans Variable fallback.
//! Only the first subtitle or font Preferences access initializes the fontdb catalog.

use super::super::player::PlayerView;
use crate::settings::{BUNDLED_SUB_FONT_FAMILY, DEFAULT_SUB_FONT_FAMILY};
use gpui::Context;
use std::sync::{
    OnceLock,
    atomic::{AtomicBool, Ordering},
};

const EMBEDDED_SUB_FONT: &[u8] = include_bytes!("../../assets/fonts/OpenSans.ttf");
static BUNDLED_REGISTERED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefaultFontSource {
    System,
    Bundled,
    Unavailable,
}

struct FontCatalog {
    db: fontdb::Database,
    /// Names gathered before the bundled font was added.
    system_names: std::collections::HashSet<String>,
}

fn catalog() -> &'static FontCatalog {
    static DB: OnceLock<FontCatalog> = OnceLock::new();
    DB.get_or_init(|| {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        let names = db.faces().flat_map(|f| f.families.iter().map(|(name, _)| name.to_lowercase())).collect();
        db.load_font_data(EMBEDDED_SUB_FONT.to_vec());
        FontCatalog { db, system_names: names }
    })
}

fn system_has(family: &str) -> bool {
    catalog().system_names.contains(&family.trim().to_lowercase())
}

/// The spelling registered with fontdb (and expected by GPUI), rather than
/// the possibly differently cased name typed by the user.
fn canonical_system_family(requested: &str) -> Option<String> {
    let requested = requested.trim();
    if !system_has(requested) {
        return None;
    }
    catalog()
        .db
        .faces()
        .flat_map(|face| face.families.iter())
        .find(|(name, _)| name.eq_ignore_ascii_case(requested))
        .map(|(name, _)| name.clone())
}

fn ensure_bundled(cx: &mut Context<PlayerView>) -> bool {
    if BUNDLED_REGISTERED.load(Ordering::Relaxed) {
        return true;
    }
    use std::borrow::Cow;
    match cx.text_system().add_fonts(vec![Cow::Borrowed(EMBEDDED_SUB_FONT)]) {
        Ok(()) => {
            BUNDLED_REGISTERED.store(true, Ordering::Relaxed);
            true
        }
        Err(e) => {
            log::warn!("could not register embedded Open Sans: {e}");
            false
        }
    }
}

/// Availability must come from the fontdb system snapshot, not GPUI's
/// resolve_font(), which can silently substitute a fallback on Windows.
pub(super) fn has_family(family: &str, _cx: &mut Context<PlayerView>) -> bool {
    system_has(family) || family.trim().eq_ignore_ascii_case(BUNDLED_SUB_FONT_FAMILY)
}

pub(super) fn default_font_source(cx: &mut Context<PlayerView>) -> DefaultFontSource {
    if system_has(DEFAULT_SUB_FONT_FAMILY) {
        DefaultFontSource::System
    } else if ensure_bundled(cx) {
        DefaultFontSource::Bundled
    } else {
        DefaultFontSource::Unavailable
    }
}

pub(super) fn resolve_family(requested: &str, cx: &mut Context<PlayerView>) -> String {
    let requested = requested.trim();
    if !requested.is_empty() {
        if let Some(family) = canonical_system_family(requested) {
            return family;
        }
        if requested.eq_ignore_ascii_case(BUNDLED_SUB_FONT_FAMILY) && ensure_bundled(cx) {
            return BUNDLED_SUB_FONT_FAMILY.to_owned();
        }
    }
    if let Some(family) = canonical_system_family(DEFAULT_SUB_FONT_FAMILY) {
        return family;
    }
    let _ = ensure_bundled(cx);
    BUNDLED_SUB_FONT_FAMILY.to_owned()
}

/// Default's weights follow the actual resolved family.
pub(super) fn available_weights(family: &str) -> Vec<u16> {
    use std::collections::BTreeSet;
    let family = if family.trim().is_empty() {
        if system_has(DEFAULT_SUB_FONT_FAMILY) { DEFAULT_SUB_FONT_FAMILY } else { BUNDLED_SUB_FONT_FAMILY }
    } else {
        family.trim()
    };
    let db = &catalog().db;
    let mut weights = BTreeSet::new();
    for face in db.faces() {
        if face.style != fontdb::Style::Normal
            || !face.families.iter().any(|(name, _)| name.eq_ignore_ascii_case(family))
        {
            continue;
        }
        let range = db
            .with_face_data(face.id, |bytes, index| {
                ttf_parser::Face::parse(bytes, index).ok().and_then(|font| {
                    font.variation_axes()
                        .into_iter()
                        .find(|a| a.tag == ttf_parser::Tag::from_bytes(b"wght"))
                        .map(|a| (a.min_value, a.max_value))
                })
            })
            .flatten();
        if let Some((min, max)) = range {
            for w in (min.ceil().clamp(1., 1000.) as u16)..=(max.floor().clamp(1., 1000.) as u16) {
                weights.insert(w);
            }
        } else {
            weights.insert(face.weight.0);
        }
    }
    weights.into_iter().collect()
}

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

pub(super) fn installed_families() -> &'static [String] {
    static FAMILIES: OnceLock<Vec<String>> = OnceLock::new();
    FAMILIES.get_or_init(|| {
        let mut names: Vec<String> =
            catalog().db.faces().flat_map(|face| face.families.iter().map(|(name, _)| name.clone())).collect();
        names.sort_unstable_by_key(|s| s.to_lowercase());
        names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
        names
    })
}
