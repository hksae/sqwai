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

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;

use super::theme::Theme;

/// Ticks per full sweep at the 50ms UI tick (2 seconds).
pub const SHIMMER_PERIOD_TICKS: usize = 40;

/// One-shot finish-wave length for tool rows: green/red sweep, then static.
pub const FLASH_MS: u64 = 700;

/// Band base (outside the wave) and crest, Codex defaults for unknown palettes.
const BASE_RGB: (u8, u8, u8) = (128, 128, 128);
const CREST_RGB: (u8, u8, u8) = (255, 255, 255);

/// Linear blend between two grays, like Codex `color::blend`.
fn blend(base: (u8, u8, u8), crest: (u8, u8, u8), t: f64) -> (u8, u8, u8) {
    let mix = |b: u8, c: u8| (b as f64 + (c as f64 - b as f64) * t).round() as u8;
    (mix(base.0, crest.0), mix(base.1, crest.1), mix(base.2, crest.2))
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
    if let Ok(tp) = std::env::var("TERM_PROGRAM") {
        if matches!(
            tp.as_str(),
            "iTerm.app"
                | "WezTerm"
                | "vscode"
                | "Hyper"
                | "ghostty"
                | "kitty"
                | "Alacritty"
                | "foot"
        ) {
            return true;
        }
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
    let pos =
        (tick % SHIMMER_PERIOD_TICKS) as f64 / SHIMMER_PERIOD_TICKS as f64 * (total + 2.0 * half)
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

/// Truecolor path: every char BOLD, fg blended gray → white by the wave.
fn shimmer_rgb(text: &str, tick: usize) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let widths: Vec<f64> = chars
        .iter()
        .map(|c| UnicodeWidthChar::width(*c).unwrap_or(0) as f64)
        .collect();
    let total: f64 = widths.iter().sum();
    let mut column = 0.0;
    chars
        .into_iter()
        .zip(widths)
        .map(|(ch, w)| {
            let center = column + w / 2.0;
            column += w;
            let (r, g, b) = blend(BASE_RGB, CREST_RGB, wave_t(tick, center, total));
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

/// ANSI fallback: dim → plain → bold steps, Codex `color_for_level`.
fn shimmer_ansi(text: &str, tick: usize) -> Vec<Span<'static>> {
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
    let mut column = 0.0;
    chars
        .into_iter()
        .zip(widths)
        .map(|(ch, w)| {
            let center = column + w / 2.0;
            column += w;
            let t = wave_t(tick, center, total);
            let style = if t < 0.2 {
                Theme::dim()
            } else if t < 0.6 {
                Theme::base()
            } else {
                Style::new().add_modifier(Modifier::BOLD)
            };
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

/// One-shot finish wave for a tool head row: `progress` 0.0..1.0 sweeps
/// from the marker across the name, gray → green (ok) or red (failed),
/// settling into the static ✓/✗ styles the row keeps afterwards (so the
/// frozen end frame equals the normal render — no pop). Caller gates on
/// [`has_truecolor`]; without RGB there is no wave, just the static row.
pub fn flash_spans(marker: &str, name: &str, done_ok: bool, progress: f64) -> Vec<Span<'static>> {
    const BAND: f64 = 3.0;
    const GRAY: (u8, u8, u8) = (128, 128, 128);
    // GitHub-dark status hues, readable on the user strip
    const GREEN: (u8, u8, u8) = (63, 185, 80);
    const RED: (u8, u8, u8) = (248, 81, 73);
    let target = if done_ok { GREEN } else { RED };
    let marker_w: f64 = marker
        .chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as f64)
        .sum();
    let name_w: f64 = name
        .chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as f64)
        .sum();
    let total = marker_w + name_w;
    let frontier = progress.clamp(0.0, 1.0) * (total + BAND);
    let mut out = Vec::new();
    let mut column = 0.0;
    for (part, ch) in marker
        .chars()
        .map(|ch| (true, ch))
        .chain(name.chars().map(|ch| (false, ch)))
    {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as f64;
        let center = column + w / 2.0;
        column += w;
        let style = if center < frontier - BAND {
            // settled: exactly the static row's styles, no pop at the end
            if part {
                if done_ok {
                    Theme::ok()
                } else {
                    Theme::err()
                }
            } else {
                Theme::accent()
            }
        } else if center <= frontier {
            let k = 1.0 - (frontier - center) / BAND;
            let (r, g, b) = blend(GRAY, target, k.clamp(0.0, 1.0));
            Style::default()
                .fg(Color::Rgb(r, g, b))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Rgb(GRAY.0, GRAY.1, GRAY.2))
        };
        out.push(Span::styled(ch.to_string(), style));
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
            fg_at(0).iter().all(|fg| *fg == Some(Color::Rgb(128, 128, 128))),
            "band starts off the text: all base"
        );
        assert!(
            fg_at(SHIMMER_PERIOD_TICKS / 4)
                .iter()
                .any(|fg| *fg != Some(Color::Rgb(128, 128, 128))),
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
    fn flash_starts_gray_settles_to_static() {
        let at0: String = flash_spans("  ✓ ", "read", true, 0.0)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(at0, "  ✓ read");
        assert!(
            flash_spans("  ✓ ", "read", true, 0.0)
                .iter()
                .all(|s| s.style.fg == Some(Color::Rgb(128, 128, 128))),
            "nothing settled before the frontier"
        );
        // settled end equals the static row: marker ok-style, name accent
        let end = flash_spans("  ✓ ", "read", true, 1.0);
        assert_eq!(end[0].style, Theme::ok());
        assert_eq!(end[4].style, Theme::accent());
        let end_err = flash_spans("  ✗ ", "bash", false, 1.0);
        assert_eq!(end_err[0].style, Theme::err());
        // mid-sweep carries blended color between gray and green
        let mid = flash_spans("  ✓ ", "read", true, 0.5);
        assert!(
            mid.iter().any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _)))
                && s.style.fg != Some(Color::Rgb(128, 128, 128))),
            "band must tint mid-sweep"
        );
    }
}
