//! Multi-Source Meta-Search Engine with Reciprocal Rank Fusion (RRF),
//! hybrid BM25 reranking, and parallel Deep Page Extraction.

use super::ranker::{
    self, canonicalize_url, extract_relevant_excerpts, put_cached_page, score_search_hit,
};
use super::router;
use reqwest::{Client, Url};
use scraper::{Html, Selector};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchCategory {
    Auto,
    Docs,
    Code,
    Packages,
    Discussions,
    Papers,
}

impl SearchCategory {
    pub fn parse(raw: Option<&str>) -> Self {
        match raw
            .unwrap_or("auto")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "docs" | "doc" | "documentation" => Self::Docs,
            "code" | "repo" | "repos" | "github" => Self::Code,
            "packages" | "package" | "pkg" | "crates" | "npm" | "pypi" => Self::Packages,
            "discussions" | "discussion" | "forum" | "hn" | "reddit" => Self::Discussions,
            "papers" | "paper" | "arxiv" | "research" | "wiki" => Self::Papers,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RawSearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub source: &'static str,
    pub badge: Option<String>,
    pub boost: f64,
}

#[derive(Debug, Clone)]
pub struct MergedSearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub sources: Vec<&'static str>,
    pub badges: Vec<String>,
    pub score: f64,
}

pub async fn run_meta_search(
    query: &str,
    count: usize,
    timeout_secs: u64,
    category: SearchCategory,
    deep: bool,
    allow_hosts: &[String],
) -> Result<String, String> {
    let provider_timeout = timeout_secs.clamp(4, 25);
    let client = Client::builder()
        .timeout(Duration::from_secs(provider_timeout))
        .user_agent(BROWSER_UA)
        .build()
        .map_err(|e| format!("client error: {e}"))?;

    let api_client = Client::builder()
        .timeout(Duration::from_secs(provider_timeout.min(10)))
        .user_agent(crate::providers::USER_AGENT)
        .build()
        .map_err(|e| format!("client error: {e}"))?;

    let mut provider_lists: Vec<Vec<RawSearchHit>> = Vec::new();

    match category {
        SearchCategory::Auto => {
            let include_pkgs = looks_like_package_query(query);
            let (sp, bing, ddg, so, gh, pkgs) = tokio::join!(
                search_startpage(&client, query),
                search_bing(&client, query),
                search_duckduckgo(&client, query),
                search_stackoverflow(&api_client, query, 6),
                search_github(&api_client, query, 5),
                async {
                    if include_pkgs {
                        search_packages(&api_client, query, 3).await
                    } else {
                        Vec::new()
                    }
                }
            );
            provider_lists.extend([sp, bing, ddg, so, gh, pkgs]);
        }
        SearchCategory::Docs => {
            let docs_q = if query.to_ascii_lowercase().contains("doc") {
                query.to_string()
            } else {
                format!("{query} documentation")
            };
            let (sp, bing, ddg, crates, wiki) = tokio::join!(
                search_startpage(&client, &docs_q),
                search_bing(&client, &docs_q),
                search_duckduckgo(&client, &docs_q),
                search_crates_io(&api_client, query, 4),
                search_wikipedia(&api_client, query, 3),
            );
            provider_lists.extend([sp, bing, ddg, crates, wiki]);
        }
        SearchCategory::Code => {
            let (gh, so, sp, bing, crates) = tokio::join!(
                search_github(&api_client, query, 8),
                search_stackoverflow(&api_client, query, 8),
                search_startpage(&client, query),
                search_bing(&client, query),
                search_crates_io(&api_client, query, 4),
            );
            provider_lists.extend([gh, so, sp, bing, crates]);
        }
        SearchCategory::Packages => {
            let (pkgs, gh, sp, bing) = tokio::join!(
                search_packages(&api_client, query, 6),
                search_github(&api_client, query, 5),
                search_startpage(&client, query),
                search_bing(&client, query),
            );
            provider_lists.extend([pkgs, gh, sp, bing]);
        }
        SearchCategory::Discussions => {
            let (hn, so, gh, sp, bing) = tokio::join!(
                search_hackernews(&api_client, query, 6),
                search_stackoverflow(&api_client, query, 6),
                search_github_issues(&api_client, query, 5),
                search_startpage(&client, query),
                search_bing(&client, query),
            );
            provider_lists.extend([hn, so, gh, sp, bing]);
        }
        SearchCategory::Papers => {
            let (arxiv, wiki, sp, bing) = tokio::join!(
                search_arxiv(&api_client, query, 6),
                search_wikipedia(&api_client, query, 4),
                search_startpage(&client, query),
                search_bing(&client, query),
            );
            provider_lists.extend([arxiv, wiki, sp, bing]);
        }
    }

    let merged = fuse_and_rank(provider_lists, query, category, count);
    if merged.is_empty() {
        return Ok("No search results found.".into());
    }

    let mut out = String::new();
    for (idx, hit) in merged.iter().enumerate() {
        let mut meta_parts: Vec<String> = vec![hit.sources.join("+")];
        meta_parts.extend(hit.badges.iter().cloned());
        let meta = meta_parts.join(" | ");
        out.push_str(&format!(
            "{}. {} [{}]\n   {}\n   {}\n",
            idx + 1,
            hit.title,
            meta,
            hit.url,
            hit.snippet
        ));
    }

    if deep {
        let deep_budget = count.min(3);
        let deep_excerpts =
            fetch_deep_excerpts(&merged, query, deep_budget, allow_hosts).await;
        if !deep_excerpts.is_empty() {
            out.push_str("\n## Deep Page Excerpts\n\n");
            out.push_str(&deep_excerpts);
            out.push_str(
                "\n\n*[Pages above are cached locally — call `webfetch(url, section=\"...\", query=\"...\")` for instant section access]*",
            );
        }
    }

    Ok(out.trim().to_string())
}

fn looks_like_package_query(query: &str) -> bool {
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.len() <= 2 {
        return true;
    }
    let lower = query.to_ascii_lowercase();
    for kw in [
        "crate", "crates", "cargo", "rust", "npm", "pypi", "pip", "package", "version", "docs.rs",
    ] {
        if lower.contains(kw) {
            return true;
        }
    }
    false
}

/// Merge multi-engine search lists via Reciprocal Rank Fusion (RRF) + BM25 + Domain Boosts.
pub fn fuse_and_rank(
    provider_lists: Vec<Vec<RawSearchHit>>,
    query: &str,
    category: SearchCategory,
    limit: usize,
) -> Vec<MergedSearchHit> {
    let mut map: HashMap<String, MergedSearchHit> = HashMap::new();

    for list in provider_lists {
        for (rank, hit) in list.into_iter().enumerate() {
            if hit.url.is_empty() || hit.title.is_empty() {
                continue;
            }
            let canonical = canonicalize_url(&hit.url);
            if canonical.is_empty() {
                continue;
            }
            let rrf = 1.0 / (35.0 + (rank as f64) + 1.0);
            let entry = map.entry(canonical).or_insert_with(|| MergedSearchHit {
                title: hit.title.clone(),
                url: hit.url.clone(),
                snippet: hit.snippet.clone(),
                sources: Vec::new(),
                badges: Vec::new(),
                score: 0.0,
            });

            entry.score += rrf + hit.boost;
            if !entry.sources.contains(&hit.source) {
                entry.sources.push(hit.source);
            }
            if let Some(b) = hit.badge
                && !b.is_empty()
                && !entry.badges.contains(&b)
            {
                entry.badges.push(b);
            }
            if hit.snippet.len() > entry.snippet.len() {
                entry.snippet = hit.snippet;
            }
            if hit.title.len() > entry.title.len() && entry.title.len() < 24 {
                entry.title = hit.title;
            }
        }
    }

    let mut results: Vec<MergedSearchHit> = map
        .into_values()
        .map(|mut item| {
            let bm25 = score_search_hit(&item.title, &item.snippet, &item.url, query);
            let dom_boost = domain_quality_boost(&item.url, category);
            // Multi-source agreement bonus.
            let agreement_bonus = if item.sources.len() >= 2 {
                (item.sources.len() - 1) as f64 * 0.012
            } else {
                0.0
            };
            item.score += (bm25 * 0.008) + dom_boost + agreement_bonus;
            item
        })
        .collect();

    results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results.truncate(limit);
    results
}

fn domain_quality_boost(url: &str, category: SearchCategory) -> f64 {
    let lower = url.to_ascii_lowercase();
    let mut boost = 0.0;
    if lower.contains("docs.rs/")
        || lower.contains("doc.rust-lang.org/")
        || lower.contains("developer.mozilla.org/")
        || lower.contains("docs.python.org/")
        || lower.contains("pkg.go.dev/")
        || lower.contains("/docs/")
        || lower.contains("/documentation/")
    {
        boost += match category {
            SearchCategory::Docs => 0.025,
            _ => 0.010,
        };
    }
    if lower.contains("github.com/")
        || lower.contains("stackoverflow.com/")
        || lower.contains("crates.io/")
    {
        boost += 0.008;
    }
    // Slight penalty for video-only pages where text extraction yields minimal code/docs.
    if lower.contains("youtube.com/watch") || lower.contains("tiktok.com/") {
        boost -= 0.025;
    }
    boost
}

async fn fetch_deep_excerpts(
    hits: &[MergedSearchHit],
    query: &str,
    max_pages: usize,
    allow_hosts: &[String],
) -> String {
    let targets: Vec<Url> = hits
        .iter()
        .filter(|h| {
            let u = h.url.to_ascii_lowercase();
            !u.contains("youtube.com/watch") && !u.ends_with(".pdf")
        })
        .filter_map(|h| Url::parse(&h.url).ok())
        .filter(|u| crate::config::url_host_allowed(u, allow_hosts).is_ok())
        .take(max_pages)
        .collect();

    if targets.is_empty() {
        return String::new();
    }

    let Ok(deep_client) = super::client(6, crate::providers::USER_AGENT, allow_hosts.to_vec())
    else {
        return String::new();
    };

    let futs = targets.iter().map(|url| {
        let client = deep_client.clone();
        let u = url.clone();
        async move {
            if let Some(cached) = ranker::get_cached_page(u.as_str()) {
                return Some((u, cached));
            }
            let doc = router::fetch_smart(&client, &u, allow_hosts, false)
                .await
                .ok()?;
            if doc.markdown.trim().len() < 40 {
                return None;
            }
            let cached = put_cached_page(doc.final_url.as_str(), doc.title, doc.markdown);
            Some((u, cached))
        }
    });

    let fetched = futures::future::join_all(futs).await;
    let mut blocks = Vec::new();
    for item in fetched.into_iter().flatten() {
        let (url, page) = item;
        let excerpt = extract_relevant_excerpts(&page, query, 2_000);
        if !excerpt.trim().is_empty() {
            let heading = page
                .title
                .as_deref()
                .unwrap_or_else(|| url.as_str());
            blocks.push(format!("### From [{heading}]({url})\n\n{excerpt}"));
        }
    }
    blocks.join("\n\n---\n\n")
}

// ── Web Search Providers (Startpage, Bing, DuckDuckGo) ───────────────────────

async fn search_startpage(client: &Client, query: &str) -> Vec<RawSearchHit> {
    let Ok(resp) = client
        .post("https://www.startpage.com/sp/search")
        .header(reqwest::header::REFERER, "https://www.startpage.com/")
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .form(&[
            ("query", query),
            ("cat", "web"),
            ("language", "english"),
            ("lui", "english"),
        ])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(bytes) = resp.bytes().await else {
        return Vec::new();
    };
    if bytes.len() > router::MAX_BODY_BYTES {
        return Vec::new();
    }
    parse_startpage_results(&String::from_utf8_lossy(&bytes), 10)
}

pub fn parse_startpage_results(html: &str, max_count: usize) -> Vec<RawSearchHit> {
    let doc = Html::parse_document(html);
    let Ok(item_sel) = Selector::parse("div.result, div.w-gl__result") else {
        return Vec::new();
    };
    let Ok(link_sel) = Selector::parse(
        "a.result-title, a.result-link, a[data-testid='gl-title-link'], a.w-gl__result-title",
    ) else {
        return Vec::new();
    };
    let Ok(h2_sel) = Selector::parse("h2, h3") else {
        return Vec::new();
    };
    let Ok(desc_sel) = Selector::parse("p.description, p.w-gl__description, p") else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        let Some(link) = item.select(&link_sel).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href").map(str::trim) else {
            continue;
        };
        if !href.starts_with("http") || href.contains("startpage.com") {
            continue;
        }
        let title_raw: String = if let Some(h) = link.select(&h2_sel).next() {
            h.text().collect()
        } else {
            link.text().collect()
        };
        let title = collapse_ws(&title_raw);
        if title.is_empty() {
            continue;
        }
        let snippet = item
            .select(&desc_sel)
            .next()
            .map(|p| collapse_ws(&p.text().collect::<String>()))
            .unwrap_or_default();

        hits.push(RawSearchHit {
            title,
            url: href.to_string(),
            snippet,
            source: "google",
            badge: None,
            boost: 0.0,
        });
        if hits.len() >= max_count {
            break;
        }
    }
    hits
}

async fn search_bing(client: &Client, query: &str) -> Vec<RawSearchHit> {
    let Ok(resp) = client
        .get("https://www.bing.com/search")
        .query(&[("q", query), ("pq", query), ("cc", "en")])
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .header(
            reqwest::header::COOKIE,
            "_EDGE_CD=m=en-us&u=en-us; _EDGE_S=mkt=en-us&ui=en-us",
        )
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(bytes) = resp.bytes().await else {
        return Vec::new();
    };
    if bytes.len() > router::MAX_BODY_BYTES {
        return Vec::new();
    }
    parse_bing_results(&String::from_utf8_lossy(&bytes), 10)
}

pub fn parse_bing_results(html: &str, max_count: usize) -> Vec<RawSearchHit> {
    let doc = Html::parse_document(html);
    let Ok(item_sel) = Selector::parse("li.b_algo") else {
        return Vec::new();
    };
    let Ok(link_sel) = Selector::parse("h2 a") else {
        return Vec::new();
    };
    let Ok(desc_sel) = Selector::parse(".b_caption p, p") else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        let Some(link) = item.select(&link_sel).next() else {
            continue;
        };
        let Some(raw_href) = link.value().attr("href").map(str::trim) else {
            continue;
        };
        if raw_href.starts_with("https://www.bing.com/aclick?") {
            continue;
        }
        let url = unwrap_bing_url(raw_href).unwrap_or_else(|| raw_href.to_string());
        if !url.starts_with("http") {
            continue;
        }
        let title = collapse_ws(&link.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let snippet = item
            .select(&desc_sel)
            .next()
            .map(|p| collapse_ws(&p.text().collect::<String>()))
            .unwrap_or_default();

        hits.push(RawSearchHit {
            title,
            url,
            snippet,
            source: "bing",
            badge: None,
            boost: 0.0,
        });
        if hits.len() >= max_count {
            break;
        }
    }
    hits
}

/// Unwrap Bing's `https://www.bing.com/ck/a?!&&p=...&u=a1<base64url>` redirect URLs.
pub fn unwrap_bing_url(raw_url: &str) -> Option<String> {
    let parsed = Url::parse(raw_url).ok()?;
    let (_, u_val) = parsed.query_pairs().find(|(k, _)| k == "u")?;
    if u_val.len() <= 2 {
        return None;
    }
    let b64 = &u_val[2..];
    let bytes = base64url_decode(b64)?;
    let decoded = String::from_utf8(bytes).ok()?;
    if decoded.starts_with("http://") || decoded.starts_with("https://") {
        Some(decoded)
    } else {
        None
    }
}

fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(input.len() * 3 / 4);
    let mut accum = 0u32;
    let mut bits = 0u8;
    for b in input.bytes() {
        if b == b'=' {
            break;
        }
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        accum = (accum << 6) | (val as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buf.push(((accum >> bits) & 0xFF) as u8);
        }
    }
    Some(buf)
}

async fn search_duckduckgo(client: &Client, query: &str) -> Vec<RawSearchHit> {
    let Ok(resp) = client
        .post("https://html.duckduckgo.com/html/")
        .header(reqwest::header::REFERER, "https://html.duckduckgo.com/")
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-Site", "same-origin")
        .header("Sec-Fetch-User", "?1")
        .form(&[("q", query), ("b", ""), ("kl", "wt-wt")])
        .send()
        .await
    else {
        return Vec::new();
    };
    // DDG bot challenge returns HTTP 202 Accepted or a challenge-form page.
    if resp.status().as_u16() == 202 || !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(bytes) = resp.bytes().await else {
        return Vec::new();
    };
    if bytes.len() > router::MAX_BODY_BYTES {
        return Vec::new();
    }
    let html = String::from_utf8_lossy(&bytes);
    if is_ddg_captcha(&html) {
        return Vec::new();
    }
    parse_ddg_hits(&html, 10)
}

pub fn is_ddg_captcha(html: &str) -> bool {
    html.contains("challenge-form")
        || html.contains("anomaly-modal")
        || html.contains("Select all squares containing a duck")
}

pub fn parse_ddg_hits(html: &str, max_count: usize) -> Vec<RawSearchHit> {
    let doc = Html::parse_document(html);
    let Ok(item_sel) = Selector::parse("div.result, div.web-result") else {
        return Vec::new();
    };
    let Ok(title_sel) = Selector::parse("a.result__a") else {
        return Vec::new();
    };
    let Ok(snip_sel) = Selector::parse(".result__snippet") else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for item in doc.select(&item_sel) {
        if item
            .value()
            .attr("class")
            .is_some_and(|c| c.contains("result--ad"))
        {
            continue;
        }
        let Some(a) = item.select(&title_sel).next() else {
            continue;
        };
        let Some(raw_href) = a.value().attr("href").map(str::trim) else {
            continue;
        };
        let url = super::unwrap_duckduckgo_url(raw_href);
        if !url.starts_with("http") || url.starts_with("https://duckduckgo.com/y.js?") {
            continue;
        }
        let title = collapse_ws(&a.text().collect::<String>());
        if title.is_empty() {
            continue;
        }
        let snippet = item
            .select(&snip_sel)
            .next()
            .map(|s| collapse_ws(&s.text().collect::<String>()))
            .unwrap_or_default();

        hits.push(RawSearchHit {
            title,
            url,
            snippet,
            source: "ddg",
            badge: None,
            boost: 0.0,
        });
        if hits.len() >= max_count {
            break;
        }
    }
    hits
}

// ── Developer & Knowledge APIs (Zero-Key JSON/Atom) ──────────────────────────

async fn search_stackoverflow(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let page_size = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://api.stackexchange.com/2.3/search/advanced")
        .query(&[
            ("order", "desc"),
            ("sort", "relevance"),
            ("q", query),
            ("site", "stackoverflow"),
            ("pagesize", page_size.as_str()),
        ])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(items) = data["items"].as_array() else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for item in items.iter().take(limit) {
        let Some(link) = item["link"].as_str() else {
            continue;
        };
        let raw_title = item["title"].as_str().unwrap_or_default();
        let title = super::decode_entities(raw_title);
        let score = item["score"].as_i64().unwrap_or(0);
        let answers = item["answer_count"].as_u64().unwrap_or(0);
        let answered = item["is_answered"].as_bool().unwrap_or(false);
        let tags: Vec<&str> = item["tags"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        let check = if answered { "✓ " } else { "" };
        let badge = Some(format!("{check}{score}pts, {answers}ans"));
        let snippet = if tags.is_empty() {
            format!("StackOverflow question ({answers} answers, score {score})")
        } else {
            format!(
                "Tags: [{}] — {answers} answers, score {score}",
                tags.join(", ")
            )
        };
        let boost = if answered && score >= 5 { 0.010 } else { 0.0 };

        hits.push(RawSearchHit {
            title,
            url: link.to_string(),
            snippet,
            source: "stackoverflow",
            badge,
            boost,
        });
    }
    hits
}

async fn search_github(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let (repos, issues) = tokio::join!(
        search_github_repos(client, query, limit),
        search_github_issues(client, query, limit)
    );
    let mut out = repos;
    out.extend(issues);
    out
}

async fn search_github_repos(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let per_page = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://api.github.com/search/repositories")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .query(&[("q", query), ("per_page", per_page.as_str())])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(items) = data["items"].as_array() else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for item in items.iter().take(limit) {
        let Some(url) = item["html_url"].as_str() else {
            continue;
        };
        let full_name = item["full_name"].as_str().unwrap_or("repo");
        let desc = item["description"].as_str().unwrap_or_default().trim();
        let stars = item["stargazers_count"].as_u64().unwrap_or(0);
        let lang = item["language"].as_str().unwrap_or_default();

        let badge = if lang.is_empty() {
            Some(format!("★ {stars}"))
        } else {
            Some(format!("★ {stars} · {lang}"))
        };
        let title = if desc.is_empty() {
            full_name.to_string()
        } else {
            format!("{full_name} — {}", truncate_str(desc, 90))
        };

        hits.push(RawSearchHit {
            title,
            url: url.to_string(),
            snippet: desc.to_string(),
            source: "github",
            badge,
            boost: if stars >= 100 { 0.008 } else { 0.0 },
        });
    }
    hits
}

async fn search_github_issues(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let per_page = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://api.github.com/search/issues")
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .query(&[("q", query), ("per_page", per_page.as_str())])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(items) = data["items"].as_array() else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for item in items.iter().take(limit) {
        let Some(url) = item["html_url"].as_str() else {
            continue;
        };
        let title = item["title"].as_str().unwrap_or("Issue").to_string();
        let state = item["state"].as_str().unwrap_or("open");
        let comments = item["comments"].as_u64().unwrap_or(0);
        let body = item["body"].as_str().unwrap_or_default();
        let snippet = truncate_str(&collapse_ws(body), 220);

        hits.push(RawSearchHit {
            title,
            url: url.to_string(),
            snippet,
            source: "gh-issue",
            badge: Some(format!("{state}, {comments} comments")),
            boost: 0.0,
        });
    }
    hits
}

async fn search_packages(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let (crates, npm) = tokio::join!(
        search_crates_io(client, query, limit),
        search_npm(client, query, limit)
    );
    let mut out = crates;
    out.extend(npm);
    out
}

async fn search_crates_io(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let per_page = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://crates.io/api/v1/crates")
        .query(&[("q", query), ("per_page", per_page.as_str())])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(crates) = data["crates"].as_array() else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for c in crates.iter().take(limit) {
        let Some(name) = c["name"].as_str() else {
            continue;
        };
        let version = c["max_stable_version"]
            .as_str()
            .or_else(|| c["max_version"].as_str())
            .unwrap_or("latest");
        let desc = c["description"].as_str().unwrap_or_default().trim();
        let doc_url = c["documentation"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("https://docs.rs/{name}/latest/{name}/"));
        let exact = c["exact_match"].as_bool().unwrap_or(false);

        hits.push(RawSearchHit {
            title: format!("{name} (Rust crate v{version})"),
            url: format!("https://crates.io/crates/{name}"),
            snippet: format!("{desc} | Docs: {doc_url}"),
            source: "crates.io",
            badge: Some(format!("v{version}")),
            boost: if exact { 0.020 } else { 0.005 },
        });
    }
    hits
}

async fn search_npm(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let size = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://registry.npmjs.org/-/v1/search")
        .query(&[("text", query), ("size", size.as_str())])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(objs) = data["objects"].as_array() else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for obj in objs.iter().take(limit) {
        let pkg = &obj["package"];
        let Some(name) = pkg["name"].as_str() else {
            continue;
        };
        let ver = pkg["version"].as_str().unwrap_or("latest");
        let desc = pkg["description"].as_str().unwrap_or_default().trim();
        let url = pkg["links"]["npm"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| format!("https://www.npmjs.com/package/{name}"));

        hits.push(RawSearchHit {
            title: format!("{name} (npm v{ver})"),
            url,
            snippet: desc.to_string(),
            source: "npm",
            badge: Some(format!("v{ver}")),
            boost: 0.005,
        });
    }
    hits
}

async fn search_hackernews(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let hpp = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://hn.algolia.com/api/v1/search")
        .query(&[
            ("query", query),
            ("tags", "story"),
            ("hitsPerPage", hpp.as_str()),
        ])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(items) = data["hits"].as_array() else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for item in items.iter().take(limit) {
        let Some(title) = item["title"].as_str() else {
            continue;
        };
        let obj_id = item["objectID"].as_str().unwrap_or_default();
        let hn_url = format!("https://news.ycombinator.com/item?id={obj_id}");
        let url = item["url"]
            .as_str()
            .filter(|u| !u.is_empty())
            .unwrap_or(&hn_url);
        let points = item["points"].as_u64().unwrap_or(0);
        let comments = item["num_comments"].as_u64().unwrap_or(0);

        hits.push(RawSearchHit {
            title: title.to_string(),
            url: url.to_string(),
            snippet: format!("HackerNews discussion: {hn_url} ({points} points, {comments} comments)"),
            source: "hn",
            badge: Some(format!("{points}pts, {comments}c")),
            boost: 0.0,
        });
    }
    hits
}

async fn search_wikipedia(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let srlimit = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://en.wikipedia.org/w/api.php")
        .query(&[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", query),
            ("srlimit", srlimit.as_str()),
            ("format", "json"),
        ])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(data) = resp.json::<Value>().await else {
        return Vec::new();
    };
    let Some(items) = data["query"]["search"].as_array() else {
        return Vec::new();
    };

    let mut hits = Vec::new();
    for item in items.iter().take(limit) {
        let Some(title) = item["title"].as_str() else {
            continue;
        };
        let raw_snip = item["snippet"].as_str().unwrap_or_default();
        let snippet = super::clean_fragment(raw_snip);
        let slug = title.replace(' ', "_");
        hits.push(RawSearchHit {
            title: format!("{title} - Wikipedia"),
            url: format!("https://en.wikipedia.org/wiki/{slug}"),
            snippet,
            source: "wikipedia",
            badge: None,
            boost: 0.0,
        });
    }
    hits
}

async fn search_arxiv(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let max_res = limit.clamp(1, 10).to_string();
    let q_expr = format!("all:{query}");
    let Ok(resp) = client
        .get("https://export.arxiv.org/api/query")
        .query(&[
            ("search_query", q_expr.as_str()),
            ("start", "0"),
            ("max_results", max_res.as_str()),
        ])
        .send()
        .await
    else {
        return Vec::new();
    };
    if !resp.status().is_success() {
        return Vec::new();
    }
    let Ok(bytes) = resp.bytes().await else {
        return Vec::new();
    };
    parse_arxiv_atom(&String::from_utf8_lossy(&bytes), limit)
}

pub fn parse_arxiv_atom(xml: &str, limit: usize) -> Vec<RawSearchHit> {
    let entry_re = regex::Regex::new(r"(?is)<entry>(.*?)</entry>").unwrap();
    let title_re = regex::Regex::new(r"(?is)<title>(.*?)</title>").unwrap();
    let id_re = regex::Regex::new(r"(?is)<id>(.*?)</id>").unwrap();
    let sum_re = regex::Regex::new(r"(?is)<summary>(.*?)</summary>").unwrap();

    let mut hits = Vec::new();
    for cap in entry_re.captures_iter(xml).take(limit) {
        let block = &cap[1];
        let Some(title) = title_re.captures(block).map(|c| collapse_ws(&c[1])) else {
            continue;
        };
        let Some(url) = id_re.captures(block).map(|c| c[1].trim().to_string()) else {
            continue;
        };
        let summary = sum_re
            .captures(block)
            .map(|c| truncate_str(&collapse_ws(&c[1]), 260))
            .unwrap_or_default();

        hits.push(RawSearchHit {
            title,
            url,
            snippet: summary,
            source: "arxiv",
            badge: Some("paper".into()),
            boost: 0.005,
        });
    }
    hits
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}
