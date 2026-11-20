//! Small reusable UI pieces.

use super::theme::Theme;
use crate::icons::Icon;
use gpui::{AnyView, App, Context, Hsla, IntoElement, Pixels, SharedString, Window, div, prelude::*, px, svg};

pub fn icon(i: Icon, size: Pixels, color: Hsla) -> impl IntoElement {
    svg().path(i.path()).size(size).flex_none().text_color(color)
}

/// A simple text tooltip view.
pub struct Tooltip {
    text: SharedString,
    theme: Theme,
}

impl Render for Tooltip {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded(px(5.))
            .bg(self.theme.popup)
            .border_1()
            .border_color(self.theme.border)
            .text_color(self.theme.text)
            .text_size(px(12.))
            .shadow_md()
            .child(self.text.clone())
    }
}

pub fn tooltip(text: impl Into<SharedString>, theme: Theme) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let text = text.into();
    move |_w, cx| cx.new(|_| Tooltip { text: text.clone(), theme }).into()
}

/// Playback speed slider range. The scale is logarithmic so 1× sits in the
/// middle, with as much room for slowing down as for speeding up.
pub const SPEED_MIN: f64 = 0.25;
pub const SPEED_MAX: f64 = 4.0;

/// Slider position (0..=1) for a speed.
pub fn speed_to_frac(speed: f64) -> f32 {
    ((speed.clamp(SPEED_MIN, SPEED_MAX) / SPEED_MIN).ln() / (SPEED_MAX / SPEED_MIN).ln()) as f32
}

/// Speed for a slider position: 0.05 steps, catching at 1×.
pub fn frac_to_speed(f: f32) -> f64 {
    let raw = SPEED_MIN * (SPEED_MAX / SPEED_MIN).powf(f.clamp(0.0, 1.0) as f64);
    if (raw - 1.0).abs() < 0.04 {
        return 1.0;
    }
    ((raw / 0.05).round() * 0.05).clamp(SPEED_MIN, SPEED_MAX)
}

/// "1×", "1.25×", "0.5×".
pub fn format_speed(speed: f64) -> String {
    let s = format!("{speed:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}×")
}

pub fn format_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "--:--".into();
    }
    let s = secs.floor() as u64;
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
}

pub fn format_size(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", U[i]) }
}

/// Parses "1:23:45", "12:34", "90" or "1h2m3s" into seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.contains(['h', 'm', 's']) && !s.contains(':') {
        let mut total = 0.0;
        let mut num = String::new();
        for c in s.chars() {
            match c {
                '0'..='9' | '.' => num.push(c),
                'h' => total += std::mem::take(&mut num).parse::<f64>().ok()? * 3600.0,
                'm' => total += std::mem::take(&mut num).parse::<f64>().ok()? * 60.0,
                's' => total += std::mem::take(&mut num).parse::<f64>().ok()?,
                ' ' => {}
                _ => return None,
            }
        }
        if !num.is_empty() {
            total += num.parse::<f64>().ok()?;
        }
        return Some(total);
    }
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_slider_mapping() {
        assert!((speed_to_frac(1.0) - 0.5).abs() < 1e-6);
        assert_eq!(frac_to_speed(0.0), SPEED_MIN);
        assert_eq!(frac_to_speed(1.0), SPEED_MAX);
        assert_eq!(frac_to_speed(0.5), 1.0);
        assert_eq!(frac_to_speed(0.51), 1.0); // catches at 1×
        for s in [0.25, 0.5, 0.75, 1.25, 1.5, 2.0, 3.0] {
            assert!((frac_to_speed(speed_to_frac(s)) - s).abs() < 1e-9, "{s}");
        }
        assert_eq!(format_speed(1.0), "1×");
        assert_eq!(format_speed(1.25), "1.25×");
        assert_eq!(format_speed(0.5), "0.5×");
        assert_eq!(format_speed(1.1500000000000001), "1.15×");
    }
    #[test]
    fn time_roundtrip() {
        assert_eq!(format_time(3725.4), "1:02:05");
        assert_eq!(format_time(65.0), "01:05");
        assert_eq!(parse_time("1:02:05"), Some(3725.0));
        assert_eq!(parse_time("90"), Some(90.0));
        assert_eq!(parse_time("1h2m5s"), Some(3725.0));
        assert_eq!(parse_time("abc"), None);
    }
}
