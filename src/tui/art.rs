//! Logo art gallery for `/test art`: numbered static wordmark variants so
//! the empty-state mark can be picked live in the TUI instead of from
//! chat screenshots. All variants are plain Lines (no animation, no
//! data) — the generic menu path renders them, selection fill included.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::tui::theme::Theme;

/// The big sqwai mark (rounded square with the S-slash) as block art,
/// embedded from assets at compile time. White, plain, no animation.
pub const MARK_BIG: &str = include_str!("../../assets/ascii-mark.txt");

/// Big-mark rows as white Lines for the empty state.
pub fn mark_big_lines() -> Vec<Line<'static>> {
    MARK_BIG
        .lines()
        .map(|l| Line::from(Span::styled(l.to_string(), Style::new().fg(Color::White))))
        .collect()
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
}
