//! Multi-Source Meta-Search Engine with Reciprocal Rank Fusion (RRF),
//! per-provider hard timeouts, GitHub rate-limit fallback and package-to-repo
//! cross-linking.

use super::ranker::{self, canonicalize_url, score_search_hit};
use super::router;
use reqwest::Client;
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Epoch timestamp (seconds) until which unauthenticated GitHub Search API is known to be rate-limited.
static GITHUB_SEARCH_COOLDOWN_UNTIL: AtomicU64 = AtomicU64::new(0);

/// A provider call that failed is not an empty answer: the model must never
/// read a bad key or a rate limit as "this thing does not exist".
fn unavailable_note(provider: &str, reason: &str) -> String {
    format!(
        "Search unavailable: {provider} — {reason}. This is the search backend failing, not an \
empty answer; rephrasing the same question now will not help. Fix the key or provider with \
/providers-search, retry later, or use `webfetch` on a known URL."
    )
}

/// Hosted mode: the provider's own `web_search` runs inside the model
/// request, so this function has nothing to do for the general web.
fn hosted_note() -> String {
    "Web search is hosted by the model provider in this mode: its built-in `web_search` tool runs inside the response, so call that instead of this function. Category sources (code, docs, packages, discussions, papers) still work here."
        .to_string()
}

/// No provider chosen at all. Developer categories keep working, the general
/// web does not.
fn not_configured_note() -> String {
    "Web search is not configured. Pick a provider and its key with /providers-search \
(tavily has a free tier; deepseek uses the DeepSeek API key), or use `webfetch` on a known URL. \
The developer categories still work without a provider: code, docs, packages, discussions, papers."
        .to_string()
}

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
    /// Known category, or `None` for a word this tool does not have. An
    /// unknown word must not fall back to `auto`: a model that asked for
    /// `images` and silently got general web reads the junk as an image
    /// search and keeps going.
    pub fn parse_exact(raw: &str) -> Option<Self> {
        Some(match raw.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Self::Auto,
            "docs" | "doc" | "documentation" => Self::Docs,
            "code" | "repo" | "repos" | "github" => Self::Code,
            "packages" | "package" | "pkg" | "crates" | "npm" | "pypi" => Self::Packages,
            "discussions" | "discussion" | "forum" | "hn" | "reddit" => Self::Discussions,
            "papers" | "paper" | "arxiv" | "research" | "wiki" => Self::Papers,
            _ => return None,
        })
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
    /// Optional canonical repository URL (e.g. from crates.io / npm / PyPI metadata)
    /// used during RRF fusion to link package hits with their GitHub repository hits.
    pub repo_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MergedSearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub sources: Vec<&'static str>,
    pub badges: Vec<String>,
    pub score: f64,
    pub linked_repo: Option<String>,
}

/// Hard per-provider timeout wrapper so one slow/hanging endpoint inside `tokio::join!`
/// never delays fast results from the remaining providers.
async fn cap_provider<F>(fut: F, budget: Duration) -> Vec<RawSearchHit>
where
    F: Future<Output = Vec<RawSearchHit>>,
{
    tokio::time::timeout(budget, fut)
        .await
        .unwrap_or_default()
}

pub async fn run_meta_search(
    query: &str,
    count: usize,
    timeout_secs: u64,
    category: SearchCategory,
    search: &crate::config::SearchConfig,
) -> Result<String, String> {
    // Hard per-provider budget: one stalled endpoint must not delay the whole
    // meta-search, and the provider itself is bounded the same way.
    let per_provider = Duration::from_millis((timeout_secs.clamp(2, 20) * 200).clamp(2_200, 4_000));
    let api_client = Client::builder()
        .timeout(per_provider)
        .user_agent(crate::providers::USER_AGENT)
        .build()
        .map_err(|e| format!("client error: {e}"))?;

    let (lists, provider_error) = gather(&api_client, search, query, category, per_provider).await;
    let mut merged = fuse_and_rank(lists, query, category, count);
    // Quotes turn a search into an exact-phrase match; one retry without them
    // is cheap — unless the provider itself failed, where it buys the same
    // failure twice.
    if merged.is_empty()
        && provider_error.is_none()
        && search.provider != crate::config::SearchProvider::Off
        && let Some(relaxed) = without_quotes(query)
    {
        let (lists, _) = gather(&api_client, search, &relaxed, category, per_provider).await;
        merged = fuse_and_rank(lists, &relaxed, category, count);
    }
    if merged.is_empty() {
        if let Some(reason) = provider_error {
            return Ok(unavailable_note(search.name(), &reason));
        }
        if search.is_hosted() && matches!(category, SearchCategory::Auto) {
            return Ok(hosted_note());
        }
        if search.provider == crate::config::SearchProvider::Off
            && matches!(category, SearchCategory::Auto)
        {
            return Ok(not_configured_note());
        }
        return Ok(no_results_note(query, category, search));
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

    Ok(out.trim().to_string())
}

/// Which sources answer a query of this kind: the configured provider plus
/// the free developer APIs the category asks for. The provider's failure is
/// returned separately so it can be named instead of hidden.
async fn gather(
    api_client: &Client,
    search: &crate::config::SearchConfig,
    query: &str,
    category: SearchCategory,
    per_provider: Duration,
) -> (Vec<Vec<RawSearchHit>>, Option<String>) {
    let mut lists: Vec<Vec<RawSearchHit>> = Vec::new();
    let mut failure: Option<String> = None;
    macro_rules! take {
        ($result:expr) => {
            match $result {
                Ok(items) => lists.push(items),
                Err(error) => {
                    failure.get_or_insert(error);
                }
            }
        };
    }

    match category {
        SearchCategory::Auto => {
            // General web only. The provider infers the query's language on
            // its own; code sources are a deliberate choice (`code`, `docs`,
            // `packages`, `discussions`), not something to guess at from the
            // query text.
            take!(provider_hits(search, query, per_provider).await);
        }
        SearchCategory::Docs => {
            let docs_q = if query.to_ascii_lowercase().contains("doc") {
                query.to_string()
            } else {
                format!("{query} documentation")
            };
            let (web, crates, wiki) = tokio::join!(
                provider_hits(search, &docs_q, per_provider),
                cap_provider(search_crates_io(api_client, query, 4), per_provider),
                cap_provider(search_wikipedia(api_client, query, 3), per_provider),
            );
            take!(web);
            lists.extend([crates, wiki]);
        }
        SearchCategory::Code => {
            let (web, gh, so, crates) = tokio::join!(
                provider_hits(search, query, per_provider),
                cap_provider(
                    search_github_with_fallback(api_client, search, query, 8, per_provider),
                    per_provider
                ),
                cap_provider(search_stackoverflow(api_client, query, 8), per_provider),
                cap_provider(search_crates_io(api_client, query, 4), per_provider),
            );
            take!(web);
            lists.extend([gh, so, crates]);
        }
        SearchCategory::Packages => {
            let (web, pkgs, gh) = tokio::join!(
                provider_hits(search, query, per_provider),
                cap_provider(search_packages(api_client, query, 6), per_provider),
                cap_provider(
                    search_github_with_fallback(api_client, search, query, 5, per_provider),
                    per_provider
                ),
            );
            take!(web);
            lists.extend([pkgs, gh]);
        }
        SearchCategory::Discussions => {
            let (web, hn, so, gh) = tokio::join!(
                provider_hits(search, query, per_provider),
                cap_provider(search_hackernews(api_client, query, 6), per_provider),
                cap_provider(search_stackoverflow(api_client, query, 6), per_provider),
                cap_provider(
                    search_github_issues_with_fallback(
                        api_client,
                        search,
                        query,
                        5,
                        per_provider
                    ),
                    per_provider
                ),
            );
            take!(web);
            lists.extend([hn, so, gh]);
        }
        SearchCategory::Papers => {
            let (web, arxiv, wiki) = tokio::join!(
                provider_hits(search, query, per_provider),
                cap_provider(search_arxiv(api_client, query, 6), per_provider),
                cap_provider(search_wikipedia(api_client, query, 4), per_provider),
            );
            take!(web);
            lists.extend([arxiv, wiki]);
        }
    }

    (lists, failure)
}

/// The configured provider's hits, or the reason it could not answer. `Off`
/// is not an error: general search was deliberately disabled.
async fn provider_hits(
    search: &crate::config::SearchConfig,
    query: &str,
    per_provider: Duration,
) -> Result<Vec<RawSearchHit>, String> {
    if search.provider == crate::config::SearchProvider::Off || search.is_hosted() {
        return Ok(Vec::new());
    }
    match tokio::time::timeout(
        per_provider,
        super::search_api::search(search, query, 10, per_provider),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err("the request timed out".to_string()),
    }
}

/// Drop quote characters from a query, or `None` when the query has none.
fn without_quotes(query: &str) -> Option<String> {
    let collapsed = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let cleaned: String = query
        .chars()
        .filter(|c| !matches!(c, '"' | '\'' | '«' | '»' | '“' | '”'))
        .collect();
    let cleaned = collapsed(&cleaned);
    (!cleaned.is_empty() && cleaned != collapsed(query)).then_some(cleaned)
}

/// An empty set with the reason it may be empty: the model used to read "No
/// search results found." as "the fact does not exist" and spent the next call
/// rephrasing the same query.
fn no_results_note(
    query: &str,
    category: SearchCategory,
    search: &crate::config::SearchConfig,
) -> String {
    let with_provider = |base: &str| {
        if search.provider == crate::config::SearchProvider::Off {
            base.to_string()
        } else {
            format!("{base}, {}", search.name())
        }
    };
    let engines = match category {
        SearchCategory::Auto => search.name().to_string(),
        SearchCategory::Docs => with_provider("crates.io, wikipedia"),
        SearchCategory::Code => with_provider("github, stackoverflow, crates.io"),
        SearchCategory::Packages => with_provider("crates.io, npm, pypi, github"),
        SearchCategory::Discussions => with_provider("hacker news, stackoverflow, github issues"),
        SearchCategory::Papers => with_provider("arxiv, wikipedia"),
    };
    let quoted = query.contains(['"', '«', '»', '\'']);
    format!(
        "No search results from {engines}.{} Quoted or very specific wording can empty the set — retry with fewer words, or with an explicit `category`.",
        if quoted {
            " The same query without quotes was tried too."
        } else {
            ""
        }
    )
}

/// Merge multi-engine search lists via Reciprocal Rank Fusion (RRF) + BM25 + Domain Boosts,
/// and cross-link package registry hits (`crates.io` / `npm` / `PyPI`) with their GitHub
/// repository hits so shared signals reinforce each other.
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
            let linked_repo = hit
                .repo_url
                .as_deref()
                .filter(|u| !u.is_empty())
                .map(canonicalize_url);

            let entry = map.entry(canonical).or_insert_with(|| MergedSearchHit {
                title: hit.title.clone(),
                url: hit.url.clone(),
                snippet: hit.snippet.clone(),
                sources: Vec::new(),
                badges: Vec::new(),
                score: 0.0,
                linked_repo: None,
            });

            entry.score += rrf + hit.boost;
            if entry.linked_repo.is_none() && linked_repo.is_some() {
                entry.linked_repo = linked_repo;
            }
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

    // Cross-link package registry entries with their canonical GitHub repository entry:
    // if both `crates.io/crates/foo` (with `repo_url = github.com/org/foo`) and
    // `github.com/org/foo` were found, fold the package's RRF/version badge into the repo
    // and vice versa so the shared project gets the combined multi-source RRF boost!
    let repo_links: Vec<(String, String, Vec<&'static str>, Vec<String>, f64)> = map
        .iter()
        .filter_map(|(canon_key, item)| {
            let repo_canon = item.linked_repo.as_ref()?;
            if repo_canon != canon_key && map.contains_key(repo_canon) {
                Some((
                    canon_key.clone(),
                    repo_canon.clone(),
                    item.sources.clone(),
                    item.badges.clone(),
                    item.score * 0.35,
                ))
            } else {
                None
            }
        })
        .collect();

    for (pkg_key, repo_key, pkg_sources, pkg_badges, bonus) in repo_links {
        let repo_badges = map
            .get(&repo_key)
            .map(|r| r.badges.clone())
            .unwrap_or_default();
        if let Some(repo_entry) = map.get_mut(&repo_key) {
            repo_entry.score += bonus;
            for s in &pkg_sources {
                if !repo_entry.sources.contains(s) {
                    repo_entry.sources.push(s);
                }
            }
            for b in &pkg_badges {
                if !repo_entry.badges.contains(b) {
                    repo_entry.badges.push(b.clone());
                }
            }
        }
        if let Some(pkg_entry) = map.get_mut(&pkg_key) {
            pkg_entry.score += bonus;
            for b in repo_badges {
                if !pkg_entry.badges.contains(&b) {
                    pkg_entry.badges.push(b);
                }
            }
        }
    }

    // A result that shares no word with the query is not a result. Without
    // this floor the top ten was whatever one engine returned: a search
    // endpoint answering a Russian query with hiring ads, appliance stores or
    // porn links filled every slot on RRF alone, and the model spent calls
    // chasing pages that were never about the query. The floor is per result,
    // not per engine — no language or category is guessed.
    //
    // If nothing at all matches, the zero-overlap hits are kept: an empty
    // answer is worse than a weak one, and the model can still judge them.
    let q_tokens = ranker::tokenize_for_bm25(query);
    let mut matched: Vec<MergedSearchHit> = Vec::new();
    let mut unmatched: Vec<MergedSearchHit> = Vec::new();
    for mut item in map.into_values() {
        let bm25 = score_search_hit(&item.title, &item.snippet, &item.url, query);
        let dom_boost = domain_quality_boost(&item.url, category);
        let agreement_bonus = if item.sources.len() >= 2 {
            (item.sources.len() - 1) as f64 * 0.014
        } else {
            0.0
        };
        item.score += (bm25 * 0.008) + dom_boost + agreement_bonus;
        if q_tokens.is_empty() || bm25 > 0.0 {
            matched.push(item);
        } else {
            unmatched.push(item);
        }
    }
    let mut results = if matched.is_empty() { unmatched } else { matched };

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
    if lower.contains("youtube.com/watch") || lower.contains("tiktok.com/") {
        boost -= 0.025;
    }
    boost
}

// ── Developer & Knowledge APIs (Zero-Key JSON/Atom + Rate-Limit Fallbacks) ───

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
            repo_url: None,
        });
    }
    hits
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn github_in_cooldown() -> bool {
    now_secs() < GITHUB_SEARCH_COOLDOWN_UNTIL.load(Ordering::Relaxed)
}

fn mark_github_rate_limited() {
    GITHUB_SEARCH_COOLDOWN_UNTIL.store(now_secs() + 60, Ordering::Relaxed);
}

/// Query GitHub Search API (using `GITHUB_TOKEN`/`GH_TOKEN` if present); if GitHub
/// returns 403/429 (10 req/min unauthenticated limit), fall back to
/// `site:github.com <query>` through the configured search provider.
async fn search_github_with_fallback(
    api_client: &Client,
    search: &crate::config::SearchConfig,
    query: &str,
    limit: usize,
    per_provider: Duration,
) -> Vec<RawSearchHit> {
    if !github_in_cooldown() {
        let (repos, issues) = tokio::join!(
            search_github_repos(api_client, query, limit),
            search_github_issues(api_client, query, limit)
        );
        let mut out = repos;
        out.extend(issues);
        if !out.is_empty() || !github_in_cooldown() {
            return out;
        }
    }
    search_site_github_fallback(search, query, limit, per_provider).await
}

async fn search_github_issues_with_fallback(
    api_client: &Client,
    search: &crate::config::SearchConfig,
    query: &str,
    limit: usize,
    per_provider: Duration,
) -> Vec<RawSearchHit> {
    if !github_in_cooldown() {
        let issues = search_github_issues(api_client, query, limit).await;
        if !issues.is_empty() || !github_in_cooldown() {
            return issues;
        }
    }
    let query = format!("{query} issues");
    search_site_github_fallback(search, &query, limit, per_provider).await
}

async fn search_site_github_fallback(
    search: &crate::config::SearchConfig,
    query: &str,
    limit: usize,
    per_provider: Duration,
) -> Vec<RawSearchHit> {
    let site_q = format!("site:github.com {query}");
    let Ok(hits) = provider_hits(search, &site_q, per_provider).await else {
        return Vec::new();
    };
    hits.into_iter()
        .filter(|h| h.url.contains("github.com/"))
        .take(limit)
        .map(|mut h| {
            h.source = "github";
            h
        })
        .collect()
}

async fn search_github_repos(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let per_page = limit.clamp(1, 10).to_string();
    let req = router::apply_github_headers(
        client
            .get("https://api.github.com/search/repositories")
            .query(&[("q", query), ("per_page", per_page.as_str())]),
        "application/vnd.github+json",
    );
    let Ok(resp) = req.send().await else {
        return Vec::new();
    };
    if matches!(resp.status().as_u16(), 403 | 429) {
        mark_github_rate_limited();
        return Vec::new();
    }
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
            repo_url: None,
        });
    }
    hits
}

async fn search_github_issues(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let per_page = limit.clamp(1, 10).to_string();
    let req = router::apply_github_headers(
        client
            .get("https://api.github.com/search/issues")
            .query(&[("q", query), ("per_page", per_page.as_str())]),
        "application/vnd.github+json",
    );
    let Ok(resp) = req.send().await else {
        return Vec::new();
    };
    if matches!(resp.status().as_u16(), 403 | 429) {
        mark_github_rate_limited();
        return Vec::new();
    }
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
            repo_url: None,
        });
    }
    hits
}

/// Strip registry/meta noise words (`crate`, `crates.io`, `latest`, `version`, `rust`, `npm`, etc.)
/// before querying package registry APIs (`crates.io`, `registry.npmjs.org`) so queries like
/// `"tokio-util crate latest version crates.io"` resolve to `"tokio-util"`.
pub fn sanitize_package_query(query: &str) -> String {
    const META_NOISE: &[&str] = &[
        "crate",
        "crates",
        "crates.io",
        "cargo",
        "rust",
        "npm",
        "npmjs",
        "npmjs.com",
        "pypi",
        "pypi.org",
        "pip",
        "package",
        "packages",
        "pkg",
        "latest",
        "current",
        "newest",
        "version",
        "versions",
        "release",
        "releases",
        "docs",
        "doc",
        "docs.rs",
        "documentation",
        "feature",
        "features",
        "flag",
        "flags",
    ];
    let kept: Vec<&str> = query
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_' && c != '@' && c != '/' && c != '.'))
        .filter(|w| !w.is_empty() && !META_NOISE.contains(&w.to_ascii_lowercase().as_str()))
        .collect();
    if kept.is_empty() {
        query.trim().to_string()
    } else {
        kept.join(" ")
    }
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
    let clean_q = sanitize_package_query(query);
    let per_page = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://crates.io/api/v1/crates")
        .query(&[("q", clean_q.as_str()), ("per_page", per_page.as_str())])
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
        let repo_url = c["repository"].as_str().map(String::from);
        let exact = c["exact_match"].as_bool().unwrap_or(false);

        hits.push(RawSearchHit {
            title: format!("{name} (Rust crate v{version})"),
            url: format!("https://crates.io/crates/{name}"),
            snippet: format!("{desc} | Docs: {doc_url}"),
            source: "crates.io",
            badge: Some(format!("v{version}")),
            boost: if exact { 0.020 } else { 0.005 },
            repo_url,
        });
    }
    hits
}

async fn search_npm(client: &Client, query: &str, limit: usize) -> Vec<RawSearchHit> {
    let clean_q = sanitize_package_query(query);
    let size = limit.clamp(1, 10).to_string();
    let Ok(resp) = client
        .get("https://registry.npmjs.org/-/v1/search")
        .query(&[("text", clean_q.as_str()), ("size", size.as_str())])
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
        let repo_url = pkg["links"]["repository"].as_str().map(String::from);

        hits.push(RawSearchHit {
            title: format!("{name} (npm v{ver})"),
            url,
            snippet: desc.to_string(),
            source: "npm",
            badge: Some(format!("v{ver}")),
            boost: 0.005,
            repo_url,
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
    };
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
            snippet: format!(
                "HackerNews discussion: {hn_url} ({points} points, {comments} comments)"
            ),
            source: "hn",
            badge: Some(format!("{points}pts, {comments}c")),
            boost: 0.0,
            repo_url: None,
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
            repo_url: None,
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
            repo_url: None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// One engine hit, for the ranking tests.
    fn hit(title: &str, url: &str, snippet: &str, source: &'static str) -> RawSearchHit {
        RawSearchHit {
            title: title.to_string(),
            url: url.to_string(),
            snippet: snippet.to_string(),
            source,
            badge: None,
            boost: 0.0,
            repo_url: None,
        }
    }

    /// The live failure this floor exists for: a Russian query answered by an
    /// engine with unrelated pages ("Summer Place Apartment Homes", hiring
    /// ads, porn links). They matched none of the query words and still took
    /// the top slots on rank fusion alone.
    #[test]
    fn hits_sharing_no_word_with_the_query_are_dropped() {
        let relevant = vec![hit(
            "Мем «Серость» — кот из TikTok",
            "https://example.org/serost",
            "Серость — серый кот из тиктока, мем 2026 года",
            "deepseek",
        )];
        let junk = vec![
            hit(
                "Summer Place Apartment Homes",
                "https://summerplace.example/",
                "Apartments for rent in the United States",
                "deepseek",
            ),
            hit(
                "15 of the Best Self-Employed Jobs",
                "https://jobs.example/",
                "Careers and hiring advice",
                "deepseek",
            ),
        ];
        let merged = fuse_and_rank(vec![relevant, junk], "серость кот мем", SearchCategory::Auto, 10);
        assert_eq!(merged.len(), 1, "{merged:#?}");
        assert!(merged[0].url.contains("serost"), "{merged:#?}");
    }

    /// Quotes make some engines search for an exact phrase; the retry drops
    /// them once, and only when there is something to drop.
    #[test]
    fn quotes_are_dropped_once_and_only_when_present() {
        assert_eq!(
            without_quotes("\"серость\" \"кот\" мем").as_deref(),
            Some("серость кот мем")
        );
        assert_eq!(without_quotes("мем «серость» кот").as_deref(), Some("мем серость кот"));
        assert_eq!(without_quotes("серость кот мем"), None);
        assert_eq!(without_quotes("\"\""), None, "nothing left to search");
    }

    /// "No results" must carry the reason: which engines ran, and that quotes
    /// were already retried without.
    #[test]
    fn an_empty_set_names_the_provider_and_the_quote_retry() {
        let search = crate::config::SearchConfig {
            provider: crate::config::SearchProvider::Deepseek,
            base_url: None,
            api_key: Some("key".into()),
        };
        let note = no_results_note("\"серость\" кот", SearchCategory::Auto, &search);
        assert!(note.contains("deepseek"), "{note}");
        assert!(note.contains("without quotes"), "{note}");
        let plain = no_results_note("серость кот", SearchCategory::Code, &search);
        assert!(plain.contains("github"), "{plain}");
        assert!(!plain.contains("without quotes"), "{plain}");
    }

    /// The floor must not turn a weak answer into no answer: when every hit
    /// misses, they stay.
    #[test]
    fn an_all_junk_result_set_still_reaches_the_model() {
        let junk = vec![hit(
            "Summer Place Apartment Homes",
            "https://summerplace.example/",
            "Apartments for rent",
            "deepseek",
        )];
        let merged = fuse_and_rank(vec![junk], "серость кот мем", SearchCategory::Auto, 10);
        assert_eq!(merged.len(), 1, "an empty answer is worse than a weak one");
    }

    /// A provider failing must not read as "the fact does not exist": a live
    /// run turned a rate limit into forty rephrased searches.
    #[test]
    fn a_provider_failure_is_not_an_empty_answer() {
        let note = unavailable_note("tavily", "HTTP 429 (rate-limited)");
        assert!(note.contains("tavily"), "{note}");
        assert!(note.contains("429"), "{note}");
        assert!(note.contains("rephrasing"), "{note}");
        assert!(note.contains("webfetch"), "{note}");
        let off = not_configured_note();
        assert!(off.contains("/providers-search"), "{off}");
        assert!(off.contains("webfetch"), "{off}");
    }
}
