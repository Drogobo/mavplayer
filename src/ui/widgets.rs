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
    fn time_roundtrip() {
        assert_eq!(format_time(3725.4), "1:02:05");
        assert_eq!(format_time(65.0), "01:05");
        assert_eq!(parse_time("1:02:05"), Some(3725.0));
        assert_eq!(parse_time("90"), Some(90.0));
        assert_eq!(parse_time("1h2m5s"), Some(3725.0));
        assert_eq!(parse_time("abc"), None);
    }
}
