use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;

use super::theme::Theme;

/// Terminal markdown renderer with syntax-highlighted code blocks.
pub struct Highlighter {
    ps: SyntaxSet,
    ts: ThemeSet,
    theme_name: &'static str,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            ps: SyntaxSet::load_defaults_newlines(),
            ts: ThemeSet::load_defaults(),
            theme_name: "base16-eighties.dark",
        }
    }

    fn theme(&self) -> &syntect::highlighting::Theme {
        &self.ts.themes[self.theme_name]
    }

    /// Highlight a code block; falls back to plain text for unknown languages.
    pub fn highlight_code(&self, code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
        let syntax = lang
            .and_then(|l| self.ps.find_syntax_by_token(l))
            .unwrap_or_else(|| self.ps.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, self.theme());
        let mut out = Vec::new();
        for line in syntect::util::LinesWithEndings::from(code) {
            let Ok(regions) = hl.highlight_line(line, &self.ps) else {
                continue;
            };
            let mut spans: Vec<Span> = Vec::new();
            for (style, chunk) in regions {
                if chunk.is_empty() {
                    continue;
                }
                let mut s = Style::new()
                    .fg(Color::Rgb(
                        style.foreground.r,
                        style.foreground.g,
                        style.foreground.b,
                    ))
                    .bg(Theme::SURFACE());
                if style.font_style.contains(FontStyle::BOLD) {
                    s = s.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    s = s.add_modifier(Modifier::ITALIC);
                }
                if style.font_style.contains(FontStyle::UNDERLINE) {
                    s = s.add_modifier(Modifier::UNDERLINED);
                }
                spans.push(Span::styled(chunk.trim_end_matches('\n').to_string(), s));
            }
            if spans.is_empty() {
                spans.push(Span::styled(String::new(), base_style()));
            }
            out.push(Line::from(spans));
        }
        if out.is_empty() {
            out.push(Line::from(vec![Span::styled(String::new(), base_style())]));
        }
        out
    }
}

fn base_style() -> Style {
    Theme::base()
}

fn code_style() -> Style {
    Style::new().fg(Theme::ACCENT_SOFT()).bg(Theme::SURFACE())
}

/// Render markdown text into styled lines. Width is used only by tables.
pub fn render(text: &str, width: u16, hl: &Highlighter) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut lines = text.lines().peekable();
    let mut in_code = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();

    while let Some(raw) = lines.next() {
        let trimmed_start = raw.trim_start();

        // fenced code blocks
        if trimmed_start.starts_with("```") {
            if in_code {
                emit_code(&mut out, &code_buf.clone(), code_lang.as_deref(), hl, width);
                code_buf.clear();
                code_lang = None;
                in_code = false;
            } else {
                in_code = true;
                code_lang = Some(
                    trimmed_start[3..]
                        .trim()
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string(),
                );
            }
            continue;
        }
        if in_code {
            code_buf.push_str(raw);
            code_buf.push('\n');
            continue;
        }

        if trimmed_start.is_empty() {
            if matches!(out.last(), Some(l) if !l.spans.is_empty()) {
                out.push(Line::from(vec![Span::styled(String::new(), base_style())]));
            }
            continue;
        }

        // heading
        if let Some(rest) = try_heading(trimmed_start) {
            out.push(Line::from(rest));
            continue;
        }

        // horizontal rule
        if trimmed_start.len() >= 3
            && (trimmed_start.chars().all(|c| c == '-')
                || trimmed_start.chars().all(|c| c == '*')
                || trimmed_start.chars().all(|c| c == '_'))
        {
            let w = width.min(60) as usize;
            out.push(Line::from(vec![Span::styled(
                "─".repeat(w),
                Style::new().fg(Theme::rule_color()).bg(Theme::BG()),
            )]));
            continue;
        }

        // table block
        if trimmed_start.starts_with('|') && lines.peek().is_some_and(|l| is_table_sep(l)) {
            let mut rows = vec![parse_table_row(trimmed_start)];
            while let Some(l) = lines.peek() {
                if l.trim_start().starts_with('|') {
                    let raw = lines.next().unwrap();
                    if !is_table_sep(raw) {
                        rows.push(parse_table_row(raw));
                    }
                } else {
                    break;
                }
            }
            emit_table(&mut out, rows, width);
            continue;
        }

        // blockquote
        if let Some(rest) = trimmed_start.strip_prefix('>') {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            let mut spans = vec![Span::styled("▐ ".to_string(), Theme::accent())];
            spans.extend(inline(rest, Theme::dim().add_modifier(Modifier::ITALIC)));
            out.push(Line::from(spans));
            continue;
        }

        // list item
        if let Some((marker, rest)) = try_list(trimmed_start) {
            let mut spans = vec![
                Span::styled("  ".to_string(), base_style()),
                Span::styled(format!("{marker} "), Theme::accent()),
            ];
            spans.extend(inline(rest, base_style()));
            out.push(Line::from(spans));
            continue;
        }

        out.push(Line::from(inline(trimmed_start, base_style())));
    }

    // unclosed fence during streaming — render what we have
    if in_code {
        emit_code(&mut out, &code_buf, code_lang.as_deref(), hl, width);
    }
    out
}

fn is_table_sep(l: &str) -> bool {
    let t = l.trim();
    t.starts_with('|')
        && t.contains('-')
        && t.chars()
            .all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
}

fn parse_table_row(l: &str) -> Vec<String> {
    l.trim()
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

fn emit_table(out: &mut Vec<Line<'static>>, rows: Vec<Vec<String>>, width: u16) {
    if rows.is_empty() {
        return;
    }
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return;
    }
    let avail = width.saturating_sub(ncols as u16 + 1).max(4);
    let mut widths: Vec<usize> = (0..ncols)
        .map(|i| {
            rows.iter()
                .map(|r| r.get(i).map(|c| c.chars().count()).unwrap_or(0))
                .max()
                .unwrap_or(0)
                .max(3)
        })
        .collect();
    // shrink to fit
    loop {
        let total: usize = widths.iter().sum::<usize>() + widths.len();
        let max_total = avail as usize * 1;
        if total <= max_total {
            break;
        }
        let (mi, _) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > 3)
            .max_by_key(|(_, w)| **w)
            .unwrap_or((usize::MAX, &0));
        if mi == usize::MAX {
            break;
        }
        widths[mi] -= 1;
    }
    let w = |i: usize| widths.get(i).copied().unwrap_or(0);
    let border = |out: &mut Vec<Line<'static>>, lft: &str, mid: &str, rgt: &str| {
        let mut spans = Vec::new();
        let bs = Style::new().fg(Theme::rule_color()).bg(Theme::BG());
        spans.push(Span::styled(lft.to_string(), bs));
        for i in 0..ncols {
            if i > 0 {
                spans.push(Span::styled(mid.to_string(), bs));
            }
            spans.push(Span::styled("─".repeat(w(i)), bs));
        }
        spans.push(Span::styled(rgt.to_string(), bs));
        out.push(Line::from(spans));
    };
    // wrap a cell's inline markdown into physical rows of exactly `wi` columns
    fn cell_rows(c: &str, wi: usize, st: Style) -> Vec<Vec<Span<'static>>> {
        let budget = wi.saturating_sub(2).max(1); // one padding space each side
        let styled = inline(c, st);
        let mut chars: Vec<(Style, char)> = Vec::new();
        for s in &styled {
            for ch in s.content.chars() {
                chars.push((s.style, ch));
            }
        }
        // greedy word wrap
        let mut lines: Vec<Vec<(Style, char)>> = Vec::new();
        let mut cur: Vec<(Style, char)> = Vec::new();
        let mut last_space: Option<usize> = None;
        for (s, ch) in chars {
            if cur.len() >= budget && !cur.is_empty() {
                match last_space {
                    Some(sp) => {
                        let rest = cur.split_off(sp + 1);
                        trim_end_spaces(&mut cur);
                        lines.push(std::mem::take(&mut cur));
                        cur = rest;
                    }
                    None => {
                        lines.push(std::mem::take(&mut cur));
                    }
                }
                last_space = None;
            }
            if ch == ' ' {
                last_space = Some(cur.len());
            }
            cur.push((s, ch));
        }
        trim_end_spaces(&mut cur);
        lines.push(cur);

        let lines_out = lines
            .into_iter()
            .map(|v| {
                let used = 1 + v.len();
                let mut line = cells_to_line(v, st);
                let mut spans: Vec<Span> = vec![Span::styled(" ".to_string(), st)];
                spans.append(&mut line.spans);
                spans.push(Span::styled(" ".repeat(wi.saturating_sub(used)), st));
                spans
            })
            .collect::<Vec<_>>();
        if lines_out.is_empty() {
            vec![vec![Span::styled(" ".repeat(wi), st)]]
        } else {
            lines_out
        }
    }
    let bar = || {
        Span::styled(
            "│".to_string(),
            Style::new().fg(Theme::rule_color()).bg(Theme::BG()),
        )
    };
    // a physical table row may span several lines when cells wrap
    let render_row = |r: &[String], header: bool| -> Vec<Line<'static>> {
        let st = if header {
            Style::new()
                .fg(Theme::ACCENT())
                .bg(Theme::SURFACE())
                .add_modifier(Modifier::BOLD)
        } else {
            base_style()
        };
        let cols: Vec<Vec<Vec<Span<'static>>>> = (0..ncols)
            .map(|i| cell_rows(r.get(i).map(|s| s.as_str()).unwrap_or(""), w(i), st))
            .collect();
        // drop the synthetic blank row from every full-height column
        let h = cols.iter().map(|c| c.len()).max().unwrap_or(1);
        (0..h)
            .map(|li| {
                let mut spans = vec![bar()];
                for i in 0..ncols {
                    match cols[i].get(li) {
                        Some(cell) => spans.extend(cell.iter().cloned()),
                        None => spans.push(Span::styled(" ".repeat(w(i)), st)),
                    }
                    spans.push(bar());
                }
                Line::from(spans)
            })
            .collect()
    };

    border(out, "┌", "┬", "┐");
    out.extend(render_row(&rows[0], true));
    border(out, "├", "┼", "┤");
    for (ri, r) in rows.iter().skip(1).enumerate() {
        if ri > 0 {
            // separator between logical data rows
            border(out, "├", "┼", "┤");
        }
        out.extend(render_row(r, false));
    }
    border(out, "└", "┴", "┘");
}

fn emit_code(
    out: &mut Vec<Line<'static>>,
    code: &str,
    lang: Option<&str>,
    hl: &Highlighter,
    width: u16,
) {
    // highlight first to measure, then frame the block in a rounded outline
    let mut lines: Vec<Line<'static>> = Vec::new();
    let padded: String = code
        .lines()
        .map(|l| format!(" {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    for line in hl.highlight_code(&padded, lang) {
        let mut spans = Vec::with_capacity(line.spans.len() + 1);
        spans.push(Span::styled(" ".to_string(), surface_pad()));
        for s in line.spans {
            let st = s.style.patch(Style::new().bg(Theme::SURFACE()));
            spans.push(Span::styled(s.content.to_string(), st));
        }
        lines.push(Line::from(spans));
    }
    let max_w = (width.saturating_sub(4) as usize).max(12);
    let iw = lines
        .iter()
        .map(|l| line_text_pub(l).chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, max_w);
    let b = Style::new().fg(Theme::ACCENT_SOFT());

    // top border with the language embedded: ╭─ rust ────╮
    let lang_txt = match lang {
        Some(l) if !l.is_empty() => format!("─ {l} "),
        _ => String::new(),
    };
    let rest = (iw + 2).saturating_sub(lang_txt.chars().count());
    out.push(Line::from(vec![
        Span::styled("╭".to_string(), b),
        Span::styled(
            lang_txt,
            Style::new()
                .fg(Theme::ACCENT())
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{}╮", "─".repeat(rest)), b),
    ]));
    for l in lines {
        let t = line_text_pub(&l);
        let pad = " ".repeat(iw.saturating_sub(t.chars().count()));
        let mut spans = vec![Span::styled("│ ".to_string(), b)];
        spans.extend(l.spans);
        spans.push(Span::styled(format!("{pad} │"), b));
        out.push(Line::from(spans));
    }
    out.push(Line::from(vec![
        Span::styled("╰".to_string(), b),
        Span::styled(format!("{}╯", "─".repeat(iw + 2)), b),
    ]));
}

fn line_text_pub(l: &Line<'_>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn surface_pad() -> Style {
    Style::new().fg(Theme::SURFACE()).bg(Theme::SURFACE())
}

fn try_heading(s: &str) -> Option<Vec<Span<'static>>> {
    let level = s.bytes().take_while(|b| *b == b'#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = s[level..].strip_prefix(' ').unwrap_or(&s[level..]);
    let style = match level {
        1..=2 => Theme::accent_bold(),
        3..=4 => Style::new()
            .fg(Theme::FG())
            .bg(Theme::BG())
            .add_modifier(Modifier::BOLD),
        _ => Style::new()
            .fg(Theme::DIM())
            .bg(Theme::BG())
            .add_modifier(Modifier::BOLD),
    };
    let mut spans = inline(rest, style);
    spans.insert(0, Span::styled("# ".to_string(), Theme::dim()));
    Some(spans)
}

fn try_list(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    if bytes.first() == Some(&b'-') || bytes.first() == Some(&b'*') || bytes.first() == Some(&b'+')
    {
        let rest = &s[1..];
        return rest.starts_with(' ').then(|| ("-".to_string(), &rest[1..]));
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after = &s[digits.len()..];
        if after.starts_with('.') || after.starts_with(')') {
            let rest = after[1..].strip_prefix(' ').unwrap_or(&after[1..]);
            return Some((format!("{digits}.", digits = digits), rest));
        }
    }
    None
}

/// Inline markdown: **bold**, *italic*, `code` (minimal recursive scanner).
pub fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    push_inline(text, base, &mut out);
    out
}

fn push_inline(text: &str, style: Style, out: &mut Vec<Span<'static>>) {
    const MARKERS: [&str; 5] = ["**", "__", "`", "*", "_"];
    let mut rest = text;
    'outer: while !rest.is_empty() {
        let mut best: Option<(usize, &str)> = None;
        for m in MARKERS {
            if let Some(pos) = rest.find(m) {
                best = Some(match best {
                    // on equal position prefer the longer marker (** over *)
                    Some((bp, bm)) if bp < pos || (bp == pos && bm.len() >= m.len()) => (bp, bm),
                    _ => (pos, m),
                });
            }
        }
        let Some((pos, marker)) = best else { break };
        if pos > 0 {
            out.push(Span::styled(rest[..pos].to_string(), style));
            rest = &rest[pos..];
        }
        // find closing marker
        let close_from = marker.len();
        let close = rest[close_from..].find(marker).map(|i| i + close_from);
        let inner_end = match close {
            Some(i) => i,
            None => {
                out.push(Span::styled(marker.to_string(), style));
                rest = &rest[marker.len()..];
                continue 'outer;
            }
        };
        if inner_end == close_from {
            // empty markers like `` — literal
            out.push(Span::styled(marker.to_string(), style));
            rest = &rest[marker.len()..];
            continue 'outer;
        }
        let inner = &rest[close_from..inner_end];
        let after = &rest[inner_end + marker.len()..];
        match marker {
            "`" => {
                out.push(Span::styled(inner.to_string(), code_style()));
            }
            "**" | "__" => push_inline(inner, style.add_modifier(Modifier::BOLD), out),
            "*" | "_" => push_inline(inner, style.add_modifier(Modifier::ITALIC), out),
            _ => unreachable!(),
        }
        rest = after;
    }
    if !rest.is_empty() {
        out.push(Span::styled(rest.to_string(), style));
    }
}

/// Greedy word wrap preserving span styles and carrying a per-source-line tag
/// through to every visual row it produces.
pub fn wrap_tagged(
    lines: Vec<(Line<'static>, Option<usize>)>,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    const MIN_W: usize = 10;
    let width = (width as usize).max(MIN_W);
    let fallback = Style::new().fg(Theme::rule_color()).bg(Theme::BG());

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut tags: Vec<Option<usize>> = Vec::new();

    for (line, tag) in lines {
        let mut cells: Vec<(Style, char)> = Vec::new();
        for span in &line.spans {
            for c in span.content.chars() {
                cells.push((span.style, c));
            }
        }
        if cells.is_empty() {
            rows.push(Line::from(vec![Span::styled(String::new(), Theme::base())]));
            tags.push(tag);
            continue;
        }

        let mut cur: Vec<(Style, char)> = Vec::new();
        let mut last_space: Option<usize> = None;
        for (st, ch) in cells {
            if ch == '\n' {
                continue;
            }
            if cur.len() >= width {
                match last_space {
                    Some(sp) => {
                        let rest = cur.split_off(sp + 1);
                        trim_end_spaces(&mut cur);
                        rows.push(cells_to_line(std::mem::take(&mut cur), fallback));
                        tags.push(tag);
                        cur = rest;
                        last_space = None;
                    }
                    None => {
                        rows.push(cells_to_line(std::mem::take(&mut cur), fallback));
                        tags.push(tag);
                    }
                }
            }
            if ch == ' ' {
                last_space = Some(cur.len());
            }
            cur.push((st, ch));
        }
        trim_end_spaces(&mut cur);
        rows.push(cells_to_line(cur, fallback));
        tags.push(tag);
    }
    (rows, tags)
}

fn trim_end_spaces(cells: &mut Vec<(Style, char)>) {
    while matches!(cells.last(), Some((_, ' '))) {
        cells.pop();
    }
}

fn cells_to_line(mut cells: Vec<(Style, char)>, fallback: Style) -> Line<'static> {
    if cells.is_empty() {
        return Line::from(vec![Span::styled(String::new(), fallback)]);
    }
    let mut spans: Vec<Span> = Vec::new();
    let mut buf = String::new();
    let mut prev_style: Option<Style> = None;
    for (st, ch) in std::mem::take(&mut cells) {
        if prev_style.is_some_and(|p| p == st) {
            buf.push(ch);
        } else {
            if let Some(p) = prev_style.take() {
                spans.push(Span::styled(std::mem::take(&mut buf), p));
            }
            buf.push(ch);
            prev_style = Some(st);
        }
    }
    if !buf.is_empty() || spans.is_empty() {
        spans.push(Span::styled(buf, prev_style.unwrap_or(fallback)));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_block_gets_label_and_lines() {
        let hl = Highlighter::new();
        let lines = render("```rust\nfn main() {}\n```\ntext", 80, &hl);
        assert!(lines.len() >= 3);
        let label: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(label.contains("rust"), "label was {label:?}");
        let body: String = lines[1]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(body.contains("fn main"), "{body:?}");
    }

    #[test]
    fn inline_styles() {
        let spans = inline("**bold** and `code`", Theme::base());
        let all: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(all, "bold and code");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wrap_carries_tags() {
        let line = Line::from(Span::styled("a ".repeat(50), Theme::base()));
        let (rows, tags) = wrap_tagged(vec![(line, Some(7))], 20);
        assert!(rows.len() > 1);
        assert!(tags.iter().all(|t| *t == Some(7)));
    }

    #[test]
    fn table_renders_grid() {
        let hl = Highlighter::new();
        let md = "| a | b |\n|---|---|\n| 1 | 2 |\n";
        let lines = render(md, 40, &hl);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(joined.iter().any(|r| r.contains('┌')), "{joined:?}");
        assert!(joined.iter().any(|r| r.contains(" a ")), "{joined:?}");
    }

    #[test]
    fn bold_cyrillic_parses() {
        let spans = inline("**Управление памятью**", Theme::base());
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "Управление памятью");
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn table_cells_render_inline_markdown() {
        let hl = Highlighter::new();
        let md = "| a | b |\n|---|---|\n| **x** | y |\n";
        let lines = render(md, 40, &hl);
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("");
        assert!(!all.contains("**"), "{all:?}");
        assert!(all.contains('x'));
    }

    #[test]
    fn table_wraps_long_cells_instead_of_truncating() {
        let hl = Highlighter::new();
        let md = "| col | desc |\n|-----|------|\n| k | alpha beta gamma delta epsilon |\n";
        let lines = render(md, 30, &hl);
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(
            !all.contains('…'),
            "cell text must wrap, not truncate: {all:?}"
        );
        assert!(all.contains("epsilon"), "tail lost: {all:?}");
        // every rendered row stays within the requested width
        for l in &lines {
            let wdt: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(wdt <= 30, "row too wide ({wdt}): {l:?}");
        }
    }

    #[test]
    fn code_box_borders_align() {
        let hl = Highlighter::new();
        let lines = render("```rust\nfn a() {}\nlet x = 12345;\n```\n", 60, &hl);
        let widths: Vec<usize> = lines
            .iter()
            .take(5)
            .map(|l| l.spans.iter().map(|s| s.content.chars().count()).sum())
            .collect();
        assert!(widths.iter().all(|&w| w == widths[0]), "{widths:?}");
    }
}
