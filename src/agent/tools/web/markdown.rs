//! Structural HTML-to-Markdown converter with DOM Readability extraction.
//!
//! Replaces naive tag-stripping so documentation pages keep their headings,
//! fenced code blocks (with newlines and indentation intact), Markdown tables,
//! nested lists, definition lists, and resolved links while discarding site
//! chrome (`<nav>`, `<footer>`, `<aside>`, cookie banners, sidebars).

use reqwest::Url;
use scraper::{ElementRef, Html, Node, Selector};

/// Result of parsing an HTML page into structured Markdown.
#[derive(Debug, Clone)]
pub struct ExtractedDoc {
    pub title: Option<String>,
    pub markdown: String,
    /// `<link rel="alternate" type="text/markdown" href="...">` (llms.txt v2).
    pub alternate_markdown_url: Option<String>,
    /// `<link rel="describedby" href="...llms.txt">` (llms.txt v2).
    pub llms_txt_url: Option<String>,
}

/// Convert a raw HTML document into clean, structured GitHub-flavored Markdown.
pub fn html_to_markdown(html: &str, base_url: Option<&Url>) -> ExtractedDoc {
    let doc = Html::parse_document(html);
    let title = extract_title(&doc);
    let (alternate_markdown_url, llms_txt_url) = extract_llm_links(&doc, base_url);

    let root = select_content_root(&doc);
    let mut conv = DomConverter::new(base_url);
    match root {
        Some(el) => conv.walk_element(el),
        None => conv.walk_children(doc.root_element()),
    }

    let mut markdown = conv.finish();

    // Prepend document title as H1 only if the body does not already contain a top-level H1 near the start.
    let has_leading_h1 = markdown.starts_with("# ")
        || markdown.lines().take(8).any(|l| l.starts_with("# "));
    if let Some(ref t) = title
        && !t.is_empty()
        && !has_leading_h1
    {
        if markdown.is_empty() {
            markdown = format!("# {t}");
        } else {
            markdown = format!("# {t}\n\n{markdown}");
        }
    }

    ExtractedDoc {
        title,
        markdown,
        alternate_markdown_url,
        llms_txt_url,
    }
}

/// Extract plain text or Markdown for a specific CSS selector.
pub fn extract_selector_markdown(
    html: &str,
    selector_str: &str,
    base_url: Option<&Url>,
) -> Result<String, String> {
    let parsed = Selector::parse(selector_str)
        .map_err(|e| format!("invalid selector '{selector_str}': {e}"))?;
    let doc = Html::parse_document(html);
    let mut blocks: Vec<String> = Vec::new();
    for element in doc.select(&parsed) {
        let mut conv = DomConverter::new(base_url);
        conv.walk_element(element);
        let rendered = conv.finish();
        if !rendered.is_empty() {
            blocks.push(rendered);
        }
        if blocks.len() >= 200 {
            break;
        }
    }
    if blocks.is_empty() {
        Err(format!(
            "selector '{selector_str}' matched nothing (or only empty elements)"
        ))
    } else {
        Ok(blocks.join("\n\n"))
    }
}

fn extract_title(doc: &Html) -> Option<String> {
    if let Ok(sel) = Selector::parse("meta[property='og:title'], meta[name='twitter:title']") {
        for el in doc.select(&sel) {
            if let Some(content) = el.value().attr("content") {
                let cleaned = collapse_inline_ws(content);
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
        }
    }
    if let Ok(sel) = Selector::parse("title")
        && let Some(el) = doc.select(&sel).next()
    {
        let raw: String = el.text().collect();
        let cleaned = collapse_inline_ws(&raw);
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    if let Ok(sel) = Selector::parse("h1")
        && let Some(el) = doc.select(&sel).next()
    {
        let raw: String = el.text().collect();
        let cleaned = clean_heading_text(&collapse_inline_ws(&raw));
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    None
}

fn extract_llm_links(doc: &Html, base_url: Option<&Url>) -> (Option<String>, Option<String>) {
    let mut alt_md = None;
    let mut llms_txt = None;
    if let Ok(sel) = Selector::parse("link[rel][href]") {
        for el in doc.select(&sel) {
            let rel = el
                .value()
                .attr("rel")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let ty = el
                .value()
                .attr("type")
                .unwrap_or_default()
                .to_ascii_lowercase();
            let Some(href) = el.value().attr("href").map(str::trim) else {
                continue;
            };
            if href.is_empty() {
                continue;
            }
            let resolved = resolve_url(base_url, href);
            if rel.split_whitespace().any(|r| r == "alternate")
                && (ty.contains("text/markdown") || ty.contains("text/x-markdown"))
                && alt_md.is_none()
            {
                alt_md = Some(resolved);
            } else if rel.split_whitespace().any(|r| r == "describedby")
                && href.contains("llms.txt")
                && llms_txt.is_none()
            {
                llms_txt = Some(resolved);
            }
        }
    }
    (alt_md, llms_txt)
}

/// Priority selectors for the main content container of documentation & articles.
const CONTENT_SELECTORS: &[&str] = &[
    "#main-content",
    "article.markdown-body",
    ".markdown-body",
    "main article",
    "article[role='main']",
    "[role='main']",
    "main",
    "article",
    ".doc-content",
    ".docs-content",
    ".documentation",
    ".rst-content",
    ".theme-doc-markdown",
    ".bd-content",
    ".post-content",
    ".entry-content",
    ".article-content",
    "#content",
    ".main-content",
];

fn select_content_root(doc: &Html) -> Option<ElementRef<'_>> {
    let mut best: Option<(ElementRef<'_>, i64)> = None;

    for (priority_idx, sel_str) in CONTENT_SELECTORS.iter().enumerate() {
        let Ok(sel) = Selector::parse(sel_str) else {
            continue;
        };
        for candidate in doc.select(&sel) {
            if is_noise_element(candidate) {
                continue;
            }
            let score = readability_score(candidate)
                + ((CONTENT_SELECTORS.len() - priority_idx) as i64 * 120);
            if score > 150 && best.as_ref().is_none_or(|(_, best_score)| score > *best_score) {
                best = Some((candidate, score));
            }
        }
        // If a high-priority semantic selector matched substantial content, use it.
        if priority_idx <= 6
            && let Some((el, score)) = best
            && score >= 400
        {
            return Some(el);
        }
    }

    if let Some((el, _)) = best {
        return Some(el);
    }

    if let Ok(body_sel) = Selector::parse("body")
        && let Some(body) = doc.select(&body_sel).next()
    {
        return Some(body);
    }
    None
}

/// Score an element for Readability: rewards prose, headings, and code blocks;
/// penalizes navigation-heavy link farms.
fn readability_score(el: ElementRef<'_>) -> i64 {
    let mut total_text = 0usize;
    let mut link_text = 0usize;
    let mut code_text = 0usize;
    let mut headings = 0usize;
    let mut paragraphs = 0usize;

    for node in el.descendants() {
        if let Some(child_el) = ElementRef::wrap(node) {
            let tag = child_el.value().name();
            match tag {
                "h1" | "h2" | "h3" | "h4" => headings += 1,
                "p" | "li" | "dd" => paragraphs += 1,
                "pre" | "code" => {
                    let n: usize = child_el.text().map(|t| t.trim().len()).sum();
                    code_text += n;
                }
                "a" => {
                    let n: usize = child_el.text().map(|t| t.trim().len()).sum();
                    link_text += n;
                }
                _ => {}
            }
        } else if let Node::Text(t) = node.value() {
            total_text += t.trim().len();
        }
    }

    if total_text < 40 && code_text < 20 {
        return 0;
    }

    let link_density = if total_text > 0 {
        (link_text as f64) / (total_text as f64)
    } else {
        1.0
    };

    let base = (total_text as i64)
        + (code_text as i64 * 2)
        + (headings as i64 * 80)
        + (paragraphs as i64 * 35);

    if link_density > 0.55 {
        base / 4
    } else if link_density > 0.35 {
        (base * 2) / 3
    } else {
        base
    }
}

fn is_noise_element(el: ElementRef<'_>) -> bool {
    let v = el.value();
    let tag = v.name();
    if matches!(
        tag,
        "script"
            | "style"
            | "noscript"
            | "template"
            | "svg"
            | "canvas"
            | "iframe"
            | "object"
            | "embed"
            | "audio"
            | "video"
            | "source"
            | "track"
            | "map"
            | "area"
            | "nav"
            | "footer"
            | "aside"
            | "form"
            | "button"
            | "input"
            | "select"
            | "textarea"
            | "dialog"
            | "menu"
            | "menuitem"
    ) {
        return true;
    }

    if v.attr("hidden").is_some()
        || v.attr("aria-hidden")
            .is_some_and(|a| a.eq_ignore_ascii_case("true"))
    {
        return true;
    }

    if let Some(style) = v.attr("style") {
        let s = style.to_ascii_lowercase().replace(' ', "");
        if s.contains("display:none") || s.contains("visibility:hidden") {
            return true;
        }
    }

    if let Some(role) = v.attr("role") {
        let r = role.trim().to_ascii_lowercase();
        if matches!(
            r.as_str(),
            "navigation"
                | "banner"
                | "contentinfo"
                | "complementary"
                | "search"
                | "menubar"
                | "toolbar"
                | "dialog"
                | "alertdialog"
        ) {
            return true;
        }
    }

    // Never strip top-level semantic content wrappers by class/id heuristics.
    if matches!(tag, "html" | "body" | "main" | "article")
        || v.attr("role")
            .is_some_and(|r| r.eq_ignore_ascii_case("main"))
        || v.id().is_some_and(|id| id == "main-content" || id == "content")
    {
        return false;
    }

    let class = v.attr("class").unwrap_or_default().to_ascii_lowercase();
    let id = v.attr("id").unwrap_or_default().to_ascii_lowercase();
    if class.is_empty() && id.is_empty() {
        return false;
    }

    const NOISE_PATTERNS: &[&str] = &[
        "cookie",
        "consent",
        "gdpr",
        "onetrust",
        "cybotcookiebot",
        "popup",
        "modal",
        "newsletter",
        "sidebar",
        "sidenav",
        "side-nav",
        "navbar",
        "nav-bar",
        "topbar",
        "top-bar",
        "site-header",
        "site-footer",
        "page-footer",
        "global-nav",
        "breadcrumb",
        "pagination",
        "share-button",
        "social-link",
        "advertisement",
        "ad-banner",
        "promo-banner",
        "skip-link",
        "skip-to-content",
        "sidebar-elems",
        "rustdoc-breadcrumbs",
        "out-of-band",
        "tooltip",
        "sr-only",
        "visually-hidden",
    ];

    for pat in NOISE_PATTERNS {
        if class.contains(pat) || id.contains(pat) {
            return true;
        }
    }
    false
}

struct DomConverter<'a> {
    base_url: Option<&'a Url>,
    out: String,
    list_stack: Vec<ListKind>,
    in_pre: bool,
    in_heading: bool,
    blockquote_depth: usize,
}

#[derive(Clone, Copy)]
enum ListKind {
    Unordered,
    Ordered(usize),
}

impl<'a> DomConverter<'a> {
    fn new(base_url: Option<&'a Url>) -> Self {
        Self {
            base_url,
            out: String::with_capacity(4096),
            list_stack: Vec::new(),
            in_pre: false,
            in_heading: false,
            blockquote_depth: 0,
        }
    }

    fn walk_children(&mut self, el: ElementRef<'_>) {
        for child in el.children() {
            match child.value() {
                Node::Text(text) => {
                    if self.in_pre {
                        self.out.push_str(text);
                    } else {
                        self.push_inline_text(text);
                    }
                }
                Node::Element(_) => {
                    if let Some(child_el) = ElementRef::wrap(child) {
                        self.walk_element(child_el);
                    }
                }
                _ => {}
            }
        }
    }

    fn walk_element(&mut self, el: ElementRef<'_>) {
        if is_noise_element(el) {
            return;
        }

        let tag = el.value().name();
        match tag {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = tag[1..].parse::<usize>().unwrap_or(2).clamp(1, 6);
                let text = self.collect_inline_clean(el);
                let cleaned = clean_heading_text(&text);
                if !cleaned.is_empty() {
                    self.ensure_blank_line();
                    self.out.push_str(&"#".repeat(level));
                    self.out.push(' ');
                    self.out.push_str(&cleaned);
                    self.ensure_blank_line();
                }
            }
            "pre" => {
                self.render_pre(el);
            }
            "code" | "tt" | "kbd" | "samp" => {
                if self.in_pre {
                    self.walk_children(el);
                } else {
                    let raw: String = el.text().collect();
                    let cleaned = collapse_inline_ws(&raw);
                    if !cleaned.is_empty() {
                        self.maybe_space_before_inline();
                        if cleaned.contains('`') {
                            self.out.push_str("`` ");
                            self.out.push_str(&cleaned);
                            self.out.push_str(" ``");
                        } else {
                            self.out.push('`');
                            self.out.push_str(&cleaned);
                            self.out.push('`');
                        }
                    }
                }
            }
            "p" => {
                self.ensure_blank_line();
                self.emit_blockquote_prefix();
                self.walk_children(el);
                self.ensure_blank_line();
            }
            "br" => {
                self.out.push('\n');
                if !self.in_pre {
                    self.emit_blockquote_prefix();
                }
            }
            "hr" => {
                self.ensure_blank_line();
                self.out.push_str("---");
                self.ensure_blank_line();
            }
            "blockquote" => {
                self.ensure_blank_line();
                self.blockquote_depth += 1;
                self.emit_blockquote_prefix();
                self.walk_children(el);
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.ensure_blank_line();
            }
            "ul" => {
                if self.list_stack.is_empty() {
                    self.ensure_blank_line();
                } else {
                    self.ensure_newline();
                }
                self.list_stack.push(ListKind::Unordered);
                self.walk_children(el);
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.ensure_blank_line();
                }
            }
            "ol" => {
                if self.list_stack.is_empty() {
                    self.ensure_blank_line();
                } else {
                    self.ensure_newline();
                }
                let start = el
                    .value()
                    .attr("start")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1);
                self.list_stack.push(ListKind::Ordered(start));
                self.walk_children(el);
                self.list_stack.pop();
                if self.list_stack.is_empty() {
                    self.ensure_blank_line();
                }
            }
            "li" => {
                self.ensure_newline();
                let depth = self.list_stack.len().saturating_sub(1);
                let indent = "  ".repeat(depth);
                let marker = match self.list_stack.last_mut() {
                    Some(ListKind::Ordered(n)) => {
                        let current = *n;
                        *n += 1;
                        format!("{indent}{current}. ")
                    }
                    _ => format!("{indent}- "),
                };
                self.out.push_str(&marker);
                self.walk_children(el);
                self.ensure_newline();
            }
            "dl" => {
                self.ensure_blank_line();
                self.walk_children(el);
                self.ensure_blank_line();
            }
            "dt" => {
                let term = self.collect_inline_clean(el);
                if !term.is_empty() {
                    self.ensure_blank_line();
                    self.out.push_str("**");
                    self.out.push_str(&term);
                    self.out.push_str("**");
                    self.ensure_newline();
                }
            }
            "dd" => {
                self.ensure_newline();
                self.out.push_str(": ");
                self.walk_children(el);
                self.ensure_newline();
            }
            "table" => {
                self.render_table(el);
            }
            "a" => {
                self.render_link(el);
            }
            "strong" | "b" => {
                let inner = self.collect_inline_clean(el);
                if !inner.is_empty() {
                    self.maybe_space_before_inline();
                    self.out.push_str("**");
                    self.out.push_str(&inner);
                    self.out.push_str("**");
                }
            }
            "em" | "i" => {
                let inner = self.collect_inline_clean(el);
                if !inner.is_empty() {
                    self.maybe_space_before_inline();
                    self.out.push('*');
                    self.out.push_str(&inner);
                    self.out.push('*');
                }
            }
            "del" | "s" => {
                let inner = self.collect_inline_clean(el);
                if !inner.is_empty() {
                    self.maybe_space_before_inline();
                    self.out.push_str("~~");
                    self.out.push_str(&inner);
                    self.out.push_str("~~");
                }
            }
            "div" | "section" | "article" | "main" | "header" | "details" | "summary"
            | "figure" | "figcaption" => {
                if !self.in_pre && self.list_stack.is_empty() {
                    self.ensure_newline();
                }
                self.walk_children(el);
                if !self.in_pre && self.list_stack.is_empty() {
                    self.ensure_newline();
                }
            }
            _ => {
                self.walk_children(el);
            }
        }
    }

    fn render_pre(&mut self, el: ElementRef<'_>) {
        let lang = detect_code_language(el);
        let mut raw = String::new();
        collect_pre_text(el, &mut raw);
        let trimmed = raw.trim_matches('\n').trim_end();
        if trimmed.is_empty() {
            return;
        }
        self.ensure_blank_line();
        let fence = if trimmed.contains("```") {
            "~~~~"
        } else {
            "```"
        };
        self.out.push_str(fence);
        if let Some(l) = lang {
            self.out.push_str(&l);
        }
        self.out.push('\n');
        self.out.push_str(trimmed);
        self.out.push('\n');
        self.out.push_str(fence);
        self.ensure_blank_line();
    }

    fn render_link(&mut self, el: ElementRef<'_>) {
        let Some(href) = el.value().attr("href").map(str::trim) else {
            self.walk_children(el);
            return;
        };
        if href.is_empty()
            || href.starts_with('#')
            || href.to_ascii_lowercase().starts_with("javascript:")
            || href.to_ascii_lowercase().starts_with("mailto:")
        {
            self.walk_children(el);
            return;
        }

        let label = self.collect_inline_clean(el);
        if label.is_empty()
            || matches!(
                label.as_str(),
                "¶" | "#" | "§" | "source" | "[src]" | "src"
            )
        {
            return;
        }

        let resolved = resolve_url(self.base_url, href);
        self.maybe_space_before_inline();
        if label == resolved || label == href {
            self.out.push_str(&resolved);
        } else {
            self.out.push('[');
            self.out.push_str(&label);
            self.out.push_str("](");
            self.out.push_str(&resolved);
            self.out.push(')');
        }
    }

    fn render_table(&mut self, el: ElementRef<'_>) {
        // Many syntax highlighters wrap code in a 2-column table (<td class="lineno"> + <td class="code">).
        if let Ok(pre_sel) = Selector::parse("pre") {
            let pres: Vec<_> = el.select(&pre_sel).collect();
            if !pres.is_empty() {
                // Render the last <pre> (which holds the code column, not line numbers).
                if let Some(code_pre) = pres.last() {
                    self.render_pre(*code_pre);
                    return;
                }
            }
        }

        let Ok(tr_sel) = Selector::parse("tr") else {
            return;
        };
        let Ok(cell_sel) = Selector::parse("th, td") else {
            return;
        };

        let mut rows: Vec<Vec<String>> = Vec::new();
        for tr in el.select(&tr_sel) {
            let mut row: Vec<String> = Vec::new();
            for cell in tr.select(&cell_sel) {
                let text = self.collect_table_cell(cell);
                row.push(text);
            }
            if row.iter().any(|c| !c.is_empty()) {
                rows.push(row);
            }
            if rows.len() >= 150 {
                break;
            }
        }

        let Some(cols) = rows.iter().map(Vec::len).max() else {
            return;
        };
        if cols == 0 {
            return;
        }

        self.ensure_blank_line();
        for (idx, row) in rows.iter().enumerate() {
            self.out.push_str("| ");
            for c in 0..cols {
                if c > 0 {
                    self.out.push_str(" | ");
                }
                self.out.push_str(row.get(c).map(String::as_str).unwrap_or(""));
            }
            self.out.push_str(" |\n");
            if idx == 0 {
                self.out.push_str("| ");
                for c in 0..cols {
                    if c > 0 {
                        self.out.push_str(" | ");
                    }
                    self.out.push_str("---");
                }
                self.out.push_str(" |\n");
            }
        }
        self.ensure_blank_line();
    }

    /// Render a `<th>` or `<td>` cell: preserves inline `code`, **bold**, and
    /// `[links](url)`, and converts internal block breaks (`<br>`, `<p>`, `<li>`)
    /// into `<br>` so multi-line cell content never breaks Markdown table rows.
    fn collect_table_cell(&self, cell: ElementRef<'_>) -> String {
        let mut sub = DomConverter::new(self.base_url);
        sub.walk_children(cell);
        let raw = sub.finish();
        let lines: Vec<String> = raw
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.replace('|', "\\|"))
            .collect();
        lines.join(" <br> ")
    }

    fn collect_inline_clean(&self, el: ElementRef<'_>) -> String {
        let mut sub = DomConverter::new(self.base_url);
        sub.in_heading = true;
        for child in el.children() {
            if let Some(child_el) = ElementRef::wrap(child) {
                let tag = child_el.value().name();
                if matches!(tag, "code" | "tt" | "kbd" | "samp") {
                    let raw: String = child_el.text().collect();
                    let cleaned = collapse_inline_ws(&raw);
                    if !cleaned.is_empty() {
                        sub.maybe_space_before_inline();
                        sub.out.push('`');
                        sub.out.push_str(&cleaned);
                        sub.out.push('`');
                    }
                    continue;
                }
            }
            if let Node::Text(t) = child.value() {
                sub.push_inline_text(t);
            } else if let Some(child_el) = ElementRef::wrap(child)
                && !is_noise_element(child_el)
            {
                let inner: String = child_el.text().collect();
                sub.push_inline_text(&inner);
            }
        }
        sub.out.trim().to_string()
    }

    fn push_inline_text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let starts_ws = raw.chars().next().is_some_and(char::is_whitespace);
        let ends_ws = raw.chars().last().is_some_and(char::is_whitespace);
        let collapsed = collapse_inline_ws(raw);
        if collapsed.is_empty() {
            if starts_ws && !self.out.ends_with([' ', '\n', '[', '(', '*', '`']) {
                self.out.push(' ');
            }
            return;
        }
        if starts_ws && !self.out.is_empty() && !self.out.ends_with([' ', '\n', '[', '(']) {
            self.out.push(' ');
        }
        self.out.push_str(&collapsed);
        if ends_ws && !self.out.ends_with(' ') {
            self.out.push(' ');
        }
    }

    fn maybe_space_before_inline(&mut self) {
        if let Some(last) = self.out.chars().last()
            && !last.is_whitespace()
            && !matches!(last, '[' | '(' | '>' | '-' | '*')
        {
            self.out.push(' ');
        }
    }

    fn emit_blockquote_prefix(&mut self) {
        for _ in 0..self.blockquote_depth {
            self.out.push_str("> ");
        }
    }

    fn ensure_newline(&mut self) {
        while self.out.ends_with(' ') || self.out.ends_with('\t') {
            self.out.pop();
        }
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn ensure_blank_line(&mut self) {
        while self.out.ends_with(' ') || self.out.ends_with('\t') {
            self.out.pop();
        }
        if self.out.is_empty() {
            return;
        }
        if self.out.ends_with("\n\n") {
            return;
        }
        if self.out.ends_with('\n') {
            self.out.push('\n');
        } else {
            self.out.push_str("\n\n");
        }
    }

    fn finish(self) -> String {
        normalize_markdown(&self.out)
    }
}

fn collect_pre_text(el: ElementRef<'_>, out: &mut String) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                if e.name() == "br" {
                    out.push('\n');
                } else if let Some(child_el) = ElementRef::wrap(child) {
                    if is_noise_element(child_el) || is_code_gutter(e) {
                        continue;
                    }
                    // Some syntax highlighters put each line in a `<span class="line">` or `<div>` without `\n`.
                    let is_line_block = matches!(e.name(), "div" | "p")
                        || e.attr("class")
                            .is_some_and(|c| c.split_whitespace().any(|k| k == "line"));
                    collect_pre_text(child_el, out);
                    if is_line_block && !out.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
}

fn is_code_gutter(e: &scraper::node::Element) -> bool {
    if e.attr("data-nosnippet").is_some() || e.attr("data-line-number").is_some() {
        return true;
    }
    let Some(class) = e.attr("class") else {
        return false;
    };
    class.split_whitespace().any(|c| {
        matches!(
            c,
            "lineno"
                | "linenos"
                | "line-number"
                | "line-numbers"
                | "src-line-numbers"
                | "hljs-ln-numbers"
        )
    })
}

fn detect_code_language(pre: ElementRef<'_>) -> Option<String> {
    let mut classes = Vec::new();
    if let Some(c) = pre.value().attr("class") {
        classes.push(c);
    }
    if let Some(lang) = pre.value().attr("data-lang").or_else(|| pre.value().attr("data-language"))
    {
        let cleaned = sanitize_lang(lang);
        if !cleaned.is_empty() {
            return Some(cleaned);
        }
    }
    if let Ok(code_sel) = Selector::parse("code") {
        for code in pre.select(&code_sel) {
            if let Some(c) = code.value().attr("class") {
                classes.push(c);
            }
            if let Some(lang) = code
                .value()
                .attr("data-lang")
                .or_else(|| code.value().attr("data-language"))
            {
                let cleaned = sanitize_lang(lang);
                if !cleaned.is_empty() {
                    return Some(cleaned);
                }
            }
        }
    }

    for class_str in classes {
        for token in class_str.split_whitespace() {
            let lower = token.to_ascii_lowercase();
            for prefix in ["language-", "lang-", "highlight-source-", "brush:"] {
                if let Some(rest) = lower.strip_prefix(prefix) {
                    let cleaned = sanitize_lang(rest);
                    if !cleaned.is_empty() {
                        return Some(cleaned);
                    }
                }
            }
            if matches!(
                lower.as_str(),
                "rust"
                    | "python"
                    | "py"
                    | "javascript"
                    | "js"
                    | "typescript"
                    | "ts"
                    | "tsx"
                    | "jsx"
                    | "go"
                    | "golang"
                    | "c"
                    | "cpp"
                    | "csharp"
                    | "java"
                    | "kotlin"
                    | "swift"
                    | "ruby"
                    | "php"
                    | "shell"
                    | "bash"
                    | "sh"
                    | "zsh"
                    | "powershell"
                    | "ps1"
                    | "json"
                    | "toml"
                    | "yaml"
                    | "yml"
                    | "xml"
                    | "html"
                    | "css"
                    | "sql"
                    | "diff"
            ) {
                return Some(lower);
            }
        }
    }
    None
}

fn sanitize_lang(raw: &str) -> String {
    raw.chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '+')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn resolve_url(base_url: Option<&Url>, href: &str) -> String {
    if let Some(base) = base_url
        && let Ok(joined) = base.join(href)
    {
        return joined.to_string();
    }
    href.to_string()
}

fn collapse_inline_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_heading_text(s: &str) -> String {
    s.trim()
        .trim_start_matches('§')
        .trim_end_matches(['¶', '§', '#'])
        .trim_end_matches("source")
        .trim_end_matches("[src]")
        .trim()
        .to_string()
}

/// Normalize Markdown spacing while keeping fenced code blocks byte-exact.
pub fn normalize_markdown(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut in_fence: Option<&str> = None;
    let mut blank_run = 0usize;

    for line in raw.lines() {
        let trimmed_start = line.trim_start();
        if let Some(fence) = in_fence {
            out.push_str(line);
            out.push('\n');
            if trimmed_start.starts_with(fence) {
                in_fence = None;
            }
            continue;
        }

        if trimmed_start.starts_with("~~~~") {
            in_fence = Some("~~~~");
            blank_run = 0;
            out.push_str(line.trim_end());
            out.push('\n');
            continue;
        }
        if trimmed_start.starts_with("```") {
            in_fence = Some("```");
            blank_run = 0;
            out.push_str(line.trim_end());
            out.push('\n');
            continue;
        }

        let trimmed = line.trim_end();
        // Skip raw HTML container tags (`<div ...>`, `</div>`) that some servers
        // (e.g. Discourse `text/markdown`) emit outside code fences.
        let t_both = trimmed.trim();
        if (t_both.starts_with("<div") && t_both.ends_with('>')) || t_both == "</div>" {
            continue;
        }
        if trimmed.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push('\n');
            }
        } else {
            blank_run = 0;
            out.push_str(trimmed);
            out.push('\n');
        }
    }

    out.trim().to_string()
}
