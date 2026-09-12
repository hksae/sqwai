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
            let (r, g, b) = blend(base, crest, wave_t(tick, center, total));
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

/// Gallery tints (base → crest) for the color-transition shimmers.
pub const TINT_OCEAN: [(u8, u8, u8); 2] = [(24, 80, 130), (130, 225, 255)];
pub const TINT_EMBER: [(u8, u8, u8); 2] = [(135, 65, 25), (255, 185, 95)];
pub const TINT_MINT: [(u8, u8, u8); 2] = [(30, 115, 70), (150, 255, 195)];

/// Tinted wave: the same travelling band as [`shimmer_spans`], but sweeping
/// from `base` to `crest` — a color-to-color transition. Truecolor only;
/// elsewhere the shared ANSI steps (dim → plain → bold) stand in, since a
/// 16-color terminal cannot lerp.
pub fn shimmer_tint_spans(
    text: &str,
    tick: usize,
    base: (u8, u8, u8),
    crest: (u8, u8, u8),
) -> Vec<Span<'static>> {
    if has_truecolor() {
        shimmer_rgb_tint(text, tick, base, crest)
    } else {
        shimmer_ansi(text, tick)
    }
}

/// Whole-text breathing pulse (no travel): every char shares one phase that
/// runs base → crest → base over the same two-second period. Truecolor only;
/// elsewhere the ANSI steps.
pub fn shimmer_pulse_spans(text: &str, tick: usize) -> Vec<Span<'static>> {
    if !has_truecolor() {
        return shimmer_ansi(text, tick);
    }
    let t = 0.5
        * (1.0
            + (tick as f64 / SHIMMER_PERIOD_TICKS as f64 * 2.0 * std::f64::consts::PI).cos());
    shimmer_pulse_rgb(text, t)
}

fn shimmer_pulse_rgb(text: &str, t: f64) -> Vec<Span<'static>> {
    let (r, g, b) = blend(BASE_RGB, CREST_RGB, t.clamp(0.0, 1.0));
    let style = Style::default()
        .fg(Color::Rgb(r, g, b))
        .add_modifier(Modifier::BOLD);
    text.chars()
        .map(|ch| Span::styled(ch.to_string(), style))
        .collect()
}

/// Gallery entry point: picks the shimmer by row name, classic by default.
pub fn shimmer_named(text: &str, tick: usize, name: &str) -> Vec<Span<'static>> {
    match name {
        "shimmer-ocean" => shimmer_tint_spans(text, tick, TINT_OCEAN[0], TINT_OCEAN[1]),
        "shimmer-ember" => shimmer_tint_spans(text, tick, TINT_EMBER[0], TINT_EMBER[1]),
        "shimmer-mint" => shimmer_tint_spans(text, tick, TINT_MINT[0], TINT_MINT[1]),
        "shimmer-pulse" => shimmer_pulse_spans(text, tick),
        _ => shimmer_spans(text, tick),
    }
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

/// One-shot finish wave for a tool head row: a green (ok) or red (failed)
/// band sweeps from the marker across the name over `progress` 0.0..1.0.
/// The base is exactly the static row's styles (marker ok/err, name accent),
/// so tool rows stay blue and only the wave carries green/red — no gray
/// phase, and the frozen end frame equals the normal render (no pop).
/// Caller gates on [`has_truecolor`]; without RGB there is no wave, just
/// the static row.
pub fn flash_spans(marker: &str, name: &str, done_ok: bool, progress: f64) -> Vec<Span<'static>> {
    const BAND: f64 = 3.0;
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
        // settled: exactly the static row's styles, no pop at either end
        let settled = if part {
            if done_ok {
                Theme::ok()
            } else {
                Theme::err()
            }
        } else {
            Theme::accent()
        };
        let style = if center <= frontier && center > frontier - BAND {
            Style::default()
                .fg(Color::Rgb(target.0, target.1, target.2))
                .add_modifier(Modifier::BOLD)
        } else {
            settled
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
    fn tint_wave_travels_between_colors() {
        // env-independent: the rgb core directly, past the truecolor gate
        let base = TINT_OCEAN[0];
        let at0 = shimmer_rgb_tint("Working", 0, base, TINT_OCEAN[1]);
        assert!(
            at0.iter()
                .all(|s| s.style.fg == Some(Color::Rgb(base.0, base.1, base.2))),
            "band starts off the text: all base"
        );
        let mid = shimmer_rgb_tint("Working", SHIMMER_PERIOD_TICKS / 4, base, TINT_OCEAN[1]);
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
    fn pulse_breathes_uniformly() {
        // env-independent core: every char shares one phase
        let dim = shimmer_pulse_rgb("Working", 0.0);
        assert!(
            dim.iter()
                .all(|s| s.style.fg == Some(Color::Rgb(128, 128, 128))),
            "t=0 is all base"
        );
        let crest = shimmer_pulse_rgb("Working", 1.0);
        assert!(
            crest
                .iter()
                .all(|s| s.style.fg == Some(Color::Rgb(255, 255, 255))),
            "t=1 is all crest"
        );
        assert!(
            shimmer_pulse_spans("Working", 3) == shimmer_pulse_spans("Working", 3),
            "same tick is deterministic"
        );
    }

    #[test]
    fn named_dispatch_covers_the_gallery() {
        // dispatch only: colors depend on the terminal, text must not
        for name in [
            "shimmer-live",
            "shimmer-ocean",
            "shimmer-ember",
            "shimmer-mint",
            "shimmer-pulse",
        ] {
            let text: String = shimmer_named("Working", 9, name)
                .iter()
                .map(|s| s.content.as_ref())
                .collect();
            assert_eq!(text, "Working", "{name} must keep the text");
        }
    }

    #[test]
    fn flash_rides_static_without_gray() {
        const GRAY: Option<Color> = Some(Color::Rgb(128, 128, 128));
        // start: exactly the static row (marker ok-style, blue name)
        let at0 = flash_spans("  ✓ ", "read", true, 0.0);
        let text: String = at0.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "  ✓ read");
        assert_eq!(at0[0].style, Theme::ok());
        assert_eq!(at0[4].style, Theme::accent());
        assert!(
            at0.iter().all(|s| s.style.fg != GRAY),
            "no gray phase at the start"
        );
        // mid-sweep: the green band travels over the blue name
        let mid = flash_spans("  ✓ ", "read", true, 0.5);
        assert!(
            mid.iter()
                .any(|s| s.style.fg == Some(Color::Rgb(63, 185, 80))),
            "green band must ride mid-sweep"
        );
        let mid_err = flash_spans("  ✗ ", "bash", false, 0.5);
        assert!(
            mid_err
                .iter()
                .any(|s| s.style.fg == Some(Color::Rgb(248, 81, 73))),
            "red band must ride mid-sweep"
        );
        assert!(
            mid.iter().all(|s| s.style.fg != GRAY)
                && mid_err.iter().all(|s| s.style.fg != GRAY),
            "no gray phase mid-sweep either"
        );
        // end: settled back to the static row, no pop
        let end = flash_spans("  ✓ ", "read", true, 1.0);
        assert_eq!(end[0].style, Theme::ok());
        assert_eq!(end[4].style, Theme::accent());
        let end_err = flash_spans("  ✗ ", "bash", false, 1.0);
        assert_eq!(end_err[0].style, Theme::err());
    }
}
