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

/// One-shot finish-wave lengths for tool rows: success reads fast,
/// failure lingers — a failure must not be missable.
pub const FLASH_OK_MS: u64 = 1200;
pub const FLASH_ERR_MS: u64 = 1800;

/// Finish-wave duration for a tool row by outcome.
pub fn flash_ms(done_ok: bool) -> u64 {
    if done_ok { FLASH_OK_MS } else { FLASH_ERR_MS }
}

/// Band base (outside the wave) and crest. Base sits high (180) so the
/// trough reads bright, not muddy, on dark terminals; the crest still pops.
const BASE_RGB: (u8, u8, u8) = (180, 180, 180);
const CREST_RGB: (u8, u8, u8) = (255, 255, 255);

/// Linear blend between two RGB colors, like Codex `color::blend`.
/// Shared with the mode-chip sweep (yellow ACT ↔ blue PLAN).
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

/// Render `text` with a moving brightness band for the given tick.
/// Empty text renders to no spans.
pub fn shimmer_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
    if has_truecolor() {
        shimmer_rgb(text, tick)
    } else {
        shimmer_ansi(text, tick)
    }
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

/// Truecolor path: every char BOLD, fg blended gray → white by the wave.
fn shimmer_rgb(text: &str, tick: usize) -> Vec<Span<'static>> {
    shimmer_rgb_tint(text, tick, BASE_RGB, CREST_RGB)
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

/// One-shot finish wave for a tool head row: a saturated green (ok) or
/// muted red (failed) band sweeps from the marker across the name over
/// `progress` 0.0..1.0, then the static row. Saturated by design — the old
/// pastel whisper was invisible on white text; the sweep is brief, so it
/// reads instead of shouting.
/// The base is exactly the static row's styles (white marker + name),
/// so the frozen end frame equals the normal render (no pop).
/// Caller gates on [`has_truecolor`]; without RGB there is no wave, just
/// the static row.
pub fn flash_spans(marker: &str, name: &str, done_ok: bool, progress: f64) -> Vec<Span<'static>> {
    const BAND: f64 = 6.0;
    // the house green and red, so the wave sweeps in the same colors the row
    // settles to; bold stays here alone because a wave that cannot be seen is
    // not a wave, and it is gone the moment the row is
    let target = if done_ok {
        Theme::OK_RGB
    } else {
        Theme::ERR_RGB
    };
    let mut count = 0usize;
    let mut marker_w = 0.0f64;
    for c in marker.chars() {
        count += 1;
        marker_w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as f64;
    }
    let mut name_w = 0.0f64;
    for c in name.chars() {
        count += 1;
        name_w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as f64;
    }
    let total = marker_w + name_w;
    let frontier = progress.clamp(0.0, 1.0) * (total + BAND);
    let mut out = Vec::with_capacity(count);
    let mut column = 0.0;
    let wave_style = Style::default()
        .fg(Color::Rgb(target.0, target.1, target.2))
        .add_modifier(Modifier::BOLD);
    let head_bold = Theme::tool_head_bold();
    let head_plain = Theme::tool_head();
    for (part, ch) in marker
        .chars()
        .map(|ch| (true, ch))
        .chain(name.chars().map(|ch| (false, ch)))
    {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as f64;
        let center = column + w / 2.0;
        column += w;
        // settled: exactly the static row's styles, no pop at either end
        let settled = if part { head_bold } else { head_plain };
        let style = if center <= frontier && center > frontier - BAND {
            wave_style
        } else {
            settled
        };
        out.push(Span::styled(char_cow(ch), style));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_renders_no_spans() {
        assert!(shimmer_spans("", 0).is_empty());
        assert!(shimmer_ansi("", 0).is_empty());
        assert!(shimmer_rgb("", 0).is_empty());
    }

    #[test]
    fn same_tick_is_deterministic() {
        for tick in [0, 7, 20] {
            let spans = shimmer_spans("Working", tick);
            let again = shimmer_spans("Working", tick);
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
    fn rgb_wave_travels_from_base_to_crest() {
        let fg_at = |tick: usize| {
            shimmer_rgb("Working", tick)
                .iter()
                .map(|s| s.style.fg)
                .collect::<Vec<_>>()
        };
        assert!(
            fg_at(0)
                .iter()
                .all(|fg| *fg == Some(Color::Rgb(180, 180, 180))),
            "band starts off the text: all base"
        );
        assert!(
            fg_at(SHIMMER_PERIOD_TICKS / 4)
                .iter()
                .any(|fg| *fg != Some(Color::Rgb(180, 180, 180))),
            "band must brighten the text mid-sweep"
        );
        assert!(
            shimmer_rgb("Working", 13)
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "rgb path stays bold throughout, like Codex"
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

    #[test]
    fn tint_wave_travels_between_colors() {
        // env-independent: the rgb core directly, past the truecolor gate
        let base = BASE_RGB;
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

    #[test]
    fn flash_rides_static_without_gray() {
        const GRAY: Option<Color> = Some(Color::Rgb(128, 128, 128));
        // start: exactly the static row (white marker, white name)
        let at0 = flash_spans("  ✓ ", "read", true, 0.0);
        let text: String = at0.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "  ✓ read");
        assert_eq!(at0[0].style, Theme::tool_head_bold());
        assert_eq!(at0[4].style, Theme::tool_head());
        assert!(
            at0.iter().all(|s| s.style.fg != GRAY),
            "no gray phase at the start"
        );
        // mid-sweep: the house green band travels over the white name
        let mid = flash_spans("  ✓ ", "read", true, 0.5);
        assert!(
            mid.iter().any(|s| s.style.fg
                == Some(Color::Rgb(
                    Theme::OK_RGB.0,
                    Theme::OK_RGB.1,
                    Theme::OK_RGB.2
                ))),
            "the house green band must ride mid-sweep"
        );
        let mid_err = flash_spans("  ✗ ", "bash", false, 0.5);
        assert!(
            mid_err.iter().any(|s| s.style.fg
                == Some(Color::Rgb(
                    Theme::ERR_RGB.0,
                    Theme::ERR_RGB.1,
                    Theme::ERR_RGB.2
                ))),
            "the house red band must ride mid-sweep"
        );
        assert!(
            mid.iter().all(|s| s.style.fg != GRAY) && mid_err.iter().all(|s| s.style.fg != GRAY),
            "no gray phase mid-sweep either"
        );
        // end: settled back to the static row, no pop
        let end = flash_spans("  ✓ ", "read", true, 1.0);
        assert_eq!(end[0].style, Theme::tool_head_bold());
        assert_eq!(end[4].style, Theme::tool_head());
        let end_err = flash_spans("  ✗ ", "bash", false, 1.0);
        assert_eq!(end_err[0].style, Theme::tool_head_bold());
    }
}
