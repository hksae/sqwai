//! Keyed search backends. `websearch` has exactly one general engine: the
//! provider selected in `[web]`, called over its JSON API. There is no HTML
//! scraping path — every free engine now answers a scripted client with a
//! captcha or a 429, and both are indistinguishable from "no results" to the
//! model, which then rephrases forever.

use super::meta_search::RawSearchHit;
use crate::config::{SearchConfig, SearchProvider};
use serde_json::Value;
use std::time::Duration;

const DEEPSEEK_BASE: &str = "https://api.deepseek.com";
const TAVILY_BASE: &str = "https://api.tavily.com";

pub async fn search(
    cfg: &SearchConfig,
    query: &str,
    count: usize,
    timeout: Duration,
) -> Result<Vec<RawSearchHit>, String> {
    if !cfg.is_ready() {
        return Err(match cfg.provider {
            SearchProvider::Off => "no search provider is configured".to_string(),
            provider => format!("{} has no API key", provider.as_str()),
        });
    }
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(crate::providers::USER_AGENT)
        .build()
        .map_err(|e| format!("client error: {e}"))?;
    let key = cfg.api_key.as_deref().unwrap_or_default();
    match cfg.provider {
        SearchProvider::Off => unreachable!("readiness checked above"),
        SearchProvider::Deepseek => {
            let url = endpoint(cfg.base_url.as_deref(), DEEPSEEK_BASE, "/v1/tools/search");
            let body = serde_json::json!({
                "query": query,
                "count": count.clamp(1, 50),
            });
            let text = post(&client, &url, key, &body).await?;
            parse_deepseek(&text)
        }
        SearchProvider::Tavily => {
            let url = endpoint(cfg.base_url.as_deref(), TAVILY_BASE, "/search");
            let body = serde_json::json!({
                "query": query,
                "max_results": count.clamp(1, 20),
            });
            let text = post(&client, &url, key, &body).await?;
            parse_tavily(&text)
        }
    }
}

fn endpoint(base: Option<&str>, default_base: &str, path: &str) -> String {
    let base = base.unwrap_or(default_base).trim_end_matches('/');
    // a base that already carries the API prefix (`.../v1`) keeps it
    match path.strip_prefix("/v1").filter(|_| base.ends_with("/v1")) {
        Some(rest) => format!("{base}{rest}"),
        None => format!("{base}{path}"),
    }
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    key: &str,
    body: &Value,
) -> Result<String, String> {
    let resp = client
        .post(url)
        .bearer_auth(key)
        .json(body)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let hint = match status.as_u16() {
            401 | 403 => "key rejected",
            402 => "credits exhausted",
            429 => "rate-limited",
            _ => "search failed",
        };
        let detail = text.trim().chars().take(200).collect::<String>();
        return Err(if detail.is_empty() {
            format!("HTTP {status} ({hint})")
        } else {
            format!("HTTP {status} ({hint}): {detail}")
        });
    }
    Ok(text)
}

pub fn parse_deepseek(body: &str) -> Result<Vec<RawSearchHit>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("deepseek response is not JSON: {e}"))?;
    let Some(results) = value["results"].as_array() else {
        return Err("deepseek response has no results array".to_string());
    };
    Ok(results
        .iter()
        .filter_map(|result| {
            let url = result["url"].as_str()?.trim();
            if url.is_empty() {
                return None;
            }
            let title = result["title"]
                .as_str()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(url);
            let snippet = result["snippet"]
                .as_str()
                .map(str::trim)
                .filter(|snippet| !snippet.is_empty())
                .or_else(|| result["summary"].as_str().map(str::trim))
                .unwrap_or_default();
            Some(RawSearchHit {
                title: title.to_string(),
                url: url.to_string(),
                snippet: snippet.to_string(),
                source: "deepseek",
                badge: None,
                boost: 0.0,
                repo_url: None,
            })
        })
        .collect())
}

pub fn parse_tavily(body: &str) -> Result<Vec<RawSearchHit>, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|e| format!("tavily response is not JSON: {e}"))?;
    let Some(results) = value["results"].as_array() else {
        return Err("tavily response has no results array".to_string());
    };
    Ok(results
        .iter()
        .filter_map(|result| {
            let url = result["url"].as_str()?.trim();
            if url.is_empty() {
                return None;
            }
            let title = result["title"]
                .as_str()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(url);
            let snippet = result["content"].as_str().map(str::trim).unwrap_or_default();
            Some(RawSearchHit {
                title: title.to_string(),
                url: url.to_string(),
                snippet: snippet.to_string(),
                source: "tavily",
                badge: None,
                boost: 0.0,
                repo_url: None,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deepseek_results_map_to_hits() {
        let body = r#"{"results":[
            {"rerank_score":2.4,"title":"Мем «Котость»","url":"https://vc.ru/ai/1","snippet":"кот Куки","summary":"summary text"},
            {"title":"","url":"https://example.org/x","snippet":""},
            {"title":"no url","url":""}
        ]}"#;
        let hits = parse_deepseek(body).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].title, "Мем «Котость»");
        assert_eq!(hits[0].snippet, "кот Куки");
        assert_eq!(hits[0].source, "deepseek");
        // an empty title falls back to the URL, never to an empty row
        assert_eq!(hits[1].title, "https://example.org/x");
    }

    #[test]
    fn tavily_results_map_to_hits() {
        let body = r#"{"results":[
            {"title":"El Drifto","url":"https://example.org/drifto","content":"bus meme","score":0.9}
        ]}"#;
        let hits = parse_tavily(body).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].snippet, "bus meme");
        assert_eq!(hits[0].source, "tavily");
        assert!(parse_tavily("{}").is_err());
    }

    #[test]
    fn the_endpoint_keeps_a_prefix_a_custom_base_already_carries() {
        assert_eq!(
            endpoint(None, DEEPSEEK_BASE, "/v1/tools/search"),
            "https://api.deepseek.com/v1/tools/search"
        );
        assert_eq!(
            endpoint(Some("https://gw.example/deepseek/"), DEEPSEEK_BASE, "/v1/tools/search"),
            "https://gw.example/deepseek/v1/tools/search"
        );
        assert_eq!(
            endpoint(Some("https://gw.example/v1"), DEEPSEEK_BASE, "/v1/tools/search"),
            "https://gw.example/v1/tools/search"
        );
    }
}
