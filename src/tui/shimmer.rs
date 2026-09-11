//! Brightness-wave shimmer for activity labels (Codex-style).
//!
//! A highlight band sweeps across the text on a two-second period while the
//! agent works. Only brightness steps are used (dim → plain → bold →
//! cyan-bold crest), so it reads on any dark terminal without truecolor
//! probing: this mirrors Codex `shimmer.rs`' ANSI fallback, promoted to the
//! only mode since the palette is hardcoded for dark terminals anyway.
//!
//! The wave is tick-driven, not wall-clock: the UI loop already repaints at
//! 50ms while work runs, so one tick is one frame (40 ticks per sweep) and
//! the output is fully deterministic under test.

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use super::theme::Theme;

/// Ticks per full sweep at the 50ms UI tick (2 seconds).
pub const SHIMMER_PERIOD_TICKS: usize = 40;

/// Render `text` with a moving brightness band for the given tick.
/// Empty text renders to no spans.
pub fn shimmer_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    // display-column centers keep wide glyphs aligned with the wave
    let widths: Vec<f64> = chars
        .iter()
        .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0) as f64)
        .collect();
    let total: f64 = widths.iter().sum();
    let half = (total * 0.15).max(3.0);
    let pos = (tick % SHIMMER_PERIOD_TICKS) as f64 / SHIMMER_PERIOD_TICKS as f64
        * (total + 2.0 * half)
        - half;
    let mut column = 0.0;
    chars
        .into_iter()
        .zip(widths)
        .map(|(ch, w)| {
            let center = column + w / 2.0;
            column += w;
            let dist = ((center - pos).abs() / half).min(1.0);
            let t = 0.5 * (1.0 + (std::f64::consts::PI * dist).cos());
            let style = if t < 0.2 {
                Theme::dim()
            } else if t < 0.5 {
                Theme::base()
            } else if t < 0.8 {
                Style::new().add_modifier(Modifier::BOLD)
            } else {
                Theme::accent_bold()
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_renders_no_spans() {
        assert!(shimmer_spans("", 0).is_empty());
    }

    #[test]
    fn same_tick_is_deterministic() {
        let a: Vec<String> = shimmer_spans("Working", 7)
            .iter()
            .map(|s| format!("{:?}", s.style))
            .collect();
        let b: Vec<String> = shimmer_spans("Working", 7)
            .iter()
            .map(|s| format!("{:?}", s.style))
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn wave_travels_with_ticks() {
        // crest starts off-text (all dim) and reaches the text mid-period
        let dim_at = |tick: usize| {
            shimmer_spans("Working", tick)
                .iter()
                .filter(|s| s.style == Theme::dim())
                .count()
        };
        assert_eq!(dim_at(0), 7, "band starts off the text");
        assert!(
            dim_at(SHIMMER_PERIOD_TICKS / 4) < 7,
            "band must touch the text mid-sweep"
        );
    }

    #[test]
    fn text_survives_in_order() {
        let text: String = shimmer_spans("Working", 13)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "Working");
    }
}
