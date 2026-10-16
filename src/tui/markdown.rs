use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, ThemeSet};
use syntect::parsing::SyntaxSet;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::theme::Theme;

/// Terminal markdown renderer with syntax-highlighted code blocks.
///
/// The syntax/theme dumps are process-wide singletons: loading them takes
/// ~a second in debug builds, and every test builds its own `App` (plus a
/// dozen markdown unit tests build one directly). Sharing costs nothing —
/// both tables are read-only after load.
pub struct Highlighter {
    ps: &'static SyntaxSet,
    ts: &'static ThemeSet,
    theme_name: &'static str,
    code_cache: std::cell::RefCell<CodeCache>,
}

/// Bounded memo of highlighted+wrapped code blocks. Closed fences never
/// change mid-stream, so every per-tick re-render of a growing answer hits
/// this instead of re-parsing (syntect regexes dominate the stream profile).
/// The key covers everything the output depends on — resolved language,
/// code bytes, wrap width, theme — so hits are always exact; eviction is
/// oldest-first, memory is bounded by the cap times block size.
const CODE_CACHE_CAP: usize = 32;

#[derive(Clone, PartialEq, Eq, Hash)]
struct CodeCacheKey {
    lang: String,
    code_hash: u64,
    width: u16,
    theme: &'static str,
}

#[derive(Default)]
struct CodeCache {
    map: std::collections::HashMap<CodeCacheKey, Vec<Line<'static>>>,
    order: Vec<CodeCacheKey>,
}

impl CodeCache {
    fn get(&mut self, key: &CodeCacheKey) -> Option<Vec<Line<'static>>> {
        let hit = self.map.get(key)?.clone();
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(pos);
            self.order.push(k);
        }
        Some(hit)
    }

    fn put(&mut self, key: CodeCacheKey, rows: Vec<Line<'static>>) {
        if self.map.contains_key(&key) {
            return;
        }
        if self.order.len() >= CODE_CACHE_CAP {
            let old = self.order.remove(0);
            self.map.remove(&old);
        }
        self.order.push(key.clone());
        self.map.insert(key, rows);
    }
}

static SYNTAX_SET: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
static THEME_SET: std::sync::OnceLock<ThemeSet> = std::sync::OnceLock::new();

impl Highlighter {
    pub fn new() -> Self {
        Self {
            ps: SYNTAX_SET.get_or_init(SyntaxSet::load_defaults_newlines),
            ts: THEME_SET.get_or_init(ThemeSet::load_defaults),
            theme_name: "base16-eighties.dark",
            code_cache: std::cell::RefCell::new(CodeCache::default()),
        }
    }

    fn theme(&self) -> &syntect::highlighting::Theme {
        &self.ts.themes[self.theme_name]
    }

    /// Highlight a code block and wrap it to `width`, memoised. Repeat
    /// renders of the same closed fence return the cached rows.
    pub fn highlight_wrapped(
        &self,
        code: &str,
        lang: Option<&str>,
        width: u16,
    ) -> Vec<Line<'static>> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        code.hash(&mut h);
        let key = CodeCacheKey {
            // same normalisation as the syntax lookup below: `Rust` and
            // `rust` resolve identically, so they must share the key
            lang: lang.map(|l| l.to_ascii_lowercase()).unwrap_or_default(),
            code_hash: h.finish(),
            width,
            theme: self.theme_name,
        };
        if let Some(hit) = self.code_cache.borrow_mut().get(&key) {
            return hit;
        }
        let mut rows = Vec::new();
        for line in self.highlight_code(code, lang) {
            rows.extend(wrap_code_line(line, width as usize));
        }
        self.code_cache.borrow_mut().put(key, rows.clone());
        rows
    }

    #[cfg(test)]
    fn cache_len(&self) -> usize {
        self.code_cache.borrow().map.len()
    }

    /// Highlight a code block; falls back to plain text for unknown languages.
    pub fn highlight_code(&self, code: &str, lang: Option<&str>) -> Vec<Line<'static>> {
        // syntect cost explodes on huge inputs (a pasted bundle, a minified
        // single-liner): bound it. Past the caps the text stays fully
        // visible, just uncolored — never truncated, never dropped.
        const MAX_HIGHLIGHT_LINES: usize = 400;
        const MAX_HIGHLIGHT_COLS: usize = 2000;
        let syntax = lang
            .and_then(|l| {
                self.ps
                    .find_syntax_by_token(l)
                    .or_else(|| self.ps.find_syntax_by_token(&l.to_ascii_lowercase()))
            })
            .unwrap_or_else(|| self.ps.find_syntax_plain_text());
        let mut hl = HighlightLines::new(syntax, self.theme());
        let mut out = Vec::new();
        // skipping a line would desync the highlighter state for every
        // line after it (multiline strings/comments), so the first skip
        // retires highlighting for the rest of the block
        let mut plain_rest = false;
        for (i, line) in syntect::util::LinesWithEndings::from(code).enumerate() {
            if !plain_rest
                && (i >= MAX_HIGHLIGHT_LINES || line.chars().count() > MAX_HIGHLIGHT_COLS)
            {
                plain_rest = true;
            }
            if plain_rest {
                out.push(Line::from(Span::styled(
                    line.trim_end_matches('\n').to_string(),
                    base_style(),
                )));
                continue;
            }
            let Ok(regions) = hl.highlight_line(line, self.ps) else {
                // never drop source text on a highlighter error: fall back
                // to the plain line so failures stay visible, not silent
                out.push(Line::from(Span::styled(
                    line.trim_end_matches('\n').to_string(),
                    base_style(),
                )));
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

/// Prose base: dim gray, not terminal-default white. Emphasis goes brighter
/// (white), links take the house accent — the antigravity recipe: mostly gray
/// text, sparse color, readable answers.
fn base_style() -> Style {
    Style::new().fg(Color::Gray)
}

fn code_style() -> Style {
    Style::new().fg(Color::White)
}

/// Emphasis: bright white on default prose, but a colored band (quote keeps
/// its monochrome green) wins so strips stay uniform.
fn with_emphasis(style: Style, m: Modifier) -> Style {
    match style.fg {
        None | Some(Color::Gray) => style.fg(Color::White).add_modifier(m),
        _ => style.add_modifier(m),
    }
}

/// Showcase document for `/test md`: small, exercises every renderer
/// branch (headings, inline styles, quote, lists, table, rule, fences,
/// math, links) so the render can be eyeballed in one place.
pub const SHOWCASE: &str = "# Title\n\n## Section\n\nText with **bold**, *italic*, \
***both***, ~~struck~~, `code` and [link](https://example.com).\n\n\
> quoted line\n\n- one\n- two\n  - nested\n\n1. first\n2. second\n\n\
| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\n```rust\nfn main() {}\n```\n\n\
```python\nprint(\"hi\")\n```\n\n$\nx + y\n$\n";

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
            // collapse runs of blank source lines: only the first one leaves
            // a row (checked by content, not by span count — an empty styled
            // span is still a blank row)
            if matches!(out.last(), Some(l) if l.spans.iter().all(|s| s.content.is_empty())) {
                continue;
            }
            out.push(Line::from(vec![Span::styled(String::new(), base_style())]));
            continue;
        }

        // display math block ($$ ... $$). Models also habitually write
        // lone `$` lines as delimiters: accept them the same way.
        if trimmed_start == "$$" || trimmed_start == "$" {
            let mut math_lines = Vec::new();
            for ml in lines.by_ref() {
                if ml.trim() == "$$" || ml.trim() == "$" {
                    break;
                }
                math_lines.push(ml);
            }
            for ml in math_lines {
                out.push(Line::from(vec![
                    Span::styled("  ", base_style()),
                    Span::styled(render_math(ml.trim()), base_style()),
                ]));
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
            let quote = Style::new().fg(Theme::GREEN());
            // Wrap the quoted text to what fits beside the rail, then put
            // the rail on EVERY visual row: the outer wrapper would
            // otherwise split long quotes and leave continuation rows bare.
            // Rows pad with the base background — no band, the green text
            // and rail carry the quote alone.
            let rail_cols = level * 2;
            let avail = (width as usize).saturating_sub(rail_cols).max(1);
            let (rows, _) =
                wrap_tagged(vec![(Line::from(inline(rest, quote)), None)], avail as u16);
            for row in rows {
                // the rail glyph stays on the default background; rows pad
                // plain like any other text
                let mut spans = vec![Span::styled(rail.clone(), Style::new().fg(Theme::GREEN()))];
                spans.extend(row.spans);
                let used: usize = spans
                    .iter()
                    .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                    .sum();
                let pad = (width as usize).saturating_sub(used);
                if pad > 0 {
                    spans.push(Span::styled(" ".repeat(pad), Theme::base()));
                }
                out.push(Line::from(spans));
            }
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
            // Ordered markers get the house accent, unordered stay default.
            let marker_style = if marker.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                Style::new().fg(Theme::ACCENT())
            } else {
                Style::new()
            };
            // hanging indent: wrap the body to what fits beside the marker,
            // then put the marker on the first visual row and blank space of
            // the same width on every continuation — same shape as the quote
            // rail above, or wrapped rows would start at column zero.
            let prefix_cols = 2 * (nest + 1) + marker.len() + 1;
            let avail = (width as usize).saturating_sub(prefix_cols).max(1);
            let (rows, _) = wrap_tagged(
                vec![(Line::from(inline(rest, base_style())), None)],
                avail as u16,
            );
            for (i, row) in rows.into_iter().enumerate() {
                let head = if i == 0 {
                    format!("{marker} ")
                } else {
                    " ".repeat(marker.len() + 1)
                };
                let mut spans = vec![
                    Span::styled("  ".repeat(nest + 1), base_style()),
                    Span::styled(head, marker_style),
                ];
                spans.extend(row.spans);
                out.push(Line::from(spans));
            }
            continue;
        }

        out.push(Line::from(inline(trimmed_start, base_style())));
    }

    // unclosed fence during streaming — render what we have
    if in_code {
        emit_code(&mut out, &code_buf, code_lang.as_deref(), hl, width);
    }
    // untrusted content reaches the terminal through these rows (assistant
    // segments bypass wrap_tagged): strip controls here so no caller can
    // forget. sanitize_line is a no-op pass-through for clean lines.
    out.into_iter().map(sanitize_line).collect()
}

/// Incremental renderer for the one segment that repaints while it renders:
/// the live assistant answer. The completed part of the text is parsed once
/// and kept as unwrapped lines; each reveal re-parses only the tail past the
/// last safe split point. `lines` always returns exactly what
/// [`render`] returns for the same text — a guard that fails falls back to a
/// full parse and re-establishes the prefix, never to a stale visual.
pub struct LiveRender {
    per_id: std::collections::HashMap<u64, LivePrefix>,
}

/// Reusable prefix for one live segment: unwrapped and wrapped lines for `text[..cut]`.
struct LivePrefix {
    cut: usize,
    hash: [u8; 32],
    width: u16,
    lines: Vec<Line<'static>>,
    wrapped: Vec<Line<'static>>,
}

impl LiveRender {
    pub fn new() -> Self {
        Self {
            per_id: std::collections::HashMap::new(),
        }
    }

    /// Forget entries whose segment no longer renders live (turn finished,
    /// segment removed).
    pub fn retain(&mut self, keep: impl Fn(u64) -> bool) {
        self.per_id.retain(|id, _| keep(*id));
    }

    pub fn is_empty(&self) -> bool {
        self.per_id.is_empty()
    }

    /// Render `text` at `width`, reusing the cached prefix lines for `id`.
    pub fn lines(
        &mut self,
        id: u64,
        text: &str,
        width: u16,
        hl: &Highlighter,
    ) -> Vec<Line<'static>> {
        let prefix_ok = match self.per_id.get(&id) {
            Some(prefix) => {
                prefix.width == width
                    && prefix.cut > 0
                    && text.len() > prefix.cut
                    && text.is_char_boundary(prefix.cut)
                    && blake3::hash(&text.as_bytes()[..prefix.cut]).as_bytes() == &prefix.hash
            }
            None => false,
        };
        if prefix_ok {
            let old_cut = self.per_id[&id].cut;
            if let Some(cut) = safe_split_from(text, old_cut) {
                let prefix = self.per_id.get_mut(&id).expect("prefix_ok above");
                if cut > old_cut {
                    let mid = render(&text[old_cut..cut], width, hl);
                    let (mid_wrapped, _) = wrap_tagged_inner(
                        mid.iter().cloned().map(|l| (l, None)).collect(),
                        width,
                        false,
                    );
                    prefix.lines.extend(mid);
                    prefix.wrapped.extend(mid_wrapped);
                    prefix.cut = cut;
                    prefix.hash = *blake3::hash(&text.as_bytes()[..cut]).as_bytes();
                }
                let tail = render(&text[cut..], width, hl);
                let mut lines = Vec::with_capacity(prefix.lines.len() + tail.len());
                lines.extend(prefix.lines.iter().cloned());
                lines.extend(tail);
                return lines;
            }
        }
        if let Some(cut) = safe_split_from(text, 0)
            && cut > 0
        {
            let head = render(&text[..cut], width, hl);
            let (head_wrapped, _) = wrap_tagged_inner(
                head.iter().cloned().map(|l| (l, None)).collect(),
                width,
                false,
            );
            let tail = render(&text[cut..], width, hl);
            let mut lines = Vec::with_capacity(head.len() + tail.len());
            lines.extend(head.iter().cloned());
            lines.extend(tail);
            self.per_id.insert(
                id,
                LivePrefix {
                    cut,
                    hash: *blake3::hash(&text.as_bytes()[..cut]).as_bytes(),
                    width,
                    lines: head,
                    wrapped: head_wrapped,
                },
            );
            return lines;
        }
        self.per_id.remove(&id);
        render(text, width, hl)
    }

    /// Render and wrap `text` at `width` for segment `idx`, reusing the
    /// already-wrapped prefix rows for `id` so streaming only wraps the
    /// active tail past the last safe split point.
    pub fn wrapped_lines(
        &mut self,
        id: u64,
        idx: usize,
        text: &str,
        width: u16,
        hl: &Highlighter,
    ) -> Vec<(Line<'static>, Option<usize>)> {
        let tag = Some(idx);
        let prefix_ok = match self.per_id.get(&id) {
            Some(prefix) => {
                prefix.width == width
                    && prefix.cut > 0
                    && text.len() > prefix.cut
                    && text.is_char_boundary(prefix.cut)
                    && blake3::hash(&text.as_bytes()[..prefix.cut]).as_bytes() == &prefix.hash
            }
            None => false,
        };
        if prefix_ok {
            let old_cut = self.per_id[&id].cut;
            if let Some(cut) = safe_split_from(text, old_cut) {
                let prefix = self.per_id.get_mut(&id).expect("prefix_ok above");
                if cut > old_cut {
                    let mid = render(&text[old_cut..cut], width, hl);
                    let (mid_wrapped, _) = wrap_tagged_inner(
                        mid.iter().cloned().map(|l| (l, None)).collect(),
                        width,
                        false,
                    );
                    prefix.lines.extend(mid);
                    prefix.wrapped.extend(mid_wrapped);
                    prefix.cut = cut;
                    prefix.hash = *blake3::hash(&text.as_bytes()[..cut]).as_bytes();
                }
                let tail = render(&text[cut..], width, hl);
                let (tail_wrapped, _) =
                    wrap_tagged_inner(tail.into_iter().map(|l| (l, tag)).collect(), width, true);
                let mut out = Vec::with_capacity(prefix.wrapped.len() + tail_wrapped.len());
                out.extend(prefix.wrapped.iter().cloned().map(|l| (l, tag)));
                out.extend(tail_wrapped.into_iter().map(|l| (l, tag)));
                return out;
            }
        }
        if let Some(cut) = safe_split_from(text, 0)
            && cut > 0
        {
            let head = render(&text[..cut], width, hl);
            let (head_wrapped, _) = wrap_tagged_inner(
                head.iter().cloned().map(|l| (l, None)).collect(),
                width,
                false,
            );
            let tail = render(&text[cut..], width, hl);
            let (tail_wrapped, _) =
                wrap_tagged_inner(tail.into_iter().map(|l| (l, tag)).collect(), width, true);
            let mut out = Vec::with_capacity(head_wrapped.len() + tail_wrapped.len());
            out.extend(head_wrapped.iter().cloned().map(|l| (l, tag)));
            out.extend(tail_wrapped.into_iter().map(|l| (l, tag)));
            self.per_id.insert(
                id,
                LivePrefix {
                    cut,
                    hash: *blake3::hash(&text.as_bytes()[..cut]).as_bytes(),
                    width,
                    lines: head,
                    wrapped: head_wrapped,
                },
            );
            return out;
        }
        self.per_id.remove(&id);
        let full = render(text, width, hl);
        let (wrapped, tags) =
            wrap_tagged_inner(full.into_iter().map(|l| (l, tag)).collect(), width, true);
        wrapped.into_iter().zip(tags).collect()
    }
}

/// Byte offset of the last line start at or past `from` where splitting keeps
/// `render(a) · render(b)` identical to `render(a + b)`: no fenced code or
/// `$$` math block may be open (their state is the only cross-line renderer
/// state), the line itself must be non-blank (blank runs collapse), the
/// previous and this line may not both carry a pipe (a GFM table never
/// straddles the cut), and the line must not be a setext underline (it would
/// reinterpret the paragraph above the cut — see [`is_setext_underline`]).
///
/// Precondition when `from > 0`: `from` was itself a valid split point for
/// `text[..from]`, so at `offset == from` the cross-line state is guaranteed
/// to be `(!in_code, !in_math, !prev_pipe)`. This allows scanning only
/// `&text[from..]` in O(tail) instead of rescanning the entire document.
fn safe_split_from(text: &str, from: usize) -> Option<usize> {
    let mut in_code = false;
    let mut in_math = false;
    let mut best: Option<usize> = None;
    let mut prev_pipe = false;
    let mut offset = from;
    for line in text[from..].split('\n') {
        let start = offset;
        offset += line.len() + 1;
        let lead = line.trim_start();
        let valid_here = !in_code
            && !in_math
            && !lead.is_empty()
            && !prev_pipe
            && !is_setext_underline(line);
        if start == from && from > 0 && !valid_here {
            // The byte at `from` was mutated such that `from` itself is no
            // longer a valid split boundary; force a full re-establish.
            return None;
        }
        if start > 0 && start >= from && valid_here {
            best = Some(start);
        }
        // render's own construct order: a fence line toggles code only
        // outside math (math consumes lines wholesale), `$$` (or a lone
        // `$` line, which models use as the same delimiter) opens math only
        // outside code, and a math block closes on either delimiter
        if in_math {
            if line.trim() == "$$" || line.trim() == "$" {
                in_math = false;
            }
        } else if in_code {
            if lead.starts_with("```") {
                in_code = false;
            }
        } else if lead == "$$" || lead == "$" {
            in_math = true;
        } else if lead.starts_with("```") {
            in_code = true;
        }
        prev_pipe = lead.contains('|');
    }
    best
}

/// A line of only `-` or `=`: a potential setext underline. The renderer has
/// no setext headings today (an all-`-` line is an HR, an all-`=` line is
/// inline text), so a cut before one happens to render identically — but the
/// cached prefix only grows within a turn, so if the renderer ever learns
/// setext, every such cut would freeze a paragraph-shaped prefix while the
/// tail re-renders the same bytes as a heading, until the turn ends. Split
/// only where the next line cannot reinterpret the paragraph above it.
fn is_setext_underline(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && (t.bytes().all(|b| b == b'=') || t.bytes().all(|b| b == b'-'))
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
    // Split on unescaped pipes outside inline code spans: `\|` or a pipe inside
    // `` `a || b` `` is literal content inside the cell, not a column boundary.
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
        } else if ch == '`' {
            let mut ticks = 1usize;
            while chars.peek() == Some(&'`') {
                chars.next();
                ticks += 1;
            }
            for _ in 0..ticks {
                cur.push('`');
            }
            // Check if a matching closing backtick run exists on this line
            let rest: String = chars.clone().collect();
            let needle = "`".repeat(ticks);
            if rest.contains(&needle) {
                while let Some(c) = chars.next() {
                    if c == '`' {
                        let mut close = 1usize;
                        while chars.peek() == Some(&'`') {
                            chars.next();
                            close += 1;
                        }
                        for _ in 0..close {
                            cur.push('`');
                        }
                        if close == ticks {
                            break;
                        }
                    } else {
                        cur.push(c);
                    }
                }
            }
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
    // Minimum column width. Two frame-adjacent padding spaces plus two
    // content columns: any single char (max width 2) always fits, so a cell
    // can never overflow its frame. Narrower tables would trade content loss
    // for width; overflow past the viewport stays rectangular instead.
    const MIN_COL: usize = 4;
    let avail = width.saturating_sub(ncols as u16 + 1) as usize;
    // Column widths are terminal columns, not characters: the border below is
    // drawn as `"─".repeat(w(i))`, and a CJK glyph occupies two cells while a
    // combining mark occupies none. Counting characters here made every table
    // with non-Latin text draw its right border inside the cell text.
    let mut widths: Vec<usize> = (0..ncols)
        .map(|i| {
            let content = rows
                .iter()
                .map(|r| {
                    r.get(i)
                        .map(|c| UnicodeWidthStr::width(c.as_str()))
                        .unwrap_or(0)
                })
                .max()
                .unwrap_or(0);
            // +2 frame-adjacent padding spaces live inside the column: a
            // token that fits the content must never wrap. Without this the
            // longest token always overflowed its own `wi - 2` budget by
            // exactly two columns and shredded character-by-character.
            content.saturating_add(2).max(MIN_COL)
        })
        .collect();
    // Shrink to fit the frame: content budget is `avail`, because the frame
    // itself costs `ncols + 1` border columns. Longest-first water-filling:
    // the widest columns absorb the cut while narrow ones (tool names,
    // flags, short cells) keep their natural width and never shred.
    let content_total: usize = widths.iter().sum();
    if content_total > avail {
        let mut over = content_total - avail;
        while over > 0 {
            let max_w = widths.iter().copied().max().unwrap_or(0);
            if max_w <= MIN_COL {
                break;
            }
            let next = widths
                .iter()
                .copied()
                .filter(|w| *w < max_w)
                .max()
                .unwrap_or(MIN_COL);
            let target = next.max(MIN_COL);
            let holders = widths.iter().filter(|w| **w == max_w).count().max(1);
            // even share per max column, at least one column each pass
            let share = over.div_ceil(holders).max(1);
            let mut cut_any = false;
            for w in widths.iter_mut() {
                if over == 0 {
                    break;
                }
                if *w == max_w && *w > target {
                    let cut = (*w - target).min(share).min(over);
                    *w -= cut;
                    over -= cut;
                    cut_any = true;
                }
            }
            if !cut_any {
                break;
            }
        }
        // Still over (everything at MIN_COL): the frame overflows the
        // viewport, but every row keeps the same width — rectangular.
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
                .fg(Color::White)
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
    // Codex-style: plain syntax-highlighted rows, no frame or language badge.
    // Long source lines still wrap to the chat width (the outer wrapper must
    // never see an over-wide row).
    let w = (width as usize).max(1);
    for l in hl.highlight_wrapped(code, lang, w as u16) {
        out.push(l);
    }
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

#[cfg(test)]
fn line_text_pub(l: &Line<'_>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
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
    let mut rest = after.trim_start_matches([' ', '\t']);
    // Optional closing run (`## Title ##`): trailing `#`s preceded by a
    // space are syntax, not content.
    {
        let bytes = rest.as_bytes();
        let mut end = bytes.len();
        while end > 0 && bytes[end - 1] == b'#' {
            end -= 1;
        }
        if end < bytes.len() && end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
            rest = rest[..end].trim_end();
        }
    }
    // Headings: h1 wears the brand gradient (no underline anymore — the
    // ramp carries the level), h2 muted berry, h3 light berry, h4 white,
    // h5-h6 dim italic. h3/h4+ deliberately do not rely on italic alone: terminals
    // without italic support (conhost) would render them as plain text.
    // The `#` markers are syntax, not content: they are never printed,
    // the heading style alone carries the level.
    if level == 1 {
        return Some(crate::tui::art::gradient_spans(inline(
            rest,
            Style::new().add_modifier(Modifier::BOLD),
        )));
    }
    let style = match level {
        2 => crate::tui::art::h2_style(),
        3 => {
            // light berry: headings stay one family (gradient, muted,
            // light), brightness falling with depth
            if crate::tui::shimmer::has_truecolor() {
                Style::new()
                    .fg(Color::Rgb(230, 99, 140))
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC)
            } else {
                Style::new()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD | Modifier::ITALIC)
            }
        }
        4 => Style::new()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD | Modifier::ITALIC),
        _ => Style::new().fg(Theme::DIM()).add_modifier(Modifier::ITALIC),
    };
    Some(inline(rest, style))
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
                // ASCII boxes only: the prettiest glyphs (U+2610/U+2611)
                // fall back to the color-emoji font on Windows Terminal,
                // and even U+FE0E does not stop it
                let mark = if rest.as_bytes()[1] == b' ' {
                    "[ ]"
                } else {
                    "[x]"
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

/// Inline markdown: **bold**, *italic*, ***bold*** (folded — italic over
/// bold is invisible in terminals), ~~struck~~, `code`
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

/// `$`-family markers must not open if followed by whitespace or another `$` (unless `$$`).
fn can_open_math(rest: &str, pos: usize, marker_len: usize) -> bool {
    let after = &rest[pos + marker_len..];
    if marker_len == 1 {
        !after.is_empty() && !after.starts_with(char::is_whitespace) && !after.starts_with('$')
    } else {
        !after.is_empty()
    }
}

/// Single `$` closing marker must not be preceded by whitespace, and not followed by a digit.
fn can_close_math(rest: &str, close_pos: usize) -> bool {
    let before = &rest[..close_pos];
    let after = &rest[close_pos + 1..];
    !before.ends_with(char::is_whitespace) && !after.starts_with(|c: char| c.is_ascii_digit())
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

/// Bounds for single-pass inline scanning. Unbounded searches turn pasted
/// content (`"[".repeat(n)`, marker runs) into quadratic work: every failed
/// open re-scans the whole remainder. Caps convert the worst case to linear;
/// only single constructs longer than the cap degrade to literals, which is
/// unreachable for real chat text.
const MAX_LINK_LABEL: usize = 1024;
const MAX_LINK_TARGET: usize = 4096;
const MAX_EMPHASIS_SPAN: usize = 4096;

/// Largest byte index at or below `cap` that sits on a char boundary.
fn floor_boundary(s: &str, cap: usize) -> usize {
    s.floor_char_boundary(cap.min(s.len()))
}

fn parse_md_link(s: &str, image: bool) -> Option<InlineLink> {
    debug_assert!(s.starts_with('['));
    let close = s[..floor_boundary(s, MAX_LINK_LABEL + 1)].find(']')?;
    let text = &s[1..close];
    let url_part = s[close + 1..].strip_prefix('(')?;
    let end = url_part[..floor_boundary(url_part, MAX_LINK_TARGET + 1)].find(')')?;
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
    let end = s[..floor_boundary(s, MAX_LINK_LABEL + 1)].find('>')?;
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

/// The render-side link signature: accent + underline. Nothing else in
/// chat rows wears exactly this combination (markers are bare accent,
/// code tokens never use the indexed accent), so hit-testing can trust it.
fn is_link_style(style: Style) -> bool {
    style.fg == Some(Theme::ACCENT()) && style.add_modifier.contains(Modifier::UNDERLINED)
}

/// URL under a cursor inside one wrapped row. Styles carry no URLs, so the
/// cursor must sit on a link-styled run and the target must be adjacent:
/// an `<autolink>` URL under the cursor, or `(url)` parens right after the
/// label (same row, or opening the next one when the label fills its row).
/// Anything ambiguous — two links one row, a URL cut at the row edge, a
/// bare URL in code text — resolves to nothing: a click must never open
/// the wrong address. Only `http(s)` targets resolve, so exotic schemes
/// from untrusted model output cannot reach the opener.
pub fn link_url_at(row: &[Span<'_>], next: Option<&[Span<'_>]>, char_col: usize) -> Option<String> {
    let text: String = row.iter().map(|s| s.content.as_ref()).collect();
    let chars: Vec<char> = text.chars().collect();
    // per-char link mask over the row
    let mut mask = vec![false; chars.len()];
    let mut i = 0;
    for span in row {
        let link = is_link_style(span.style);
        for _ in span.content.chars() {
            if i < mask.len() {
                mask[i] = link;
            }
            i += 1;
        }
    }
    if char_col >= mask.len() || !mask[char_col] {
        return None;
    }
    // maximal link-styled run around the cursor (a label with nested
    // emphasis is several spans, all link-styled)
    let mut start = char_col;
    while start > 0 && mask[start - 1] {
        start -= 1;
    }
    let mut end = char_col;
    while end + 1 < mask.len() && mask[end + 1] {
        end += 1;
    }
    let run: String = chars[start..=end].iter().collect();
    // autolink: the run IS the URL. A run cut at the row edge may continue
    // on the next row — gluing is a guess either way, so abstain.
    if run.starts_with("http://") || run.starts_with("https://") {
        if end + 1 == chars.len()
            && next.is_some_and(|n| {
                n.iter()
                    .flat_map(|s| s.content.chars())
                    .next()
                    .is_some_and(|c| !c.is_whitespace())
            })
        {
            return None;
        }
        return clean_url(&run);
    }
    // label: ` (url)` follows on this row, or opens the next one when the
    // label runs to the row edge (wrap eats the separating space)
    let after: String = chars[end + 1..].iter().collect();
    let after = after.trim_start();
    if let Some(rest) = after.strip_prefix('(') {
        return paren_url(rest).and_then(|u| clean_url(&u));
    }
    if !after.is_empty() {
        return None;
    }
    let ahead: String = next?.iter().map(|s| s.content.as_ref()).collect();
    let rest = ahead.trim_start().strip_prefix('(')?;
    paren_url(rest).and_then(|u| clean_url(&u))
}

/// Balanced `(...)` scan: nested parens in the URL survive, the match ends
/// at the paren that closes the opener.
fn paren_url(s: &str) -> Option<String> {
    let mut depth = 1u32;
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '(' => {
                depth += 1;
                out.push(c);
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(out);
                }
                out.push(c);
            }
            '\\' => {
                // an escaped char is literal text, never a delimiter
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            _ => out.push(c),
        }
    }
    None
}

/// Second lock (the first is adjacency above): only web targets resolve.
fn clean_url(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    if rest.is_empty() || rest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    Some(url.to_string())
}

/// Longest emphasis marker starting at `bytes[i]`, if any. Order matters:
/// on equal positions the longer marker wins (`**` over `*`).
fn marker_at(bytes: &[u8], i: usize) -> Option<&'static str> {
    const MARKERS: [&str; 11] = [
        "***", "___", "**", "__", "~~", "``", "$$", "*", "_", "`", "$",
    ];
    MARKERS
        .iter()
        .find(|m| bytes[i..].starts_with(m.as_bytes()))
        .copied()
}

/// Single left-to-right pass: every byte position is visited once, so the
/// whole scan is linear in the input length (bounded closer/link searches
/// keep adversarial pasted runs from going quadratic). Precedence matches
/// the old multi-search version: strictly earliest construct wins, escapes
/// beat a marker at the same-or-later position, links beat markers only when
/// strictly earlier, and on equal positions the longer marker wins.
pub(crate) fn lookup_math_symbol(cmd: &str) -> Option<&'static str> {
    match cmd {
        // Arrows
        "rightarrow" | "to" => Some("→"),
        "longrightarrow" => Some("⟶"),
        "leftarrow" | "gets" => Some("←"),
        "longleftarrow" => Some("⟵"),
        "leftrightarrow" => Some("↔"),
        "longleftrightarrow" => Some("⟷"),
        "Rightarrow" | "implies" => Some("⇒"),
        "Longrightarrow" => Some("⟹"),
        "Leftarrow" => Some("⇐"),
        "Longleftarrow" => Some("⟸"),
        "Leftrightarrow" | "iff" => Some("⇔"),
        "Longleftrightarrow" => Some("⟺"),
        "mapsto" => Some("↦"),
        "longmapsto" => Some("⟼"),
        "uparrow" => Some("↑"),
        "downarrow" => Some("↓"),
        "updownarrow" => Some("↕"),
        "Uparrow" => Some("⇑"),
        "Downarrow" => Some("⇓"),
        "nearrow" => Some("↗"),
        "searrow" => Some("↘"),
        "swarrow" => Some("↙"),
        "nwarrow" => Some("↖"),

        // Comparisons and relations
        "le" | "leq" => Some("≤"),
        "ge" | "geq" => Some("≥"),
        "ne" | "neq" => Some("≠"),
        "approx" => Some("≈"),
        "equiv" => Some("≡"),
        "sim" => Some("∼"),
        "simeq" => Some("≃"),
        "ll" => Some("≪"),
        "gg" => Some("≫"),
        "propto" => Some("∝"),
        "perp" => Some("⊥"),
        "parallel" => Some("∥"),

        // Operations and symbols
        "times" => Some("×"),
        "div" => Some("÷"),
        "pm" => Some("±"),
        "mp" => Some("∓"),
        "cdot" => Some("·"),
        "circ" => Some("∘"),
        "bullet" => Some("•"),
        "dots" | "cdots" | "ldots" => Some("…"),
        "vdots" => Some("⋮"),
        "ddots" => Some("⋱"),
        "infty" => Some("∞"),
        "in" => Some("∈"),
        "notin" => Some("∉"),
        "subset" => Some("⊂"),
        "subseteq" => Some("⊆"),
        "supset" => Some("⊃"),
        "supseteq" => Some("⊇"),
        "cup" => Some("∪"),
        "cap" => Some("∩"),
        "setminus" => Some("∖"),
        "forall" => Some("∀"),
        "exists" => Some("∃"),
        "nexists" => Some("∄"),
        "emptyset" | "varnothing" => Some("∅"),
        "partial" => Some("∂"),
        "nabla" => Some("∇"),
        "sum" => Some("∑"),
        "prod" => Some("∏"),
        "int" => Some("∫"),
        "oint" => Some("∮"),
        "sqrt" => Some("√"),
        "angle" => Some("∠"),

        // Greek lowercase
        "alpha" => Some("α"),
        "beta" => Some("β"),
        "gamma" => Some("γ"),
        "delta" => Some("δ"),
        "epsilon" | "varepsilon" => Some("ε"),
        "zeta" => Some("ζ"),
        "eta" => Some("η"),
        "theta" | "vartheta" => Some("θ"),
        "iota" => Some("ι"),
        "kappa" => Some("κ"),
        "lambda" => Some("λ"),
        "mu" => Some("μ"),
        "nu" => Some("ν"),
        "xi" => Some("ξ"),
        "pi" | "varpi" => Some("π"),
        "rho" | "varrho" => Some("ρ"),
        "sigma" | "varsigma" => Some("σ"),
        "tau" => Some("τ"),
        "upsilon" => Some("υ"),
        "phi" | "varphi" => Some("φ"),
        "chi" => Some("χ"),
        "psi" => Some("ψ"),
        "omega" => Some("ω"),

        // Greek uppercase
        "Gamma" => Some("Γ"),
        "Delta" => Some("Δ"),
        "Theta" => Some("Θ"),
        "Lambda" => Some("Λ"),
        "Xi" => Some("Ξ"),
        "Pi" => Some("Π"),
        "Sigma" => Some("Σ"),
        "Upsilon" => Some("Υ"),
        "Phi" => Some("Φ"),
        "Psi" => Some("Ψ"),
        "Omega" => Some("Ω"),

        // Spacing
        "quad" => Some("  "),
        "qquad" => Some("    "),

        _ => None,
    }
}

pub(crate) fn render_math(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '\\' {
            let rem = &s[i + 1..];
            let cmd_len = rem
                .chars()
                .take_while(|ch| ch.is_ascii_alphabetic())
                .map(|ch| ch.len_utf8())
                .sum::<usize>();
            if cmd_len > 0 {
                let cmd = &rem[..cmd_len];
                if let Some(sym) = lookup_math_symbol(cmd) {
                    out.push_str(sym);
                    for _ in 0..cmd.chars().count() {
                        chars.next();
                    }
                    continue;
                }
                if matches!(
                    cmd,
                    "text" | "mathrm" | "mathbf" | "mathit" | "operatorname"
                ) {
                    for _ in 0..cmd.chars().count() {
                        chars.next();
                    }
                    if chars.peek().is_some_and(|(_, ch)| *ch == '{') {
                        chars.next(); // consume '{'
                        let mut depth = 1;
                        let mut inner_text = String::new();
                        for (_, ch) in chars.by_ref() {
                            if ch == '{' {
                                depth += 1;
                                inner_text.push(ch);
                            } else if ch == '}' {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                                inner_text.push(ch);
                            } else {
                                inner_text.push(ch);
                            }
                        }
                        out.push_str(&inner_text);
                        continue;
                    }
                }
            } else if let Some((_, ch)) = chars.peek().copied() {
                match ch {
                    ',' | ';' | ':' => {
                        chars.next();
                        out.push(' ');
                        continue;
                    }
                    '!' => {
                        chars.next();
                        continue;
                    }
                    '{' | '}' => {
                        chars.next();
                        out.push(ch);
                        continue;
                    }
                    _ => {}
                }
            }
        }
        out.push(c);
    }
    out
}

fn push_styled_text(text: &str, style: Style, out: &mut Vec<Span<'static>>) {
    if text.is_empty() {
        return;
    }
    if text.contains(r"\rightarrow") {
        let replaced = text.replace(r"\rightarrow", "→");
        out.push(Span::styled(replaced, style));
    } else {
        out.push(Span::styled(text.to_string(), style));
    }
}

/// Single left-to-right pass over the input: see the precedence rules in
/// `marker_at`/`can_open_math` — strictly earliest construct wins, escapes
/// beat a marker at the same-or-later position, links only when strictly
/// earlier, longer marker wins ties.
fn push_inline(text: &str, style: Style, out: &mut Vec<Span<'static>>) {
    let ambient_bg = style.bg.unwrap_or(Theme::BG());
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut lit_start = 0usize;
    // Every cursor below sits on an ASCII token boundary, so slicing `text`
    // at it is always safe.
    let flush = |out: &mut Vec<Span<'static>>, up_to: usize, lit_start: &mut usize| {
        if up_to > *lit_start {
            push_styled_text(&text[*lit_start..up_to], style, out);
            *lit_start = up_to;
        }
    };
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\\' {
            let valid = text[i + 1..]
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_punctuation());
            if valid {
                flush(out, i, &mut lit_start);
                let ch = text[i + 1..].chars().next().unwrap();
                out.push(Span::styled(ch.to_string(), style));
                i += 1 + ch.len_utf8();
                lit_start = i;
            } else {
                i += 1;
            }
            continue;
        }
        if b == b'[' || b == b'<' || (b == b'!' && bytes.get(i + 1) == Some(&b'[')) {
            let parsed = if b == b'<' {
                parse_autolink(&text[i..])
            } else if b == b'!' {
                parse_md_link(&text[i + 1..], true)
            } else {
                parse_md_link(&text[i..], false)
            };
            if let Some(kind) = parsed {
                flush(out, i, &mut lit_start);
                // links are the sparse accent: house orange, underlined.
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
                        // `len` already covers a leading `!` for images
                        i += len;
                        lit_start = i;
                    }
                    InlineLink::Auto { len, url } => {
                        out.push(Span::styled(url, link_style));
                        i += len;
                        lit_start = i;
                    }
                }
                continue;
            }
            i += 1;
            continue;
        }
        let Some(marker) = marker_at(bytes, i) else {
            i += text[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
            continue;
        };
        if marker.contains('_') && !can_open_us(text, i) {
            i += 1;
            continue;
        }
        if (marker == "$" || marker == "$$") && !can_open_math(text, i, marker.len()) {
            i += 1;
            continue;
        }
        // Bounded closer search: one linear walk, `_` closers filtered by
        // the word rule. Anything past the cap stays literal.
        let from = i + marker.len();
        let limit = floor_boundary(text, (from + MAX_EMPHASIS_SPAN).min(text.len()));
        let mut close = None;
        let mut p = from;
        while p + marker.len() <= limit {
            if text[p..].starts_with(marker)
                && (!marker.contains('_') || can_close_us(text, p + marker.len()))
                && (marker != "$" || can_close_math(text, p))
            {
                close = Some(p);
                break;
            }
            p += text[p..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        }
        let Some(inner_end) = close else {
            flush(out, i, &mut lit_start);
            out.push(Span::styled(marker.to_string(), style));
            i += marker.len();
            lit_start = i;
            continue;
        };
        if inner_end == from {
            // empty markers like `` — literal
            flush(out, i, &mut lit_start);
            out.push(Span::styled(marker.to_string(), style));
            i += marker.len();
            lit_start = i;
            continue;
        }
        flush(out, i, &mut lit_start);
        let inner = &text[from..inner_end];
        match marker {
            "`" | "``" => {
                // white code, but never drops the ambient background: inside
                // a quote the band must continue uninterrupted behind `code`
                // (and bold/links keep it by deriving from `style` below).
                // Inside a colored band the band color wins on fg too, so the
                // strip stays monochrome instead of flashing white.
                let mut cs = code_style();
                if let Some(bg) = style.bg {
                    cs = cs.bg(bg);
                }
                if let Some(fg) = style.fg
                    && fg != Color::Gray
                {
                    cs = cs.fg(fg);
                }
                out.push(Span::styled(inner.to_string(), cs));
            }
            "$" | "$$" => {
                let rendered = render_math(inner);
                out.push(Span::styled(rendered, style));
            }
            "***" | "___" => push_inline(inner, with_emphasis(style, Modifier::BOLD), out),
            "**" | "__" => push_inline(inner, with_emphasis(style, Modifier::BOLD), out),
            "*" | "_" => push_inline(inner, style.add_modifier(Modifier::ITALIC), out),
            "~~" => push_inline(inner, style.add_modifier(Modifier::CROSSED_OUT), out),
            _ => unreachable!(),
        }
        i = inner_end + marker.len();
        lit_start = i;
    }
    flush(out, text.len(), &mut lit_start);
}

thread_local! {
    /// Call counter (also feeds the `/debug` perf log), per thread: the
    /// idle-frame tests assert "this frame wrapped nothing", and a process-wide
    /// counter would let any other test thread rendering in parallel trip them.
    static WRAP_TAGGED_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Wraps counted on this thread.
pub fn wrap_tagged_calls() -> usize {
    WRAP_TAGGED_CALLS.with(|c| c.get())
}

/// Zero the counter on this thread (idle-frame tests).
#[cfg(test)]
pub fn reset_wrap_tagged_calls() {
    WRAP_TAGGED_CALLS.with(|c| c.set(0));
}

/// Replace control characters with their visible terminal behavior before
/// any width math runs. Ratatui models them as zero-width cells while the
/// real terminal acts on them — that disagreement shifts every later cell
/// and sprays neighbor-row fragments across the screen (seen on tool
/// expand/collapse with tabbed output like `read`'s `{:>6}\tline` rows or
/// `\r` progress bars). Tabs become spaces to the next multiple-of-8 stop,
/// a carriage return keeps only the tail after its last occurrence
/// (overwrite semantics), and other C0/C1 controls are dropped for lack of
/// a form both sides agree on.
/// `\n` passes through: the wrapper below splits rows on it. Lines without
/// controls return untouched (no allocation on the streaming hot path).
fn sanitize_line(line: Line<'static>) -> Line<'static> {
    if !line
        .spans
        .iter()
        .any(|s| s.content.chars().any(|c| c.is_control()))
    {
        return line;
    }
    let mut spans = Vec::with_capacity(line.spans.len());
    let mut col = 0usize;
    for span in line.spans {
        let text = span.content.as_ref();
        // A carriage return overwrites the row: anything before the last one
        // was never visible (progress bars). But a `\r` that only *terminates*
        // a line — `\r\n`, which every Windows-native command emits — is not an
        // overwrite. Cutting at it left only the bytes after the final CRLF, so
        // a whole tool-output line rendered as nothing. `\r` is one byte, so
        // `i + 1` is a char boundary either way; a surviving `\r` is dropped by
        // the control loop below.
        let overwrite = text
            .rfind('\r')
            .filter(|&i| text.as_bytes().get(i + 1) != Some(&b'\n'));
        let text = match overwrite {
            Some(i) => &text[i + 1..],
            None => text,
        };
        let mut buf = String::with_capacity(text.len());
        for ch in text.chars() {
            if ch == '\t' {
                let spaces = 8 - col % 8;
                for _ in 0..spaces {
                    buf.push(' ');
                }
                col += spaces;
            } else if ch == '\n' {
                buf.push(ch);
                col = 0;
            } else if ch.is_control() {
                // dropped: zero width on both sides, no visible form
            } else {
                col += UnicodeWidthChar::width(ch).unwrap_or(0);
                buf.push(ch);
            }
        }
        spans.push(Span::styled(buf, span.style));
    }
    Line::from(spans)
}

pub fn wrap_tagged(
    lines: Vec<(Line<'static>, Option<usize>)>,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    wrap_tagged_inner(lines, width, true)
}

pub(crate) fn wrap_tagged_uncounted(
    lines: Vec<(Line<'static>, Option<usize>)>,
    width: u16,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    wrap_tagged_inner(lines, width, false)
}

fn wrap_tagged_inner(
    lines: Vec<(Line<'static>, Option<usize>)>,
    width: u16,
    count_call: bool,
) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    if count_call {
        WRAP_TAGGED_CALLS.with(|c| c.set(c.get() + 1));
    }
    let width = (width as usize).max(1);
    let fallback = Style::new().fg(Theme::rule_color()).bg(Theme::BG());

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut tags: Vec<Option<usize>> = Vec::new();

    for (line, tag) in lines {
        // tool outputs and pasted text carry raw terminal controls. Ratatui
        // treats them as zero-width cells, but the real terminal EXECUTES
        // them (tab stops, carriage return), desyncing every cell after the
        // character: fragments of neighboring rows spray across toggles.
        let line = sanitize_line(line);
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
        let mut prev_zwj = false;
        for (st, ch) in cells {
            if ch == '\n' {
                // explicit line break: finish the row instead of dropping it
                trim_end_spaces(&mut cur);
                rows.push(cells_to_line(std::mem::take(&mut cur), fallback));
                tags.push(tag);
                cur_width = 0;
                last_space = None;
                prev_zwj = false;
                continue;
            }
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            // zero-width marks always join the current row, even past budget;
            // a joiner continuation never starts a new row either, matching
            // cell_rows/wrap_code_line so all wrappers share one width model
            if ch_width > 0 && !prev_zwj && cur_width + ch_width > width && !cur.is_empty() {
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
            prev_zwj = ch == '\u{200d}';
        }
        trim_end_spaces(&mut cur);
        if !cur.is_empty() {
            rows.push(cells_to_line(cur, fallback));
            tags.push(tag);
        }
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
mod tests;
