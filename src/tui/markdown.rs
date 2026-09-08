use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
            .and_then(|l| {
                self.ps
                    .find_syntax_by_token(l)
                    .or_else(|| self.ps.find_syntax_by_token(&l.to_ascii_lowercase()))
            })
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
                emit_code(&mut out, &code_buf, code_lang.as_deref(), hl, width);
                code_buf.clear();
                code_lang = None;
                in_code = false;
            } else {
                in_code = true;
                let label = trimmed_start
                    .strip_prefix("```")
                    .unwrap_or("")
                    .split_whitespace()
                    .next()
                    .unwrap_or("");
                // Generic fence labels add no useful context and make prose
                // blocks look like source code. Keep the language badge only
                // when the model supplied an actual language name.
                code_lang = match label.to_ascii_lowercase().as_str() {
                    "" | "text" | "txt" | "plain" | "plaintext" => None,
                    _ => Some(label.to_string()),
                };
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

        // horizontal rule (`---`, `***`, `___`, spaces allowed: `- - -`)
        if is_hr(trimmed_start) {
            let w = width.min(60) as usize;
            out.push(Line::from(vec![Span::styled(
                "─".repeat(w),
                Style::new().fg(Theme::rule_color()).bg(Theme::BG()),
            )]));
            continue;
        }

        // table block (GFM, outer pipes optional)
        if trimmed_start.contains('|') && lines.peek().is_some_and(|l| is_table_sep(l)) {
            let align = parse_table_align(lines.peek().unwrap());
            let mut rows = vec![parse_table_row(trimmed_start)];
            while let Some(l) = lines.peek() {
                if l.trim_start().contains('|') {
                    let raw = lines.next().unwrap();
                    if !is_table_sep(raw) {
                        rows.push(parse_table_row(raw));
                    }
                } else {
                    break;
                }
            }
            emit_table(&mut out, rows, align, width);
            continue;
        }

        // blockquote (supports nesting: `>> text`, `> > text`)
        if trimmed_start.starts_with('>') {
            let mut rest = trimmed_start;
            let mut level = 0usize;
            while let Some(after) = rest.strip_prefix('>') {
                level += 1;
                rest = after
                    .strip_prefix(' ')
                    .or_else(|| after.strip_prefix('\t'))
                    .unwrap_or(after);
            }
            let level = level.min(4);
            let rail = "▐ ".repeat(level);
            let mut spans = vec![Span::styled(rail, Theme::accent())];
            spans.extend(inline(rest, Theme::dim().add_modifier(Modifier::ITALIC)));
            out.push(Line::from(spans));
            continue;
        }

        // list item (nesting follows the leading indent, 2 spaces per level)
        if let Some((marker, rest)) = try_list(trimmed_start) {
            let indent_cols: usize = raw
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .map(|c| if c == '\t' { 4 } else { 1 })
                .sum();
            let nest = (indent_cols / 2).min(3);
            let mut spans = vec![
                Span::styled("  ".repeat(nest + 1), base_style()),
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

fn is_hr(s: &str) -> bool {
    let compact: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    compact.len() >= 3
        && (compact.iter().all(|c| *c == '-')
            || compact.iter().all(|c| *c == '*')
            || compact.iter().all(|c| *c == '_'))
}

fn is_table_sep(l: &str) -> bool {
    // Delimiter row: pipe-separated cells of `-`/`:` (`---`, `:--`, `--:`,
    // `:-:`). A pipe is required so setext headings (`foo\n---`) and plain
    // `---` rules never parse as tables; outer pipes stay optional per GFM.
    let t = l.trim();
    if !t.contains('|') || !t.contains('-') {
        return false;
    }
    let inner = t
        .strip_prefix('|')
        .unwrap_or(t)
        .strip_suffix('|')
        .unwrap_or(t.strip_prefix('|').unwrap_or(t));
    let cells: Vec<&str> = inner.split('|').map(str::trim).collect();
    if cells.is_empty() {
        return false;
    }
    cells
        .iter()
        .all(|c| !c.is_empty() && c.contains('-') && c.chars().all(|ch| ch == '-' || ch == ':'))
}

#[derive(Clone, Copy, PartialEq)]
enum Align {
    Left,
    Center,
    Right,
}

fn parse_table_align(sep: &str) -> Vec<Align> {
    let t = sep.trim();
    let inner = t
        .strip_prefix('|')
        .unwrap_or(t)
        .strip_suffix('|')
        .unwrap_or(t.strip_prefix('|').unwrap_or(t));
    inner
        .split('|')
        .map(|c| {
            let c = c.trim();
            let l = c.starts_with(':');
            let r = c.ends_with(':');
            match (l, r) {
                (true, true) => Align::Center,
                (false, true) => Align::Right,
                _ => Align::Left,
            }
        })
        .collect()
}

fn parse_table_row(l: &str) -> Vec<String> {
    // Split on unescaped pipes: `\|` is a literal pipe inside the cell.
    let t = l.trim();
    let mut cells: Vec<String> = Vec::new();
    let mut cur = String::new();
    // drop one optional outer pipe on each side (they delimit, not content)
    let mut body = t;
    if body.starts_with('|') {
        body = body[1..].trim_start();
    }
    if body.ends_with('|') && !body.ends_with("\\|") {
        body = body[..body.len() - 1].trim_end();
    }
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && matches!(chars.peek(), Some('|') | Some('\\')) {
            cur.push(chars.next().unwrap());
        } else if ch == '|' {
            cells.push(cur.trim().to_string());
            cur = String::new();
        } else {
            cur.push(ch);
        }
    }
    cells.push(cur.trim().to_string());
    cells
}

fn emit_table(out: &mut Vec<Line<'static>>, rows: Vec<Vec<String>>, align: Vec<Align>, width: u16) {
    if rows.is_empty() {
        return;
    }
    let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
    if ncols == 0 {
        return;
    }
    let avail = width.saturating_sub(ncols as u16 + 1).max(4);
    // Column widths are terminal columns, not characters: the border below is
    // drawn as `"─".repeat(w(i))`, and a CJK glyph occupies two cells while a
    // combining mark occupies none. Counting characters here made every table
    // with non-Latin text draw its right border inside the cell text.
    let mut widths: Vec<usize> = (0..ncols)
        .map(|i| {
            rows.iter()
                .map(|r| {
                    r.get(i)
                        .map(|c| UnicodeWidthStr::width(c.as_str()))
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0)
                .max(3)
        })
        .collect();
    // shrink to fit
    loop {
        let total: usize = widths.iter().sum::<usize>() + widths.len();
        let max_total = avail as usize;
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
    fn cell_rows(c: &str, wi: usize, st: Style, al: Align) -> Vec<Vec<Span<'static>>> {
        let budget = wi.saturating_sub(2).max(1); // one padding space each side
        let styled = inline(c, st);
        let mut chars: Vec<(Style, char)> = Vec::new();
        for s in &styled {
            for ch in s.content.chars() {
                chars.push((s.style, ch));
            }
        }
        // Take up to `budget` columns without splitting a zero-width mark
        // off its base char or a joiner off the sequence it glues.
        let take_budget = |cur: &mut Vec<(Style, char)>| -> Vec<(Style, char)> {
            let mut cols = 0;
            let mut split_idx = 0;
            let mut prev_zwj = false;
            for (i, (_, ch)) in cur.iter().enumerate() {
                let w = cell_width(*ch);
                let no_break_before = w == 0 || prev_zwj;
                if cols + w > budget && split_idx > 0 && !no_break_before {
                    break;
                }
                cols += w;
                split_idx = i + 1;
                prev_zwj = *ch == '\u{200d}';
            }
            let rest = cur.split_off(split_idx);
            std::mem::replace(cur, rest)
        };

        // greedy word wrap (running width is tracked incrementally: re-summing
        // the remainder after every split was quadratic on long cells)
        let mut lines: Vec<Vec<(Style, char)>> = Vec::new();
        let mut cur: Vec<(Style, char)> = Vec::new();
        // `budget` is a column count, so the running total has to be one too
        let mut cur_cols = 0usize;
        let mut last_space: Option<(usize, usize)> = None;
        for (s, ch) in chars {
            let ch_cols = cell_width(ch);
            // zero-width marks always join the current run, even past budget
            if ch_cols > 0 && cur_cols + ch_cols > budget && !cur.is_empty() {
                match last_space {
                    Some((sp, w_at_space)) => {
                        let rest = cur.split_off(sp + 1);
                        trim_end_spaces(&mut cur);
                        lines.push(std::mem::take(&mut cur));
                        cur = rest;
                        // remainder width = old width minus everything up to
                        // and including the consumed space (always 1 column)
                        cur_cols = cur_cols.saturating_sub(w_at_space + 1);
                    }
                    None => {
                        lines.push(take_budget(&mut cur));
                        cur_cols = columns_of(&cur);
                    }
                }
                while cur_cols > budget {
                    let taken = take_budget(&mut cur);
                    cur_cols = cur_cols.saturating_sub(columns_of(&taken));
                    lines.push(taken);
                }
                last_space = None;
            }
            if ch == ' ' {
                last_space = Some((cur.len(), cur_cols));
            }
            cur.push((s, ch));
            cur_cols += ch_cols;
        }
        trim_end_spaces(&mut cur);
        while columns_of(&cur) > budget {
            lines.push(take_budget(&mut cur));
        }
        if !cur.is_empty() || lines.is_empty() {
            lines.push(cur);
        }

        let lines_out = lines
            .into_iter()
            .map(|v| {
                let content_cols = columns_of(&v);
                // pad the content area (wi minus the two frame-adjacent
                // spaces) per the delimiter-row alignment
                let extra = wi.saturating_sub(2).saturating_sub(content_cols);
                let (lpad, rpad) = match al {
                    Align::Left => (0, extra),
                    Align::Right => (extra, 0),
                    Align::Center => (extra / 2, extra - extra / 2),
                };
                let mut line = cells_to_line(v, st);
                let mut spans: Vec<Span> = vec![Span::styled(" ".to_string(), st)];
                spans.push(Span::styled(" ".repeat(lpad), st));
                spans.append(&mut line.spans);
                spans.push(Span::styled(" ".repeat(rpad), st));
                spans.push(Span::styled(" ".to_string(), st));
                spans
            })
            .collect::<Vec<_>>();
        if lines_out.is_empty() {
            vec![vec![Span::styled(" ".repeat(wi), st)]]
        } else {
            lines_out
        }
    }
    let bar_with = |bg: ratatui::style::Color| {
        Span::styled("│".to_string(), Style::new().fg(Theme::rule_color()).bg(bg))
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
        // vertical rails share the row background so header rows don't checker
        let bar = bar_with(if header {
            Theme::SURFACE()
        } else {
            Theme::BG()
        });
        let cols: Vec<Vec<Vec<Span<'static>>>> = (0..ncols)
            .map(|i| {
                let al = align.get(i).copied().unwrap_or(Align::Left);
                cell_rows(r.get(i).map(|s| s.as_str()).unwrap_or(""), w(i), st, al)
            })
            .collect();
        // drop the synthetic blank row from every full-height column
        let h = cols.iter().map(|c| c.len()).max().unwrap_or(1);
        (0..h)
            .map(|li| {
                let mut spans = vec![bar.clone()];
                for (i, col) in cols.iter().enumerate() {
                    match col.get(li) {
                        Some(cell) => spans.extend(cell.iter().cloned()),
                        None => spans.push(Span::styled(" ".repeat(w(i)), st)),
                    }
                    spans.push(bar.clone());
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
    for line in hl.highlight_code(code, lang) {
        lines.push(surface_line(line));
    }
    // Degenerate narrow viewport: the frame alone needs 5 columns. Skip it
    // and emit plain (still wrapped) highlighted rows instead of overflowing.
    if width < 6 {
        let w = (width as usize).max(1);
        for l in lines.into_iter().flat_map(|line| wrap_code_line(line, w)) {
            out.push(l);
        }
        return;
    }
    // The code frame must never be wider than the chat viewport. Long source
    // lines are wrapped into additional framed rows below, rather than being
    // passed to the outer wrapper where the right border could be split away.
    let max_w = width.saturating_sub(4) as usize;

    // top border with the language embedded: ╭─ rust ────╮
    // If language text is longer than the content, expand iw so that
    // the top border, body lines, and bottom border all have identical width (iw + 4).
    let lang_txt = match lang {
        Some(l) if !l.is_empty() => {
            // Truncate language label if it would overflow max_w + 2
            let available = max_w.saturating_add(2);
            let mut s = format!("─ {l} ");
            if UnicodeWidthStr::width(s.as_str()) > available {
                let mut truncated = String::new();
                for ch in format!("─ {l}").chars() {
                    if UnicodeWidthStr::width(format!("{truncated}{ch}… ").as_str()) > available {
                        break;
                    }
                    truncated.push(ch);
                }
                s = format!("{truncated}… ");
            }
            s
        }
        _ => String::new(),
    };
    let lang_w = UnicodeWidthStr::width(lang_txt.as_str());
    let min_iw_for_lang = lang_w.saturating_sub(2);

    let iw = lines
        .iter()
        .map(|l| UnicodeWidthStr::width(line_text_pub(l).as_str()))
        .max()
        .unwrap_or(0)
        .max(min_iw_for_lang)
        .min(max_w)
        .max(1);
    // Code frame is intentionally a little quieter than the accent text.
    let b = Style::new().fg(Theme::code_border()).bg(Theme::BG());

    let rest = (iw + 2).saturating_sub(UnicodeWidthStr::width(lang_txt.as_str()));
    out.push(Line::from(vec![
        Span::styled("╭".to_string(), b),
        // The label is part of the frame, not an accent badge: one quiet
        // border color keeps the left cap and the rest of the outline uniform.
        Span::styled(lang_txt, b),
        Span::styled(format!("{}╮", "─".repeat(rest)), b),
    ]));
    for l in lines.into_iter().flat_map(|line| wrap_code_line(line, iw)) {
        let t = line_text_pub(&l);
        let pad = " ".repeat(iw.saturating_sub(UnicodeWidthStr::width(t.as_str())));
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

fn wrap_code_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut cells = Vec::new();
    for span in line.spans {
        for ch in span.content.chars() {
            cells.push((span.style, ch));
        }
    }
    if cells.is_empty() {
        return vec![Line::from(Vec::<Span<'static>>::new())];
    }
    let mut current = Vec::new();
    let mut used = 0usize;
    let mut prev_zwj = false;
    for (style, ch) in cells {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        // zero-width marks and joiner continuations never start a new row
        if !current.is_empty() && cw > 0 && !prev_zwj && used + cw > width {
            rows.push(cells_to_line(current, Style::default()));
            current = Vec::new();
            used = 0;
        }
        if cw <= width || current.is_empty() {
            current.push((style, ch));
            used += cw;
        }
        prev_zwj = ch == '\u{200d}';
    }
    if !current.is_empty() {
        rows.push(cells_to_line(current, Style::default()));
    }
    rows
}

/// Patch one highlighted code line onto the surface background.
fn surface_line(line: Line<'static>) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(" ".to_string(), surface_pad()));
    for s in line.spans {
        let st = s.style.patch(Style::new().bg(Theme::SURFACE()));
        spans.push(Span::styled(s.content.to_string(), st));
    }
    Line::from(spans)
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
    // ATX headings require a space/tab (or end of line) after the markers:
    // `#Foo` is a paragraph, not a heading.
    let after = &s[level..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let rest = after.trim_start_matches([' ', '\t']);
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
    let spans = inline(rest, style);
    // The Markdown marker is syntax, not content: do not print it in the
    // terminal. The heading style itself provides the visual distinction
    // (no underline: it collides with link styling and looks noisy).
    Some(spans)
}

fn try_list(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    if matches!(bytes.first(), Some(b'-' | b'*' | b'+')) {
        let rest = &s[1..];
        // `-foo` is a paragraph; require a space/tab (or end of line).
        if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
            return None;
        }
        let rest = rest.trim_start_matches([' ', '\t']);
        // task list items: `- [ ] todo`, `- [x] done`
        if rest.len() >= 3
            && rest.as_bytes()[0] == b'['
            && rest.as_bytes()[2] == b']'
            && matches!(rest.as_bytes()[1], b' ' | b'x' | b'X')
        {
            let after = &rest[3..];
            if after.is_empty() || after.starts_with(' ') || after.starts_with('\t') {
                let mark = if rest.as_bytes()[1] == b' ' {
                    "☐"
                } else {
                    "☑"
                };
                return Some((mark.to_string(), after.trim_start_matches([' ', '\t'])));
            }
        }
        return Some(("-".to_string(), rest));
    }
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let after = &s[digits.len()..];
        if after.starts_with('.') || after.starts_with(')') {
            let delim = after.as_bytes()[0] as char;
            let rest = &after[1..];
            // `1.foo` is a paragraph; require a space/tab (or end of line).
            if !(rest.is_empty() || rest.starts_with(' ') || rest.starts_with('\t')) {
                return None;
            }
            let rest = rest.trim_start_matches([' ', '\t']);
            return Some((format!("{digits}{delim}"), rest));
        }
    }
    None
}

/// Inline markdown: **bold**, *italic*, ***both***, ~~struck~~, `code`
/// (minimal recursive scanner).
pub fn inline(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    push_inline(text, base, &mut out);
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `_`-family markers must not open inside a word: `some_var` is literal.
fn can_open_us(rest: &str, pos: usize) -> bool {
    pos == 0 || !rest[..pos].chars().next_back().is_some_and(is_word_char)
}

/// ...nor close inside one: `a_ b_c` keeps the trailing underscore literal.
fn can_close_us(rest: &str, close_end: usize) -> bool {
    rest[close_end..]
        .chars()
        .next()
        .is_none_or(|c| !is_word_char(c))
}

enum InlineLink {
    Md {
        len: usize,
        text: String,
        url: String,
        image: bool,
    },
    Auto {
        len: usize,
        url: String,
    },
}

fn parse_md_link(s: &str, image: bool) -> Option<InlineLink> {
    debug_assert!(s.starts_with('['));
    let close = s.find(']')?;
    let text = &s[1..close];
    let url_part = s[close + 1..].strip_prefix('(')?;
    let end = url_part.find(')')?;
    // `[t](url "title")`: the title is dropped, only the target is shown
    let mut url = url_part[..end].split_whitespace().next().unwrap_or("");
    if url.starts_with('<') && url.ends_with('>') && url.len() >= 2 {
        url = &url[1..url.len() - 1];
    }
    if url.is_empty() || url.contains(char::is_whitespace) {
        return None;
    }
    let len = close + end + 3 + usize::from(image);
    Some(InlineLink::Md {
        len,
        text: text.to_string(),
        url: url.to_string(),
        image,
    })
}

fn parse_autolink(s: &str) -> Option<InlineLink> {
    debug_assert!(s.starts_with('<'));
    let end = s.find('>')?;
    let inner = &s[1..end];
    if inner.is_empty() || inner.chars().any(|c| c.is_whitespace() || c == '<') {
        return None;
    }
    if !(inner.contains('.') || inner.contains(':') || inner.contains('@')) {
        return None;
    }
    Some(InlineLink::Auto {
        len: end + 1,
        url: inner.to_string(),
    })
}

/// Earliest parseable `[text](url)`, `![alt](src)` or `<url>` in `rest`.
fn find_link_start(rest: &str) -> Option<(usize, InlineLink)> {
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'!' if bytes.get(i + 1) == Some(&b'[') => {
                if let Some(link) = parse_md_link(&rest[i + 1..], true) {
                    return Some((i, link));
                }
                i += 2;
            }
            b'[' => {
                if let Some(link) = parse_md_link(&rest[i..], false) {
                    return Some((i, link));
                }
                i += 1;
            }
            b'<' => {
                if let Some(link) = parse_autolink(&rest[i..]) {
                    return Some((i, link));
                }
                i += 1;
            }
            _ => {
                i += rest[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            }
        }
    }
    None
}

fn push_inline(text: &str, style: Style, out: &mut Vec<Span<'static>>) {
    const MARKERS: [&str; 9] = ["***", "___", "``", "**", "__", "~~", "`", "*", "_"];
    let ambient_bg = style.bg.unwrap_or(Theme::BG());
    let mut rest = text;
    'outer: while !rest.is_empty() {
        // earliest backslash escape (`\*` -> literal `*`)
        let mut esc_pos = None;
        let mut search = 0;
        while let Some(rel) = rest[search..].find('\\') {
            let p = search + rel;
            let valid = rest[p + 1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_punctuation());
            if valid {
                esc_pos = Some(p);
                break;
            }
            search = p + 1;
        }
        let link = find_link_start(rest);
        // earliest openable emphasis marker (longest wins on ties)
        let mut best: Option<(usize, &str)> = None;
        for m in MARKERS {
            let mut from = 0;
            while let Some(rel) = rest[from..].find(m) {
                let pos = from + rel;
                if m.contains('_') && !can_open_us(rest, pos) {
                    from = pos + 1;
                    continue;
                }
                best = Some(match best {
                    // on equal position prefer the longer marker (** over *)
                    Some((bp, bm)) if bp < pos || (bp == pos && bm.len() >= m.len()) => (bp, bm),
                    _ => (pos, m),
                });
                break;
            }
        }
        let mark_pos = best.map(|(p, _)| p);

        // links win only when strictly earliest, so `*em* [l](u)` still
        // parses the emphasis first and `[**b**](u)` keeps bold link text
        if let Some((lp, kind)) = link
            && esc_pos.is_none_or(|ep| lp < ep)
            && mark_pos.is_none_or(|mp| lp < mp)
        {
            if lp > 0 {
                out.push(Span::styled(rest[..lp].to_string(), style));
            }
            let link_style = style.fg(Theme::ACCENT()).add_modifier(Modifier::UNDERLINED);
            let url_style = Style::new().fg(Theme::DIM()).bg(ambient_bg);
            match kind {
                InlineLink::Md {
                    len,
                    text,
                    url,
                    image,
                } => {
                    let label = if image && text.is_empty() {
                        "image".to_string()
                    } else {
                        text
                    };
                    push_inline(&label, link_style, out);
                    out.push(Span::styled(format!(" ({url})"), url_style));
                    rest = &rest[lp + len..];
                }
                InlineLink::Auto { len, url } => {
                    out.push(Span::styled(url, link_style));
                    rest = &rest[lp + len..];
                }
            }
            continue 'outer;
        }
        if let Some(ep) = esc_pos
            && mark_pos.is_none_or(|mp| ep <= mp)
        {
            if ep > 0 {
                out.push(Span::styled(rest[..ep].to_string(), style));
            }
            let ch = rest[ep + 1..].chars().next().unwrap();
            out.push(Span::styled(ch.to_string(), style));
            rest = &rest[ep + 1 + ch.len_utf8()..];
            continue 'outer;
        }
        let Some((pos, marker)) = best else { break };
        if pos > 0 {
            out.push(Span::styled(rest[..pos].to_string(), style));
            rest = &rest[pos..];
        }
        // find a closable closing marker
        let close_from = marker.len();
        let mut close = None;
        let mut from = close_from;
        while let Some(rel) = rest[from..].find(marker) {
            let i = from + rel;
            if marker.contains('_') && !can_close_us(rest, i + marker.len()) {
                from = i + 1;
                continue;
            }
            close = Some(i);
            break;
        }
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
            "`" | "``" => {
                out.push(Span::styled(inner.to_string(), code_style()));
            }
            "***" | "___" => push_inline(
                inner,
                style.add_modifier(Modifier::BOLD | Modifier::ITALIC),
                out,
            ),
            "**" | "__" => push_inline(inner, style.add_modifier(Modifier::BOLD), out),
            "*" | "_" => push_inline(inner, style.add_modifier(Modifier::ITALIC), out),
            "~~" => push_inline(inner, style.add_modifier(Modifier::CROSSED_OUT), out),
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
    let width = (width as usize).max(1);
    let fallback = Style::new().fg(Theme::rule_color()).bg(Theme::BG());

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut tags: Vec<Option<usize>> = Vec::new();

    for (line, tag) in lines {
        // blank line: render an explicit empty styled row so the buffer cell
        // is reset (a span-less line would leave stale content behind)
        if line.spans.iter().all(|s| s.content.is_empty()) {
            rows.push(Line::from(vec![Span::styled(String::new(), Theme::base())]));
            tags.push(tag);
            continue;
        }
        // fast path: a line that already fits (and has no newlines) is its
        // own wrap result — push it unchanged instead of paying the per-char
        // cells buffer on every assembly pass. This pass runs on every
        // streamed frame, so the constant factor matters for long chats.
        let has_newline = line.spans.iter().any(|s| s.content.contains('\n'));
        let total_width: usize = line
            .spans
            .iter()
            .map(|s| {
                s.content
                    .chars()
                    .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                    .sum::<usize>()
            })
            .sum();
        if !has_newline && total_width <= width {
            rows.push(line);
            tags.push(tag);
            continue;
        }

        let mut cells: Vec<(Style, char)> = Vec::new();
        for span in &line.spans {
            for c in span.content.chars() {
                cells.push((span.style, c));
            }
        }

        let mut cur: Vec<(Style, char)> = Vec::new();
        let mut cur_width = 0usize;
        let mut last_space: Option<(usize, usize)> = None;
        for (st, ch) in cells {
            if ch == '\n' {
                continue;
            }
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            // zero-width marks always join the current row, even past budget
            if ch_width > 0 && cur_width + ch_width > width && !cur.is_empty() {
                if let Some((space_index, width_at_space)) = last_space {
                    let rest = cur.split_off(space_index + 1);
                    trim_end_spaces(&mut cur);
                    rows.push(cells_to_line(std::mem::take(&mut cur), fallback));
                    tags.push(tag);
                    cur = rest;
                    // the consumed space was 1 column wide; no re-sum needed
                    cur_width = cur_width.saturating_sub(width_at_space + 1);
                    last_space = None;
                } else {
                    rows.push(cells_to_line(std::mem::take(&mut cur), fallback));
                    tags.push(tag);
                    cur_width = 0;
                }
            }
            if ch == ' ' {
                last_space = Some((cur.len(), cur_width));
            }
            cur.push((st, ch));
            cur_width += ch_width;
        }
        trim_end_spaces(&mut cur);
        rows.push(cells_to_line(cur, fallback));
        tags.push(tag);
    }
    (rows, tags)
}

fn cell_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Terminal columns occupied by a run of styled characters.
fn columns_of(cells: &[(Style, char)]) -> usize {
    cells.iter().map(|(_, ch)| cell_width(*ch)).sum()
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
        let narrow = render(
            "```cpp\nstd::cout << \"Hello, world!\" << std::endl;\n```",
            24,
            &hl,
        );
        for line in &narrow {
            assert!(UnicodeWidthStr::width(line_text_pub(line).as_str()) <= 24);
        }
        let label: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(label.contains("rust"), "label was {label:?}");
        assert!(
            lines[0]
                .spans
                .iter()
                .all(|span| span.style == lines[0].spans[0].style),
            "language label must share the quiet frame style"
        );
        let generic = render("```text\nplain note\n```", 80, &hl);
        let generic_top = line_text_pub(&generic[0]);
        assert!(
            !generic_top.contains("text"),
            "generic text label must be hidden: {generic_top:?}"
        );
        let body: String = lines[1]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert!(body.contains("fn main"), "{body:?}");
    }

    #[test]
    fn headings_render_without_underline() {
        let hl = Highlighter::new();
        let lines = render("# Catalog\ndescription\n# Additional example", 80, &hl);
        assert_eq!(lines.len(), 3);
        let first: String = lines[0]
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect();
        assert_eq!(first, "Catalog");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        );
        for l in &lines {
            assert!(
                l.spans
                    .iter()
                    .all(|s| !s.style.add_modifier.contains(Modifier::UNDERLINED)),
                "headings must not be underlined: {l:?}"
            );
        }
        // `#Foo` is a paragraph, not a heading: the marker stays visible
        let nospace = render("#NoSpace", 80, &hl);
        let text: String = nospace
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect();
        assert!(text.contains("#NoSpace"), "{text:?}");
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
    fn bold_inline_parses() {
        let spans = inline("**Memory management**", Theme::base());
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "Memory management");
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

    /// Every line of a table — borders and rows alike — must occupy the same
    /// number of terminal columns. Measuring cells in characters instead of
    /// columns made the grid ragged for any non-Latin text: a CJK glyph takes
    /// two cells, so `"─".repeat(w)` came out shorter than the row it framed
    /// and the right border landed inside the text.
    #[test]
    fn table_grid_is_rectangular_with_wide_and_cyrillic_text() {
        let hl = Highlighter::new();
        let md = "| lang | note |\n|---|---|\n| 日本語 | データ |\n| кириллица | текст |\n";
        for width in [40u16, 60, 80] {
            let lines = render(md, width, &hl);
            let cols: Vec<usize> = lines
                .iter()
                .map(|l| UnicodeWidthStr::width(line_text_pub(l).as_str()))
                .collect();
            assert!(!cols.is_empty(), "width {width}: nothing rendered");
            assert!(
                cols.iter().all(|&c| c == cols[0]),
                "width {width}: ragged grid {cols:?}"
            );
            assert!(
                cols[0] <= width as usize,
                "width {width}: grid overflows the terminal {cols:?}"
            );
        }
    }

    #[test]
    fn table_wraps_long_unbreakable_words_and_urls() {
        let hl = Highlighter::new();
        let md = "| col | url |\n|---|---|\n| a | https://example.com/a/very/long/unbreakable/link/that/exceeds/the/column/budget/by/far |\n| b | prefix https://another.example.com/very/long/path/with/words/and/no/space/runs |\n";
        for width in [30u16, 40, 60] {
            let lines = render(md, width, &hl);
            let cols: Vec<usize> = lines
                .iter()
                .map(|l| UnicodeWidthStr::width(line_text_pub(l).as_str()))
                .collect();
            assert!(!cols.is_empty(), "width {width}: nothing rendered");
            assert!(
                cols.iter().all(|&c| c == cols[0]),
                "width {width}: ragged grid {cols:?}"
            );
            assert!(
                cols[0] <= width as usize,
                "width {width}: grid overflows terminal ({w} > {width}): {cols:?}",
                w = cols[0]
            );
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

    #[test]
    fn code_box_borders_align_with_long_language() {
        let hl = Highlighter::new();
        let lines = render("```javascript\n1\n```\n", 60, &hl);
        let widths: Vec<usize> = lines
            .iter()
            .take(3)
            .map(|l| UnicodeWidthStr::width(line_text_pub(l).as_str()))
            .collect();
        assert_eq!(widths.len(), 3);
        assert!(widths.iter().all(|&w| w == widths[0]), "{widths:?}");
    }

    #[test]
    fn intra_word_underscores_stay_literal() {
        let spans = inline("some_var_name and a*b*c", Theme::base());
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        // emphasis markers are consumed, so `*b*` collapses to `b`
        assert_eq!(text, "some_var_name and abc");
        // no italic leaked from the identifier
        assert!(
            spans
                .iter()
                .filter(|s| s.content.contains("some_var_name"))
                .all(|s| !s.style.add_modifier.contains(Modifier::ITALIC))
        );
        // `*` keeps working inside words
        assert!(
            spans
                .iter()
                .any(|s| s.content == "b" && s.style.add_modifier.contains(Modifier::ITALIC)),
            "{spans:?}"
        );
    }

    #[test]
    fn escapes_and_triple_markers() {
        let spans = inline(r"\*literal\* and ***both*** and ~~gone~~", Theme::base());
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "*literal* and both and gone", "{text:?}");
        let both = spans.iter().find(|s| s.content == "both").unwrap();
        assert!(both.style.add_modifier.contains(Modifier::BOLD));
        assert!(both.style.add_modifier.contains(Modifier::ITALIC));
        let gone = spans.iter().find(|s| s.content == "gone").unwrap();
        assert!(gone.style.add_modifier.contains(Modifier::CROSSED_OUT));
        // double backticks span greedily across single ones
        let code = inline("``a ` b``", Theme::base());
        let code_text: String = code.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(code_text, "a ` b", "{code_text:?}");
    }

    #[test]
    fn links_render_text_and_dim_url() {
        let spans = inline("see [docs](https://example.com/x) now", Theme::base());
        let text: String = spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, "see docs (https://example.com/x) now", "{text:?}");
        assert!(
            spans
                .iter()
                .any(|s| s.content == "docs"
                    && s.style.add_modifier.contains(Modifier::UNDERLINED)),
            "{spans:?}"
        );
        let auto = inline("go <https://example.com> ok", Theme::base());
        let auto_text: String = auto.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(auto_text, "go https://example.com ok", "{auto_text:?}");
        // emphasis markers inside the URL must not leak out
        let tricky = inline("[a](https://x/y*z) tail", Theme::base());
        let tricky_text: String = tricky.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(tricky_text, "a (https://x/y*z) tail", "{tricky_text:?}");
    }

    #[test]
    fn lists_tasks_nesting_and_ordered_guards() {
        let hl = Highlighter::new();
        let lines = render(
            "- [ ] todo\n- [x] done\n  - nested\n1) kept\n1.foo\n-nope",
            80,
            &hl,
        );
        let all: String = lines
            .iter()
            .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains('☐'), "{all:?}");
        assert!(all.contains('☑'), "{all:?}");
        assert!(
            all.contains("1)"),
            "`)` delimiter must be preserved: {all:?}"
        );
        assert!(all.contains("1.foo"), "`1.foo` is a paragraph: {all:?}");
        assert!(all.contains("-nope"), "`-nope` is a paragraph: {all:?}");
        // nested item is indented deeper than the top-level one
        let top = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
                    .contains("todo")
            })
            .unwrap();
        let nested = lines
            .iter()
            .find(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
                    .contains("nested")
            })
            .unwrap();
        let indent = |l: &Line<'_>| line_text_pub(l).chars().take_while(|c| *c == ' ').count();
        assert!(indent(nested) > indent(top), "{top:?} vs {nested:?}");
    }

    #[test]
    fn quotes_nest_and_hr_allows_spaces() {
        let hl = Highlighter::new();
        let lines = render(">> deep\n> shallow\n* * *", 80, &hl);
        let all: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("▐ ▐ deep"), "{all:?}");
        assert!(all.contains("▐ shallow"), "{all:?}");
        assert!(all.contains('─'), "spaced `* * *` is a rule: {all:?}");
    }

    #[test]
    fn tables_align_pipes_and_escapes() {
        let hl = Highlighter::new();
        // outer pipes optional, alignment honored, `\|` stays in the cell
        // (wrapping to a second physical row when the column is narrow)
        let md = "a | b\n--: | :-:\n1 | 2\nx \\| y | z\n";
        let lines = render(md, 40, &hl);
        let joined: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(joined.iter().any(|r| r.contains('┌')), "{joined:?}");
        let all = joined.join("\n");
        // the escaped pipe is content, not a delimiter: every body row keeps
        // exactly 2 columns (3 rails), and the cell text survives the wrap
        for l in &lines {
            if line_text_pub(l).contains('│') {
                assert_eq!(
                    line_text_pub(l).chars().filter(|c| *c == '│').count(),
                    3,
                    "{l:?}"
                );
            }
        }
        assert!(
            all.contains('x') && all.contains("| y"),
            "escaped pipe lost: {all:?}"
        );
        // right-aligned numeric cell: padding sits left of the digit
        let body = lines
            .iter()
            .find(|l| line_text_pub(l).contains('1') && line_text_pub(l).contains('│'))
            .unwrap();
        let t = line_text_pub(body);
        let cell = t.split('│').nth(1).unwrap();
        assert!(
            cell.starts_with("  ") || cell.starts_with("   "),
            "right column must pad left: {cell:?}"
        );
        for l in &lines {
            assert!(
                UnicodeWidthStr::width(line_text_pub(l).as_str()) <= 40,
                "{l:?}"
            );
        }
    }

    #[test]
    fn code_block_survives_narrow_width_and_unknown_lang() {
        let hl = Highlighter::new();
        // `foobarlang` is unknown -> plain-text fallback, still framed
        let lines = render("```foobarlang\nlet x = 1;\n```", 40, &hl);
        assert!(!lines.is_empty());
        // degenerate viewport: no frame, but nothing overflows
        let tiny = render("```rust\nfn main() {}\n```", 4, &hl);
        assert!(!tiny.is_empty());
        for l in &tiny {
            assert!(
                UnicodeWidthStr::width(line_text_pub(l).as_str()) <= 4,
                "{l:?}"
            );
        }
        // uppercase language tag still highlights instead of falling back:
        // the body keeps syntax spans, not one uniform style
        let up = render("```RUST\nfn main() {}\n```", 60, &hl);
        assert!(up.len() >= 3);
        let styles: std::collections::HashSet<String> = up[1]
            .spans
            .iter()
            .map(|s| format!("{:?}", s.style))
            .collect();
        assert!(styles.len() > 1, "expected highlight spans: {:?}", up[1]);
    }
}
