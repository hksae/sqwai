//! Brightness-wave shimmer for activity labels (Codex-style).
//!
//! A highlight band sweeps across the text on a two-second period while the
//! agent works. Two paths, like Codex `shimmer.rs`: a truecolor blend from
//! mid gray to near-white when the terminal advertises RGB, otherwise the
//! ANSI fallback (dim → plain → bold). Detection is env heuristics only —
//! no probing, no new dependency. The rest of the palette stays ANSI-16.
//!
//! The wave is tick-driven, not wall-clock: the UI loop already repaints at
//! 50ms while work runs, so one tick is one frame (40 ticks per sweep) and
//! the output is fully deterministic under test.

use std::borrow::Cow;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use super::theme::Theme;

/// Single-byte ASCII table (`0x00..=0x7f`) so per-character spans in
/// shimmers, flashes, and gradients borrow `'static` slices instead of
/// heap-allocating a `String` per glyph on every animation tick.
const ASCII_TABLE: &str = concat!(
    "\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
    "\x10\x11\x12\x13\x14\x15\x16\x17\x18\x19\x1a\x1b\x1c\x1d\x1e\x1f",
    " !\"#$%&'()*+,-./0123456789:;<=>?",
    "@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_",
    "`abcdefghijklmnopqrstuvwxyz{|}~\x7f"
);

#[inline]
pub(crate) fn char_cow(ch: char) -> Cow<'static, str> {
    let u = ch as u32;
    if u < 128 {
        let i = u as usize;
        Cow::Borrowed(&ASCII_TABLE[i..i + 1])
    } else {
        match ch {
            '•' => Cow::Borrowed("•"),
            '✓' => Cow::Borrowed("✓"),
            '✗' => Cow::Borrowed("✗"),
            '…' => Cow::Borrowed("…"),
            '·' => Cow::Borrowed("·"),
            '─' => Cow::Borrowed("─"),
            '│' => Cow::Borrowed("│"),
            '›' => Cow::Borrowed("›"),
            _ => Cow::Owned(ch.to_string()),
        }
    }
}

/// Ticks per full sweep at the 50ms UI tick (2 seconds).
pub const SHIMMER_PERIOD_TICKS: usize = 40;

/// Crest of the traveling band: white, so it pops over any base it is given.
const CREST_RGB: (u8, u8, u8) = (255, 255, 255);

/// Linear blend between two RGB colors, like Codex `color::blend`.
/// Shared by the status-chip and effort-slider sweeps.
pub(crate) fn blend(base: (u8, u8, u8), crest: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let mix = |b: u8, c: u8| (b as f64 + (c as f64 - b as f64) * t).round() as u8;
    (
        mix(base.0, crest.0),
        mix(base.1, crest.1),
        mix(base.2, crest.2),
    )
}

/// Terminal env does not change mid-process: detect once, reuse every
/// frame instead of re-reading env vars per row.
static TRUECOLOR: OnceLock<bool> = OnceLock::new();

/// Truecolor advertised via env (same signals `supports-color` reads):
/// explicit COLORTERM, Windows Terminal, known-good TERM_PROGRAM/TERM.
/// Unknown terminals get the ANSI fallback — never garbage.
pub(crate) fn has_truecolor() -> bool {
    *TRUECOLOR.get_or_init(detect_truecolor)
}

fn detect_truecolor() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if let Ok(ct) = std::env::var("COLORTERM") {
        let ct = ct.to_ascii_lowercase();
        if ct.contains("truecolor") || ct.contains("24bit") {
            return true;
        }
    }
    if std::env::var_os("WT_SESSION").is_some() {
        return true;
    }
    if let Ok(tp) = std::env::var("TERM_PROGRAM")
        && matches!(
            tp.as_str(),
            "iTerm.app"
                | "WezTerm"
                | "vscode"
                | "Hyper"
                | "ghostty"
                | "kitty"
                | "Alacritty"
                | "foot"
        )
    {
        return true;
    }
    if let Ok(term) = std::env::var("TERM") {
        let t = term.to_ascii_lowercase();
        if t.contains("truecolor")
            || t.contains("24bit")
            || t.ends_with("-direct")
            || t.starts_with("xterm-kitty")
            || t == "foot"
        {
            return true;
        }
    }
    false
}

/// Wave position 0.0..1.0 across `total` columns at `tick` (shared by both
/// paths so they stay in phase).
fn wave_t(tick: usize, center: f64, total: f64) -> f64 {
    let half = (total * 0.15).max(3.0);
    let pos = (tick % SHIMMER_PERIOD_TICKS) as f64 / SHIMMER_PERIOD_TICKS as f64
        * (total + 2.0 * half)
        - half;
    let dist = ((center - pos).abs() / half).min(1.0);
    0.5 * (1.0 + (std::f64::consts::PI * dist).cos())
}

/// Render `text` in `Theme::WARN_RGB` with a bright white crest sweeping
/// across it for the given tick (used on the approval command line).
pub fn shimmer_warn_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
    if has_truecolor() {
        shimmer_rgb_tint(text, tick, Theme::WARN_RGB, CREST_RGB)
    } else {
        shimmer_ansi(text, tick)
    }
}

/// Render `text` in the brand accent (`CORAL_START`) with a bright white crest
/// sweeping across it for the given tick (used on the transcript header `sqwai`).
pub fn shimmer_brand_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
    if has_truecolor() {
        shimmer_rgb_tint(text, tick, crate::tui::art::CORAL_START, CREST_RGB)
    } else {
        if text.is_empty() {
            return Vec::new();
        }
        let mut count = 0usize;
        let mut total = 0.0f64;
        for c in text.chars() {
            count += 1;
            total += UnicodeWidthChar::width(c).unwrap_or(0) as f64;
        }
        let mut out = Vec::with_capacity(count);
        let mut column = 0.0;
        let crest_style = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
        let base_style = Theme::brand_ink().add_modifier(Modifier::BOLD);
        for ch in text.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0) as f64;
            let center = column + w / 2.0;
            column += w;
            let t = wave_t(tick, center, total);
            let style = if t >= 0.6 { crest_style } else { base_style };
            out.push(Span::styled(char_cow(ch), style));
        }
        out
    }
}

/// Truecolor path with an arbitrary color transition: every char BOLD, fg
/// blended `base` → `crest` by the travelling wave. This is the whole trick
/// behind a color-to-color shimmer — the wave math is shared, only the
/// gradient endpoints change.
fn shimmer_rgb_tint(
    text: &str,
    tick: usize,
    base: (u8, u8, u8),
    crest: (u8, u8, u8),
) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut count = 0usize;
    let mut total = 0.0f64;
    for c in text.chars() {
        count += 1;
        total += UnicodeWidthChar::width(c).unwrap_or(0) as f64;
    }
    let mut out = Vec::with_capacity(count);
    let mut column = 0.0;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as f64;
        let center = column + w / 2.0;
        column += w;
        let (r, g, b) = blend(base, crest, wave_t(tick, center, total));
        out.push(Span::styled(
            char_cow(ch),
            Style::default()
                .fg(Color::Rgb(r, g, b))
                .add_modifier(Modifier::BOLD),
        ));
    }
    out
}

/// ANSI fallback: dim → plain → bold steps, Codex `color_for_level`.
fn shimmer_ansi(text: &str, tick: usize) -> Vec<Span<'static>> {
    if text.is_empty() {
        return Vec::new();
    }
    // display-column centers keep wide glyphs aligned with the wave
    let mut count = 0usize;
    let mut total = 0.0f64;
    for c in text.chars() {
        count += 1;
        total += UnicodeWidthChar::width(c).unwrap_or(0) as f64;
    }
    let mut out = Vec::with_capacity(count);
    let mut column = 0.0;
    let dim_style = Theme::dim();
    let base_style = Theme::base();
    let bold_style = Style::new().add_modifier(Modifier::BOLD);
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as f64;
        let center = column + w / 2.0;
        column += w;
        let t = wave_t(tick, center, total);
        let style = if t < 0.2 {
            dim_style
        } else if t < 0.6 {
            base_style
        } else {
            bold_style
        };
        out.push(Span::styled(char_cow(ch), style));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gray the removed plain-white shimmer used as its base: kept here so
    /// the shared wave math is still tested against a real endpoint pair.
    const GRAY: (u8, u8, u8) = (180, 180, 180);

    #[test]
    fn empty_text_renders_no_spans() {
        assert!(shimmer_rgb_tint("", 0, GRAY, CREST_RGB).is_empty());
        assert!(shimmer_ansi("", 0).is_empty());
    }

    #[test]
    fn same_tick_is_deterministic() {
        for tick in [0, 7, 20] {
            let spans = shimmer_rgb_tint("Working", tick, GRAY, CREST_RGB);
            let again = shimmer_rgb_tint("Working", tick, GRAY, CREST_RGB);
            assert_eq!(spans, again);
        }
    }

    #[test]
    fn ansi_wave_travels_with_ticks() {
        // crest starts off-text (all dim) and reaches the text mid-period
        let dim_at = |tick: usize| {
            shimmer_ansi("Working", tick)
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
    fn rgb_path_boldens_every_char() {
        assert!(
            shimmer_rgb_tint("Working", 13, GRAY, CREST_RGB)
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "rgb path stays bold throughout, like Codex"
        );
    }

    #[test]
    fn tint_wave_travels_between_colors() {
        // env-independent: the rgb core directly, past the truecolor gate
        let base = GRAY;
        let at0 = shimmer_rgb_tint("Working", 0, base, CREST_RGB);
        assert!(
            at0.iter()
                .all(|s| s.style.fg == Some(Color::Rgb(base.0, base.1, base.2))),
            "band starts off the text: all base"
        );
        let mid = shimmer_rgb_tint("Working", SHIMMER_PERIOD_TICKS / 4, base, CREST_RGB);
        assert!(
            mid.iter()
                .any(|s| s.style.fg != Some(Color::Rgb(base.0, base.1, base.2))),
            "band must leave the base color mid-sweep"
        );
        assert!(
            mid.iter()
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "tint path stays bold throughout, like the classic one"
        );
        let text: String = mid.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "Working");
    }
}
