//! Bounded web retrieval for coding research.

use super::Outcome;
use reqwest::{Client, Url};
use serde_json::Value;
use std::time::Duration;

const MAX_BODY_BYTES: usize = 1_000_000;
const MAX_OUTPUT_CHARS: usize = 40_000;

fn url_arg(args: &Value) -> Result<Url, String> {
    let raw = args
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if raw.is_empty() {
        return Err("webfetch requires a non-empty url".into());
    }
    let url = Url::parse(raw).map_err(|e| format!("invalid url: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("webfetch only allows http and https URLs".into());
    }
    if url.host_str().is_none() {
        return Err("webfetch URL must include a host".into());
    }
    Ok(url)
}

fn client(timeout: u64, agent: &'static str) -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(timeout.clamp(1, 60)))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(agent)
        .build()
        .map_err(|e| format!("client error: {e}"))
}

pub async fn fetch(args: &Value) -> Outcome {
    let url = match url_arg(args) {
        Ok(url) => url,
        Err(error) => return Outcome::err(error),
    };
    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(15);
    let client = match client(timeout, "sqwai/0.1 webfetch") {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("webfetch {e}")),
    };
    let response = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return Outcome::err(format!("webfetch request failed: {e}")),
    };
    let status = response.status();
    if !status.is_success() {
        return Outcome::err(format!("webfetch HTTP error: {status}"));
    }
    let is_html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("text/html"));
    if response
        .content_length()
        .is_some_and(|n| n as usize > MAX_BODY_BYTES)
    {
        return Outcome::err("webfetch response is too large (maximum 1 MB)");
    }
    let bytes = match response.bytes().await {
        Ok(b) if b.len() <= MAX_BODY_BYTES => b,
        Ok(_) => return Outcome::err("webfetch response is too large (maximum 1 MB)"),
        Err(e) => return Outcome::err(format!("webfetch read failed: {e}")),
    };
    let raw = String::from_utf8_lossy(&bytes);
    let text = if is_html || looks_like_html(&raw) {
        html_to_text(&raw)
    } else {
        raw.to_string()
    };
    Outcome::ok(truncate(&text))
}

pub async fn search(args: &Value) -> Outcome {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if query.is_empty() {
        return Outcome::err("websearch requires a non-empty query");
    }
    if query.chars().count() > 500 {
        return Outcome::err("websearch query is too long (maximum 500 characters)");
    }
    let count = args
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(5)
        .clamp(1, 10) as usize;
    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(15);
    let client = match client(timeout, "sqwai/0.1 websearch") {
        Ok(c) => c,
        Err(e) => return Outcome::err(format!("websearch {e}")),
    };
    let response = match client
        .get("https://html.duckduckgo.com/html/")
        .query(&[("q", query)])
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return Outcome::err(format!("websearch request failed: {e}")),
    };
    if !response.status().is_success() {
        return Outcome::err(format!("websearch HTTP error: {}", response.status()));
    }
    let bytes = match response.bytes().await {
        Ok(b) if b.len() <= MAX_BODY_BYTES => b,
        Ok(_) => return Outcome::err("websearch response is too large (maximum 1 MB)"),
        Err(e) => return Outcome::err(format!("websearch read failed: {e}")),
    };
    let results = parse_search_results(&String::from_utf8_lossy(&bytes), count);
    Outcome::ok(if results.is_empty() {
        "No search results found.".into()
    } else {
        results
    })
}

fn looks_like_html(raw: &str) -> bool {
    let head = raw
        .trim_start()
        .get(..256)
        .unwrap_or(raw.trim_start())
        .to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html") || head.contains("<body")
}

fn parse_search_results(html: &str, count: usize) -> String {
    let block_re = regex::Regex::new(
        r#"(?is)<div[^>]+class=[\"'][^\"']*result[^\"']*[\"'][^>]*>(.*?)</div>\s*</div>"#,
    )
    .unwrap();
    let title_re = regex::Regex::new(r#"(?is)<a[^>]+class=[\"'][^\"']*result__a[^\"']*[\"'][^>]*href=[\"']([^\"']+)[\"'][^>]*>(.*?)</a>"#).unwrap();
    let snippet_re = regex::Regex::new(r#"(?is)<(?:a|div)[^>]+class=[\"'][^\"']*result__snippet[^\"']*[\"'][^>]*>(.*?)</(?:a|div)>"#).unwrap();
    let mut out = Vec::new();
    for block in block_re.captures_iter(html).take(count) {
        let body = &block[1];
        let Some(title) = title_re.captures(body) else {
            continue;
        };
        let url = decode_entities(&title[1]);
        let name = clean_fragment(&title[2]);
        let snippet = snippet_re
            .captures(body)
            .map(|c| clean_fragment(&c[1]))
            .unwrap_or_default();
        out.push(format!(
            "{}. {}\n   {}\n   {}",
            out.len() + 1,
            name,
            url,
            snippet
        ));
    }
    out.join("\n")
}

fn clean_fragment(html: &str) -> String {
    let tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    decode_entities(&tags.replace_all(html, " "))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_to_text(html: &str) -> String {
    let no_script = regex::Regex::new(r"(?is)<(script|style|noscript)[^>]*>.*?</\1>").unwrap();
    let no_tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let without_embedded = no_script.replace_all(html, "");
    let text = no_tags.replace_all(&without_embedded, " ");
    decode_entities(&text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_OUTPUT_CHARS {
        return text.trim().to_string();
    }
    format!(
        "{}\n[webfetch output truncated]",
        text.chars()
            .take(MAX_OUTPUT_CHARS)
            .collect::<String>()
            .trim_end()
    )
}
