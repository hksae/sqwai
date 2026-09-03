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

pub async fn fetch(args: &Value) -> Outcome {
    let url = match url_arg(args) {
        Ok(url) => url,
        Err(error) => return Outcome::err(error),
    };
    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(15)
        .clamp(1, 60);
    let client = match Client::builder()
        .timeout(Duration::from_secs(timeout))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("sqwai/0.1 webfetch")
        .build()
    {
        Ok(client) => client,
        Err(error) => return Outcome::err(format!("webfetch client error: {error}")),
    };
    let response = match client.get(url.clone()).send().await {
        Ok(response) => response,
        Err(error) => return Outcome::err(format!("webfetch request failed: {error}")),
    };
    let status = response.status();
    if !status.is_success() {
        return Outcome::err(format!("webfetch HTTP error: {status}"));
    }
    let is_html = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/html"));
    if response
        .content_length()
        .is_some_and(|n| n as usize > MAX_BODY_BYTES)
    {
        return Outcome::err("webfetch response is too large (maximum 1 MB)");
    }
    let bytes = match response.bytes().await {
        Ok(bytes) if bytes.len() <= MAX_BODY_BYTES => bytes,
        Ok(_) => return Outcome::err("webfetch response is too large (maximum 1 MB)"),
        Err(error) => return Outcome::err(format!("webfetch read failed: {error}")),
    };
    let raw = String::from_utf8_lossy(&bytes);
    let text = if is_html || response_content_type(&raw) {
        html_to_text(&raw)
    } else {
        raw.to_string()
    };
    Outcome::ok(truncate(&text))
}

fn response_content_type(raw: &str) -> bool {
    // The response headers are not retained here; detect HTML conservatively.
    let head = raw
        .trim_start()
        .get(..256)
        .unwrap_or(raw.trim_start())
        .to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html") || head.contains("<body")
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
    let tail: String = text.chars().take(MAX_OUTPUT_CHARS).collect();
    format!("{}\n[webfetch output truncated]", tail.trim_end())
}
