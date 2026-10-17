//! Section chunking, document outline generation, in-memory LRU page cache,
//! URL canonicalization, and local Okapi BM25 ranking with code-block boosting.

use crate::agent::tools::trim_middle;
use reqwest::Url;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const CACHE_CAP: usize = 32;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
/// Pages at or below this size are returned in full when no `query`/`section` filter is set.
pub const FULL_PAGE_CHARS: usize = 22_000;
/// Maximum characters returned from `project_page`.
pub const MAX_PROJECTED_CHARS: usize = 28_000;
/// Maximum size of a single chunk before paragraph subdivision.
const MAX_SECTION_CHARS: usize = 5_500;

#[derive(Debug, Clone)]
pub struct PageSection {
    pub index: usize,
    pub level: u8,
    pub heading: String,
    pub breadcrumb: String,
    pub body: String,
    pub char_len: usize,
    pub has_code_block: bool,
}

#[derive(Debug, Clone)]
pub struct CachedPage {
    pub canonical_url: String,
    pub title: Option<String>,
    pub markdown: String,
    pub sections: Vec<PageSection>,
    pub fetched_at: Instant,
}

static PAGE_CACHE: LazyLock<Mutex<VecDeque<CachedPage>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(CACHE_CAP)));

pub fn get_cached_page(url: &str) -> Option<CachedPage> {
    let key = canonicalize_url(url);
    let mut guard = PAGE_CACHE.lock().ok()?;
    let now = Instant::now();
    guard.retain(|entry| now.duration_since(entry.fetched_at) < CACHE_TTL);
    if let Some(pos) = guard.iter().position(|e| e.canonical_url == key) {
        let page = guard.remove(pos)?;
        guard.push_front(page.clone());
        return Some(page);
    }
    None
}

pub fn put_cached_page(url: &str, title: Option<String>, markdown: String) -> CachedPage {
    let canonical_url = canonicalize_url(url);
    let sections = parse_sections(&markdown);
    let page = CachedPage {
        canonical_url: canonical_url.clone(),
        title,
        markdown,
        sections,
        fetched_at: Instant::now(),
    };
    if let Ok(mut guard) = PAGE_CACHE.lock() {
        let now = Instant::now();
        guard.retain(|e| {
            now.duration_since(e.fetched_at) < CACHE_TTL && e.canonical_url != canonical_url
        });
        if guard.len() >= CACHE_CAP {
            guard.pop_back();
        }
        guard.push_front(page.clone());
    }
    page
}

/// Normalize a URL for deduplication and cache keys: strips tracking query
/// parameters, `#fragment`, `www.` prefix, and trailing slashes.
pub fn canonicalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let Ok(mut parsed) = Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    parsed.set_fragment(None);
    if parsed.scheme() == "http" {
        let _ = parsed.set_scheme("https");
    }
    if let Some(host) = parsed.host_str().map(|h| h.to_ascii_lowercase()) {
        let stripped = host.strip_prefix("www.").unwrap_or(&host).to_string();
        let _ = parsed.set_host(Some(&stripped));
    }

    let kept_pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .filter(|(k, _)| {
            let lower = k.to_ascii_lowercase();
            !lower.starts_with("utm_")
                && !matches!(
                    lower.as_str(),
                    "fbclid"
                        | "gclid"
                        | "msclkid"
                        | "mc_cid"
                        | "mc_eid"
                        | "_hsenc"
                        | "_hsmi"
                        | "ref"
                        | "ref_src"
                        | "spm"
                )
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    if kept_pairs.is_empty() {
        parsed.set_query(None);
    } else {
        let mut ser = url_query_string(&kept_pairs);
        if ser.is_empty() {
            parsed.set_query(None);
        } else {
            parsed.set_query(Some(&ser));
        }
        let _ = &mut ser;
    }

    let mut out = parsed.to_string();
    if out.ends_with('/') && parsed.path() != "/" {
        out.pop();
    } else if out.ends_with('/') && parsed.path() == "/" && parsed.query().is_none() {
        out.pop();
    }
    out
}

fn url_query_string(pairs: &[(String, String)]) -> String {
    let mut u = Url::parse("https://example.com").unwrap();
    {
        let mut qp = u.query_pairs_mut();
        for (k, v) in pairs {
            qp.append_pair(k, v);
        }
    }
    u.query().unwrap_or_default().to_string()
}

/// Split a Markdown document into logical sections by `#..####` headings,
/// respecting fenced code blocks so `# comments` inside code are never split.
pub fn parse_sections(markdown: &str) -> Vec<PageSection> {
    let mut raw_sections: Vec<(u8, String, String, String)> = Vec::new();
    let mut stack: Vec<(u8, String)> = Vec::new();

    let mut cur_level = 0u8;
    let mut cur_heading = "Overview".to_string();
    let mut cur_breadcrumb = "Overview".to_string();
    let mut cur_body = String::new();
    let mut in_fence: Option<&str> = None;

    for line in markdown.lines() {
        let trimmed_start = line.trim_start();
        if let Some(fence) = in_fence {
            cur_body.push_str(line);
            cur_body.push('\n');
            if trimmed_start.starts_with(fence) {
                in_fence = None;
            }
            continue;
        }
        if trimmed_start.starts_with("~~~~") {
            in_fence = Some("~~~~");
            cur_body.push_str(line);
            cur_body.push('\n');
            continue;
        }
        if trimmed_start.starts_with("```") {
            in_fence = Some("```");
            cur_body.push_str(line);
            cur_body.push('\n');
            continue;
        }

        if let Some((level, heading_text)) = parse_heading_line(line) {
            if !cur_body.trim().is_empty() {
                raw_sections.push((
                    cur_level,
                    cur_heading.clone(),
                    cur_breadcrumb.clone(),
                    cur_body.trim().to_string(),
                ));
                cur_body.clear();
            }
            while stack.last().is_some_and(|(l, _)| *l >= level) {
                stack.pop();
            }
            stack.push((level, heading_text.clone()));
            cur_level = level;
            cur_heading = heading_text;
            cur_breadcrumb = stack
                .iter()
                .map(|(_, h)| h.as_str())
                .collect::<Vec<_>>()
                .join(" > ");
        }

        cur_body.push_str(line);
        cur_body.push('\n');
    }

    if !cur_body.trim().is_empty() {
        raw_sections.push((
            cur_level,
            cur_heading,
            cur_breadcrumb,
            cur_body.trim().to_string(),
        ));
    }

    let mut sections = Vec::new();
    for (level, heading, breadcrumb, body) in raw_sections {
        let char_len = body.chars().count();
        if char_len <= MAX_SECTION_CHARS {
            let has_code_block = body.contains("```") || body.contains("~~~~");
            sections.push(PageSection {
                index: sections.len(),
                level,
                heading,
                breadcrumb,
                body,
                char_len,
                has_code_block,
            });
        } else {
            for (part_idx, chunk) in split_large_section(&body, MAX_SECTION_CHARS)
                .into_iter()
                .enumerate()
            {
                let c_len = chunk.chars().count();
                let has_code_block = chunk.contains("```") || chunk.contains("~~~~");
                let h = if part_idx == 0 {
                    heading.clone()
                } else {
                    format!("{heading} (part {})", part_idx + 1)
                };
                let bc = if part_idx == 0 {
                    breadcrumb.clone()
                } else {
                    format!("{breadcrumb} (part {})", part_idx + 1)
                };
                sections.push(PageSection {
                    index: sections.len(),
                    level,
                    heading: h,
                    breadcrumb: bc,
                    body: chunk,
                    char_len: c_len,
                    has_code_block,
                });
            }
        }
    }

    sections
}

fn parse_heading_line(line: &str) -> Option<(u8, String)> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with('#') {
        return None;
    }
    let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
    if !(1..=4).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    if title.is_empty() {
        return None;
    }
    Some((hashes as u8, title.to_string()))
}

fn split_large_section(body: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    let mut in_fence: Option<&str> = None;

    for line in body.lines() {
        let trimmed_start = line.trim_start();
        if let Some(fence) = in_fence {
            if trimmed_start.starts_with(fence) {
                in_fence = None;
            }
        } else if trimmed_start.starts_with("~~~~") {
            in_fence = Some("~~~~");
        } else if trimmed_start.starts_with("```") {
            in_fence = Some("```");
        }

        let line_chars = line.chars().count() + 1;
        if in_fence.is_none()
            && line.trim().is_empty()
            && current_chars >= max_chars
            && !current.trim().is_empty()
        {
            chunks.push(current.trim().to_string());
            current.clear();
            current_chars = 0;
            continue;
        }
        current.push_str(line);
        current.push('\n');
        current_chars += line_chars;
    }

    if !current.trim().is_empty() {
        chunks.push(current.trim().to_string());
    }
    chunks
}

/// Build a compact Table of Contents for a parsed page.
pub fn build_outline(sections: &[PageSection]) -> String {
    let mut lines = Vec::new();
    for sec in sections {
        if sec.heading.ends_with(')') && sec.heading.contains("(part ") {
            continue;
        }
        let indent = "  ".repeat(sec.level.saturating_sub(1) as usize);
        let code_tag = if sec.has_code_block { " [code]" } else { "" };
        lines.push(format!(
            "{indent}- {} ({} chars{code_tag})",
            sec.heading, sec.char_len
        ));
        if lines.len() >= 45 {
            lines.push("  - ...".to_string());
            break;
        }
    }
    lines.join("\n")
}

/// Render a page for `webfetch(url, query?, section?)`.
pub fn project_page(page: &CachedPage, query: Option<&str>, section: Option<&str>) -> String {
    let total_chars = page.markdown.chars().count();
    let section_filter = section.map(str::trim).filter(|s| !s.is_empty());
    let query_filter = query.map(str::trim).filter(|q| !q.is_empty());

    // 1. Explicit section lookup (`section="..."`).
    if let Some(sec_q) = section_filter {
        let lower_q = sec_q.to_ascii_lowercase();
        let mut matched_indices: Vec<usize> = Vec::new();
        let mut include_below_level: Option<u8> = None;

        for sec in &page.sections {
            if let Some(parent_lvl) = include_below_level {
                let base_part = sec.heading.contains("(part ");
                if base_part || (sec.level > parent_lvl && sec.level > 0) {
                    matched_indices.push(sec.index);
                    continue;
                } else {
                    include_below_level = None;
                }
            }
            if sec.heading.to_ascii_lowercase().contains(&lower_q)
                || sec.breadcrumb.to_ascii_lowercase().contains(&lower_q)
            {
                matched_indices.push(sec.index);
                if sec.level > 0 && !sec.heading.contains("(part ") {
                    include_below_level = Some(sec.level);
                }
            }
        }

        if !matched_indices.is_empty() {
            return assemble_selected_sections(
                page,
                &matched_indices,
                MAX_PROJECTED_CHARS,
                Some(&format!("Section match: '{sec_q}'")),
                false,
            );
        }

        // Fallback if exact heading substring didn't match: rank by BM25 + show Outline.
        let ranked = rank_sections(&page.sections, sec_q);
        let top_indices: Vec<usize> = ranked.into_iter().take(4).map(|(i, _)| i).collect();
        return assemble_selected_sections(
            page,
            &top_indices,
            MAX_PROJECTED_CHARS,
            Some(&format!(
                "No exact heading matched '{sec_q}'; showing outline and closest sections"
            )),
            true,
        );
    }

    // 2. Query-focused retrieval (`query="..."`).
    if let Some(q) = query_filter {
        let ranked = rank_sections(&page.sections, q);
        if ranked.is_empty() && total_chars <= FULL_PAGE_CHARS {
            return page.markdown.clone();
        }
        let mut selected: Vec<usize> = Vec::new();
        // Always include section 0 (title / overview) if reasonably small so context is clear.
        if let Some(first) = page.sections.first()
            && first.char_len <= 2_500
        {
            selected.push(0);
        }
        let mut used_chars: usize = selected.iter().map(|&i| page.sections[i].char_len).sum();
        for (idx, _score) in ranked {
            if selected.contains(&idx) {
                continue;
            }
            let len = page.sections[idx].char_len;
            if used_chars + len > MAX_PROJECTED_CHARS && selected.len() >= 2 {
                continue;
            }
            selected.push(idx);
            used_chars += len;
            if used_chars >= MAX_PROJECTED_CHARS {
                break;
            }
        }
        selected.sort_unstable();
        return assemble_selected_sections(
            page,
            &selected,
            MAX_PROJECTED_CHARS,
            Some(&format!("Focused on query: '{q}'")),
            page.sections.len() >= 3,
        );
    }

    // 3. No filter: return full page if it fits within budget.
    if total_chars <= FULL_PAGE_CHARS || page.sections.len() <= 1 {
        return trim_middle(page.markdown.trim(), MAX_PROJECTED_CHARS);
    }

    // Large page without filter: show Outline + Overview + highest-density sections in order.
    let mut selected: Vec<usize> = Vec::new();
    let mut used = 0usize;
    for (idx, sec) in page.sections.iter().enumerate() {
        if idx == 0 || (used + sec.char_len <= MAX_PROJECTED_CHARS - 2_500) {
            selected.push(idx);
            used += sec.char_len;
        } else if sec.has_code_block && used + sec.char_len <= MAX_PROJECTED_CHARS {
            selected.push(idx);
            used += sec.char_len;
        }
    }
    assemble_selected_sections(
        page,
        &selected,
        MAX_PROJECTED_CHARS,
        Some(&format!(
            "Large page ({total_chars} chars, {} sections) — pass `section` or `query` to webfetch to read any specific section from cache",
            page.sections.len()
        )),
        true,
    )
}

fn assemble_selected_sections(
    page: &CachedPage,
    indices: &[usize],
    budget: usize,
    banner: Option<&str>,
    include_outline: bool,
) -> String {
    let mut out = String::new();
    if let Some(b) = banner {
        out.push_str(&format!("> [{b}]\n\n"));
    }
    if include_outline && page.sections.len() >= 2 {
        out.push_str("## Document Outline\n");
        out.push_str(&build_outline(&page.sections));
        out.push_str("\n\n---\n\n");
    }

    let mut used = out.chars().count();
    let mut omitted = page.sections.len().saturating_sub(indices.len());
    for &idx in indices {
        let Some(sec) = page.sections.get(idx) else {
            continue;
        };
        if used + sec.char_len > budget && !out.is_empty() && indices.len() > 1 {
            omitted += 1;
            continue;
        }
        if !sec.breadcrumb.is_empty() && sec.level > 1 && !sec.body.starts_with('#') {
            out.push_str(&format!("### {}\n\n", sec.breadcrumb));
        }
        out.push_str(&sec.body);
        out.push_str("\n\n");
        used += sec.char_len + 2;
    }

    if omitted > 0 {
        out.push_str(&format!(
            "\n---\n*[{omitted} other section(s) cached — call `webfetch` with `section` or `query` to read them instantly]*\n"
        ));
    }

    trim_middle(out.trim(), MAX_PROJECTED_CHARS)
}

/// Extract compact, high-signal excerpts from a page for `websearch(deep=true)`.
pub fn extract_relevant_excerpts(page: &CachedPage, query: &str, max_chars: usize) -> String {
    if page.sections.is_empty() {
        return String::new();
    }
    let ranked = rank_sections(&page.sections, query);
    let mut chosen: Vec<usize> = ranked.into_iter().take(2).map(|(i, _)| i).collect();
    if chosen.is_empty() {
        chosen.push(0);
    }
    chosen.sort_unstable();

    let mut parts = Vec::new();
    let mut used = 0usize;
    for idx in chosen {
        let sec = &page.sections[idx];
        let remain = max_chars.saturating_sub(used);
        if remain < 120 {
            break;
        }
        let body = if sec.char_len <= remain {
            sec.body.clone()
        } else {
            truncate_preserving_code(&sec.body, remain)
        };
        let header = if sec.breadcrumb != "Overview" && !body.starts_with('#') {
            format!("**[{}]**\n{}", sec.breadcrumb, body)
        } else {
            body
        };
        used += header.chars().count();
        parts.push(header);
    }
    parts.join("\n\n")
}

fn truncate_preserving_code(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = String::new();
    let mut count = 0usize;
    let mut in_fence = false;
    for line in text.lines() {
        let l_len = line.chars().count() + 1;
        if count + l_len > max_chars.saturating_sub(20) {
            break;
        }
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
        out.push_str(line);
        out.push('\n');
        count += l_len;
    }
    if in_fence {
        out.push_str("```\n");
    }
    out.push_str("…");
    out
}

/// Rank `PageSection`s against `query` using Okapi BM25 + heading & code boosts.
/// Returns `(section_index, score)` sorted highest-first.
pub fn rank_sections(sections: &[PageSection], query: &str) -> Vec<(usize, f64)> {
    let q_tokens = tokenize_for_bm25(query);
    if q_tokens.is_empty() || sections.is_empty() {
        return Vec::new();
    }

    let doc_tokens: Vec<Vec<String>> = sections
        .iter()
        .map(|s| tokenize_for_bm25(&format!("{} {}", s.breadcrumb, s.body)))
        .collect();

    let n_docs = sections.len() as f64;
    let avg_dl = (doc_tokens.iter().map(Vec::len).sum::<usize>() as f64 / n_docs).max(1.0);

    // Document frequencies per query term.
    let mut df: HashMap<&str, usize> = HashMap::new();
    for q in &q_tokens {
        let count = doc_tokens
            .iter()
            .filter(|tokens| tokens.iter().any(|t| t == q))
            .count();
        df.insert(q.as_str(), count);
    }

    let k1 = 1.5;
    let b = 0.75;
    let query_lower = query.trim().to_ascii_lowercase();

    let mut scored: Vec<(usize, f64)> = Vec::new();
    for (idx, sec) in sections.iter().enumerate() {
        let tokens = &doc_tokens[idx];
        if tokens.is_empty() {
            continue;
        }
        let dl = tokens.len() as f64;
        let mut tf_map: HashMap<&str, usize> = HashMap::new();
        for t in tokens {
            *tf_map.entry(t.as_str()).or_insert(0) += 1;
        }

        let mut score = 0.0;
        let bc_tokens: HashSet<String> = tokenize_for_bm25(&sec.breadcrumb).into_iter().collect();

        for q in &q_tokens {
            let tf = *tf_map.get(q.as_str()).unwrap_or(&0) as f64;
            if tf > 0.0 {
                let doc_freq = *df.get(q.as_str()).unwrap_or(&0) as f64;
                let idf = ((n_docs - doc_freq + 0.5) / (doc_freq + 0.5) + 1.0).ln();
                let denom = tf + k1 * (1.0 - b + b * (dl / avg_dl));
                score += idf * ((tf * (k1 + 1.0)) / denom);
            }
            if bc_tokens.contains(q) {
                score += 2.5;
            }
        }

        if score > 0.0 {
            if query_lower.len() >= 4 && sec.body.to_ascii_lowercase().contains(&query_lower) {
                score += 3.0;
            }
            if sec.has_code_block {
                score *= 1.25;
            }
            scored.push((idx, score));
        }
    }

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored
}

/// Score a snippet/title pair against `query` for search result hybrid reranking.
pub fn score_search_hit(title: &str, snippet: &str, url: &str, query: &str) -> f64 {
    let q_tokens = tokenize_for_bm25(query);
    if q_tokens.is_empty() {
        return 0.0;
    }
    let title_tokens: HashSet<String> = tokenize_for_bm25(title).into_iter().collect();
    let snippet_tokens = tokenize_for_bm25(snippet);
    let url_tokens: HashSet<String> = tokenize_for_bm25(url).into_iter().collect();

    let mut snip_counts: HashMap<&str, usize> = HashMap::new();
    for t in &snippet_tokens {
        *snip_counts.entry(t.as_str()).or_insert(0) += 1;
    }

    let mut score = 0.0;
    let mut matched_unique = 0usize;
    for q in &q_tokens {
        let mut hit = false;
        if title_tokens.contains(q) {
            score += 3.0;
            hit = true;
        }
        if url_tokens.contains(q) {
            score += 1.5;
            hit = true;
        }
        if let Some(&c) = snip_counts.get(q.as_str()) {
            score += (c.min(4) as f64) * 1.2;
            hit = true;
        }
        if hit {
            matched_unique += 1;
        }
    }

    // Coverage bonus when most or all query terms match.
    let coverage = (matched_unique as f64) / (q_tokens.len() as f64);
    score * (0.5 + 0.5 * coverage)
}

/// Tokenize text for BM25: splits both prose words and code identifiers
/// (`tokio::spawn_blocking` -> `tokio::spawn_blocking`, `tokio`, `spawn`, `blocking`,
/// `useEffect` -> `useeffect`, `use`, `effect`) and applies light suffix stemming.
pub fn tokenize_for_bm25(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw_word in text.split(|c: char| {
        !c.is_alphanumeric() && c != '_' && c != '-' && c != ':' && c != '.' && c != '+'
    }) {
        let trimmed = raw_word.trim_matches(|c: char| !c.is_alphanumeric());
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if is_stop_word(&lower) {
            continue;
        }

        // Keep compound technical symbol (e.g. `tokio::spawn`, `v0.30`) if it has separators.
        if (lower.contains("::") || lower.contains('_') || lower.contains('.')) && lower.len() >= 3
        {
            out.push(lower.clone());
        }

        // Also split compound identifiers and camelCase into atomic subtokens.
        for sub in split_identifier(trimmed) {
            let sub_lower = sub.to_ascii_lowercase();
            if sub_lower.len() >= 2 && !is_stop_word(&sub_lower) {
                let stemmed = stem_token(&sub_lower);
                out.push(stemmed);
            }
        }
    }
    out
}

fn split_identifier(ident: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for segment in ident.split([':', '_', '-', '.', '/']) {
        if segment.is_empty() {
            continue;
        }
        // Split camelCase (`useEffect` -> `use`, `Effect`).
        let mut cur = String::new();
        let mut prev_lower = false;
        for ch in segment.chars() {
            if ch.is_uppercase() && prev_lower && !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            prev_lower = ch.is_lowercase();
            cur.push(ch);
        }
        if !cur.is_empty() {
            parts.push(cur);
        }
    }
    parts
}

fn stem_token(token: &str) -> String {
    if token.len() <= 4 || token.chars().any(|c| !c.is_ascii_alphabetic()) {
        return token.to_string();
    }
    if let Some(base) = token.strip_suffix("ing")
        && base.len() >= 3
    {
        return base.to_string();
    }
    if let Some(base) = token.strip_suffix("ed")
        && base.len() >= 3
    {
        return base.to_string();
    }
    if let Some(base) = token.strip_suffix("ies")
        && base.len() >= 2
    {
        return format!("{base}y");
    }
    if let Some(base) = token.strip_suffix("es")
        && base.len() >= 3
        && !base.ends_with(['s', 'x', 'z'])
    {
        return base.to_string();
    }
    if let Some(base) = token.strip_suffix('s')
        && base.len() >= 3
        && !base.ends_with(['s', 'u', 'i'])
    {
        return base.to_string();
    }
    token.to_string()
}

fn is_stop_word(w: &str) -> bool {
    matches!(
        w,
        "the"
            | "and"
            | "for"
            | "with"
            | "that"
            | "this"
            | "from"
            | "are"
            | "was"
            | "were"
            | "have"
            | "has"
            | "had"
            | "into"
            | "about"
            | "what"
            | "when"
            | "where"
            | "which"
            | "who"
            | "how"
            | "why"
            | "can"
            | "will"
            | "would"
            | "should"
            | "could"
            | "their"
            | "there"
            | "been"
            | "being"
            | "also"
            | "than"
            | "then"
            | "them"
            | "they"
            | "your"
            | "you"
            | "not"
            | "but"
            | "all"
            | "any"
            | "some"
            | "more"
    )
}
