//! Smart URL Router, Native Domain Adapters, and LLM-Native Markdown Negotiation.
//!
//! Before falling back to generic HTML parsing, `router` checks whether the target
//! URL belongs to a known developer platform (GitHub, Crates.io, npm, PyPI,
//! StackOverflow, Reddit, Wikipedia) or supports native Markdown delivery via
//! `Accept: text/markdown` / `<link rel="alternate" type="text/markdown">` (`llms.txt` v2).

use super::markdown::{self, ExtractedDoc};
use reqwest::{Client, Url};
use scraper::{Html, Selector};
use serde_json::Value;

pub const MAX_BODY_BYTES: usize = 1_500_000;

/// Result of fetching and converting a URL into clean Markdown.
#[derive(Debug, Clone)]
pub struct FetchedDocument {
    pub title: Option<String>,
    pub markdown: String,
    pub raw_html: Option<String>,
    pub final_url: Url,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DomainRoute {
    GitHubBlob {
        owner: String,
        repo: String,
        rest: String,
    },
    GitHubIssueOrPr {
        owner: String,
        repo: String,
        kind: &'static str,
        number: u64,
    },
    GitHubRepo {
        owner: String,
        repo: String,
    },
    CratesIo {
        crate_name: String,
    },
    NpmPackage {
        pkg_name: String,
    },
    PyPiPackage {
        pkg_name: String,
    },
    RedditThread {
        json_url: String,
    },
    StackExchangeQuestion {
        site: String,
        question_id: u64,
    },
    WikipediaArticle {
        host: String,
        title: String,
    },
    Standard,
}

pub fn classify_url(url: &Url) -> DomainRoute {
    let host = url
        .host_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let host_clean = host.strip_prefix("www.").unwrap_or(&host);
    let segments: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();

    match host_clean {
        "github.com" => match segments.as_slice() {
            [owner, repo, "blob", rest @ ..] if !rest.is_empty() => DomainRoute::GitHubBlob {
                owner: (*owner).to_string(),
                repo: (*repo).to_string(),
                rest: rest.join("/"),
            },
            [owner, repo, "issues", num] => {
                if let Ok(n) = num.parse::<u64>() {
                    DomainRoute::GitHubIssueOrPr {
                        owner: (*owner).to_string(),
                        repo: (*repo).to_string(),
                        kind: "Issue",
                        number: n,
                    }
                } else {
                    DomainRoute::Standard
                }
            }
            [owner, repo, "pull", num, ..] => {
                if let Ok(n) = num.parse::<u64>() {
                    DomainRoute::GitHubIssueOrPr {
                        owner: (*owner).to_string(),
                        repo: (*repo).to_string(),
                        kind: "Pull Request",
                        number: n,
                    }
                } else {
                    DomainRoute::Standard
                }
            }
            [owner, repo]
                if !matches!(
                    *owner,
                    "orgs"
                        | "topics"
                        | "trending"
                        | "search"
                        | "settings"
                        | "notifications"
                        | "login"
                        | "pricing"
                        | "features"
                        | "marketplace"
                ) =>
            {
                DomainRoute::GitHubRepo {
                    owner: (*owner).to_string(),
                    repo: repo.trim_end_matches(".git").to_string(),
                }
            }
            _ => DomainRoute::Standard,
        },
        "crates.io" => match segments.as_slice() {
            ["crates", name, ..] if !name.is_empty() => DomainRoute::CratesIo {
                crate_name: (*name).to_string(),
            },
            _ => DomainRoute::Standard,
        },
        "npmjs.com" => match segments.as_slice() {
            ["package", scope, name, ..] if scope.starts_with('@') => DomainRoute::NpmPackage {
                pkg_name: format!("{scope}/{name}"),
            },
            ["package", name, ..] if !name.is_empty() => DomainRoute::NpmPackage {
                pkg_name: (*name).to_string(),
            },
            _ => DomainRoute::Standard,
        },
        "pypi.org" => match segments.as_slice() {
            ["project", name, ..] if !name.is_empty() => DomainRoute::PyPiPackage {
                pkg_name: (*name).to_string(),
            },
            _ => DomainRoute::Standard,
        },
        "reddit.com" | "old.reddit.com" | "np.reddit.com" => {
            if segments.len() >= 4 && segments[0] == "r" && segments[2] == "comments" {
                let path = url.path().trim_end_matches('/');
                let json_url = if path.ends_with(".json") {
                    format!("https://www.reddit.com{path}?raw_json=1&limit=25")
                } else {
                    format!("https://www.reddit.com{path}.json?raw_json=1&limit=25")
                };
                DomainRoute::RedditThread { json_url }
            } else {
                DomainRoute::Standard
            }
        }
        "stackoverflow.com" | "serverfault.com" | "superuser.com" | "askubuntu.com"
            if segments.len() >= 2 && matches!(segments[0], "questions" | "q") =>
        {
            if let Ok(qid) = segments[1].parse::<u64>() {
                DomainRoute::StackExchangeQuestion {
                    site: host_clean.trim_end_matches(".com").to_string(),
                    question_id: qid,
                }
            } else {
                DomainRoute::Standard
            }
        }
        _ if host_clean.ends_with(".stackexchange.com")
            && segments.len() >= 2
            && matches!(segments[0], "questions" | "q") =>
        {
            if let Ok(qid) = segments[1].parse::<u64>() {
                let site = host_clean
                    .strip_suffix(".stackexchange.com")
                    .unwrap_or("stackoverflow")
                    .to_string();
                DomainRoute::StackExchangeQuestion {
                    site,
                    question_id: qid,
                }
            } else {
                DomainRoute::Standard
            }
        }
        _ if host_clean.ends_with(".wikipedia.org")
            && segments.len() == 2
            && segments[0] == "wiki" =>
        {
            DomainRoute::WikipediaArticle {
                host: host_clean.to_string(),
                title: segments[1].to_string(),
            }
        }
        _ => DomainRoute::Standard,
    }
}

/// Fetch a URL using domain-specific native adapters when possible, falling back
/// to HTTP content negotiation (`Accept: text/markdown`) and DOM Readability.
pub async fn fetch_smart(
    client: &Client,
    url: &Url,
    allow_hosts: &[String],
    need_raw_html: bool,
) -> Result<FetchedDocument, String> {
    if !need_raw_html {
        let route = classify_url(url);
        match route {
            DomainRoute::GitHubBlob { owner, repo, rest } => {
                if let Ok(doc) = fetch_github_blob(client, url, &owner, &repo, &rest).await {
                    return Ok(doc);
                }
            }
            DomainRoute::GitHubIssueOrPr {
                owner,
                repo,
                kind,
                number,
            } => {
                if let Ok(doc) = fetch_github_issue(client, url, &owner, &repo, kind, number).await
                {
                    return Ok(doc);
                }
            }
            DomainRoute::GitHubRepo { owner, repo } => {
                if let Ok(doc) = fetch_github_repo(client, url, &owner, &repo).await {
                    return Ok(doc);
                }
            }
            DomainRoute::CratesIo { crate_name } => {
                if let Ok(doc) = fetch_crates_io(client, url, &crate_name).await {
                    return Ok(doc);
                }
            }
            DomainRoute::NpmPackage { pkg_name } => {
                if let Ok(doc) = fetch_npm_package(client, url, &pkg_name).await {
                    return Ok(doc);
                }
            }
            DomainRoute::PyPiPackage { pkg_name } => {
                if let Ok(doc) = fetch_pypi_package(client, url, &pkg_name).await {
                    return Ok(doc);
                }
            }
            DomainRoute::RedditThread { json_url } => {
                if let Ok(doc) = fetch_reddit_thread(client, url, &json_url).await {
                    return Ok(doc);
                }
            }
            DomainRoute::StackExchangeQuestion { site, question_id } => {
                if let Ok(doc) = fetch_stackexchange_question(client, url, &site, question_id).await
                {
                    return Ok(doc);
                }
            }
            DomainRoute::WikipediaArticle { host, title } => {
                if let Ok(doc) = fetch_wikipedia_article(client, url, &host, &title).await {
                    return Ok(doc);
                }
            }
            DomainRoute::Standard => {}
        }
    }

    fetch_standard_http(client, url, allow_hosts, need_raw_html).await
}

async fn fetch_standard_http(
    client: &Client,
    url: &Url,
    allow_hosts: &[String],
    need_raw_html: bool,
) -> Result<FetchedDocument, String> {
    let response = client
        .get(url.clone())
        .header(
            reqwest::header::ACCEPT,
            "text/markdown, text/x-markdown, text/plain;q=0.95, text/html;q=0.9, application/xhtml+xml;q=0.8, */*;q=0.5",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .send()
        .await
        .map_err(|e| format!("webfetch request failed: {e}"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(format!("webfetch HTTP error: {status}"));
    }

    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();

    let header_alt_md = extract_link_header_markdown(response.headers(), &final_url);

    if response
        .content_length()
        .is_some_and(|n| n as usize > MAX_BODY_BYTES)
    {
        return Err("webfetch response is too large (maximum 1.5 MB)".into());
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("webfetch read failed: {e}"))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err("webfetch response is too large (maximum 1.5 MB)".into());
    }

    let raw = String::from_utf8_lossy(&bytes).into_owned();

    // 1. Native Markdown response (Cloudflare Markdown for Agents / Mintlify / raw .md).
    if !need_raw_html
        && (content_type.contains("text/markdown") || content_type.contains("text/x-markdown"))
    {
        let normalized = markdown::normalize_markdown(&raw);
        return Ok(FetchedDocument {
            title: None,
            markdown: normalized,
            raw_html: None,
            final_url,
        });
    }

    // 2. JSON response: pretty-print if compact so it's readable.
    if !need_raw_html && content_type.contains("application/json") {
        let formatted = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| raw.clone());
        return Ok(FetchedDocument {
            title: None,
            markdown: format!("```json\n{formatted}\n```"),
            raw_html: None,
            final_url,
        });
    }

    let is_html = content_type.contains("text/html")
        || content_type.contains("application/xhtml")
        || super::looks_like_html(&raw);

    if !is_html {
        return Ok(FetchedDocument {
            title: None,
            markdown: raw.clone(),
            raw_html: if need_raw_html { Some(raw) } else { None },
            final_url,
        });
    }

    // Specialised HTML extraction for StackOverflow if API was bypassed/rate-limited.
    if !need_raw_html
        && final_url
            .host_str()
            .is_some_and(|h| h.contains("stackoverflow.com") || h.contains("stackexchange.com"))
        && let Some(so_md) = extract_stackoverflow_dom(&raw, &final_url)
    {
        return Ok(FetchedDocument {
            title: None,
            markdown: so_md,
            raw_html: None,
            final_url,
        });
    }

    let ExtractedDoc {
        title,
        mut markdown,
        alternate_markdown_url,
        llms_txt_url,
    } = markdown::html_to_markdown(&raw, Some(&final_url));

    if let Some(ref llms_url) = llms_txt_url
        && !markdown.contains(llms_url)
    {
        markdown = format!("> [Site LLM index: {llms_url}]\n\n{markdown}");
    }

    // 3. `llms.txt` v2 alternate Markdown link (`<link rel="alternate" type="text/markdown">` or `Link:` header).
    if !need_raw_html
        && let Some(alt_url_str) = header_alt_md.or(alternate_markdown_url)
        && let Ok(alt_url) = Url::parse(&alt_url_str)
        && alt_url != final_url
        && crate::config::url_host_allowed(&alt_url, allow_hosts).is_ok()
        && let Ok(alt_resp) = client
            .get(alt_url)
            .header(reqwest::header::ACCEPT, "text/markdown, text/plain")
            .send()
            .await
        && alt_resp.status().is_success()
        && let Ok(alt_bytes) = alt_resp.bytes().await
        && alt_bytes.len() <= MAX_BODY_BYTES
    {
        let alt_text = String::from_utf8_lossy(&alt_bytes);
        if !super::looks_like_html(&alt_text) && alt_text.trim().len() >= 64 {
            return Ok(FetchedDocument {
                title,
                markdown: markdown::normalize_markdown(&alt_text),
                raw_html: None,
                final_url,
            });
        }
    }

    Ok(FetchedDocument {
        title,
        markdown,
        raw_html: if need_raw_html { Some(raw) } else { None },
        final_url,
    })
}

fn extract_link_header_markdown(
    headers: &reqwest::header::HeaderMap,
    base_url: &Url,
) -> Option<String> {
    for val in headers.get_all(reqwest::header::LINK) {
        let Ok(s) = val.to_str() else { continue };
        for part in s.split(',') {
            let lower = part.to_ascii_lowercase();
            if lower.contains("rel=\"alternate\"")
                && (lower.contains("type=\"text/markdown\"")
                    || lower.contains("type=\"text/x-markdown\""))
                && let Some(start) = part.find('<')
                && let Some(end) = part[start + 1..].find('>')
            {
                let href = part[start + 1..start + 1 + end].trim();
                if let Ok(resolved) = base_url.join(href) {
                    return Some(resolved.to_string());
                }
            }
        }
    }
    None
}

async fn fetch_github_blob(
    client: &Client,
    original_url: &Url,
    owner: &str,
    repo: &str,
    rest: &str,
) -> Result<FetchedDocument, String> {
    let raw_url = format!("https://raw.githubusercontent.com/{owner}/{repo}/{rest}");
    let resp = client
        .get(&raw_url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("raw github status {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if bytes.len() > MAX_BODY_BYTES {
        return Err("raw file too large".into());
    }
    let content = String::from_utf8_lossy(&bytes);
    let ext = rest.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    let title = format!("{owner}/{repo}: {rest}");
    let markdown = if matches!(ext.as_str(), "md" | "mdx" | "markdown") {
        format!("# {title}\n\n{}", content.trim())
    } else {
        let lang = match ext.as_str() {
            "rs" => "rust",
            "py" => "python",
            "js" | "mjs" | "cjs" => "javascript",
            "ts" | "mts" => "typescript",
            "tsx" => "tsx",
            "jsx" => "jsx",
            "yml" | "yaml" => "yaml",
            "sh" | "bash" => "bash",
            "ps1" => "powershell",
            "rb" => "ruby",
            "kt" => "kotlin",
            "cs" => "csharp",
            other => other,
        };
        format!("# {title}\n\n```{lang}\n{}\n```", content.trim_end())
    };

    Ok(FetchedDocument {
        title: Some(title),
        markdown,
        raw_html: None,
        final_url: original_url.clone(),
    })
}

async fn fetch_github_issue(
    client: &Client,
    original_url: &Url,
    owner: &str,
    repo: &str,
    kind: &str,
    number: u64,
) -> Result<FetchedDocument, String> {
    let issue_api = format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}");
    let comments_api =
        format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}/comments?per_page=25");

    let (issue_res, comments_res) = tokio::join!(
        client
            .get(&issue_api)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send(),
        client
            .get(&comments_api)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send()
    );

    let issue_resp = issue_res.map_err(|e| e.to_string())?;
    if !issue_resp.status().is_success() {
        return Err(format!("github api status {}", issue_resp.status()));
    }
    let issue: Value = issue_resp.json().await.map_err(|e| e.to_string())?;
    let title_str = issue["title"].as_str().unwrap_or("Untitled");
    let state = issue["state"].as_str().unwrap_or("open").to_ascii_uppercase();
    let author = issue["user"]["login"].as_str().unwrap_or("unknown");
    let created = issue["created_at"].as_str().unwrap_or_default();
    let body = issue["body"].as_str().unwrap_or("*(No description)*").trim();
    let labels: Vec<&str> = issue["labels"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|l| l["name"].as_str()).collect())
        .unwrap_or_default();

    let mut md = format!(
        "# [{state}] {kind} #{number}: {title_str}\n\n**Repository:** `{owner}/{repo}` | **Author:** @{author} | **Created:** {created}"
    );
    if !labels.is_empty() {
        md.push_str(&format!(" | **Labels:** {}", labels.join(", ")));
    }
    md.push_str("\n\n");
    md.push_str(body);
    md.push_str("\n\n");

    if let Ok(c_resp) = comments_res
        && c_resp.status().is_success()
        && let Ok(Value::Array(comments)) = c_resp.json::<Value>().await
        && !comments.is_empty()
    {
        md.push_str(&format!("## Comments ({})\n\n", comments.len()));
        for (idx, c) in comments.iter().enumerate() {
            let c_user = c["user"]["login"].as_str().unwrap_or("unknown");
            let c_date = c["created_at"].as_str().unwrap_or_default();
            let c_body = c["body"].as_str().unwrap_or_default().trim();
            if !c_body.is_empty() {
                md.push_str(&format!(
                    "### Comment {} by @{c_user} ({c_date})\n\n{c_body}\n\n",
                    idx + 1
                ));
            }
        }
    }

    Ok(FetchedDocument {
        title: Some(format!("{owner}/{repo}#{number}: {title_str}")),
        markdown: markdown::normalize_markdown(&md),
        raw_html: None,
        final_url: original_url.clone(),
    })
}

async fn fetch_github_repo(
    client: &Client,
    original_url: &Url,
    owner: &str,
    repo: &str,
) -> Result<FetchedDocument, String> {
    let repo_api = format!("https://api.github.com/repos/{owner}/{repo}");
    let readme_api = format!("https://api.github.com/repos/{owner}/{repo}/readme");

    let (repo_res, readme_res) = tokio::join!(
        client
            .get(&repo_api)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .send(),
        client
            .get(&readme_api)
            .header(reqwest::header::ACCEPT, "application/vnd.github.raw+json")
            .send()
    );

    let mut md = String::new();
    let mut title = Some(format!("{owner}/{repo}"));

    if let Ok(r_resp) = repo_res
        && r_resp.status().is_success()
        && let Ok(info) = r_resp.json::<Value>().await
    {
        let desc = info["description"].as_str().unwrap_or_default();
        let stars = info["stargazers_count"].as_u64().unwrap_or(0);
        let forks = info["forks_count"].as_u64().unwrap_or(0);
        let lang = info["language"].as_str().unwrap_or("unknown");
        let license = info["license"]["spdx_id"].as_str().unwrap_or("unspecified");
        let branch = info["default_branch"].as_str().unwrap_or("main");
        let homepage = info["homepage"].as_str().unwrap_or_default();
        let topics: Vec<&str> = info["topics"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        if !desc.is_empty() {
            title = Some(format!("{owner}/{repo} — {desc}"));
        }
        md.push_str(&format!("# {owner}/{repo}\n\n"));
        if !desc.is_empty() {
            md.push_str(&format!("> {desc}\n\n"));
        }
        md.push_str(&format!(
            "- **Stars:** {stars} | **Forks:** {forks} | **Language:** {lang} | **License:** {license} | **Default branch:** `{branch}`\n"
        ));
        if !homepage.is_empty() {
            md.push_str(&format!("- **Homepage:** {homepage}\n"));
        }
        if !topics.is_empty() {
            md.push_str(&format!("- **Topics:** {}\n", topics.join(", ")));
        }
        md.push('\n');
    }

    if let Ok(rd_resp) = readme_res
        && rd_resp.status().is_success()
        && let Ok(bytes) = rd_resp.bytes().await
        && bytes.len() <= MAX_BODY_BYTES
    {
        let readme = String::from_utf8_lossy(&bytes);
        if !readme.trim().is_empty() {
            md.push_str("## README\n\n");
            md.push_str(readme.trim());
        }
    }

    if md.trim().is_empty() {
        return Err("empty github repo metadata".into());
    }

    Ok(FetchedDocument {
        title,
        markdown: markdown::normalize_markdown(&md),
        raw_html: None,
        final_url: original_url.clone(),
    })
}

async fn fetch_crates_io(
    client: &Client,
    original_url: &Url,
    crate_name: &str,
) -> Result<FetchedDocument, String> {
    let api_url = format!("https://crates.io/api/v1/crates/{crate_name}");
    let resp = client
        .get(&api_url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("crates.io status {}", resp.status()));
    }
    let data: Value = resp.json().await.map_err(|e| e.to_string())?;
    let c = &data["crate"];
    let name = c["name"].as_str().unwrap_or(crate_name);
    let version = c["max_stable_version"]
        .as_str()
        .or_else(|| c["max_version"].as_str())
        .unwrap_or("unknown");
    let desc = c["description"].as_str().unwrap_or_default().trim();
    let downloads = c["downloads"].as_u64().unwrap_or(0);
    let doc_url = c["documentation"]
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| format!("https://docs.rs/{name}"));
    let repo_url = c["repository"].as_str().unwrap_or_default();
    let homepage = c["homepage"].as_str().unwrap_or_default();

    let mut md = format!("# Crate `{name}` (v{version})\n\n");
    if !desc.is_empty() {
        md.push_str(&format!("> {desc}\n\n"));
    }
    md.push_str(&format!("- **Latest Version:** `{version}`\n"));
    md.push_str(&format!("- **Documentation:** {doc_url}\n"));
    if !repo_url.is_empty() {
        md.push_str(&format!("- **Repository:** {repo_url}\n"));
    }
    if !homepage.is_empty() {
        md.push_str(&format!("- **Homepage:** {homepage}\n"));
    }
    md.push_str(&format!("- **Downloads:** {downloads}\n"));

    if let Some(versions) = data["versions"].as_array()
        && let Some(latest) = versions
            .iter()
            .find(|v| v["num"].as_str() == Some(version))
            .or_else(|| versions.first())
    {
        if let Some(license) = latest["license"].as_str() {
            md.push_str(&format!("- **License:** {license}\n"));
        }
        if let Some(rust_version) = latest["rust_version"].as_str() {
            md.push_str(&format!("- **MSRV:** `{rust_version}`\n"));
        }
        if let Some(features) = latest["features"].as_object()
            && !features.is_empty()
        {
            md.push_str("\n## Cargo Features\n\n");
            for (feat, deps) in features.iter().take(40) {
                let dep_list: Vec<&str> = deps
                    .as_array()
                    .map(|a| a.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                if dep_list.is_empty() {
                    md.push_str(&format!("- `{feat}`\n"));
                } else {
                    md.push_str(&format!("- `{feat}`: `[{}]`\n", dep_list.join(", ")));
                }
            }
        }
    }

    // If repository is on GitHub, also try fetching its raw README.
    if let Some(gh_rest) = repo_url.strip_prefix("https://github.com/") {
        let parts: Vec<&str> = gh_rest.trim_end_matches('/').split('/').collect();
        if parts.len() >= 2 {
            let readme_api = format!(
                "https://api.github.com/repos/{}/{}/readme",
                parts[0],
                parts[1].trim_end_matches(".git")
            );
            if let Ok(rd_resp) = client
                .get(&readme_api)
                .header(reqwest::header::ACCEPT, "application/vnd.github.raw+json")
                .send()
                .await
                && rd_resp.status().is_success()
                && let Ok(bytes) = rd_resp.bytes().await
                && bytes.len() <= MAX_BODY_BYTES
            {
                let readme = String::from_utf8_lossy(&bytes);
                if !readme.trim().is_empty() {
                    md.push_str("\n## README\n\n");
                    md.push_str(readme.trim());
                }
            }
        }
    }

    Ok(FetchedDocument {
        title: Some(format!("{name} v{version} - crates.io")),
        markdown: markdown::normalize_markdown(&md),
        raw_html: None,
        final_url: original_url.clone(),
    })
}

async fn fetch_npm_package(
    client: &Client,
    original_url: &Url,
    pkg_name: &str,
) -> Result<FetchedDocument, String> {
    let encoded = pkg_name.replace('/', "%2F");
    let api_url = format!("https://registry.npmjs.org/{encoded}");
    let resp = client
        .get(&api_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("npm registry status {}", resp.status()));
    }
    let data: Value = resp.json().await.map_err(|e| e.to_string())?;
    let name = data["name"].as_str().unwrap_or(pkg_name);
    let latest_ver = data["dist-tags"]["latest"].as_str().unwrap_or("unknown");
    let desc = data["description"].as_str().unwrap_or_default();
    let license = data["license"].as_str().unwrap_or("unspecified");
    let homepage = data["homepage"].as_str().unwrap_or_default();
    let readme = data["readme"].as_str().unwrap_or_default();

    let mut md = format!("# npm `{name}` (v{latest_ver})\n\n");
    if !desc.is_empty() {
        md.push_str(&format!("> {desc}\n\n"));
    }
    md.push_str(&format!(
        "- **Latest Version:** `{latest_ver}` | **License:** {license}\n"
    ));
    if !homepage.is_empty() {
        md.push_str(&format!("- **Homepage:** {homepage}\n"));
    }

    if let Some(ver_obj) = data["versions"].get(latest_ver)
        && let Some(deps) = ver_obj["dependencies"].as_object()
        && !deps.is_empty()
    {
        md.push_str("\n## Dependencies\n\n");
        for (k, v) in deps.iter().take(30) {
            md.push_str(&format!("- `{k}`: `{}`\n", v.as_str().unwrap_or("*")));
        }
    }

    if !readme.trim().is_empty() {
        md.push_str("\n## README\n\n");
        md.push_str(readme.trim());
    }

    Ok(FetchedDocument {
        title: Some(format!("{name} v{latest_ver} - npm")),
        markdown: markdown::normalize_markdown(&md),
        raw_html: None,
        final_url: original_url.clone(),
    })
}

async fn fetch_pypi_package(
    client: &Client,
    original_url: &Url,
    pkg_name: &str,
) -> Result<FetchedDocument, String> {
    let api_url = format!("https://pypi.org/pypi/{pkg_name}/json");
    let resp = client
        .get(&api_url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("pypi status {}", resp.status()));
    }
    let data: Value = resp.json().await.map_err(|e| e.to_string())?;
    let info = &data["info"];
    let name = info["name"].as_str().unwrap_or(pkg_name);
    let version = info["version"].as_str().unwrap_or("unknown");
    let summary = info["summary"].as_str().unwrap_or_default();
    let req_py = info["requires_python"].as_str().unwrap_or("any");
    let desc = info["description"].as_str().unwrap_or_default();

    let mut md = format!("# PyPI `{name}` (v{version})\n\n");
    if !summary.is_empty() {
        md.push_str(&format!("> {summary}\n\n"));
    }
    md.push_str(&format!(
        "- **Latest Version:** `{version}` | **Requires Python:** `{req_py}`\n"
    ));
    if let Some(urls) = info["project_urls"].as_object() {
        for (label, u) in urls {
            if let Some(u_str) = u.as_str() {
                md.push_str(&format!("- **{label}:** {u_str}\n"));
            }
        }
    }
    if !desc.trim().is_empty() {
        md.push_str("\n## Description\n\n");
        md.push_str(desc.trim());
    }

    Ok(FetchedDocument {
        title: Some(format!("{name} v{version} - PyPI")),
        markdown: markdown::normalize_markdown(&md),
        raw_html: None,
        final_url: original_url.clone(),
    })
}

async fn fetch_reddit_thread(
    client: &Client,
    original_url: &Url,
    json_url: &str,
) -> Result<FetchedDocument, String> {
    let resp = client
        .get(json_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("reddit json status {}", resp.status()));
    }
    let listing: Value = resp.json().await.map_err(|e| e.to_string())?;
    let Some(arr) = listing.as_array() else {
        return Err("unexpected reddit json".into());
    };

    let post = &arr
        .first()
        .and_then(|v| v["data"]["children"].as_array())
        .and_then(|c| c.first())
        .map(|p| &p["data"])
        .ok_or_else(|| "missing reddit post data".to_string())?;

    let title = post["title"].as_str().unwrap_or("Reddit Thread");
    let sub = post["subreddit_name_prefixed"].as_str().unwrap_or("r/reddit");
    let author = post["author"].as_str().unwrap_or("unknown");
    let score = post["score"].as_i64().unwrap_or(0);
    let selftext = post["selftext"].as_str().unwrap_or_default().trim();

    let mut md = format!(
        "# {title}\n\n**Subreddit:** `{sub}` | **Author:** u/{author} | **Score:** {score}\n\n"
    );
    if !selftext.is_empty() {
        md.push_str(selftext);
        md.push_str("\n\n");
    }

    if let Some(comments) = arr
        .get(1)
        .and_then(|v| v["data"]["children"].as_array())
    {
        md.push_str("## Top Comments\n\n");
        let mut count = 0usize;
        for c in comments {
            let cd = &c["data"];
            let Some(body) = cd["body"].as_str() else {
                continue;
            };
            let c_author = cd["author"].as_str().unwrap_or("unknown");
            let c_score = cd["score"].as_i64().unwrap_or(0);
            md.push_str(&format!(
                "### u/{c_author} ({c_score} pts)\n\n{}\n\n",
                body.trim()
            ));
            // Include first-level replies if present.
            if let Some(replies) = cd["replies"]["data"]["children"].as_array() {
                for r in replies.iter().take(3) {
                    let rd = &r["data"];
                    if let Some(r_body) = rd["body"].as_str() {
                        let r_author = rd["author"].as_str().unwrap_or("unknown");
                        let r_score = rd["score"].as_i64().unwrap_or(0);
                        let quoted = r_body
                            .trim()
                            .lines()
                            .map(|l| format!("> {l}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        md.push_str(&format!(
                            "> **Reply by u/{r_author} ({r_score} pts):**\n{quoted}\n\n"
                        ));
                    }
                }
            }
            count += 1;
            if count >= 20 {
                break;
            }
        }
    }

    Ok(FetchedDocument {
        title: Some(format!("{title} ({sub})")),
        markdown: markdown::normalize_markdown(&md),
        raw_html: None,
        final_url: original_url.clone(),
    })
}

async fn fetch_stackexchange_question(
    client: &Client,
    original_url: &Url,
    site: &str,
    question_id: u64,
) -> Result<FetchedDocument, String> {
    let q_api = format!(
        "https://api.stackexchange.com/2.3/questions/{question_id}?order=desc&sort=activity&site={site}&filter=withbody"
    );
    let a_api = format!(
        "https://api.stackexchange.com/2.3/questions/{question_id}/answers?order=desc&sort=votes&site={site}&pagesize=8&filter=withbody"
    );

    let (q_res, a_res) = tokio::join!(client.get(&q_api).send(), client.get(&a_api).send());
    let q_resp = q_res.map_err(|e| e.to_string())?;
    if !q_resp.status().is_success() {
        return Err(format!("stackexchange api status {}", q_resp.status()));
    }
    let q_json: Value = q_resp.json().await.map_err(|e| e.to_string())?;
    let Some(q_item) = q_json["items"].as_array().and_then(|a| a.first()) else {
        return Err("question not found in stackexchange api".into());
    };

    let raw_title = q_item["title"].as_str().unwrap_or("Question");
    let title = super::decode_entities(raw_title);
    let score = q_item["score"].as_i64().unwrap_or(0);
    let q_body_html = q_item["body"].as_str().unwrap_or_default();
    let q_body_md = markdown::html_to_markdown(q_body_html, Some(original_url)).markdown;
    let tags: Vec<&str> = q_item["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut md = format!("# {title}\n\n**Score:** {score}");
    if !tags.is_empty() {
        md.push_str(&format!(" | **Tags:** `{}`", tags.join("`, `")));
    }
    md.push_str("\n\n## Question\n\n");
    md.push_str(&q_body_md);
    md.push_str("\n\n");

    if let Ok(a_resp) = a_res
        && a_resp.status().is_success()
        && let Ok(a_json) = a_resp.json::<Value>().await
        && let Some(answers) = a_json["items"].as_array()
        && !answers.is_empty()
    {
        md.push_str(&format!("## Answers ({})\n\n", answers.len()));
        for (idx, ans) in answers.iter().enumerate() {
            let a_score = ans["score"].as_i64().unwrap_or(0);
            let accepted = ans["is_accepted"].as_bool().unwrap_or(false);
            let badge = if accepted { " ✓ ACCEPTED" } else { "" };
            let a_html = ans["body"].as_str().unwrap_or_default();
            let a_md = markdown::html_to_markdown(a_html, Some(original_url)).markdown;
            md.push_str(&format!(
                "### Answer {} ({a_score} votes{badge})\n\n{a_md}\n\n",
                idx + 1
            ));
        }
    }

    Ok(FetchedDocument {
        title: Some(title),
        markdown: markdown::normalize_markdown(&md),
        raw_html: None,
        final_url: original_url.clone(),
    })
}

fn extract_stackoverflow_dom(html: &str, base_url: &Url) -> Option<String> {
    let doc = Html::parse_document(html);
    let title_sel = Selector::parse("#question-header h1").ok()?;
    let post_body_sel = Selector::parse(".js-post-body, .post-text").ok()?;
    let question_sel = Selector::parse("#question, .question").ok()?;
    let answer_sel = Selector::parse(".answer").ok()?;

    let title: String = doc.select(&title_sel).next()?.text().collect();
    let title = title.trim();
    if title.is_empty() {
        return None;
    }

    let mut md = format!("# {title}\n\n");
    if let Some(q_el) = doc.select(&question_sel).next()
        && let Some(body_el) = q_el.select(&post_body_sel).next()
    {
        let q_html = body_el.html();
        let q_md = markdown::html_to_markdown(&q_html, Some(base_url)).markdown;
        md.push_str("## Question\n\n");
        md.push_str(&q_md);
        md.push_str("\n\n");
    }

    let answers: Vec<_> = doc.select(&answer_sel).take(8).collect();
    if !answers.is_empty() {
        md.push_str("## Answers\n\n");
        for (idx, ans_el) in answers.iter().enumerate() {
            let is_accepted = ans_el
                .value()
                .attr("class")
                .is_some_and(|c| c.contains("accepted-answer"));
            let score = ans_el.value().attr("data-score").unwrap_or("0");
            let badge = if is_accepted { " ✓ ACCEPTED" } else { "" };
            if let Some(body_el) = ans_el.select(&post_body_sel).next() {
                let a_html = body_el.html();
                let a_md = markdown::html_to_markdown(&a_html, Some(base_url)).markdown;
                md.push_str(&format!(
                    "### Answer {} ({score} votes{badge})\n\n{a_md}\n\n",
                    idx + 1
                ));
            }
        }
    }

    Some(markdown::normalize_markdown(&md))
}

async fn fetch_wikipedia_article(
    client: &Client,
    original_url: &Url,
    host: &str,
    title: &str,
) -> Result<FetchedDocument, String> {
    let api_url = format!("https://{host}/w/api.php");
    let decoded_title = urlencoding_decode(title);
    let resp = client
        .get(&api_url)
        .query(&[
            ("action", "query"),
            ("prop", "extracts"),
            ("titles", decoded_title.as_str()),
            ("explaintext", "1"),
            ("redirects", "1"),
            ("format", "json"),
        ])
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("wikipedia api status {}", resp.status()));
    }
    let data: Value = resp.json().await.map_err(|e| e.to_string())?;
    let pages = data["query"]["pages"]
        .as_object()
        .ok_or_else(|| "missing wikipedia pages".to_string())?;
    let (_, page) = pages
        .iter()
        .next()
        .ok_or_else(|| "empty wikipedia pages".to_string())?;
    let page_title = page["title"].as_str().unwrap_or(&decoded_title);
    let extract = page["extract"].as_str().unwrap_or_default().trim();
    if extract.is_empty() {
        return Err("empty wikipedia extract".into());
    }

    // Convert MediaWiki plaintext headings (`== Section ==`) to Markdown (`## Section`).
    let mut md = format!("# {page_title}\n\n");
    for line in extract.lines() {
        let trimmed = line.trim();
        if let Some(h) = trimmed
            .strip_prefix("==== ")
            .and_then(|s| s.strip_suffix(" ===="))
        {
            md.push_str(&format!("\n#### {h}\n\n"));
        } else if let Some(h) = trimmed
            .strip_prefix("=== ")
            .and_then(|s| s.strip_suffix(" ==="))
        {
            md.push_str(&format!("\n### {h}\n\n"));
        } else if let Some(h) = trimmed
            .strip_prefix("== ")
            .and_then(|s| s.strip_suffix(" =="))
        {
            md.push_str(&format!("\n## {h}\n\n"));
        } else {
            md.push_str(line);
            md.push('\n');
        }
    }

    Ok(FetchedDocument {
        title: Some(format!("{page_title} - Wikipedia")),
        markdown: markdown::normalize_markdown(&md),
        raw_html: None,
        final_url: original_url.clone(),
    })
}

pub fn urlencoding_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        } else if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
