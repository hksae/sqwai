//! Section chunking, document outline generation, in-memory LRU page cache,
//! deep URL canonicalization, and local Okapi BM25 ranking (per-page and
//! cross-page global pool) with code-identifier tokenization.

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
const MAX_SECTION_CHARS: usize = 5_000;

#[derive(Debug, Clone)]
pub struct PageSection {
    pub index: usize,
    pub level: u8,
    pub heading: String,
    /// Full ancestor chain with Markdown hashes, e.g. `# Struct Tokio / ## fn spawn / ### Panics`.
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

/// Normalize a URL for deduplication, RRF fusion, and cache keys:
/// - upgrades `http` -> `https`
/// - strips `www.`, `old.`, `np.`, `m.`, `mobile.` host prefixes
/// - strips `#fragment`
/// - strips `.git` suffix on GitHub repo URLs
/// - strips tracking parameters (`utm_*`, `fbclid`, `gclid`, `ref`, etc.)
/// - strips trailing slashes
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
        let mut stripped = host.as_str();
        for prefix in ["www.", "old.", "np.", "m.", "mobile."] {
            if let Some(rest) = stripped.strip_prefix(prefix)
                && rest.contains('.')
            {
                stripped = rest;
                break;
            }
        }
        let host_owned = stripped.to_string();
        let _ = parsed.set_host(Some(&host_owned));
    }

    // Normalize GitHub `.git` suffix on repo links so Crates.io/PyPI/npm repo metadata
    // collapses with organic GitHub URLs during RRF fusion.
    if parsed.host_str() == Some("github.com") {
        let path = parsed.path().trim_end_matches('/').to_string();
        if let Some(without_git) = path.strip_suffix(".git") {
            parsed.set_path(without_git);
        }
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
                        | "si"
                )
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    if kept_pairs.is_empty() {
        parsed.set_query(None);
    } else {
        let ser = url_query_string(&kept_pairs);
        if ser.is_empty() {
            parsed.set_query(None);
        } else {
            parsed.set_query(Some(&ser));
        }
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
    let mut cur_breadcrumb = "# Overview".to_string();
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
                .map(|(lvl, h)| format!("{} {}", "#".repeat(*lvl as usize), h))
                .collect::<Vec<_>>()
                .join(" / ");
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

/// Split an oversized section along paragraph boundaries (`\n\n`) without
/// ever cutting inside a fenced code block (` ``` ` or ` ~~~~ `).
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
        // Always include ancestor breadcrumb comment when a nested section or split
        // part is rendered, so `### Examples` or `### Panics` never loses its parent context.
        if sec.breadcrumb.contains(" / ") || sec.heading.contains("(part ") {
            out.push_str(&format!("<!-- Section: {} -->\n", sec.breadcrumb));
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

/// Global cross-page BM25 pool ranker for `websearch(deep=true)`:
/// Scores all sections across all top pages together in one unified BM25 corpus
/// with a gentle RRF page-rank prior, so a high-signal code block on Page 3
/// beats generic prose on Page 1 instead of wasting budget per page.
pub fn extract_global_deep_excerpts(
    pages: &[(Url, CachedPage, usize)],
    query: &str,
    total_budget_chars: usize,
) -> String {
    if pages.is_empty() {
        return String::new();
    }

    // Build global section pool: `(page_array_idx, section_idx, page_rank)`.
    let mut pool: Vec<(usize, usize, usize)> = Vec::new();
    let mut flat_sections: Vec<PageSection> = Vec::new();
    for (p_idx, (_, page, page_rank)) in pages.iter().enumerate() {
        let title_prefix = page.title.as_deref().unwrap_or_default();
        for sec in &page.sections {
            pool.push((p_idx, sec.index, *page_rank));
            let mut enriched = sec.clone();
            if !title_prefix.is_empty() && !enriched.breadcrumb.contains(title_prefix) {
                enriched.breadcrumb = format!("{title_prefix} / {}", enriched.breadcrumb);
            }
            flat_sections.push(enriched);
        }
    }

    if flat_sections.is_empty() {
        return String::new();
    }

    let mut global_scored = rank_sections(&flat_sections, query);
    // Apply RRF page-rank prior (`1.15x` for rank 0, `1.075x` for rank 1, `1.05x` for rank 2).
    for (pool_idx, score) in &mut global_scored {
        let (_, _, page_rank) = pool[*pool_idx];
        let page_prior = 1.0 + 0.15 / (1.0 + page_rank as f64);
        *score *= page_prior;
    }
    global_scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // If no query tokens matched (rare), fall back to section 0 of each page.
    if global_scored.is_empty() {
        for (pool_idx, (_, sec_idx, _)) in pool.iter().enumerate() {
            if *sec_idx == 0 {
                global_scored.push((pool_idx, 1.0));
            }
        }
    }

    // Greedily pick the best sections globally up to `total_budget_chars`.
    // Cap each page at 2 sections and ~55% of total budget so one long forum page
    // never starves official API documentation pages.
    let per_page_char_cap = (total_budget_chars * 11) / 20;
    let mut chosen_per_page: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut chars_per_page: HashMap<usize, usize> = HashMap::new();
    let mut used_chars = 0usize;

    for (pool_idx, _) in global_scored {
        if used_chars >= total_budget_chars {
            break;
        }
        let (p_idx, sec_idx, _) = pool[pool_idx];
        let page = &pages[p_idx].1;
        let page_used = *chars_per_page.get(&p_idx).unwrap_or(&0);
        let entry = chosen_per_page.entry(p_idx).or_default();
        if entry.len() >= 2 || entry.contains(&sec_idx) || page_used >= per_page_char_cap {
            continue;
        }

        // If this is a deep section on an API reference page where an early section (0 or 1)
        // holds the compact item declaration/signature (`<= 2_400` chars with a code block),
        // automatically include that signature section so the type/function declaration is kept!
        if let Some(sig_sec) = page
            .sections
            .iter()
            .take(2)
            .find(|s| s.index < sec_idx && s.has_code_block && s.char_len <= 2_400)
            && !entry.contains(&sig_sec.index)
        {
            let sig_cost = sig_sec.char_len.min(1_200);
            if used_chars + sig_cost <= total_budget_chars {
                entry.push(sig_sec.index);
                used_chars += sig_cost;
                *chars_per_page.entry(p_idx).or_insert(0) += sig_cost;
            }
        }

        let sec_len = page.sections[sec_idx].char_len.min(2_200);
        if used_chars + sec_len > total_budget_chars && used_chars >= 1_500 {
            continue;
        }
        entry.push(sec_idx);
        used_chars += sec_len;
        *chars_per_page.entry(p_idx).or_insert(0) += sec_len;
    }

    let mut page_blocks = Vec::new();
    let mut remaining = total_budget_chars;
    for (p_idx, (url, page, _)) in pages.iter().enumerate() {
        let Some(mut sec_indices) = chosen_per_page.remove(&p_idx) else {
            continue;
        };
        if sec_indices.is_empty() || remaining < 150 {
            continue;
        }
        sec_indices.sort_unstable();

        let mut sec_renders = Vec::new();
        for sec_idx in sec_indices {
            if remaining < 120 {
                break;
            }
            let sec = &page.sections[sec_idx];
            let cap = remaining.min(2_400);
            let body = if sec.char_len <= cap {
                sec.body.clone()
            } else {
                truncate_preserving_code(&sec.body, cap)
            };
            let rendered = if sec.breadcrumb.contains(" / ") || sec.heading.contains("(part ") {
                format!("<!-- Section: {} -->\n{}", sec.breadcrumb, body)
            } else {
                body
            };
            remaining = remaining.saturating_sub(rendered.chars().count());
            sec_renders.push(rendered);
        }

        if !sec_renders.is_empty() {
            let heading = page.title.as_deref().unwrap_or_else(|| url.as_str());
            page_blocks.push(format!(
                "### From [{heading}]({url})\n\n{}",
                sec_renders.join("\n\n")
            ));
        }
    }

    page_blocks.join("\n\n---\n\n")
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
        .map(|s| {
            let clean_body = strip_url_targets_for_bm25(&s.body);
            tokenize_for_bm25(&format!("{} {}", s.breadcrumb, clean_body))
        })
        .collect();

    let n_docs = sections.len() as f64;
    let avg_dl = (doc_tokens.iter().map(Vec::len).sum::<usize>() as f64 / n_docs).max(1.0);

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
            // Boost primary H1/signature section (e.g. `# Function spawn` with `.item-decl` on docs.rs).
            if sec.level <= 1 && sec.index <= 1 {
                score += 2.0;
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

/// Strip `(https://...)` targets from Markdown links before BM25 tokenization
/// so repeated domain tokens inside hrefs (e.g. `users.rust-lang.org`) do not
/// artificially inflate term frequency over actual prose and code.
fn strip_url_targets_for_bm25(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b']'
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'('
            && let Some(close) = text[i + 2..].find(')')
        {
            out.push(']');
            i += 2 + close + 1;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
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

    let coverage = (matched_unique as f64) / (q_tokens.len() as f64);
    score * (0.5 + 0.5 * coverage)
}

/// Multi-level code-aware tokenizer for BM25:
/// For `ImmixAllocator::alloc_frame()`, emits:
/// 1. Full compound token: `immixallocator::alloc_frame`
/// 2. Path/namespace segments (`::`, `.`, `/`): `immixallocator`, `alloc_frame`
/// 3. Atomic `snake_case`, `kebab-case`, and `camelCase` sub-tokens: `immix`, `allocator`, `alloc`, `frame`
pub fn tokenize_for_bm25(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw_word in text.split(|c: char| {
        !c.is_alphanumeric() && c != '_' && c != '-' && c != ':' && c != '.' && c != '/' && c != '+'
    }) {
        let trimmed = raw_word.trim_matches(|c: char| !c.is_alphanumeric());
        if trimmed.is_empty() {
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if is_stop_word(&lower) {
            continue;
        }

        // Level 1: Full compound symbol (e.g. `immixallocator::alloc_frame`, `tokio::spawn`).
        if (lower.contains("::") || lower.contains('_') || lower.contains('.')) && lower.len() >= 3
        {
            out.push(lower.clone());
        }

        // Level 2: Segments separated by `::`, `.`, `/` (preserves `alloc_frame` and `immixallocator` whole).
        for seg in trimmed.split([':', '.', '/']) {
            let seg = seg.trim_matches(|c: char| !c.is_alphanumeric());
            if seg.is_empty() {
                continue;
            }
            let seg_lower = seg.to_ascii_lowercase();
            if seg_lower.len() >= 2 && seg_lower != lower && !is_stop_word(&seg_lower) {
                out.push(seg_lower);
            }

            // Level 3: Split `snake_case`, `kebab-case`, and `camelCase` (`ImmixAllocator` -> `immix`, `allocator`).
            for sub in split_camel_and_snake(seg) {
                let sub_lower = sub.to_ascii_lowercase();
                if sub_lower.len() >= 2 && !is_stop_word(&sub_lower) {
                    let stemmed = stem_token(&sub_lower);
                    out.push(stemmed);
                }
            }
        }
    }
    out
}

fn split_camel_and_snake(segment: &str) -> Vec<String> {
    let mut parts = Vec::new();
    for part in segment.split(['_', '-']) {
        if part.is_empty() {
            continue;
        }
        let chars: Vec<char> = part.chars().collect();
        let mut cur = String::new();
        for i in 0..chars.len() {
            let ch = chars[i];
            if i > 0 {
                let prev = chars[i - 1];
                let next_is_lower = chars.get(i + 1).is_some_and(|c| c.is_lowercase());
                // Split on `aB` (`camelCase`) or `ABc` (`HTTPClient` -> `HTTP` + `Client`).
                if (ch.is_uppercase() && prev.is_lowercase())
                    || (ch.is_uppercase() && prev.is_uppercase() && next_is_lower)
                {
                    if !cur.is_empty() {
                        parts.push(std::mem::take(&mut cur));
                    }
                }
            }
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

/// Only pure conversational filler words are filtered; programming keywords and
/// trait/method names (`from`, `into`, `where`, `for`, `with`, `some`, `any`, `all`, `not`)
/// are intentionally kept so code queries match accurately.
fn is_stop_word(w: &str) -> bool {
    matches!(
        w,
        "the"
            | "this"
            | "that"
            | "these"
            | "those"
            | "their"
            | "there"
            | "they"
            | "them"
            | "your"
            | "you"
            | "about"
            | "been"
            | "being"
            | "also"
            | "than"
            | "very"
            | "just"
            | "would"
            | "should"
            | "could"
            | "what"
            | "which"
            | "who"
            | "how"
            | "why"
    )
}
