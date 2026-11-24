//! Experimental Cosmic Text subtitle rasterization: fill + stroke in one texture.
//! A high-resolution RGBA texture is scaled to logical pixels by GPUI.

use std::cell::RefCell;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use cosmic_text::{Align, Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};
use gpui::RenderImage;

thread_local! {
    static RASTERIZER: RefCell<Rasterizer> = RefCell::new(Rasterizer::new());
}

struct Rasterizer {
    fonts: FontSystem,
    cache: SwashCache,
}

impl Rasterizer {
    fn new() -> Self {
        let mut fonts = FontSystem::new();
        // The fallback comes from the user's current subtitle-font branch.
        fonts.db_mut().load_font_data(include_bytes!("../../assets/fonts/OpenSans.ttf").to_vec());
        Self { fonts, cache: SwashCache::new() }
    }

    fn render(
        &mut self,
        text: &str,
        family: &str,
        weight: u16,
        font_size: f32,
        logical_width: f32,
        scale: f32,
    ) -> Option<(Arc<RenderImage>, u32, u32)> {
        let line_height = font_size * 1.22;
        let metrics = Metrics::new(font_size * scale, line_height * scale);
        let mut buffer = Buffer::new(&mut self.fonts, metrics);
        let w = (logical_width * scale).ceil().max(1.0) as u32;
        // Guard against pathological allocation sizes.
        if text.is_empty() || w > 16384 {
            return None;
        }
        let pad = padding(font_size, scale);
        let aw = w.checked_add(pad * 2)?;
        // The original implementation allocated only one line of height,
        // causing subsequent soft-wrapped rows to disappear. Measure the
        // shaped, wrapped text before allocating its bitmap.
        let mut buf = buffer.borrow_with(&mut self.fonts);
        const MAX_HEIGHT: f32 = 2048.0;
        buf.set_size(Some(w as f32), Some(MAX_HEIGHT));
        buf.set_text(text, &Attrs::new().family(Family::Name(family)).weight(Weight(weight)), Shaping::Advanced);
        // Center each *visual* row, including rows created by soft wrapping.
        // Centering the final image alone would not center shorter wrapped rows.
        for line in buf.lines.iter_mut() {
            line.set_align(Some(Align::Center));
        }
        buf.shape_until_scroll(true);
        let visual_lines = buf.layout_runs().count();
        if visual_lines == 0 {
            return None;
        }
        let h = (visual_lines as f32 * line_height * scale).ceil() as u32;
        if h > MAX_HEIGHT as u32 {
            return None;
        }
        let ah = h.checked_add(pad * 2)?;
        let mut alpha = vec![0u8; (aw as usize).checked_mul(ah as usize)?];
        // Draw using the same width/wrapping, now at the measured height.
        buf.set_size(Some(w as f32), Some(h as f32));
        buf.shape_until_scroll(true);
        {
            buf.draw(&mut self.cache, Color::rgb(255, 255, 255), |x, y, gw, gh, color| {
                let a = color.a();
                if a == 0 {
                    return;
                }
                for dy in 0..gh {
                    for dx in 0..gw {
                        let px = x + dx as i32 + pad as i32;
                        let py = y + dy as i32 + pad as i32;
                        if px >= 0 && py >= 0 && (px as u32) < aw && (py as u32) < ah {
                            let idx = py as usize * aw as usize + px as usize;
                            alpha[idx] = alpha[idx].max(a);
                        }
                    }
                }
            });
        }
        // Crop horizontally so the image can be centered by GPUI.
        let mut left = aw;
        let mut right = 0u32;
        for y in 0..ah {
            for x in 0..aw {
                if alpha[y as usize * aw as usize + x as usize] > 0 {
                    left = left.min(x);
                    right = right.max(x + 1);
                }
            }
        }
        if left == aw {
            return None;
        }
        left = left.saturating_sub(pad);
        right = right.saturating_add(pad).min(aw);
        let out_w = right - left;
        let out_h = ah;
        let radius = (font_size * scale / 16.0).max(1.2 * scale).round().max(1.0) as i32;
        let mut outline = vec![0u8; out_w as usize * out_h as usize];
        for y in 0..ah as i32 {
            for x in left as i32..right as i32 {
                let a = alpha[y as usize * aw as usize + x as usize];
                if a == 0 {
                    continue;
                }
                for oy in -radius..=radius {
                    for ox in -radius..=radius {
                        if ox * ox + oy * oy > radius * radius {
                            continue;
                        }
                        let xx = x + ox;
                        let yy = y + oy;
                        if xx < left as i32 || xx >= right as i32 || yy < 0 || yy >= ah as i32 {
                            continue;
                        }
                        let i = yy as usize * out_w as usize + (xx as u32 - left) as usize;
                        outline[i] = outline[i].max(a);
                    }
                }
            }
        }
        let mut rgba = vec![0u8; out_w as usize * out_h as usize * 4];
        for y in 0..out_h {
            for x in 0..out_w {
                let i = y as usize * out_w as usize + x as usize;
                let fill = alpha[y as usize * aw as usize + (left + x) as usize] as f32 / 255.0;
                let stroke = (outline[i] as f32 / 255.0) * 0.9;
                let a = fill + stroke * (1.0 - fill);
                let intensity = if a > 0.0 { (fill / a * 255.0).round() as u8 } else { 0 };
                rgba[i * 4] = intensity;
                rgba[i * 4 + 1] = intensity;
                rgba[i * 4 + 2] = intensity;
                rgba[i * 4 + 3] = (a * 255.0).round() as u8;
            }
        }
        let bitmap = image::RgbaImage::from_raw(out_w, out_h, rgba)?;
        let image = Arc::new(RenderImage::new(smallvec::smallvec![image::Frame::new(bitmap)]));
        Some((image, out_w, out_h))
    }
}

pub fn padding(font: f32, scale: f32) -> u32 {
    (font * scale / 16.0).max(1.2 * scale).ceil() as u32 + 3
}

pub fn supersampling() -> f32 {
    if std::env::var("MAVPLAYER_SUBTITLE_SS").as_deref() == Ok("2") { 2.0 } else { 1.0 }
}

pub fn cache_key(text: &str, family: &str, weight: u16, font: f32, width: f32, scale: f32) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    family.hash(&mut hasher);
    weight.hash(&mut hasher);
    font.to_bits().hash(&mut hasher);
    width.to_bits().hash(&mut hasher);
    scale.to_bits().hash(&mut hasher);
    hasher.finish()
}

pub fn render_line(
    text: &str,
    family: &str,
    weight: u16,
    font: f32,
    width: f32,
    scale: f32,
) -> Option<(Arc<RenderImage>, u32, u32)> {
    RASTERIZER.with(|r| r.borrow_mut().render(text, family, weight, font, width, scale))
}
