//! Logo art gallery for `/test art`: numbered static wordmark variants so
//! the empty-state mark can be picked live in the TUI instead of from
//! chat screenshots. All variants are plain Lines (no animation, no
//! data) — the generic menu path renders them, selection fill included.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::theme::Theme;

/// Pixel wordmark source (block font with ░ anti-aliased edges),
/// embedded from assets at compile time. Uniform rows (verified below).
pub const SQWAI_TEXT: &str = include_str!("../../assets/ascii-sqwai.txt");

/// Compact wordmark for narrow terminals (quadrant-block font, 4 rows).
/// Source rows may be ragged on the right (editors eat trailing spaces);
/// `gradient_block` normalizes, so the file can never break the render.
pub const SQWAI_COMPACT_TEXT: &str = include_str!("../../assets/ascii-sqwai-compact.txt");

/// Coral ramp ends, pipetted off the logo PNG (matches the SVG stops):
/// top-left `#ff9a5c` → bottom-right `#f43f5e`.
pub const CORAL_START: (u8, u8, u8) = (255, 154, 92);
pub const CORAL_END: (u8, u8, u8) = (244, 63, 94);
/// Muted middle of the ramp, for H2 and other quiet brand ink.
pub const CORAL_MUTED: (u8, u8, u8) = (250, 108, 93);
/// Solid warm fallback where truecolor is unavailable (salmon-ish).
const CORAL_FALLBACK: Color = Color::Indexed(209);

/// Quiet brand ink: muted coral bold, the H2 look. Menu titles and day
/// sections share it so headers read as one family across surfaces.
pub fn h2_style() -> Style {
    if crate::tui::shimmer::has_truecolor() {
        let (r, g, b) = CORAL_MUTED;
        Style::new()
            .fg(Color::Rgb(r, g, b))
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(CORAL_FALLBACK).add_modifier(Modifier::BOLD)
    }
}

/// Ramp position of column `x` across `width`: 0.0 at the left edge.
pub fn coral_at(x: usize, width: usize) -> (u8, u8, u8) {
    let t = if width <= 1 {
        0.0
    } else {
        x.min(width - 1) as f64 / (width - 1) as f64
    };
    crate::tui::shimmer::blend(CORAL_START, CORAL_END, t)
}

/// Gradient wordmark lines: one coral ramp shared across the whole block
/// (gemini-cli style) — every row uses its column's color, so the gradient
/// reads as one. Spaces stay unpainted; without truecolor the block falls
/// back to one solid warm color, never unstyled.
pub fn sqwai_gradient_lines() -> Vec<Line<'static>> {
    gradient_block(SQWAI_TEXT)
}

/// Same ramp over the compact wordmark for narrow terminals.
pub fn sqwai_compact_lines() -> Vec<Line<'static>> {
    gradient_block(SQWAI_COMPACT_TEXT)
}

/// One coral ramp shared across a whole text block. Source rows are
/// right-normalized first (trim + pad to the widest), so ragged asset
/// lines — editors love eating trailing spaces — can never skew the ramp
/// or the layout.
fn gradient_block(text: &str) -> Vec<Line<'static>> {
    let trimmed: Vec<String> = text.lines().map(|l| l.trim_end().to_string()).collect();
    let width = trimmed.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    let rows: Vec<&str> = trimmed.iter().map(String::as_str).collect();
    let truecolor = crate::tui::shimmer::has_truecolor();
    rows.into_iter()
        .map(|row| {
            // runs of one style compress into a single span; spaces break
            // runs and stay unpainted so the background shows through
            let mut spans = Vec::new();
            let mut buf = String::new();
            let mut buf_style: Option<Style> = None;
            let mut chars = row.chars().peekable();
            let mut x = 0usize;
            while let Some(ch) = chars.next() {
                if ch == ' ' && chars.peek().is_none() {
                    // padding is layout, not content: stop before it so the
                    // ramp never paints trailing filler
                    break;
                }
                if ch == ' ' {
                    push_span(&mut spans, &mut buf, &mut buf_style);
                    spans.push(Span::styled(" ".to_string(), Theme::base()));
                    x += 1;
                    continue;
                }
                let style = if truecolor {
                    let (r, g, b) = coral_at(x, width);
                    Style::new().fg(Color::Rgb(r, g, b))
                } else {
                    Style::new().fg(CORAL_FALLBACK)
                };
                if buf_style.as_ref() != Some(&style) {
                    push_span(&mut spans, &mut buf, &mut buf_style);
                    buf_style = Some(style);
                }
                buf.push(ch);
                x += 1;
            }
            // pad the visual row back to the block width with plain spaces
            let used: usize = spans
                .iter()
                .map(|s| s.content.chars().count())
                .sum::<usize>()
                + buf.chars().count();
            push_span(&mut spans, &mut buf, &mut buf_style);
            for _ in used..width {
                spans.push(Span::styled(" ".to_string(), Theme::base()));
            }
            Line::from(spans)
        })
        .collect()
}

fn push_span(spans: &mut Vec<Span>, buf: &mut String, buf_style: &mut Option<Style>) {
    if !buf.is_empty() {
        spans.push(Span::styled(
            std::mem::take(buf),
            buf_style.take().unwrap_or_else(Theme::base),
        ));
    }
}

/// Repaint a per-character coral ramp over spans (for H1), preserving
/// every other modifier. One span per char — headings are short, and the
/// ramp moves every column. Without truecolor the run falls back to one
/// solid warm color instead of garbage escapes.
pub fn gradient_spans(spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    use crate::tui::shimmer::has_truecolor;
    let truecolor = has_truecolor();
    let total: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let mut x = 0usize;
    let mut out = Vec::with_capacity(total);
    for s in spans {
        for ch in s.content.chars() {
            let mut style = s.style;
            style.fg = Some(if truecolor {
                let (r, g, b) = coral_at(x, total);
                Color::Rgb(r, g, b)
            } else {
                CORAL_FALLBACK
            });
            out.push(Span::styled(ch.to_string(), style));
            x += 1;
        }
    }
    out
}

/// 5-wide × 5-tall block capitals, one space gap added when joining.
/// Squat on purpose: tall thin letters read stretched on terminal cells.
/// Every row is exactly 5 display columns (plain ASCII blocks).
const S: &[&str] = &[" ████", "█    ", " ███ ", "    █", " ███ "];
const Q: &[&str] = &[" ███ ", "█   █", "█   █", "█ █ █", " ██ █"];
const W: &[&str] = &["█   █", "█   █", "█ █ █", "██ ██", "█   █"];
const A: &[&str] = &[" ███ ", "█   █", "█████", "█   █", "█   █"];
const I: &[&str] = &["█████", "  █  ", "  █  ", "  █  ", "█████"];

/// 3-wide × 5-tall mini capitals for the small variant.
const S3: &[&str] = &[" ██", "█  ", " █ ", "  █", "██ "];
const Q3: &[&str] = &[" ██", "█ █", "█ █", "█ █", " ██"];
const W3: &[&str] = &["█ █", "█ █", "█ █", "███", "█ █"];
const A3: &[&str] = &[" █ ", "█ █", "███", "█ █", "█ █"];
const I3: &[&str] = &["███", " █ ", " █ ", " █ ", "███"];

fn sty(fg: Color) -> Style {
    Style::new().fg(fg)
}

fn block_word(glyphs: &[&[&str]], colors: &[Style]) -> Vec<Line<'static>> {
    let rows = glyphs.first().map(|g| g.len()).unwrap_or(0);
    (0..rows)
        .map(|r| {
            let mut spans = vec![Span::styled(" ".to_string(), Theme::base())];
            for (g, s) in glyphs.iter().zip(colors.iter()) {
                spans.push(Span::styled(" ".to_string(), Theme::base()));
                spans.push(Span::styled(g[r].to_string(), *s));
            }
            Line::from(spans)
        })
        .collect()
}

fn text_line(text: &str, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ".to_string(), Theme::base()),
        Span::styled(text.to_string(), style),
    ])
}

/// (name, lines): gallery order is the numbering the user picks from.
pub fn variants() -> Vec<(&'static str, Vec<Line<'static>>)> {
    let dim = Theme::dim();
    let base = Theme::base();
    let white_bold = Style::new().fg(Color::White).add_modifier(Modifier::BOLD);
    let cyan_bold = Theme::accent_bold();
    let gray = sty(Color::Gray);
    let dark = sty(Color::DarkGray);
    let white = sty(Color::White);
    let cyan = sty(Color::Cyan);
    let light_cyan = sty(Color::LightCyan);

    // opencode-style gray ramp, per letter S Q W A I
    let ramp = [dark, dark, gray, gray, white];
    // cyan ramp ending bright
    let cyan_ramp = [cyan, cyan, light_cyan, light_cyan, white];

    let mut out = vec![
        ("plain dim word", vec![text_line("sqwai", dim)]),
        ("spaced word", vec![text_line("s q w a i", base)]),
        (
            "spaced, accent tail",
            vec![Line::from(vec![
                Span::styled(" ".to_string(), base),
                Span::styled("s q w a ".to_string(), dim),
                Span::styled("i".to_string(), cyan_bold),
            ])],
        ),
        ("upper flat white", vec![text_line("SQWAI", white_bold)]),
        ("block dim", block_word(&[S, Q, W, A, I], &[dark; 5])),
        (
            "block gray ramp (opencode style)",
            block_word(&[S, Q, W, A, I], &ramp),
        ),
        ("block cyan ramp", block_word(&[S, Q, W, A, I], &cyan_ramp)),
        ("mini caps", block_word(&[S3, Q3, W3, A3, I3], &[gray; 5])),
    ];

    // agy-style pixel triangle + wordmark under it
    let tri = [cyan, cyan, light_cyan, white];
    let mut triangle = vec![text_line("triangle mark", dim)];
    let rows = ["   █   ", "  ███  ", " █████ ", "███████"];
    for (r, row) in rows.iter().enumerate() {
        triangle.push(Line::from(vec![
            Span::styled(" ".to_string(), base),
            Span::styled((*row).to_string(), tri[r]),
        ]));
    }
    triangle.push(text_line("sqwai", gray));
    out.push(("triangle mark + word", triangle));

    // per-character gray ramp on the plain word
    let chars = ["s", "q", "w", "a", "i"];
    let char_colors = [dark, dark, gray, gray, white];
    let mut spans = vec![Span::styled(" ".to_string(), base)];
    for (ch, st) in chars.iter().zip(char_colors.iter()) {
        spans.push(Span::styled((*ch).to_string(), *st));
    }
    out.push(("word gray ramp", vec![Line::from(spans)]));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_rows_share_width() {
        use unicode_width::UnicodeWidthStr;
        // a ragged row would shift every column after it in the gallery
        for g in [S, Q, W, A, I, S3, Q3, W3, A3, I3] {
            let w = g[0].width();
            assert!(g.iter().all(|r| r.width() == w), "{g:?}");
        }
        assert_eq!(variants().len(), 10);
    }

    #[test]
    fn gradient_ramp_spans_the_brand_coral() {
        assert_eq!(coral_at(0, 65), CORAL_START);
        assert_eq!(coral_at(64, 65), CORAL_END);
        let mid = coral_at(32, 65);
        assert!(
            mid.0 > CORAL_END.0 && mid.0 < CORAL_START.0,
            "midpoint interpolates: {mid:?}"
        );
        assert_eq!(coral_at(0, 1), CORAL_START, "degenerate width");
    }

    #[test]
    fn gradient_lines_preserve_text() {
        use unicode_width::UnicodeWidthStr;
        let rows: Vec<String> = SQWAI_TEXT
            .lines()
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(rows.len(), 8, "pixel wordmark is 8 rows");
        let width = rows.iter().map(|r| r.width()).max().unwrap_or(0);
        assert_eq!(width, 67, "banner width");
        // ragged source rows (editors eat trailing spaces) normalize to
        // the block width: content kept, padded after, ramp unskewed
        let lines = sqwai_gradient_lines();
        assert_eq!(lines.len(), 8);
        for (line, src) in lines.iter().zip(&rows) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with(src.as_str()),
                "no char lost or added: {text:?}"
            );
            assert_eq!(text.width(), width, "every row exact-fit: {text:?}");
        }
        // spaces stay unpainted so the background shows through
        let first = &lines[0].spans;
        assert!(
            first
                .iter()
                .any(|s| s.content.contains(' ') && s.style.fg.is_none()),
            "gaps unpainted: {first:?}"
        );
    }

    #[test]
    fn compact_wordmark_fits_narrow() {
        use unicode_width::UnicodeWidthStr;
        let rows: Vec<String> = SQWAI_COMPACT_TEXT
            .lines()
            .map(|r| r.trim_end().to_string())
            .collect();
        assert_eq!(rows.len(), 4, "compact wordmark is 4 rows");
        let width = rows.iter().map(|r| r.width()).max().unwrap_or(0);
        assert_eq!(width, 27, "compact fits narrow terminals");
        // ragged source rows normalize to the block width, never skewing
        // the ramp or dropping middle content
        let lines = sqwai_compact_lines();
        assert_eq!(lines.len(), 4);
        for (line, src) in lines.iter().zip(&rows) {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.starts_with(src.as_str()),
                "content kept, padded after: {text:?}"
            );
            assert_eq!(text.width(), width, "every row exact-fit: {text:?}");
        }
    }
}
