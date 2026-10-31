//! Keyless search through the vendors' public MCP endpoints.
//!
//! Exa (`mcp.exa.ai/mcp`) and Parallel (`search.parallel.ai/mcp`) answer the
//! standard streamable-HTTP MCP handshake without a key: initialize, then
//! `tools/call`. The host owns a tiny client instead of a general MCP layer —
//! two endpoints, one tool each, no reason to build a protocol surface.

use super::meta_search::RawSearchHit;
use serde_json::{json, Value};
use std::time::Duration;

const EXA_URL: &str = "https://mcp.exa.ai/mcp";
const PARALLEL_URL: &str = "https://search.parallel.ai/mcp";

pub async fn exa(
    query: &str,
    count: usize,
    timeout: Duration,
    base: Option<&str>,
) -> Result<Vec<RawSearchHit>, String> {
    let client = client(timeout)?;
    let url = base.unwrap_or(EXA_URL);
    let result = call(
        &client,
        url,
        "web_search_exa",
        json!({
            "query": query,
            "objective": query,
            "numResults": count.clamp(1, 20),
        }),
    )
    .await?;
    Ok(exa_hits(&result))
}

pub async fn parallel(
    query: &str,
    count: usize,
    timeout: Duration,
    base: Option<&str>,
) -> Result<Vec<RawSearchHit>, String> {
    let client = client(timeout)?;
    let url = base.unwrap_or(PARALLEL_URL);
    let result = call(
        &client,
        url,
        "web_search",
        json!({
            "objective": query,
            "search_queries": [query],
            "max_results": count.clamp(1, 20),
        }),
    )
    .await?;
    Ok(parallel_hits(&result))
}

fn client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .user_agent(crate::providers::USER_AGENT)
        .build()
        .map_err(|e| format!("client error: {e}"))
}

/// initialize → notifications/initialized → tools/call, returning the call's
/// `result` object. The session id comes from the initialize response header
/// and must ride every later request.
async fn call(client: &reqwest::Client, url: &str, tool: &str, args: Value) -> Result<Value, String> {
    let mut session: Option<String> = None;
    let _ = post(
        client,
        url,
        &mut session,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "sqwai", "version": env!("CARGO_PKG_VERSION")},
            },
        }),
    )
    .await?;
    let _ = post(
        client,
        url,
        &mut session,
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    )
    .await;
    let (_, mut response) = post(
        client,
        url,
        &mut session,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        }),
    )
    .await?;
    if let Some(error) = response.get("error") {
        return Err(format!("mcp {tool}: {error}"));
    }
    Ok(response
        .get_mut("result")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

async fn post(
    client: &reqwest::Client,
    url: &str,
    session: &mut Option<String>,
    body: Value,
) -> Result<(Option<String>, Value), String> {
    let mut req = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .header(reqwest::header::CONTENT_TYPE, "application/json");
    if let Some(id) = session.as_deref() {
        req = req.header("Mcp-Session-Id", id);
    }
    let response = req
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("mcp request failed: {e}"))?;
    let status = response.status();
    if let Some(id) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        *session = Some(id.to_string());
    }
    let is_stream = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ctype| ctype.contains("text/event-stream"));
    let text = response
        .text()
        .await
        .map_err(|e| format!("mcp body: {e}"))?;
    if !status.is_success() {
        let detail = text.trim().chars().take(200).collect::<String>();
        return Err(if detail.is_empty() {
            format!("mcp HTTP {status}")
        } else {
            format!("mcp HTTP {status}: {detail}")
        });
    }
    let value = parse_body(&text, is_stream)?;
    Ok((session.clone(), value))
}

/// A streamable-HTTP MCP response is either one JSON document or an SSE
/// stream whose `data:` lines carry JSON-RPC messages.
fn parse_body(text: &str, is_stream: bool) -> Result<Value, String> {
    if !is_stream {
        return serde_json::from_str(text)
            .map_err(|e| format!("mcp response is not JSON: {e}"));
    }
    let mut found: Option<Value> = None;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("data:") else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(rest.trim()) else {
            continue;
        };
        if value.get("result").is_some() || value.get("error").is_some() {
            found = Some(value);
        }
    }
    found.ok_or_else(|| "mcp stream carried no response".to_string())
}

/// Exa answers with one text block of `Title: / URL: / Highlights:` records
/// separated by `---` lines.
fn exa_hits(result: &Value) -> Vec<RawSearchHit> {
    let text = result
        .pointer("/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut hits = Vec::new();
    for block in text.split("\n---\n") {
        let mut title = None;
        let mut url = None;
        let mut highlights = String::new();
        let mut in_highlights = false;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("Title:") {
                title = Some(rest.trim().to_string());
                in_highlights = false;
            } else if let Some(rest) = line.strip_prefix("URL:") {
                url = Some(rest.trim().to_string());
                in_highlights = false;
            } else if line.starts_with("Highlights:") {
                in_highlights = true;
            } else if in_highlights && !line.trim().is_empty() && highlights.chars().count() < 600
            {
                if !highlights.is_empty() {
                    highlights.push(' ');
                }
                highlights.push_str(line.trim());
            }
        }
        let (Some(url), Some(title)) = (url, title) else {
            continue;
        };
        if url.is_empty() || title.is_empty() {
            continue;
        }
        hits.push(RawSearchHit {
            title,
            url,
            snippet: highlights,
            source: "exa",
            badge: None,
            boost: 0.0,
            repo_url: None,
        });
    }
    hits
}

/// Parallel answers with a JSON payload (in `structuredContent` or the first
/// text block) holding `results[]` with `url`, `title` and `excerpts[]`.
fn parallel_hits(result: &Value) -> Vec<RawSearchHit> {
    let results = result
        .pointer("/structuredContent/results")
        .cloned()
        .or_else(|| {
            result
                .pointer("/content/0/text")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .and_then(|value| value.get("results").cloned())
        })
        .unwrap_or_default();
    let Some(results) = results.as_array() else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|item| {
            let url = item["url"].as_str()?.trim();
            if url.is_empty() {
                return None;
            }
            let title = item["title"]
                .as_str()
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(url);
            let snippet = item["excerpts"]
                .as_array()
                .map(|excerpts| {
                    excerpts
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .collect::<Vec<_>>()
                        .join(" … ")
                })
                .or_else(|| item["description"].as_str().map(str::to_string))
                .unwrap_or_default();
            Some(RawSearchHit {
                title: title.to_string(),
                url: url.to_string(),
                snippet: snippet.chars().take(600).collect(),
                source: "parallel",
                badge: None,
                boost: 0.0,
                repo_url: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_sse_body_yields_the_rpc_response() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let value = parse_body(body, true).unwrap();
        assert_eq!(value["result"]["ok"], true);
        assert!(parse_body("event: ping\n", true).is_err());
        let plain = parse_body("{\"result\":{\"ok\":true}}", false).unwrap();
        assert_eq!(plain["result"]["ok"], true);
    }

    #[test]
    fn exa_text_blocks_become_hits() {
        let result = json!({
            "content": [{"type": "text", "text":
                "Title: First page\nURL: https://example.org/a\nPublished: N/A\nHighlights:\nsome useful text\n\n---\n\nTitle: Second\nURL: https://example.org/b\nHighlights:\nmore text"}]
        });
        let hits = exa_hits(&result);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://example.org/a");
        assert!(hits[0].snippet.contains("useful"), "{:?}", hits[0].snippet);
        assert_eq!(hits[0].source, "exa");
    }

    #[test]
    fn parallel_payloads_become_hits() {
        let structured = json!({"structuredContent": {"results": [
            {"url": "https://example.org/x", "title": "X", "excerpts": ["one", "two"]}
        ]}});
        let hits = parallel_hits(&structured);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].snippet.contains("one"), "{:?}", hits[0].snippet);

        let text_payload = json!({"content": [{"text": "{\"results\":[{\"url\":\"https://example.org/y\",\"title\":\"Y\"}]}"}]});
        let hits = parallel_hits(&text_payload);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].url, "https://example.org/y");
    }
}
