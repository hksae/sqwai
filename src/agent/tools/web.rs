//! Bounded web retrieval for coding research.

use super::Outcome;
use reqwest::{Client, Url};
use serde_json::Value;
use std::time::Duration;

const MAX_BODY_BYTES: usize = 1_000_000;
/// output cap in chars: head+tail mid-trim, same budget as exec/git
const MAX_OUTPUT_CHARS: usize = 30_000;

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
    if url.host_str().is_none_or(str::is_empty) {
        return Err("webfetch URL must include a host".into());
    }
    host_allowed(&url)?;
    Ok(url)
}

/// SSRF gate: the model chooses the URL, so literal non-public IPs and
/// local metadata names never leave the box. Runs on the initial URL and
/// on every redirect hop (a 302 to 127.0.0.1 is the classic bypass).
/// DNS names are NOT resolved here: resolution races the connect
/// (rebinding), so a hostile-DNS residual remains — documented, not fixed.
fn host_allowed(url: &Url) -> Result<(), String> {
    let host = url.host_str().unwrap_or_default();
    // host_str keeps IPv6 brackets; strip them before parsing
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    // the url crate normalizes WHATWG numeric forms (2130706433,
    // 0x7f.0.0.1) to dotted quads before we ever see them, so parsing
    // the normalized host catches the obfuscated literals too
    if let Ok(addr) = host.parse::<std::net::IpAddr>() {
        if ip_blocked(&addr) {
            return Err(format!("webfetch refuses non-public IP literal {host}"));
        }
        return Ok(());
    }
    let name = host.trim_end_matches('.').to_ascii_lowercase();
    if name == "localhost"
        || name.ends_with(".localhost")
        || name == "metadata.google.internal"
        || name == "metadata.google"
    {
        return Err(format!("webfetch refuses local/metadata host {host}"));
    }
    Ok(())
}

/// True for every IPv4/IPv6 range that is not public unicast: loopback,
/// unspecified, private, link-local (cloud metadata lives at
/// 169.254.169.254), multicast, reserved, documentation, and the v6
/// wrappers that embed a v4 address (mapped/compat/6to4).
fn ip_blocked(addr: &std::net::IpAddr) -> bool {
    match addr {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 0
                || o[0] == 10
                || (o[0] == 100 && (64..128).contains(&o[1]))
                || o[0] == 127
                || (o[0] == 169 && o[1] == 254)
                || (o[0] == 172 && (16..32).contains(&o[1]))
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2))
                || (o[0] == 192 && o[1] == 88 && o[2] == 99)
                || (o[0] == 198 && (18..20).contains(&o[1]))
                || (o[0] == 198 && o[1] == 51 && o[2] == 100)
                || (o[0] == 203 && o[1] == 0 && o[2] == 113)
                || o[0] >= 224
        }
        std::net::IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (s[0] & 0xffc0) == 0xfe80
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xff00) == 0xff00
                || s[0] == 0x2001 && (s[1] == 0xdb8 || s[1] == 0)
                || s[0] == 0x2002
                || s[0] == 0x0064 && s[1] == 0xff9b
                || (s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0)
                || (s[0..5] == [0, 0, 0, 0, 0] || (s[0..5] == [0, 0, 0, 0, 0xffff]))
                    && ip_blocked(&std::net::IpAddr::V4(std::net::Ipv4Addr::new(
                        (s[6] >> 8) as u8,
                        (s[6] & 0xff) as u8,
                        (s[7] >> 8) as u8,
                        (s[7] & 0xff) as u8,
                    )))
        }
    }
}

fn client(timeout: u64, agent: &'static str) -> Result<Client, String> {
    Client::builder()
        .timeout(Duration::from_secs(timeout.clamp(1, 60)))
        .redirect(redirect_policy())
        .user_agent(agent)
        .build()
        .map_err(|e| format!("client error: {e}"))
}

/// Redirects re-enter the SSRF gate at every hop with the same 5-hop
/// budget as before: a benign short link follows, a bounce into
/// 127.0.0.1 or metadata stops the request.
fn redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt: reqwest::redirect::Attempt| {
        if attempt.previous().len() >= 5 {
            return attempt.stop();
        }
        match host_allowed(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(_) => attempt.stop(),
        }
    })
}

pub async fn fetch(args: &Value) -> Outcome {
    let url = match url_arg(args) {
        Ok(url) => url,
        Err(error) => return Outcome::err(error),
    };
    let timeout = args.get("timeout").and_then(Value::as_u64).unwrap_or(15);
    let client = match client(timeout, crate::providers::USER_AGENT) {
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
    let is_html_page = is_html || looks_like_html(&raw);
    let selector = args
        .get("selector")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(selector) = selector {
        if !is_html_page {
            return Outcome::err(
                "webfetch selector requires an HTML page; drop the selector for plain text",
            );
        }
        let fragments = match select_fragments(&raw, selector) {
            Ok(f) => f,
            Err(e) => return Outcome::err(format!("webfetch: {e}")),
        };
        return Outcome::ok(truncate(&fragments));
    }
    let text = if is_html_page {
        html_to_text(&raw)
    } else {
        raw.to_string()
    };
    Outcome::ok(truncate(&text))
}

/// Extract the text of every element matching a CSS selector, one block per
/// element. Errors on a malformed selector or when nothing matches, so the
/// model can retry instead of staring at an empty result.
fn select_fragments(html: &str, selector: &str) -> Result<String, String> {
    use scraper::{Html, Selector};
    let parsed =
        Selector::parse(selector).map_err(|e| format!("invalid selector '{selector}': {e}"))?;
    let doc = Html::parse_document(html);
    let mut blocks: Vec<String> = Vec::new();
    for element in doc.select(&parsed) {
        let text: String = element
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if !text.is_empty() {
            blocks.push(text);
        }
        if blocks.len() >= 200 {
            break;
        }
    }
    if blocks.is_empty() {
        Err(format!(
            "selector '{selector}' matched nothing (or only empty elements)"
        ))
    } else {
        Ok(blocks.join("\n\n"))
    }
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
        let raw_url = decode_entities(&title[1]);
        let url = unwrap_duckduckgo_url(&raw_url);
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

fn unwrap_duckduckgo_url(raw: &str) -> String {
    // DuckDuckGo redirects typically look like:
    // /l/?uddg=https%3A%2F%2Fexample.com%2F... or //duckduckgo.com/l/?uddg=...
    let candidate = if raw.starts_with("//") {
        format!("https:{raw}")
    } else if raw.starts_with('/') {
        format!("https://duckduckgo.com{raw}")
    } else {
        raw.to_string()
    };

    if let Ok(parsed) = Url::parse(&candidate)
        && let Some((_, target)) = parsed.query_pairs().find(|(k, _)| k == "uddg")
        && !target.is_empty()
    {
        return target.into_owned();
    }
    raw.to_string()
}

fn clean_fragment(html: &str) -> String {
    let tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    decode_entities(&tags.replace_all(html, " "))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn html_to_text(html: &str) -> String {
    let no_script =
        regex::Regex::new(r"(?is)<(?:script|style|noscript)[^>]*>.*?</(?:script|style|noscript)>")
            .unwrap();
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
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn truncate(text: &str) -> String {
    super::trim_middle(text.trim(), MAX_OUTPUT_CHARS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_http_urls_and_rejects_other_schemes() {
        assert_eq!(
            url_arg(&json!({"url": " https://example.com/a "}))
                .unwrap()
                .path(),
            "/a"
        );
        for value in ["", "not a url", "file:///tmp/a", "https://"] {
            assert!(
                url_arg(&json!({"url": value})).is_err(),
                "accepted {value:?}"
            );
        }
    }

    /// SSRF gate, no network: loopback/private/link-local literals (plain
    /// and WHATWG-obfuscated), local and metadata names are refused;
    /// public names and public literals pass.
    #[test]
    fn refuses_non_public_hosts() {
        for value in [
            "http://127.0.0.1/",
            "http://127.0.0.1:8080/admin",
            "http://2130706433/",
            "http://0x7f.0.0.1/",
            "http://10.0.0.1/",
            "http://172.16.5.4/",
            "http://172.31.255.255/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost/",
            "http://localhost:3000/",
            "http://LOCALHOST/",
            "http://api.localhost/",
            "http://metadata.google.internal/",
            "https://user:pass@192.168.0.1/",
        ] {
            assert!(
                url_arg(&json!({"url": value})).is_err(),
                "SSRF gate passed {value:?}"
            );
        }
        for value in [
            "https://example.com/",
            "https://example.com./",
            "http://8.8.8.8/",
            "https://crates.io/crates/tokio",
        ] {
            assert!(
                url_arg(&json!({"url": value})).is_ok(),
                "SSRF gate blocked {value:?}"
            );
        }
        // redirect hops re-enter the same gate
        let bounced: Url = "http://127.0.0.1:8080/".parse().unwrap();
        assert!(host_allowed(&bounced).is_err());
        let fine: Url = "https://example.com/target".parse().unwrap();
        assert!(host_allowed(&fine).is_ok());
    }

    #[test]
    fn detects_html_and_removes_embedded_content() {
        assert!(looks_like_html("  <!doctype html><html>"));
        assert!(looks_like_html("<body>content</body>"));
        assert!(!looks_like_html("plain text"));
        let text = html_to_text(
            "<html><head><style>hidden style</style></head><body>Hello &amp; <b>world</b>!<script>secret()</script></body></html>",
        );
        assert_eq!(text, "Hello & world !");
    }

    #[test]
    fn parses_search_results_with_markup_and_limits_count() {
        let html = r#"
            <div class="result results_links">
              <a class="result__a" href="https://example.com/?x=1&amp;y=2">First <b>result</b></a>
              <div class="result__snippet">A useful <b>snippet</b>.</div>
            </div>
            <div class="result results_links">
              <a class="result__a" href="https://second.example/">Second</a>
              <div class="result__snippet">Another result.</div>
            </div>
        "#;
        let output = parse_search_results(html, 1);
        assert!(output.contains("1. First result"));
        assert!(output.contains("https://example.com/?x=1&y=2"));
        assert!(!output.contains("Second"));
        assert_eq!(
            clean_fragment("A useful <b>snippet</b>."),
            "A useful snippet ."
        );

        let ddg_html = r#"
            <div class="result results_links">
              <a class="result__a" href="/l/?uddg=https%3A%2F%2Fcrates.io%2Fcrates%2Ftokio&amp;rut=123">Tokio crate</a>
              <div class="result__snippet">Async runtime.</div>
            </div>
        "#;
        let ddg_output = parse_search_results(ddg_html, 1);
        assert!(ddg_output.contains("https://crates.io/crates/tokio"));
    }

    #[test]
    fn truncates_by_unicode_character_count_and_marks_output() {
        let output = truncate(&"€".repeat(MAX_OUTPUT_CHARS + 10));
        assert!(output.contains("output truncated"), "{output}");
        assert!(output.starts_with("€"), "head survives: {output}");
        assert!(output.ends_with("€"), "tail survives: {output}");
        assert!(output.chars().count() <= MAX_OUTPUT_CHARS + 60, "{output}");
    }

    #[test]
    fn decode_entities_handles_named_and_numeric_refs() {
        assert_eq!(
            decode_entities("&lt;x&gt; &quot;y&quot; &#39;z&#39;"),
            "<x> \"y\" 'z'"
        );
        assert_eq!(
            decode_entities("&amp;lt;"),
            "&lt;",
            "&amp;lt; must not double-decode to <"
        );
    }

    #[test]
    fn selector_extracts_matching_elements_only() {
        let html = r#"<html><head><title>t</title></head><body>
            <table>
              <tr><td>alpha</td><td>1</td></tr>
              <tr><td>beta</td><td>2</td></tr>
            </table>
            <p>outside text</p>
        </body></html>"#;

        let out = select_fragments(html, "td").unwrap();
        assert!(out.contains("alpha"), "{}", out);
        assert!(out.contains("2"), "{}", out);
        assert!(!out.contains("outside"), "{}", out);
        // blocks are separated, not glued into one line
        assert!(out.contains("\n\n"), "{}", out);

        // attribute selectors work
        let out = select_fragments(html, "tr td:first-child").unwrap();
        assert!(out.contains("alpha") && out.contains("beta"), "{}", out);
        assert!(!out.contains("\n1"), "{}", out);

        // malformed selector and empty match are errors, not empty output
        assert!(select_fragments(html, "td[").is_err());
        assert!(select_fragments(html, ".does-not-exist").is_err());
    }
}
